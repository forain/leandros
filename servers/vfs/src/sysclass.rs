//! Hook-backed `/sys/class/power_supply`, `/sys/class/backlight` and `/sys/class/leds`.
//!
//! The VFS cannot depend on the drivers, so the kernel registers an [`Ops`]
//! table (see `drivers::power_sysfs`). The overlay is NARROW ON PURPOSE (the
//! 2026-09-15 /sys shadowing regression blacked out the desktop): only these
//! few class directories, and only while a table is registered AND reports at
//! least one device in that class. Otherwise every path resolves exactly as
//! before (the on-disk /sys).
//!
//! Directories are served as `VnodeKind::SysBlock` with a reserved `dev`
//! encoding (`SC_BASE | class << 8 | device index`, device 0xFF = the class
//! directory) and `level` `LVL_SC_CLASS` / `LVL_SC_DEV`; `block.rs` never sees
//! those values.

use core::sync::atomic::{AtomicUsize, Ordering};

pub const SC_BASE: u16 = 0xF000;
pub const LVL_SC_CLASS: u8 = 0xE0;
pub const LVL_SC_DEV: u8 = 0xE1;
pub const NAME_LEN: usize = 32;

const CLASSES: [&str; 5] = ["power_supply", "backlight", "rtc", "power", "leds"];
/// Index of the pseudo class served directly under `/sys/power` (files, no devices).
const POWER: usize = 3;

/// What the kernel registers. All names are ASCII; every fn must be callable
/// with no VFS lock held and must not re-enter the VFS.
pub struct Ops {
    /// The `idx`-th device of `class`; returns the name length.
    pub devices: fn(class: &str, idx: usize, out: &mut [u8; NAME_LEN]) -> Option<usize>,
    /// The `idx`-th attribute name of `class/dev`.
    pub attrs: fn(class: &str, dev: &str, idx: usize, out: &mut [u8; NAME_LEN]) -> Option<usize>,
    /// Attribute text (with trailing newline).
    pub read: fn(class: &str, dev: &str, attr: &str, out: &mut [u8]) -> Option<usize>,
    /// Is the attribute writable (stat mode 0644 and open for write allowed)?
    pub writable: fn(class: &str, dev: &str, attr: &str) -> bool,
    /// Write attribute text; 0 on success, else a negative errno.
    pub write: fn(class: &str, dev: &str, attr: &str, data: &[u8]) -> i32,
}

static OPS: AtomicUsize = AtomicUsize::new(0);

/// Register the table (last registration wins).
pub fn set_sysclass_ops(ops: &'static Ops) {
    OPS.store(ops as *const Ops as usize, Ordering::Release);
}

fn ops() -> Option<&'static Ops> {
    let p = OPS.load(Ordering::Acquire);
    if p == 0 { None } else { Some(unsafe { &*(p as *const Ops) }) }
}

fn dev_name<'a>(o: &Ops, class: &str, idx: usize, buf: &'a mut [u8; NAME_LEN]) -> Option<&'a str> {
    let n = (o.devices)(class, idx, buf)?;
    core::str::from_utf8(&buf[..n.min(NAME_LEN)]).ok()
}

fn find_dev(o: &Ops, class: &str, name: &str) -> Option<usize> {
    let mut b = [0u8; NAME_LEN];
    (0..32).find(|&i| dev_name(o, class, i, &mut b) == Some(name))
}

/// `(ops, class index, device name, attribute name)` split of an overlay path.
struct Split<'a> { o: &'static Ops, cls: usize, dev: Option<&'a str>, attr: Option<&'a str> }

fn split(path: &[u8]) -> Option<Split<'_>> {
    if let Some(rest) = path.strip_prefix(b"/sys/power") {
        let o = ops()?;
        let mut b = [0u8; NAME_LEN];
        (o.attrs)("power", "", 0, &mut b)?; // registered only while the kernel serves it
        let rest = core::str::from_utf8(rest).ok()?;
        if !rest.is_empty() && !rest.starts_with('/') { return None; }
        let mut it = rest.split('/').skip(1);
        let attr = it.next().filter(|s| !s.is_empty());
        if it.next().is_some() { return Some(Split { o, cls: POWER, dev: None, attr: Some("") }); }
        return Some(Split { o, cls: POWER, dev: None, attr });
    }
    let rest = path.strip_prefix(b"/sys/class/")?;
    let rest = core::str::from_utf8(rest).ok()?;
    let mut it = rest.split('/');
    let class = it.next()?;
    let cls = CLASSES.iter().position(|c| *c == class)?;
    let o = ops()?;
    let mut b = [0u8; NAME_LEN];
    dev_name(o, class, 0, &mut b)?; // at least one device, else off
    let dev = it.next().filter(|s| !s.is_empty());
    let attr = it.next().filter(|s| !s.is_empty());
    if it.next().is_some() { return Some(Split { o, cls, dev: Some(""), attr: Some("") }); }
    Some(Split { o, cls, dev, attr })
}

