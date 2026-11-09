//! FUSE (filesystem in userspace): `/dev/fuse` plus the mount side.
//!
//! A userland daemon opens `/dev/fuse`, passes the fd to
//! `mount(src, dir, "fuse[.sub]", flags, "fd=N,rootmode=...,user_id=...")`
//! and then serves the kernel protocol 7.x by read()/write() on that fd.
//! Every filesystem call that reaches the mount's IPC port is turned into a
//! request, queued on the connection and the caller parks until the daemon
//! writes the matching reply. The daemon dying (last close of the device
//! fd) aborts the connection: queued and later requests fail with ENOTCONN.
//!
//! The VFS speaks paths, FUSE speaks node ids, so each path operation walks
//! the path with LOOKUP and FORGETs what it looked up when done; an open
//! file keeps the one lookup of its node until RELEASE.
#![no_std]
extern crate alloc;

mod proto;
mod ops;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, Ordering};
use ipc::{port, Message};
use proto::*;
use spin::Mutex;

pub(crate) const ENOTCONN: i32 = -107;

pub(crate) struct OFile {
    pub nodeid: u64,
    pub fh: u64,
    pub dir: bool,
    pub writable: bool,
    pub append: bool,
    pub pos: u64,
    pub path: Vec<u8>,
}

pub(crate) struct Conn {
    pub open_id: u32,
    pub mounted: bool,
    pub port: u32,
    pub prefix: &'static str,
    pub dead: bool,
    pub uid: u32,
    pub gid: u32,
    pub allow_other: bool,
    pub rdonly: bool,
    pub next_unique: u64,
    pub pending: VecDeque<Vec<u8>>,
    /// (unique, opcode, someone waits for the reply)
    pub inflight: Vec<(u64, u32, bool)>,
    pub replies: Vec<(u64, i32, Vec<u8>)>,
    pub wbuf: Vec<u8>,
    pub init_done: bool,
    pub max_write: u32,
    pub files: Vec<Option<OFile>>,
    pub seq: u64,
}

pub(crate) static CONNS: Mutex<Vec<Option<Conn>>> = Mutex::new(Vec::new());
static DEV_PORT: AtomicU32 = AtomicU32::new(0);

pub(crate) fn conn_mut(g: &mut Vec<Option<Conn>>, ci: usize) -> Option<&mut Conn> {
    g.get_mut(ci).and_then(|c| c.as_mut())
}
fn conn_by_open(g: &Vec<Option<Conn>>, open_id: u32) -> Option<usize> {
    g.iter().position(|c| c.as_ref().map_or(false, |c| c.open_id == open_id))
}
pub(crate) fn conn_by_port(p: u32) -> Option<usize> {
    CONNS.lock().iter().position(|c| c.as_ref().map_or(false, |c| c.mounted && c.port == p))
}

fn arg(m: &Message, n: usize) -> u64 {
    u64::from_le_bytes(m.data[n * 8..n * 8 + 8].try_into().unwrap())
}
pub(crate) fn reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&(v as u64).to_le_bytes());
    m
}

/// Register `/dev/fuse` (char 10:229). Returns the device server's port.
pub fn init(owner_pid: u32) -> Option<u32> {
    let p = port::create(owner_pid)?;
    port::register_kernel_handler(p, dev_handle);
    DEV_PORT.store(p, Ordering::Relaxed);
    vfs_server::register_device_notify_open("/dev/fuse", p, 0, 10, 229);
    Some(p)
}

/// Does `(port, dev_id)` name `/dev/fuse`?
pub fn is_fuse_device(p: u32, dev_id: u32) -> bool {
    p != 0 && p == DEV_PORT.load(Ordering::Relaxed) && dev_id == 0
}

fn build(op: u32, unique: u64, nodeid: u64, c: &ops::Cr, parts: &[&[u8]]) -> Vec<u8> {
    let body: usize = parts.iter().map(|p| p.len()).sum();
    let mut v = Vec::with_capacity(IN_HDR + body);
    v.extend_from_slice(&((IN_HDR + body) as u32).to_le_bytes());
    v.extend_from_slice(&op.to_le_bytes());
    v.extend_from_slice(&unique.to_le_bytes());
    v.extend_from_slice(&nodeid.to_le_bytes());
    v.extend_from_slice(&c.uid.to_le_bytes());
    v.extend_from_slice(&c.gid.to_le_bytes());
    v.extend_from_slice(&c.pid.to_le_bytes());
    v.extend_from_slice(&0u32.to_le_bytes());
    for p in parts { v.extend_from_slice(p); }
    v
}

