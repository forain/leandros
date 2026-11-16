//! Hook-backed `/sys/class/input` and `/sys/dev/char/13:<minor>`.
//!
//! Generated from the evdev server's live registry (the VFS cannot depend on
//! the evdev crate, so it registers an [`Ops`] table at init), which means a
//! hot-plugged node (event2..) appears here the moment it is registered and
//! vanishes on unplug, and `capabilities/*` are exactly what EVIOCGBIT answers.
//!
//! NARROW ON PURPOSE (the 2026-09-15 /sys shadowing regression blacked out the
//! desktop): only `/sys/class/input[/..]` and `/sys/dev/char/13:*` are claimed,
//! and only while a table is registered. Everything else under /sys resolves
//! from disk as before. The `/sys/dev/char` directory itself stays on disk, so
//! a listing of it does not show the 13:* entries; lookups by number do.
//!
//! Layout (event N -> minor 64+N; directories are plain directories, the
//! overlay has no symlinks):
//!
//! ```text
//! /sys/class/input/event<N> = /sys/dev/char/13:<64+N>
//!     dev  uevent  device/
//! /sys/class/input/input<N> = .../event<N>/device
//!     name  phys  uniq  uevent  id/{bustype,vendor,product,version}
//!     capabilities/{ev,key,rel,abs,msc,led,sw,ff,snd}
//! ```
//!
//! Directories are served as `VnodeKind::SysBlock` with `dev = SC_BASE | 0xD << 8 | N`
//! (0xFF = the class directory), dispatched from `sysclass`.

use core::sync::atomic::{AtomicUsize, Ordering};
use super::sysclass::{SC_BASE, NAME_LEN};

pub const SI_CLS: u16 = 0xD;
pub const LVL_CLASS: u8 = 0xD0;
pub const LVL_EVENT: u8 = 0xD1;
pub const LVL_DEVICE: u8 = 0xD2;
pub const LVL_CAPS: u8 = 0xD3;
pub const LVL_ID: u8 = 0xD4;
pub const MAX_N: usize = 16;
const MINOR_BASE: usize = 64;

pub const NAME_MAX: usize = 64;

#[derive(Clone, Copy)]
pub struct Info {
    pub name: [u8; NAME_MAX],
    pub name_len: usize,
    /// bustype, vendor, product, version
    pub ids: [u16; 4],
}

impl Info {
    pub const fn empty() -> Self { Info { name: [0; NAME_MAX], name_len: 0, ids: [0; 4] } }
}

/// What the evdev server registers; both must be callable with no VFS lock held.
pub struct Ops {
    /// Identity of live node `n`, `None` when it does not exist.
    pub info: fn(n: usize) -> Option<Info>,
    /// Capability bitmap for event type `ev` (what EVIOCGBIT answers), into zeroed `out`.
    pub bits: fn(n: usize, ev: usize, out: &mut [u8]),
}

static OPS: AtomicUsize = AtomicUsize::new(0);

pub fn set_ops(ops: &'static Ops) { OPS.store(ops as *const Ops as usize, Ordering::Release); }

fn ops() -> Option<&'static Ops> {
    let p = OPS.load(Ordering::Acquire);
    if p == 0 { None } else { Some(unsafe { &*(p as *const Ops) }) }
}

const CLASS: &[u8] = b"/sys/class/input";
const CHAR13: &[u8] = b"/sys/dev/char/13:";

/// True when `path` is one of the paths this overlay answers (cheap prefix test first).
pub fn is_path(path: &[u8]) -> bool {
    let p = trim(path);
    (p == CLASS || p.starts_with(b"/sys/class/input/") || p.starts_with(CHAR13)) && ops().is_some()
}

fn trim(p: &[u8]) -> &[u8] {
    let mut p = p;
    while p.len() > 1 && p[p.len() - 1] == b'/' { p = &p[..p.len() - 1]; }
    p
}