/// True when `path` is inside an active overlay class directory.
pub fn is_path(path: &[u8]) -> bool {
    is_chardir(path) || super::sysinput::is_path(path) || super::sysdrm::is_path(path) || cpu_split(path).is_some() || split(path).is_some()
}

/// Resolve `path` to an overlay directory as `(dev, level)`.
pub fn dir(path: &[u8]) -> Option<(u16, u8)> {
    if is_chardir(path) { return Some((CHAR_DEV, LVL_CHAR)); }
    if super::sysinput::is_path(path) { return super::sysinput::dir(path); }
    if super::sysdrm::is_path(path) { return super::sysdrm::dir(path); }
    if let Some(c) = cpu_split(path) { return cpu_dir(c); }
    let s = split(path)?;
    if s.cls == POWER { return if s.attr.is_none() { Some((SC_BASE | (POWER as u16) << 8 | 0xFF, LVL_SC_CLASS)) } else { None }; }
    match (s.dev, s.attr) {
        (None, _) => Some((SC_BASE | (s.cls as u16) << 8 | 0xFF, LVL_SC_CLASS)),
        (Some(d), None) => {
            let i = find_dev(s.o, CLASSES[s.cls], d)?;
            Some((SC_BASE | (s.cls as u16) << 8 | i as u16, LVL_SC_DEV))
        }
        _ => None,
    }
}

/// Read an attribute file.
pub fn attr(path: &[u8], out: &mut [u8]) -> Option<usize> {
    if super::sysinput::is_path(path) { return super::sysinput::attr(path, out); }
    if super::sysdrm::is_path(path) { return super::sysdrm::attr(path, out); }
    if let Some(c) = cpu_split(path) { return cpu_attr(c, out); }
    let s = split(path)?;
    if s.cls == POWER { return (s.o.read)("power", "", s.attr?, out); }
    let (d, a) = (s.dev?, s.attr?);
    find_dev(s.o, CLASSES[s.cls], d)?;
    (s.o.read)(CLASSES[s.cls], d, a, out)
}

/// Whether the attribute at `path` exists and is writable.
pub fn is_writable(path: &[u8]) -> bool {
    let Some(s) = split(path) else { return false };
    if s.cls == POWER { return s.attr.map_or(false, |a| (s.o.writable)("power", "", a)); }
    let (Some(d), Some(a)) = (s.dev, s.attr) else { return false };
    (s.o.writable)(CLASSES[s.cls], d, a)
}

/// Gid 44 = `video` (`scripts/mkfs-f2fs-populated.py` group file). Members may
/// write a backlight's `brightness`, as the distro udev rule (0664 root:video)
/// allows; login1 runs as the session user here, not as root.
pub const VIDEO_GID: u32 = 44;

/// Is `path` a `backlight/<dev>/brightness` attribute that exists and is writable?
/// The only sysclass attribute group `video` may write.
pub fn is_video_writable(path: &[u8]) -> bool {
    let Some(s) = split(path) else { return false };
    if !matches!(CLASSES[s.cls], "backlight" | "leds") || s.attr != Some("brightness") { return false; }
    is_writable(path)
}

/// Permission decision for opening a sysclass attribute for writing.
/// Root always (when the attribute is writable at all); members of
/// `VIDEO_GID` (egid or supplementary) only for `video_attr`.
pub fn write_allowed(host_root: bool, egid: u32, groups: &[u32], writable: bool, video_attr: bool) -> bool {
    if !writable { return false; }
    host_root || (video_attr && (egid == VIDEO_GID || groups.contains(&VIDEO_GID)))
}

