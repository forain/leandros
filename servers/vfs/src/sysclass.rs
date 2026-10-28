//! Hook-backed `/sys/class/power_supply` and `/sys/class/backlight`.
//!
//! The VFS cannot depend on the drivers, so the kernel registers an [`Ops`]
//! table (see `drivers::power_sysfs`). The overlay is NARROW ON PURPOSE (the
//! 2026-09-15 /sys shadowing regression blacked out the desktop): only these
//! two class directories, and only while a table is registered AND reports at
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

const CLASSES: [&str; 4] = ["power_supply", "backlight", "rtc", "power"];
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
    split(path).is_some()
}

/// Resolve `path` to an overlay directory as `(dev, level)`.
pub fn dir(path: &[u8]) -> Option<(u16, u8)> {
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