#[derive(Clone, Copy, PartialEq)]
enum Node {
    Class,
    /// event dir (or char node dir)
    Event(usize),
    /// device-level dir (`inputN`, `eventN/device`, `13:M/device`)
    Device(usize),
    Caps(usize),
    Id(usize),
    /// file under Event: dev / uevent
    EvFile(usize, &'static str),
    /// file under Device
    DevFile(usize, &'static str),
    CapFile(usize, &'static str),
    IdFile(usize, &'static str),
}

const EV_FILES: [&str; 2] = ["dev", "uevent"];
const DEV_FILES: [&str; 3] = ["name", "phys", "uniq"];
const DEV_UEVENT: &str = "uevent";
const CAPS: [(&str, usize); 9] = [("ev", 0), ("key", 1), ("rel", 2), ("abs", 3), ("msc", 4),
                                  ("led", 0x11), ("sw", 5), ("ff", 0x15), ("snd", 0x12)];
const IDS: [&str; 4] = ["bustype", "vendor", "product", "version"];

fn parse_num(s: &str, prefix: &str) -> Option<usize> {
    let r = s.strip_prefix(prefix)?;
    if r.is_empty() || r.len() > 3 || (r.len() > 1 && r.starts_with('0')) || !r.bytes().all(|c| c.is_ascii_digit()) { return None; }
    r.parse().ok()
}

fn live(o: &Ops, n: usize) -> Option<usize> {
    if n < MAX_N && (o.info)(n).is_some() { Some(n) } else { None }
}

fn parse(path: &[u8]) -> Option<Node> {
    let o = ops()?;
    let s = core::str::from_utf8(trim(path)).ok()?;
    let mut seg = [""; 8];
    let mut cnt = 0;
    for p in s.split('/').skip(1) {
        if cnt == seg.len() || p.is_empty() { return None; }
        seg[cnt] = p;
        cnt += 1;
    }
    if cnt < 2 || seg[0] != "sys" { return None; }
    let (n, input_dir, rest): (usize, bool, &[&str]) = match seg[1] {
        "class" => {
            if cnt < 3 || seg[2] != "input" { return None; }
            if cnt == 3 { return Some(Node::Class); }
            if let Some(n) = parse_num(seg[3], "event") { (live(o, n)?, false, &seg[4..cnt]) }
            else if let Some(n) = parse_num(seg[3], "input") { (live(o, n)?, true, &seg[4..cnt]) }
            else { return None }
        }
        "dev" => {
            if cnt < 4 || seg[2] != "char" { return None; }
            let m: usize = seg[3].strip_prefix("13:")?.parse().ok()?;
            if m < MINOR_BASE { return None; }
            (live(o, m - MINOR_BASE)?, false, &seg[4..cnt])
        }
        _ => return None,
    };
    if input_dir {
        return match rest {
            [] => Some(Node::Device(n)),
            [a] => device_level(n, a, None, None),
            [a, b] => device_level(n, a, Some(b), None),
            _ => None,
        };
    }
    match rest {
        [] => Some(Node::Event(n)),
        [f] if EV_FILES.contains(f) => Some(Node::EvFile(n, EV_FILES.iter().find(|x| *x == f).copied()?)),
        ["device"] => Some(Node::Device(n)),
        ["device", a] => device_level(n, a, None, None),
        ["device", a, b] => device_level(n, a, Some(b), None),
        _ => None,
    }
}

fn device_level(n: usize, first: &str, second: Option<&str>, third: Option<&str>) -> Option<Node> {
    if third.is_some() { return None; }
    if first == "capabilities" {
        return match second { None => Some(Node::Caps(n)),
            Some(f) => CAPS.iter().find(|c| c.0 == f).map(|c| Node::CapFile(n, c.0)) };
    }
    if first == "id" {
        return match second { None => Some(Node::Id(n)),
            Some(f) => IDS.iter().find(|c| **c == f).map(|c| Node::IdFile(n, c)) };
    }
    if second.is_some() { return None; }
    if first == DEV_UEVENT { return Some(Node::DevFile(n, DEV_UEVENT)); }
    DEV_FILES.iter().find(|f| **f == first).map(|f| Node::DevFile(n, f))
}

fn dir_id(n: usize, level: u8) -> (u16, u8) { (SC_BASE | SI_CLS << 8 | n as u16, level) }

/// Resolve `path` to an overlay directory as `(dev, level)`.
pub fn dir(path: &[u8]) -> Option<(u16, u8)> {
    Some(match parse(path)? {
        Node::Class => dir_id(0xFF, LVL_CLASS),
        Node::Event(n) => dir_id(n, LVL_EVENT),
        Node::Device(n) => dir_id(n, LVL_DEVICE),
        Node::Caps(n) => dir_id(n, LVL_CAPS),
        Node::Id(n) => dir_id(n, LVL_ID),
        _ => return None,
    })
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

/// Linux `input_print_bitmap`: unsigned-long (64 bit) words, most significant
/// first, the top word unpadded, the rest 16 hex digits; "0" when empty.
fn put_bitmap(w: &mut W<'_>, bits: &[u8]) {
    use core::fmt::Write;
    let nw = (bits.len() + 7) / 8;
    let word = |i: usize| -> u64 {
        let mut b = [0u8; 8];
        let lo = i * 8;
        let hi = (lo + 8).min(bits.len());
        b[..hi - lo].copy_from_slice(&bits[lo..hi]);
        u64::from_le_bytes(b)
    };
    let top = (0..nw).rev().find(|&i| word(i) != 0);
    match top {
        None => { let _ = write!(w, "0"); }
        Some(t) => {
            for i in (0..=t).rev() {
                if i != t { let _ = write!(w, " "); }
                if i == t { let _ = write!(w, "{:x}", word(i)); } else { let _ = write!(w, "{:016x}", word(i)); }
            }
        }
    }
}

/// Read an attribute file.
pub fn attr(path: &[u8], out: &mut [u8]) -> Option<usize> {
    use core::fmt::Write;
    let o = ops()?;
    let node = parse(path)?;
    let mut w = W { out, pos: 0 };
    match node {
        Node::EvFile(n, "dev") => { let _ = writeln!(w, "13:{}", MINOR_BASE + n); }
        Node::EvFile(n, _) => {
            let _ = write!(w, "MAJOR=13\nMINOR={}\nDEVNAME=input/event{}\n", MINOR_BASE + n, n);
        }
        Node::DevFile(n, "name") => {
            let i = (o.info)(n)?;
            for &c in &i.name[..i.name_len] { let _ = w.write_char(c as char); }
            let _ = w.write_str("\n");
        }
        Node::DevFile(n, "uevent") => {
            let i = (o.info)(n)?;
            let _ = write!(w, "PRODUCT={:x}/{:x}/{:x}/{:x}\nNAME=\"", i.ids[0], i.ids[1], i.ids[2], i.ids[3]);
            for &c in &i.name[..i.name_len] { let _ = w.write_char(c as char); }
            let _ = w.write_str("\"\n");
        }
        Node::DevFile(n, _) => { (o.info)(n)?; let _ = w.write_str("\n"); }
        Node::IdFile(n, f) => {
            let i = (o.info)(n)?;
            let k = IDS.iter().position(|x| *x == f)?;
            let _ = writeln!(w, "{:04x}", i.ids[k]);
        }
        Node::CapFile(n, f) => {
            (o.info)(n)?;
            let ev = CAPS.iter().find(|c| c.0 == f)?.1;
            let mut bits = [0u8; 96];
            (o.bits)(n, ev, &mut bits);
            put_bitmap(&mut w, &bits);
            let _ = w.write_str("\n");
        }
        _ => return None,
    }
    Some(w.pos)
}

fn put_name(out: &mut [u8; NAME_LEN], s: &str) -> usize {
    out[..s.len()].copy_from_slice(s.as_bytes());
    s.len()
}

/// The `idx`-th entry of overlay directory `(dev, level)`: `(name length, is_dir)`.
pub fn dirent(dev: u16, level: u8, idx: usize, out: &mut [u8; NAME_LEN]) -> Option<(usize, bool)> {
    use core::fmt::Write;
    let o = ops()?;
    let n = (dev & 0xFF) as usize;
    match level {
        LVL_CLASS => {
            let mut live_n = [0u8; MAX_N];
            let mut cnt = 0;
            for i in 0..MAX_N { if (o.info)(i).is_some() { live_n[cnt] = i as u8; cnt += 1; } }
            if idx >= cnt * 2 { return None; }
            let i = live_n[idx % cnt] as usize;
            let mut s = [0u8; NAME_LEN];
            let mut w = W { out: &mut s, pos: 0 };
            let _ = write!(w, "{}{}", if idx < cnt { "event" } else { "input" }, i);
            let l = w.pos;
            out[..l].copy_from_slice(&s[..l]);
            Some((l, true))
        }
        LVL_EVENT => {
            (o.info)(n)?;
            match idx { 0 => Some((put_name(out, "dev"), false)), 1 => Some((put_name(out, "uevent"), false)),
                        2 => Some((put_name(out, "device"), true)), _ => None }
        }
        LVL_DEVICE => {
            (o.info)(n)?;
            match idx {
                0..=2 => Some((put_name(out, DEV_FILES[idx]), false)),
                3 => Some((put_name(out, DEV_UEVENT), false)),
                4 => Some((put_name(out, "id"), true)),
                5 => Some((put_name(out, "capabilities"), true)),
                _ => None,
            }
        }
        LVL_CAPS => { (o.info)(n)?; CAPS.get(idx).map(|c| (put_name(out, c.0), false)) }
        LVL_ID => { (o.info)(n)?; IDS.get(idx).map(|c| (put_name(out, c), false)) }
        _ => None,
    }
}