#[cfg(test)]
mod perm_tests {
    use super::*;
    #[test]
    fn decision() {
        assert!(write_allowed(true, 0, &[], true, false));
        assert!(!write_allowed(true, 0, &[], false, false));
        assert!(write_allowed(false, 1000, &[1000, 44], true, true));
        assert!(write_allowed(false, 44, &[], true, true));
        assert!(!write_allowed(false, 1000, &[1000, 10], true, true));
        assert!(!write_allowed(false, 1000, &[44], true, false));
        assert!(!write_allowed(false, 44, &[], false, true));
    }
}

/// Write to an attribute (0 or a negative errno).
pub fn write(path: &[u8], data: &[u8]) -> i32 {
    let Some(s) = split(path) else { return -2 };
    if s.cls == POWER {
        let Some(a) = s.attr else { return -21 };
        if !(s.o.writable)("power", "", a) { return -13; }
        return (s.o.write)("power", "", a, data);
    }
    let (Some(d), Some(a)) = (s.dev, s.attr) else { return -21 };
    if !(s.o.writable)(CLASSES[s.cls], d, a) { return -13; }
    (s.o.write)(CLASSES[s.cls], d, a, data)
}

/// The `idx`-th entry of an overlay directory: `(name length, is_dir)`.
pub fn dirent(dev: u16, level: u8, idx: usize, out: &mut [u8; NAME_LEN]) -> Option<(usize, bool)> {
    if dev == SC_TABLES { return tables_dirent(idx, out); }
    if dev == CHAR_DEV { return chardir_dirent(idx, out); }
    if (dev >> 8) & 0xF == super::sysinput::SI_CLS { return super::sysinput::dirent(dev, level, idx, out); }
    if (dev >> 8) & 0xF == super::sysdrm::SD_CLS { return super::sysdrm::dirent(dev, level, idx, out); }
    if (dev >> 8) & 0xF == CPU_CLS as u16 { return cpu_dirent(dev, idx, out); }
    let o = ops()?;
    let cls = ((dev >> 8) & 0xF) as usize;
    let class = CLASSES.get(cls)?;
    match level {
        LVL_SC_CLASS if cls == POWER => Some(((o.attrs)("power", "", idx, out)?, false)),
        LVL_SC_CLASS => Some(((o.devices)(class, idx, out)?, true)),
        LVL_SC_DEV => {
            let mut b = [0u8; NAME_LEN];
            let d = dev_name(o, class, (dev & 0xFF) as usize, &mut b)?;
            Some(((o.attrs)(class, d, idx, out)?, false))
        }
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// `/sys/firmware/acpi/tables/<SIG>[n]`: the raw ACPI tables, Linux-style
// (DSDT, SSDT1.., FACP, ECDT), root-only. Served while a provider is
// registered; the directory uses `SC_BASE | 0xF << 8 | 0xFF`.

/// `idx`-th table: writes its file name into `name`, returns (name length, bytes).
pub type TableFn = fn(idx: usize, name: &mut [u8; NAME_LEN]) -> Option<(usize, &'static [u8])>;

static TABLES: AtomicUsize = AtomicUsize::new(0);
const TABLES_DIR: &[u8] = b"/sys/firmware/acpi/tables";
// Must not collide with block.rs's NO_DEV (0xFFFF, /sys/class/block itself) or
// LOOP_CONTROL (0xFFFE): both live above SC_BASE too, and with the tables dir
// at 0xFFFF `ls /sys/class/block` listed the ACPI tables (DSDT, FACP).
pub const SC_TABLES: u16 = SC_BASE | 0xF << 8 | 0xFD;

/// True when `dev` of a `VnodeKind::SysBlock` is a sysclass overlay directory
/// rather than a block-registry slot (block.rs reserves 0xFFFE/0xFFFF).
pub fn is_overlay_dev(dev: u16) -> bool { dev >= SC_BASE && dev < 0xFFFE }

pub fn set_acpi_tables(f: TableFn) {
    TABLES.store(f as usize, Ordering::Release);
}

fn tables() -> Option<TableFn> {
    let p = TABLES.load(Ordering::Acquire);
    if p == 0 { None } else { Some(unsafe { core::mem::transmute::<usize, TableFn>(p) }) }
}

/// True for the tables directory itself (when a provider is registered).
pub fn is_tables_dir(path: &[u8]) -> bool {
    tables().is_some() && (path == TABLES_DIR || path.strip_prefix(TABLES_DIR) == Some(b"/"))
}

/// The bytes of `/sys/firmware/acpi/tables/<name>`.
pub fn acpi_table(path: &[u8]) -> Option<&'static [u8]> {
    let f = tables()?;
    let name = path.strip_prefix(TABLES_DIR)?.strip_prefix(b"/")?;
    let mut b = [0u8; NAME_LEN];
    (0..64).find_map(|i| f(i, &mut b).filter(|(n, _)| &b[..*n] == name).map(|(_, t)| t))
}

/// True for any path this table overlay answers.
pub fn is_tables_path(path: &[u8]) -> bool {
    is_tables_dir(path) || acpi_table(path).is_some()
}

/// The `idx`-th entry of the tables directory.
pub fn tables_dirent(idx: usize, out: &mut [u8; NAME_LEN]) -> Option<(usize, bool)> {
    Some((tables()?(idx, out)?.0, false))
}

// ---------------------------------------------------------------------------
// `/sys/devices/system/cpu` and `/sys/devices/cpu`: the CPU topology files
// userspace (Firefox's sandbox broker, glibc/musl sysconf, nproc) reads.
// Purely additive: ONLY these two subtrees are claimed (not `/sys/devices` or
// `/sys/devices/system`, which stay on-disk lookups), so no existing /sys
// entry changes resolution. Dirs reuse `SC_BASE | CPU_CLS << 8 | id` with id
// 0xFF = system/cpu, 0xFE = devices/cpu, n = cpu<n>.

const CPU_CLS: u8 = 0xE;
const CPU_ROOT: &[u8] = b"/sys/devices/system/cpu";
const PMU_ROOT: &[u8] = b"/sys/devices/cpu";
const CPU_FILES: [&str; 4] = ["present", "possible", "online", "kernel_max"];

/// `(id, file)`: id 0xFF system/cpu, 0xFE devices/cpu, n = cpu<n>; `file` is the leaf, if any.
#[derive(Clone, Copy)]
struct CpuSplit<'a> { id: u8, file: Option<&'a str> }

fn ncpu() -> usize { sched::active_cpu_count().clamp(1, 0xF0) }

fn cpu_split(path: &[u8]) -> Option<CpuSplit<'_>> {
    if let Some(r) = path.strip_prefix(PMU_ROOT) {
        return if r.is_empty() || r == b"/" { Some(CpuSplit { id: 0xFE, file: None }) } else { None };
    }
    let r = path.strip_prefix(CPU_ROOT)?;
    if r.is_empty() || r == b"/" { return Some(CpuSplit { id: 0xFF, file: None }); }
    let r = core::str::from_utf8(r.strip_prefix(b"/")?).ok()?;
    let mut it = r.split('/');
    let first = it.next()?;
    let rest = it.next().filter(|s| !s.is_empty());
    if it.next().is_some() { return None; }
    if let Some(n) = first.strip_prefix("cpu") {
        if n.is_empty() || n.starts_with('0') && n.len() > 1 { return None; }
        let n: usize = n.parse().ok()?;
        if n >= ncpu() { return None; }
        return match rest { None => Some(CpuSplit { id: n as u8, file: None }),
                            Some(f) if f == "online" && n > 0 => Some(CpuSplit { id: n as u8, file: Some("online") }),
                            _ => None };
    }
    if rest.is_some() || !CPU_FILES.contains(&first) { return None; }
    Some(CpuSplit { id: 0xFF, file: Some(first) })
}

