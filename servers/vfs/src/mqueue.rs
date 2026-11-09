//! POSIX message queues (mq_open & friends) and the mqueue filesystem.
//!
//! Queues are named per IPC namespace: the table is keyed by
//! `(ipcns id, name)`, so a task in a fresh CLONE_NEWIPC never sees a queue of
//! its parent. A queue is reached through an `mqd` -- an ordinary fd of kind
//! `VnodeKind::Mq { slot }` -- or, for ls/stat/rm/cat, through the path
//! `/dev/mqueue/<name>` (the tree an `mqueue` mount aliases to). Blocking and
//! the syscall ABI live in the kernel (`sys_mq_*`); this module owns the data
//! and the non-blocking primitives, and reports "would block" as EAGAIN.

use alloc::vec::Vec;
use spin::Mutex;
use crate::{VnodeKind, FdEntry, TblSlot, find_tbl, get_or_create, ofd};

pub const MQ_DIR: &[u8] = b"/dev/mqueue";
const NAME_MAX: usize = 255;
const O_ACCMODE: u32 = 3;
const O_CREAT: u32 = 0o100;
const O_EXCL: u32 = 0o200;
const O_NONBLOCK: u32 = 0o4000;
const O_CLOEXEC: u32 = 0x8_0000;
pub const SI_MESGQ: i32 = -3;

// /proc/sys/fs/mqueue/*
pub static MSG_MAX: Mutex<i64> = Mutex::new(10);
pub static MSGSIZE_MAX: Mutex<i64> = Mutex::new(8192);
pub static QUEUES_MAX: Mutex<i64> = Mutex::new(256);
pub static MSG_DEFAULT: Mutex<i64> = Mutex::new(10);
pub static MSGSIZE_DEFAULT: Mutex<i64> = Mutex::new(8192);

struct Msg { prio: u32, data: Vec<u8> }

struct Mq {
    ns: u32,
    name: Vec<u8>,
    maxmsg: i64,
    msgsize: i64,
    mode: u32,
    uid: u32,
    gid: u32,
    refs: u32,
    unlinked: bool,
    seq: u64,
    bytes: u64,
    msgs: Vec<Msg>, // highest priority first, FIFO within a priority
    recv_waiters: u32,
    /// (tgid, signo, sigev_value); None = no registration.
    notify: Option<(u32, u32, u64)>,
}

static MQS: Mutex<Vec<Option<Mq>>> = Mutex::new(Vec::new());

pub fn key(slot: usize) -> u64 { sched::poll_key(sched::poll_class::MQ, slot as u32) }

fn clean_name(name: &[u8]) -> Result<&[u8], isize> {
    let n = name.strip_prefix(b"/").unwrap_or(name);
    if n.is_empty() { return Err(-22); }
    if n == b"." || n == b".." { return Err(-22); }
    if n.contains(&b'/') { return Err(-13); } // EACCES, as Linux
    if n.len() > NAME_MAX { return Err(-36); } // ENAMETOOLONG
    Ok(n)
}

fn find(mqs: &[Option<Mq>], ns: u32, name: &[u8]) -> Option<usize> {
    mqs.iter().position(|q| matches!(q, Some(q) if !q.unlinked && q.ns == ns && q.name == name))
}

/// mq_open: look up or create `name` in `pid`'s IPC namespace and install an
/// mqd. `attr` = (mq_maxmsg, mq_msgsize) when the caller passed one.
pub fn open(pid: u32, name: &[u8], oflag: u32, mode: u32, attr: Option<(i64, i64)>) -> isize {
    let name = match clean_name(name) { Ok(n) => n, Err(e) => return e };
    let ns = sched::ns::ipcns_of(pid) as u32;
    let slot = {
        let mut mqs = MQS.lock();
        match find(&mqs, ns, name) {
            Some(s) => {
                if oflag & O_CREAT != 0 && oflag & O_EXCL != 0 { return -17; } // EEXIST
                s
            }
            None => {
                if oflag & O_CREAT == 0 { return -2; }
                let (maxmsg, msgsize) = match attr {
                    Some((m, s)) => {
                        if m <= 0 || s <= 0 { return -22; }
                        if m > *MSG_MAX.lock() || s > *MSGSIZE_MAX.lock() { return -22; }
                        (m, s)
                    }
                    None => (*MSG_DEFAULT.lock(), *MSGSIZE_DEFAULT.lock()),
                };
                let live = mqs.iter().filter(|q| q.is_some()).count() as i64;
                if live >= *QUEUES_MAX.lock() { return -28; } // ENOSPC
                let q = Mq {
                    ns, name: name.to_vec(), maxmsg, msgsize, mode: mode & 0o777,
                    uid: sched::current_euid(), gid: sched::current_egid(),
                    refs: 0, unlinked: false, seq: 1, bytes: 0, msgs: Vec::new(),
                    recv_waiters: 0, notify: None,
                };
                match mqs.iter().position(|q| q.is_none()) {
                    Some(i) => { mqs[i] = Some(q); i }
                    None => { mqs.push(Some(q)); mqs.len() - 1 }
                }
            }
        }
    };
    install_fd(pid, slot, oflag)
}

