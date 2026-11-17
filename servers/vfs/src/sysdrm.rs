//! Hook-backed `/sys/class/drm/card0-<TYPE>-<n>` connector directories.
//!
//! The DRM stack registers an [`Ops`] table (the VFS cannot depend on the
//! drivers) that reports the live GETCONNECTOR state, so `status`, `modes`,
//! `enabled` and `dpms` agree with what the ioctl answers.
//!
//! NARROW ON PURPOSE (the 2026-09-15 /sys shadowing regression blacked out the
//! desktop): only paths under `/sys/class/drm/card0-*` are claimed, and only
//! while a table is registered. The class directory itself, the `card0` /
//! `renderD128` symlinks, `/sys/dev/char/226:*` and `device/*` stay on disk
//! (staged Linux-shaped by mkfs-f2fs-populated.py: real directories under
//! `/sys/devices/pci.../drm/cardN`, the class and char entries are links into
//! it). The same connector directories are also answered at
//! `/sys/devices/.../drm/card0/card0-<TYPE>-<n>` (lookup by name; the staged
//! card directory's own listing does not show them).
//!
//! ```text
//! /sys/class/drm/card0-<TYPE>-<n>/{status,enabled,modes,dpms,edid}
//! ```
//!
//! Directories are `VnodeKind::SysBlock` with `dev = SC_BASE | 0xC << 8 | i`.

use core::sync::atomic::{AtomicUsize, Ordering};
use super::sysclass::{SC_BASE, NAME_LEN};

pub const SD_CLS: u16 = 0xC;
pub const LVL_CONN: u8 = 0xC0;
pub const LVL_CLASS: u8 = 0xC1;
pub const LVL_CARD: u8 = 0xC2;
const CLASS_DIR: &[u8] = b"/sys/class/drm";
pub const MAX_CONN: usize = 8;

#[derive(Clone, Copy)]
pub struct Conn {
    /// DRM_MODE_CONNECTOR_*
    pub conn_type: u32,
    pub type_id: u32,
    pub connected: bool,
    pub nmodes: usize,
    /// width, height, Hz
    pub modes: [(u32, u32, u32); 17],
}

pub struct Ops {
    /// Connector `i` (dense from 0), `None` past the end or without a DRM device.
    pub conn: fn(i: usize) -> Option<Conn>,
}

static OPS: AtomicUsize = AtomicUsize::new(0);

pub fn set_ops(ops: &'static Ops) { OPS.store(ops as *const Ops as usize, Ordering::Release); }

fn ops() -> Option<&'static Ops> {
    let p = OPS.load(Ordering::Acquire);
    if p == 0 { None } else { Some(unsafe { &*(p as *const Ops) }) }
}

const PREFIX: &[u8] = b"/sys/class/drm/card0-";
const FILES: [&str; 5] = ["status", "enabled", "modes", "dpms", "edid"];

/// Linux `drm_connector_enum_list` names.
fn type_name(t: u32) -> &'static str {
    match t {
        1 => "VGA", 2 => "DVI-I", 3 => "DVI-D", 4 => "DVI-A", 5 => "Composite", 6 => "SVIDEO",
        7 => "LVDS", 8 => "Component", 9 => "DIN", 10 => "DP", 11 => "HDMI-A", 12 => "HDMI-B",
        13 => "TV", 14 => "eDP", 15 => "Virtual", 16 => "DSI", 17 => "DPI", 18 => "Writeback",
        19 => "SPI", 20 => "USB", _ => "Unknown",
    }
}

struct W<'a> { out: &'a mut [u8], pos: usize }
impl core::fmt::Write for W<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let b = s.as_bytes();
        let n = b.len().min(self.out.len() - self.pos);
        self.out[self.pos..self.pos + n].copy_from_slice(&b[..n]);
        self.pos += n;
        Ok(())
    }
}

/// `card0-<TYPE>-<n>` of connector `c`, into `out`; returns the length.
fn conn_name(c: &Conn, out: &mut [u8]) -> usize {
    use core::fmt::Write;
    let mut w = W { out, pos: 0 };
    let _ = write!(w, "card0-{}-{}", type_name(c.conn_type), c.type_id);
    w.pos
}

pub fn present() -> bool { ops().is_some() }

pub fn is_path(path: &[u8]) -> bool {
    let p = trim(path);
    (p.starts_with(PREFIX) || p == CLASS_DIR || card_rest(p).is_some() || is_card_dir(p)) && ops().is_some()
}

/// `/sys/devices/<...>/drm/card0` itself: its listing is generated (the staged
/// entries plus the connector directories, as Linux nests them under the card).
fn is_card_dir(p: &[u8]) -> bool {
    p.starts_with(b"/sys/devices/") && p.ends_with(b"/drm/card0")
}

const CLASS_CARD0: &[u8] = b"/sys/class/drm/card0";

/// Target of the generated `/sys/class/drm/card0-<TYPE>-<n>` symlink: the
/// connector directory under the card's real directory, relative to the class
/// directory (the card link's own target plus the connector name).
pub fn link(path: &[u8]) -> Option<alloc::vec::Vec<u8>> {
    let p = trim(path);
    if !p.starts_with(PREFIX) || p[PREFIX.len()..].contains(&b'/') { return None; }
    let o = ops()?;
    let name = &p[b"/sys/class/drm/".len()..];
    let mut nb = [0u8; 48];
    (0..MAX_CONN).find(|&i| (o.conn)(i).map_or(false, |c| { let l = conn_name(&c, &mut nb); &nb[..l] == name }))?;
    let mut t = super::sys_readlink(CLASS_CARD0)?;
    t.push(b'/');
    t.extend_from_slice(name);
    Some(t)
}