fn cpu_dir(c: CpuSplit<'_>) -> Option<(u16, u8)> {
    if c.file.is_some() { return None; }
    Some((SC_BASE | (CPU_CLS as u16) << 8 | c.id as u16, LVL_SC_CLASS))
}

fn cpu_attr(c: CpuSplit<'_>, out: &mut [u8]) -> Option<usize> {
    let f = c.file?;
    let n = ncpu();
    let mut s = alloc::string::String::new();
    use core::fmt::Write;
    let _ = match f {
        "kernel_max" => writeln!(s, "{}", n - 1),
        "online" if c.id != 0xFF => writeln!(s, "1"),
        _ if n > 1 => writeln!(s, "0-{}", n - 1),
        _ => writeln!(s, "0"),
    };
    let b = s.as_bytes();
    if b.len() > out.len() { return None; }
    out[..b.len()].copy_from_slice(b);
    Some(b.len())
}

fn cpu_dirent(dev: u16, idx: usize, out: &mut [u8; NAME_LEN]) -> Option<(usize, bool)> {
    let id = (dev & 0xFF) as u8;
    let (name, dir): (alloc::string::String, bool) = match id {
        0xFF => {
            if idx < CPU_FILES.len() { (CPU_FILES[idx].into(), false) }
            else if idx - CPU_FILES.len() < ncpu() { (alloc::format!("cpu{}", idx - CPU_FILES.len()), true) }
            else { return None }
        }
        0xFE => return None,
        n if (n as usize) < ncpu() && n > 0 && idx == 0 => ("online".into(), false),
        _ => return None,
    };
    let b = name.as_bytes();
    out[..b.len()].copy_from_slice(b);
    Some((b.len(), dir))
}

