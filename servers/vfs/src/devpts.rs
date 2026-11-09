//! devpts instances.
//!
//! Every `mount("devpts", dir, "devpts", ...)` creates a new instance, as
//! Linux does since 4.7 whether or not `newinstance` is given: its own pty
//! number space (`tty_server::pty::alloc_in`), its own `ptmx` and the
//! `mode`/`ptmxmode`/`uid`/`gid` it was mounted with. The instance object is
//! independent of the mount table: the mount is an alias (see `alias`) from
//! the mount point to the backing directory `/dev/.devpts/<id>`, and that
//! directory is what the VFS recognises here. Instance 0 is the initial
//! `/dev/pts`.

use alloc::vec::Vec;
use spin::Mutex;

/// Instances 1..=MAX_INST (0 is the default and always exists).
pub const MAX_INST: usize = 16;
pub const PREFIX: &[u8] = b"/dev/.devpts/";

#[derive(Clone, Copy)]
pub struct Opts {
    /// Mode of the slave nodes.
    pub mode: u16,
    /// Mode of `<mnt>/ptmx`.
    pub ptmxmode: u16,
    pub uid: u32,
    pub gid: u32,
    pub set_uid: bool,
    pub set_gid: bool,
}

static INST: Mutex<[Option<Opts>; MAX_INST]> = Mutex::new([None; MAX_INST]);

/// What a path below an instance names.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tail { Ptmx, Slave(usize) }

fn parse_num(b: &[u8], radix: u32) -> Option<u32> {
    if b.is_empty() || b.len() > 10 { return None; }
    let mut v: u64 = 0;
    for &c in b {
        let d = (c as char).to_digit(radix)?;
        v = v * radix as u64 + d as u64;
    }
    u32::try_from(v).ok()
}

/// Parse a mount option string (`newinstance,ptmxmode=0666,mode=0620,gid=5`)
/// and register a new instance. ENOSPC when all instances are in use.
pub fn create(data: &[u8]) -> Result<u16, isize> {
    let mut o = Opts { mode: 0o600, ptmxmode: 0, uid: 0, gid: 0, set_uid: false, set_gid: false };
    for opt in data.split(|&c| c == b',') {
        let (k, v) = match opt.iter().position(|&c| c == b'=') {
            Some(i) => (&opt[..i], Some(&opt[i + 1..])),
            None => (opt, None),
        };
        match (k, v) {
            (b"mode", Some(v)) => o.mode = (parse_num(v, 8).ok_or(-22isize)? & 0o7777) as u16,
            (b"ptmxmode", Some(v)) => o.ptmxmode = (parse_num(v, 8).ok_or(-22isize)? & 0o7777) as u16,
            (b"uid", Some(v)) => { o.uid = parse_num(v, 10).ok_or(-22isize)?; o.set_uid = true; }
            (b"gid", Some(v)) => { o.gid = parse_num(v, 10).ok_or(-22isize)?; o.set_gid = true; }
            // newinstance, max=, nosuid, ...: accepted, nothing to record.
            _ => {}
        }
    }
    let mut t = INST.lock();
    let i = t.iter().position(|e| e.is_none()).ok_or(-28isize)?;
    t[i] = Some(o);
    Ok(i as u16 + 1)
}

/// Drop instance `id` (its mount went away). Open ptys keep working.
pub fn destroy(id: u16) {
    if id >= 1 && (id as usize) <= MAX_INST { INST.lock()[id as usize - 1] = None; }
}

/// Options of instance `id`; `None` for 0 (the default) or an unknown id.
pub fn opts(id: u16) -> Option<Opts> {
    if id == 0 || id as usize > MAX_INST { return None; }
    INST.lock()[id as usize - 1]
}

/// The backing directory an instance's mount point aliases.
pub fn backing(id: u16) -> Vec<u8> {
    let mut v = PREFIX.to_vec();
    v.extend_from_slice(alloc::format!("{}", id).as_bytes());
    v
}

/// Instance id of backing path `p` (`/dev/.devpts/<id>` exactly), if it is one.
pub fn inst_of_backing(p: &[u8]) -> Option<u16> {
    let id = parse_num(p.strip_prefix(PREFIX)?, 10)?;
    if id >= 1 && (id as usize) <= MAX_INST && INST.lock()[id as usize - 1].is_some() { Some(id as u16) } else { None }
}

/// Split `path` into (instance, tail) for `/dev/pts/<tail>` (instance 0) and
/// `/dev/.devpts/<id>/<tail>`; `tail` is `ptmx` or a pty number.
pub fn parse(path: &[u8]) -> Option<(u16, Tail)> {
    let (inst, rest) = if let Some(r) = path.strip_prefix(b"/dev/pts/".as_slice()) {
        (0u16, r)
    } else {
        let r = path.strip_prefix(PREFIX)?;
        let slash = r.iter().position(|&c| c == b'/')?;
        let id = parse_num(&r[..slash], 10)? as usize;
        if id < 1 || id > MAX_INST || INST.lock()[id - 1].is_none() { return None; }
        (id as u16, &r[slash + 1..])
    };
    if rest == b"ptmx" { return Some((inst, Tail::Ptmx)); }
    if rest.is_empty() || rest.len() > 3 { return None; }
    Some((inst, Tail::Slave(parse_num(rest, 10)? as usize)))
}

/// `Some(instance)` when `dir` is the directory of a devpts instance.
pub fn dir_inst(dir: &[u8]) -> Option<u16> {
    if dir == b"/dev/pts" { Some(0) } else { inst_of_backing(dir) }
}