fn install_fd(pid: u32, slot: usize, oflag: u32) -> isize {
    MQS.lock()[slot].as_mut().unwrap().refs += 1;
    let stored = (oflag & (O_NONBLOCK | O_CLOEXEC)) | (oflag & O_ACCMODE);
    let mut tbls = TblSlot::new();
    let fd = match get_or_create(pid, &mut tbls).and_then(|t| t.alloc_fd().map(|f| (t, f))) {
        Some((tbl, fd)) => {
            tbl.fds.set(fd, FdEntry { kind: VnodeKind::Mq { slot }, flags: stored, in_use: true, ofd: ofd::alloc() });
            fd
        }
        None => { drop(tbls); ref_dec(slot, pid); return -24; } // EMFILE
    };
    fd as isize
}

/// Open `/dev/mqueue/<name>` through plain open(2) (read it for QSIZE).
pub fn open_path(pid: u32, name: &[u8], oflag: u32) -> isize {
    let ns = sched::ns::ipcns_of(pid) as u32;
    let slot = match find(&MQS.lock(), ns, name) { Some(s) => s, None => return -2 };
    install_fd(pid, slot, oflag)
}

pub fn ref_inc(slot: usize) {
    if let Some(Some(q)) = MQS.lock().get_mut(slot) { q.refs += 1; }
}

/// An mqd closed: drop the reference, the notification it registered, and the
/// queue itself once unlinked and unreferenced.
pub fn ref_dec(slot: usize, pid: u32) {
    let tg = sched::tgid_of(pid);
    let mut mqs = MQS.lock();
    if let Some(e) = mqs.get_mut(slot) {
        if let Some(q) = e {
            q.refs = q.refs.saturating_sub(1);
            if matches!(q.notify, Some((p, _, _)) if p == tg) { q.notify = None; }
            if q.refs == 0 && q.unlinked { *e = None; }
        }
    }
}

pub fn unlink(pid: u32, name: &[u8]) -> isize {
    let name = match clean_name(name) { Ok(n) => n, Err(e) => return e };
    let ns = sched::ns::ipcns_of(pid) as u32;
    let mut mqs = MQS.lock();
    match find(&mqs, ns, name) {
        None => -2,
        Some(s) => {
            let q = mqs[s].as_mut().unwrap();
            q.unlinked = true;
            if q.refs == 0 { mqs[s] = None; }
            0
        }
    }
}

/// `(slot, fd flags)` of an mqd.
pub fn lookup_fd(pid: u32, fd: usize) -> Result<(usize, u32), isize> {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    let tbl = match find_tbl(pid, &mut tbls) { Some(t) => t, None => return Err(-9) };
    if !tbl.fds.used(fd) { return Err(-9); }
    match tbl.fds[fd].kind {
        VnodeKind::Mq { slot } => Ok((slot, tbl.fds[fd].flags)),
        _ => Err(-9),
    }
}

/// Queue a message. EAGAIN = full (the caller blocks or fails).
pub fn try_send(slot: usize, fdflags: u32, data: &[u8], prio: u32) -> Result<(), isize> {
    if fdflags & O_ACCMODE == 0 { return Err(-9); }
    if prio >= 32768 { return Err(-22); }
    let sig = {
        let mut mqs = MQS.lock();
        let q = match mqs.get_mut(slot).and_then(|q| q.as_mut()) { Some(q) => q, None => return Err(-9) };
        if data.len() as i64 > q.msgsize { return Err(-90); } // EMSGSIZE
        if q.msgs.len() as i64 >= q.maxmsg { return Err(-11); }
        let at = q.msgs.iter().position(|m| m.prio < prio).unwrap_or(q.msgs.len());
        let was_empty = q.msgs.is_empty();
        q.msgs.insert(at, Msg { prio, data: data.to_vec() });
        q.bytes += data.len() as u64;
        q.seq += 1;
        // Notify only an empty queue with nobody blocked in mq_receive;
        // the registration is one-shot.
        if was_empty && q.recv_waiters == 0 { q.notify.take() } else { None }
    };
    sched::wake_poll_key(key(slot));
    if let Some((tg, signo, val)) = sig {
        if signo != 0 {
            let info = sched::SigInfo {
                si_code: SI_MESGQ, si_pid: sched::current_tgid() as i32, si_uid: sched::current_euid(),
                si_status: 0, si_addr: 0, si_value: val,
            };
            let _ = sched::deliver_signal_process(tg, signo, info);
        }
    }
    Ok(())
}