// ---------------------------------------------------------------------------
// `/sys/dev/char`: the directory itself is generated so its listing shows the
// DRM nodes (226:*, staged on disk) next to the evdev nodes (13:*, symlinks to
// the class entries). Children other than `13:*` stay on disk.

const CHAR_CLS: u16 = 0xB;
pub const LVL_CHAR: u8 = 0xB0;
pub const CHAR_DEV: u16 = SC_BASE | CHAR_CLS << 8 | 0xFF;

fn is_chardir(path: &[u8]) -> bool {
    let mut p = path;
    while p.len() > 1 && p[p.len() - 1] == b'/' { p = &p[..p.len() - 1]; }
    p == b"/sys/dev/char" && (super::sysdrm::present() || super::sysinput::present())
}

fn chardir_dirent(idx: usize, out: &mut [u8; NAME_LEN]) -> Option<(usize, bool)> {
    let mut idx = idx;
    if super::sysdrm::present() {
        let names: [&str; 2] = ["226:0", "226:128"];
        if idx < 2 { out[..names[idx].len()].copy_from_slice(names[idx].as_bytes()); return Some((names[idx].len(), false)); }
        idx -= 2;
    }
    super::sysinput::char_dirent(idx, out)
}

/// Is entry `idx` of overlay directory `(dev, level)` a symlink (DT_LNK)?
pub fn dirent_link(dev: u16, level: u8, idx: usize) -> bool {
    if dev == CHAR_DEV { return true; }
    match (dev >> 8) & 0xF {
        c if c == super::sysinput::SI_CLS => super::sysinput::dirent_link(level, idx),
        c if c == super::sysdrm::SD_CLS => super::sysdrm::dirent_link(level, idx),
        _ => false,
    }
}

/// Target of the generated symlink at `path` (readlink body); `None` when it is not one.
pub fn link(path: &[u8], out: &mut [u8]) -> Option<usize> {
    if let Some(t) = super::sysdrm::link(path) {
        if t.len() > out.len() { return None; }
        out[..t.len()].copy_from_slice(&t);
        return Some(t.len());
    }
    let t = super::sysinput::link(path)?;
    let b = t.as_bytes();
    if b.len() > out.len() { return None; }
    out[..b.len()].copy_from_slice(b);
    Some(b.len())
}