/// For `/sys/devices/<...>/drm/card0/card0-<TYPE>-<n>[/file]`: the part after
/// `card0/` (the connector directories Linux nests under the card; the card
/// directory itself and its `device`/`subsystem` links are staged on disk).
fn card_rest(p: &[u8]) -> Option<&[u8]> {
    const MARK: &[u8] = b"/drm/card0/card0-";
    if !p.starts_with(b"/sys/devices/") { return None; }
    let k = p.windows(MARK.len()).position(|w| w == MARK)?;
    Some(&p[k + b"/drm/card0/".len()..])
}

fn trim(p: &[u8]) -> &[u8] {
    let mut p = p;
    while p.len() > 1 && p[p.len() - 1] == b'/' { p = &p[..p.len() - 1]; }
    p
}

enum Node { Class, Card, Dir(usize), File(usize, &'static str) }

fn parse(path: &[u8]) -> Option<Node> {
    let o = ops()?;
    if trim(path) == CLASS_DIR { return Some(Node::Class); }
    if is_card_dir(trim(path)) { return Some(Node::Card); }
    let rest = match card_rest(trim(path)) {
        Some(r) => r,
        None => trim(path).strip_prefix(b"/sys/class/drm/")?,
    };
    let (name, file) = match rest.iter().position(|&c| c == b'/') {
        Some(k) => (&rest[..k], Some(&rest[k + 1..])),
        None => (rest, None),
    };
    let mut nb = [0u8; 48];
    let i = (0..MAX_CONN).find(|&i| {
        (o.conn)(i).map_or(false, |c| { let l = conn_name(&c, &mut nb); &nb[..l] == name })
    })?;
    match file {
        None => Some(Node::Dir(i)),
        Some(f) => {
            let f = core::str::from_utf8(f).ok()?;
            FILES.iter().find(|x| **x == f).map(|x| Node::File(i, x))
        }
    }
}

/// Resolve `path` to an overlay directory as `(dev, level)`.
pub fn dir(path: &[u8]) -> Option<(u16, u8)> {
    match parse(path)? {
        Node::Dir(i) => Some((SC_BASE | SD_CLS << 8 | i as u16, LVL_CONN)),
        Node::Class => Some((SC_BASE | SD_CLS << 8 | 0xFF, LVL_CLASS)),
        Node::Card => Some((SC_BASE | SD_CLS << 8 | 0xFE, LVL_CARD)),
        _ => None,
    }
}

/// Read an attribute file.
pub fn attr(path: &[u8], out: &mut [u8]) -> Option<usize> {
    use core::fmt::Write;
    let o = ops()?;
    let (i, f) = match parse(path)? { Node::File(i, f) => (i, f), _ => return None };
    let c = (o.conn)(i)?;
    let mut w = W { out, pos: 0 };
    match f {
        "status" => { let _ = w.write_str(if c.connected { "connected\n" } else { "disconnected\n" }); }
        "enabled" => { let _ = w.write_str(if c.connected { "enabled\n" } else { "disabled\n" }); }
        "dpms" => { let _ = w.write_str(if c.connected { "On\n" } else { "Off\n" }); }
        "modes" => {
            for &(wd, ht, _) in &c.modes[..c.nmodes.min(c.modes.len())] { let _ = writeln!(w, "{}x{}", wd, ht); }
        }
        // No EDID is exposed to the guest (Linux: a zero-length file).
        _ => {}
    }
    Some(w.pos)
}

/// The `idx`-th entry of overlay directory `(dev, level)`: `(name length, is_dir)`.
pub fn dirent(_dev: u16, level: u8, idx: usize, out: &mut [u8; NAME_LEN]) -> Option<(usize, bool)> {
    if level == LVL_CLASS {
        // Linux order: the nodes first, then the connectors (`card0-<TYPE>-<n>`).
        const NODES: [&str; 2] = ["card0", "renderD128"];
        if idx < 2 { out[..NODES[idx].len()].copy_from_slice(NODES[idx].as_bytes()); return Some((NODES[idx].len(), false)); }
        let o = ops()?;
        let c = (o.conn)(idx - 2)?;
        // Linux: the connectors in the class directory are symlinks.
        return Some((conn_name(&c, &mut out[..]), false));
    }
    if level == LVL_CARD {
        // The staged entries of the card directory, then its connectors.
        const STAGED: [(&str, bool); 4] = [("uevent", false), ("dev", false), ("device", false), ("subsystem", false)];
        if idx < STAGED.len() { out[..STAGED[idx].0.len()].copy_from_slice(STAGED[idx].0.as_bytes()); return Some((STAGED[idx].0.len(), false)); }
        let c = (ops()?.conn)(idx - STAGED.len())?;
        return Some((conn_name(&c, &mut out[..]), true));
    }
    if level != LVL_CONN { return None; }
    let f = FILES.get(idx)?;
    out[..f.len()].copy_from_slice(f.as_bytes());
    Some((f.len(), false))
}

/// Class directory entries are all symlinks; `device`/`subsystem` of the card are too.
pub fn dirent_link(level: u8, idx: usize) -> bool {
    (level == LVL_CLASS) || (level == LVL_CARD && (idx == 2 || idx == 3))
}