/// Queue a request that gets no reply (FORGET, INTERRUPT).
pub(crate) fn post(ci: usize, op: u32, nodeid: u64, c: &ops::Cr, parts: &[&[u8]]) {
    {
        let mut g = CONNS.lock();
        let Some(cn) = conn_mut(&mut g, ci) else { return };
        if cn.dead { return; }
        let u = cn.next_unique; cn.next_unique += 1;
        cn.pending.push_back(build(op, u, nodeid, c, parts));
        cn.seq += 1;
    }
    sched::wake_poll();
}

/// Send a request and park until the daemon answers. `Ok(body)` or `Err(-errno)`.
pub(crate) fn call(ci: usize, op: u32, nodeid: u64, c: &ops::Cr, parts: &[&[u8]]) -> Result<Vec<u8>, i32> {
    let unique;
    {
        let mut g = CONNS.lock();
        let cn = conn_mut(&mut g, ci).ok_or(ENOTCONN)?;
        if cn.dead { return Err(ENOTCONN); }
        unique = cn.next_unique; cn.next_unique += 1;
        cn.inflight.push((unique, op, true));
        cn.pending.push_back(build(op, unique, nodeid, c, parts));
        cn.seq += 1;
    }
    sched::wake_poll();
    loop {
        if let Some(r) = take_reply(ci, unique) { return r; }
        if sched::has_deliverable_signal() {
            {
                let mut g = CONNS.lock();
                if let Some(cn) = conn_mut(&mut g, ci) {
                    if let Some(i) = cn.inflight.iter().position(|e| e.0 == unique) { cn.inflight[i].2 = false; }
                    cn.replies.retain(|r| r.0 != unique);
                }
            }
            post(ci, INTERRUPT, 0, c, &[&unique.to_le_bytes()]);
            return Err(-4);
        }
        sched::block_on_poll_prepare_until(sched::monotonic_ns() + 2_000_000);
        if let Some(r) = take_reply(ci, unique) { sched::block_on_poll_cancel(); return r; }
        sched::block_on_poll_commit();
    }
}

fn take_reply(ci: usize, unique: u64) -> Option<Result<Vec<u8>, i32>> {
    let mut g = CONNS.lock();
    let Some(cn) = conn_mut(&mut g, ci) else { return Some(Err(ENOTCONN)) };
    if let Some(i) = cn.replies.iter().position(|r| r.0 == unique) {
        let (_, e, d) = cn.replies.remove(i);
        cn.inflight.retain(|x| x.0 != unique);
        return Some(if e < 0 { Err(e) } else { Ok(d) });
    }
    if cn.dead {
        cn.inflight.retain(|x| x.0 != unique);
        return Some(Err(ENOTCONN));
    }
    None
}

// ── /dev/fuse ────────────────────────────────────────────────────────────────