/// Dequeue the best message. EAGAIN = empty, EMSGSIZE = `cap` too small.
pub fn try_recv(slot: usize, fdflags: u32, cap: usize) -> Result<(Vec<u8>, u32), isize> {
    if fdflags & O_ACCMODE == 1 { return Err(-9); }
    let r = {
        let mut mqs = MQS.lock();
        let q = match mqs.get_mut(slot).and_then(|q| q.as_mut()) { Some(q) => q, None => return Err(-9) };
        if (cap as i64) < q.msgsize { return Err(-90); }
        if q.msgs.is_empty() { return Err(-11); }
        let m = q.msgs.remove(0);
        q.bytes -= m.data.len() as u64;
        q.seq += 1;
        (m.data, m.prio)
    };
    sched::wake_poll_key(key(slot));
    Ok(r)
}

pub fn recv_waiter(slot: usize, add: bool) {
    if let Some(Some(q)) = MQS.lock().get_mut(slot) {
        if add { q.recv_waiters += 1 } else { q.recv_waiters = q.recv_waiters.saturating_sub(1) }
    }
}

/// (mq_maxmsg, mq_msgsize, mq_curmsgs)
pub fn attr(slot: usize) -> (i64, i64, i64) {
    match MQS.lock().get(slot) {
        Some(Some(q)) => (q.maxmsg, q.msgsize, q.msgs.len() as i64),
        _ => (0, 0, 0),
    }
}

/// mq_notify: `Some((signo, value))` registers, `None` removes.
pub fn notify(pid: u32, slot: usize, reg: Option<(u32, u64)>) -> isize {
    let tg = sched::tgid_of(pid);
    let mut mqs = MQS.lock();
    let q = match mqs.get_mut(slot).and_then(|q| q.as_mut()) { Some(q) => q, None => return -9 };
    match reg {
        Some((s, v)) => {
            if q.notify.is_some() { return -16; } // EBUSY
            q.notify = Some((tg, s, v));
        }
        None => { if matches!(q.notify, Some((p, _, _)) if p == tg) { q.notify = None; } }
    }
    0
}

/// Readiness for poll/epoll: (POLLIN?, POLLOUT?, edge seq).
pub fn readiness(slot: usize) -> (bool, bool, u64) {
    match MQS.lock().get(slot) {
        Some(Some(q)) => (!q.msgs.is_empty(), (q.msgs.len() as i64) < q.maxmsg, q.seq),
        _ => (false, false, 0),
    }
}

/// The text a read of the queue file returns.
pub fn qsize_text(slot: usize) -> Vec<u8> {
    let (bytes, nt, sg, np) = match MQS.lock().get(slot) {
        Some(Some(q)) => match q.notify {
            Some((p, s, _)) => (q.bytes, 1u32, s, p),
            None => (q.bytes, 0, 0, 0),
        },
        _ => (0, 0, 0, 0),
    };
    let s = alloc::format!("QSIZE:{:<10} NOTIFY:{:<5} SIGNO:{:<5} NOTIFY_PID:{:<6}\n", bytes, nt, sg, np);
    s.into_bytes()
}

/// (mode, uid, gid, ino, size) of `/dev/mqueue/<name>` for stat.
pub fn stat_path(pid: u32, name: &[u8]) -> Option<(u32, u32, u32, u64, u64)> {
    let ns = sched::ns::ipcns_of(pid) as u32;
    let mqs = MQS.lock();
    let s = find(&mqs, ns, name)?;
    let q = mqs[s].as_ref()?;
    Some((0o100000 | q.mode, q.uid, q.gid, 0x5000_0000 + s as u64, q.bytes))
}

/// Names of the queues of `pid`'s IPC namespace, with their inodes.
pub fn list(pid: u32) -> Vec<(Vec<u8>, u64)> {
    let ns = sched::ns::ipcns_of(pid) as u32;
    let mqs = MQS.lock();
    let mut v: Vec<(Vec<u8>, u64)> = Vec::new();
    for (i, q) in mqs.iter().enumerate() {
        if let Some(q) = q {
            if !q.unlinked && q.ns == ns { v.push((q.name.clone(), 0x5000_0000 + i as u64)); }
        }
    }
    v.sort();
    v
}

/// `/dev/mqueue/<name>` -> `<name>` (None if `path` is not under the tree).
pub fn path_name(path: &[u8]) -> Option<&[u8]> {
    let rest = path.strip_prefix(MQ_DIR)?.strip_prefix(b"/")?;
    if rest.is_empty() { None } else { Some(rest) }
}

/// mq_setattr: the only mutable attribute is O_NONBLOCK on this mqd.
pub fn set_nonblock(pid: u32, fd: usize, on: bool) {
    let pid = sched::tgid_of(pid);
    let mut tbls = TblSlot::new();
    if let Some(tbl) = find_tbl(pid, &mut tbls) {
        if tbl.fds.used(fd) {
            let f = tbl.fds[fd].flags;
            tbl.fds[fd].flags = if on { f | O_NONBLOCK } else { f & !O_NONBLOCK };
        }
    }
}