/// Expand generated symlinks inside `path` (all components, and the last one
/// only when `follow`). `Ok(None)` when none is met. The result is normalised.
pub fn expand(path: &[u8], follow: bool) -> Result<Option<alloc::vec::Vec<u8>>, i32> {
    if !(path.starts_with(b"/sys/dev/char/13:") || path.starts_with(b"/sys/class/input/")
        || path.starts_with(b"/sys/class/drm/card0") || path.starts_with(b"/sys/dev/char/226:0")) { return Ok(None); }
    let mut cur = path.to_vec();
    let mut hops = 0;
    let mut changed = false;
    // The staged card links (`/sys/class/drm/card0`, `/sys/dev/char/226:0`)
    // are followed here so the generated card directory (connectors) is seen
    // through them as well; the filesystem server would resolve them itself,
    // out of reach of the overlay.
    if super::sysdrm::present() {
        for (pfx, base) in [(&b"/sys/class/drm/card0"[..], &b"/sys/class/drm"[..]), (&b"/sys/dev/char/226:0"[..], &b"/sys/dev/char"[..])] {
            if !cur.starts_with(pfx) { continue; }
            let rest = &cur[pfx.len()..];
            if rest.is_empty() { if !follow { continue; } } else if rest[0] != b'/' { continue; }
            let Some(t) = super::sys_readlink(pfx) else { continue };
            let mut next = base.to_vec();
            next.push(b'/');
            next.extend_from_slice(&t);
            next.extend_from_slice(rest);
            cur = normalize(&next);
            changed = true;
            break;
        }
    }
    'again: loop {
        let mut end = 1;
        while end < cur.len() {
            let mut e = end;
            while e < cur.len() && cur[e] != b'/' { e += 1; }
            let last = e >= cur.len();
            if !(last && !follow) {
                let mut tb = [0u8; 96];
                if let Some(n) = link(&cur[..e], &mut tb) {
                    hops += 1;
                    if hops > 8 { return Err(-40); }
                    let parent_end = cur[..e].iter().rposition(|&c| c == b'/').unwrap_or(0);
                    let mut next = cur[..parent_end].to_vec();
                    next.push(b'/');
                    next.extend_from_slice(&tb[..n]);
                    next.extend_from_slice(&cur[e..]);
                    cur = normalize(&next);
                    changed = true;
                    continue 'again;
                }
            }
            end = e + 1;
        }
        break;
    }
    Ok(if changed { Some(cur) } else { None })
}

fn normalize(p: &[u8]) -> alloc::vec::Vec<u8> {
    let mut comps: alloc::vec::Vec<&[u8]> = alloc::vec::Vec::new();
    for c in p.split(|&b| b == b'/') {
        match c { b"" | b"." => {}, b".." => { comps.pop(); }, c => comps.push(c) }
    }
    let mut out = alloc::vec::Vec::new();
    for c in comps { out.push(b'/'); out.extend_from_slice(c); }
    if out.is_empty() { out.push(b'/'); }
    out
}

// ---------------------------------------------------------------------------
// Path of an overlay directory from its `(dev, level)` id: what an O_PATH fd
// of a synthesized directory answers to /proc/self/fd/N readlink, openat and
// bind-mount source resolution. Generic on purpose: it walks the overlay from
// its roots through `dir`/`dirent`, so every directory any overlay module can
// resolve names its own path with no per-module inverse to keep in sync.

const ROOTS: [&[u8]; 12] = [
    b"/sys/class/power_supply", b"/sys/class/backlight", b"/sys/class/leds", b"/sys/class/rtc", b"/sys/power",
    b"/sys/devices/system/cpu", b"/sys/devices/cpu", b"/sys/firmware/acpi/tables",
    b"/sys/class/drm", b"/sys/class/input", b"/sys/dev/char", b"/sys/dev/char/226:0",
];

/// First (breadth-first) path whose overlay directory id is `(dev, level)`.
pub fn dir_path(dev: u16, level: u8) -> Option<alloc::vec::Vec<u8>> {
    use alloc::vec::Vec;
    let mut q: Vec<Vec<u8>> = ROOTS.iter().map(|r| r.to_vec()).collect();
    // The DRM card directory sits under the card's real /sys/devices path.
    if let Some(t) = super::sys_readlink(b"/sys/class/drm/card0") {
        let mut p = b"/sys/class/drm/".to_vec();
        p.extend_from_slice(&t);
        q.push(normalize(&p));
    }
    let mut i = 0;
    while i < q.len() && i < 600 {
        let p = q[i].clone();
        i += 1;
        let Some((d, l)) = (if is_path(&p) || is_tables_path(&p) { dir_or_tables(&p) } else { None }) else { continue };
        if d == dev && l == level { return Some(p); }
        let mut name = [0u8; NAME_LEN];
        for idx in 0..256 {
            let Some((n, _)) = dirent(d, l, idx, &mut name) else { break };
            // Symlinks (class/drm/card0, dev/char/226:0) are not directories of
            // their own: the canonical path goes through the real tree.
            if dirent_link(d, l, idx) { continue; }
            let mut c = p.clone();
            c.push(b'/');
            c.extend_from_slice(&name[..n]);
            q.push(c);
        }
    }
    None
}

fn dir_or_tables(p: &[u8]) -> Option<(u16, u8)> {
    if is_tables_dir(p) { return Some((SC_TABLES, LVL_SC_CLASS)); }
    dir(p)
}