fn dev_handle(msg: &Message, _caller: u32, _port: u32) -> Message {
    let open_id = arg(msg, 4) as u32;
    let pid = arg(msg, 3) as u32;
    match msg.tag {
        vfs_server::VFS_OPEN => {
            let mut g = CONNS.lock();
            if conn_by_open(&g, open_id).is_none() {
                let cn = Conn {
                    open_id, mounted: false, port: 0, prefix: "", dead: false, uid: 0, gid: 0,
                    allow_other: false, rdonly: false, next_unique: 1, pending: VecDeque::new(),
                    inflight: Vec::new(), replies: Vec::new(), wbuf: Vec::new(), init_done: false,
                    max_write: 4096, files: Vec::new(), seq: 0,
                };
                match g.iter().position(|c| c.is_none()) {
                    Some(i) => g[i] = Some(cn),
                    None => g.push(Some(cn)),
                }
            }
            reply(0)
        }
        vfs_server::VFS_READ => {
            let (buf, count) = (arg(msg, 1) as usize, arg(msg, 2) as usize);
            let req = {
                let mut g = CONNS.lock();
                let Some(ci) = conn_by_open(&g, open_id) else { return reply(-19) };
                let cn = conn_mut(&mut g, ci).unwrap();
                match cn.pending.pop_front() {
                    Some(r) if r.len() > count => { cn.pending.push_front(r); return reply(-22); }
                    Some(r) => r,
                    None => return reply(if cn.dead { -19 } else { -11 }),
                }
            };
            let left = unsafe { sched::uaccess::copy_raw(buf as *mut u8, req.as_ptr(), req.len()) };
            if left != 0 { return reply(-14); }
            reply(req.len() as i64)
        }
        vfs_server::VFS_WRITE => {
            let (buf, count) = (arg(msg, 1) as usize, arg(msg, 2) as usize);
            if count > (4 << 20) { return reply(-22); }
            let mut data = alloc::vec![0u8; count];
            let left = unsafe { sched::uaccess::copy_raw(data.as_mut_ptr(), buf as *const u8, count) };
            if left != 0 { return reply(-14); }
            let _ = pid;
            let mut g = CONNS.lock();
            let Some(ci) = conn_by_open(&g, open_id) else { return reply(-19) };
            let cn = conn_mut(&mut g, ci).unwrap();
            if cn.dead { return reply(-19); }
            cn.wbuf.extend_from_slice(&data);
            let mut woke = false;
            loop {
                if cn.wbuf.len() < OUT_HDR { break; }
                let len = u32_at(&cn.wbuf, 0) as usize;
                if len < OUT_HDR { cn.wbuf.clear(); return reply(-22); }
                if cn.wbuf.len() < len { break; }
                let m: Vec<u8> = cn.wbuf.drain(..len).collect();
                woke |= process_reply(cn, &m);
            }
            drop(g);
            if woke { sched::wake_poll(); }
            reply(count as i64)
        }
        vfs_server::VFS_POLL => {
            let g = CONNS.lock();
            let mut m = Message::empty();
            let (ev, seq) = match conn_by_open(&g, open_id).and_then(|i| g[i].as_ref()) {
                Some(c) => (4u64 | if !c.pending.is_empty() { 1 } else { 0 } | if c.dead { 8 } else { 0 }, c.seq),
                None => (8, 0),
            };
            m.data[0..8].copy_from_slice(&ev.to_le_bytes());
            m.data[8..16].copy_from_slice(&seq.to_le_bytes());
            m
        }
        vfs_server::VFS_CLOSE => {
            let oid = arg(msg, 1) as u32;
            abort(oid);
            reply(0)
        }
        _ => reply(-25),
    }
}

/// Handle one reply message; true when a waiter should be woken.
fn process_reply(cn: &mut Conn, m: &[u8]) -> bool {
    let err = u32_at(m, 4) as i32;
    let unique = u64_at(m, 8);
    if unique == 0 { return false; }
    let Some(i) = cn.inflight.iter().position(|e| e.0 == unique) else { return false };
    let (_, op, waited) = cn.inflight[i];
    if op == INIT {
        cn.inflight.remove(i);
        if err != 0 || u32_at(m, 16) != 7 { cn.dead = true; return true; }
        let mw = u32_at(m, 16 + 20);
        cn.max_write = if mw == 0 { 4096 } else { mw.min(1 << 17) };
        cn.init_done = true;
        return true;
    }
    if !waited { cn.inflight.remove(i); return false; }
    cn.replies.push((unique, err, m[OUT_HDR..].to_vec()));
    true
}

/// Abort the connection of device open `open_id` (daemon closed or died).
fn abort(open_id: u32) {
    let mut g = CONNS.lock();
    let Some(ci) = conn_by_open(&g, open_id) else { return };
    let cn = conn_mut(&mut g, ci).unwrap();
    cn.dead = true;
    cn.pending.clear();
    if !cn.mounted { g[ci] = None; }
    drop(g);
    sched::wake_poll();
}

// ── Mount ────────────────────────────────────────────────────────────────────

/// Parsed `mount(2)` data string.
pub struct MountOpts { pub fd: i32, pub rootmode: u32, pub user_id: u32, pub group_id: u32, pub allow_other: bool }

pub fn parse_opts(s: &str) -> Option<MountOpts> {
    let mut o = MountOpts { fd: -1, rootmode: 0o40000, user_id: 0, group_id: 0, allow_other: false };
    for kv in s.split(',') {
        let (k, v) = match kv.split_once('=') { Some((k, v)) => (k, v), None => (kv, "") };
        match k {
            "fd" => o.fd = v.parse().ok()?,
            "rootmode" => o.rootmode = u32::from_str_radix(v, 8).ok()?,
            "user_id" => o.user_id = v.parse().ok()?,
            "group_id" => o.group_id = v.parse().ok()?,
            "allow_other" => o.allow_other = true,
            _ => {}
        }
    }
    if o.fd < 0 { None } else { Some(o) }
}

/// Attach the daemon's device open `open_id` to a new mount at `prefix`.
pub fn mount(open_id: u32, prefix: &'static str, source: &'static str, fstype: &'static str,
             o: &MountOpts, rdonly: bool) -> isize {
    let p = {
        let mut g = CONNS.lock();
        let Some(ci) = conn_by_open(&g, open_id) else { return -22 };
        let cn = conn_mut(&mut g, ci).unwrap();
        if cn.mounted || cn.dead { return -22; }
        if o.rootmode & S_IFMT != S_IFDIR { return -22; }
        let Some(p) = port::create(0) else { return -12 };
        port::register_kernel_handler(p, ops::dispatch);
        cn.mounted = true; cn.port = p; cn.prefix = prefix;
        cn.uid = o.user_id; cn.gid = o.group_id; cn.allow_other = o.allow_other; cn.rdonly = rdonly;
        // INIT goes first on the queue.
        let mut init = Vec::new();
        init.extend_from_slice(&7u32.to_le_bytes());
        init.extend_from_slice(&KERNEL_MINOR.to_le_bytes());
        init.extend_from_slice(&(128u32 * 1024).to_le_bytes());
        init.extend_from_slice(&0u32.to_le_bytes());
        init.extend_from_slice(&[0u8; 48]);
        let u = cn.next_unique; cn.next_unique += 1;
        cn.inflight.push((u, INIT, false));
        cn.pending.push_back(build(INIT, u, 0, &ops::Cr { uid: 0, gid: 0, pid: 0 }, &[&init]));
        cn.seq += 1;
        p
    };
    vfs_server::register_mount(prefix, p, source, fstype);
    vfs_server::set_mount_rdonly_by_port(p, rdonly);
    sched::wake_poll();
    0
}

/// umount(2) of the FUSE mount at `prefix`. `force`: MNT_FORCE/MNT_DETACH.
pub fn unmount(prefix: &str, force: bool) -> isize {
    let Some(ci) = ({ let g = CONNS.lock(); g.iter().position(|c| c.as_ref().map_or(false, |c| c.mounted && c.prefix == prefix)) })
        else { return -22 };
    let (alive, busy, p) = {
        let mut g = CONNS.lock();
        let cn = conn_mut(&mut g, ci).unwrap();
        (!cn.dead, cn.files.iter().any(|f| f.is_some()), cn.port)
    };
    if busy && !force { return -16; }
    if alive {
        // Bounded DESTROY: a daemon that does not answer must not wedge umount.
        let c = ops::Cr { uid: 0, gid: 0, pid: 0 };
        {
            let mut g = CONNS.lock();
            let cn = conn_mut(&mut g, ci).unwrap();
            let u = cn.next_unique; cn.next_unique += 1;
            cn.inflight.push((u, DESTROY, false));
            cn.pending.push_back(build(DESTROY, u, 0, &c, &[]));
            cn.seq += 1;
        }
        sched::wake_poll();
        let end = sched::monotonic_ns() + 300_000_000;
        loop {
            let done = { let mut g = CONNS.lock(); let cn = conn_mut(&mut g, ci).unwrap(); cn.dead || cn.pending.is_empty() && cn.inflight.is_empty() };
            if done || sched::monotonic_ns() > end { break; }
            sched::block_on_poll_prepare_until(sched::monotonic_ns() + 2_000_000);
            sched::block_on_poll_commit();
        }
    }
    vfs_server::unregister_mount(prefix);
    port::close(p);
    let mut g = CONNS.lock();
    let cn = conn_mut(&mut g, ci).unwrap();
    cn.mounted = false;
    cn.files.clear();
    let drop_it = cn.dead;
    if drop_it { g[ci] = None; }
    0
}
