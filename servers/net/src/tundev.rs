//! `/dev/net/tun`: the file-descriptor side of tun/tap interfaces.
//!
//! This module owns the device node, the per-open state (the opener's network
//! namespace, captured at `open()` as Linux does with `tfile->net`), the
//! reader queue, PI headers, ifreq parsing and the TUN* ioctls. It knows
//! nothing about stacks: the interface side (create the interface in the
//! opener's netns, feed frames into its stack, tear it down) is a
//! [`TunBackend`] hook table the net server registers (`tunbe`), mirroring
//! `drivers::netdev::set_link_hook`. With no backend TUNSETIFF is ENODEV.
//!
//! Locks: `OPENS` is a leaf. Backend hooks run with it released. `deliver`
//! is called by the net daemon after the stack lock is dropped.

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use ipc::Message;
use spin::Mutex;

pub type TunHandle = u32;

/// The interface side. `attach` creates (or re-attaches the persistent
/// interface called) `name` in `netns` and returns (ifindex, final name);
/// `input` hands the backend a frame the fd wrote (EIO when the link is
/// down); `detach` runs when the fd closes: `keep` leaves the interface in
/// place (TUNSETPERSIST), otherwise it is destroyed.
pub struct TunBackend {
    pub attach: fn(netns: u16, name: &[u8; 16], tap: bool, h: TunHandle) -> Result<(u32, [u8; 16]), i32>,
    pub input: fn(h: TunHandle, frame: &[u8]) -> Result<(), i32>,
    pub detach: fn(h: TunHandle, keep: bool),
}

static BACKEND: AtomicUsize = AtomicUsize::new(0);
pub fn set_backend(b: &'static TunBackend) { BACKEND.store(b as *const TunBackend as usize, Ordering::Release); }
fn backend() -> Option<&'static TunBackend> {
    match BACKEND.load(Ordering::Acquire) { 0 => None, p => Some(unsafe { &*(p as *const TunBackend) }) }
}

const IFF_TUN: u16 = 1;
const IFF_TAP: u16 = 2;
const IFF_NO_PI: u16 = 0x1000;
const CAP_NET_ADMIN: u32 = 12;
/// txqueuelen: frames waiting for the reader.
const RQ_CAP: usize = 1000;

pub const TUN_MAJOR: u32 = 10;
pub const TUN_MINOR: u32 = 200;
const TUN_PATH: &str = "/dev/net/tun";

static PORT: AtomicU32 = AtomicU32::new(u32::MAX);
pub fn device_port() -> Option<u32> {
    match PORT.load(Ordering::Acquire) { u32::MAX => None, p => Some(p) }
}

struct Attached { tap: bool, pi: bool, name: [u8; 16], persist: bool, owner: u32, group: u32 }

struct TOpen {
    id: u32,
    /// The opener's netns, pinned for as long as the fd is open.
    ns: sched::ns::NsRef,
    att: Option<Attached>,
    rq: VecDeque<Vec<u8>>,
    seq: u64,
}

static OPENS: Mutex<Vec<TOpen>> = Mutex::new(Vec::new());

/// A persistent interface with no fd: keeps its netns alive and remembers
/// who may re-attach it.
struct Persist { ns: sched::ns::NsRef, name: [u8; 16], tap: bool, owner: u32, group: u32 }
static PERSIST: Mutex<Vec<Persist>> = Mutex::new(Vec::new());

fn wake_tag(open_id: u32) -> u64 { sched::poll_tag(sched::poll_class::NETCTL, open_id) }
fn wake(mask: u64) {
    if mask != 0 && !sched::try_wake_poll_tagged(mask) { sched::request_poll_wake_tagged(mask); }
}

/// net -> reader: a frame the interface `h` transmitted. Cap 1000, dropped
/// when full. A PI header is added here unless the fd asked for IFF_NO_PI.
pub fn deliver(h: TunHandle, frame: Vec<u8>) {
    let mut mask = 0;
    {
        let mut o = OPENS.lock();
        let Some(t) = o.iter_mut().find(|x| x.id == h) else { return };
        let Some(a) = t.att.as_ref() else { return };
        if t.rq.len() >= RQ_CAP { return; }
        let out = if a.pi {
            let et = if a.tap { if frame.len() >= 14 { u16::from_be_bytes([frame[12], frame[13]]) } else { 0 } }
                     else if frame.first().map_or(false, |b| b >> 4 == 6) { 0x86dd } else { 0x0800 };
            let mut v = alloc::vec![0u8, 0, (et >> 8) as u8, et as u8];
            v.extend_from_slice(&frame);
            v
        } else { frame };
        t.rq.push_back(out);
        t.seq += 1;
        mask |= wake_tag(h);
    }
    wake(mask);
}

pub fn init() {
    let p = match ipc::port::create(0) { Some(p) => p, None => return };
    if !ipc::port::register_kernel_handler(p, handle_msg) { return; }
    PORT.store(p, Ordering::Release);
    vfs::register_device_notify_open(TUN_PATH, p, 0, TUN_MAJOR, TUN_MINOR);
}

// ── VFS protocol ─────────────────────────────────────────────────────────────

fn arg(msg: &Message, n: usize) -> u64 { u64::from_le_bytes(msg.data[n * 8..n * 8 + 8].try_into().unwrap()) }
fn reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.tag = 0x8000_0000_0000_0000;
    m.data[0..8].copy_from_slice(&v.to_le_bytes());
    m
}
fn err(e: i32) -> Message { reply(-(e as i64)) }

fn handle_msg(msg: &Message, _caller: u32, _port: u32) -> Message {
    match msg.tag {
        vfs::VFS_OPEN => {
            let open_id = arg(msg, 4) as u32;
            let pid = arg(msg, 3) as u32;
            let ns = sched::ns::NsRef::get(sched::ns::netns_of(pid));
            let mut o = OPENS.lock();
            if o.len() >= 32 { return err(23); }
            o.push(TOpen { id: open_id, ns, att: None, rq: VecDeque::new(), seq: 0 });
            reply(0)
        }
        vfs::VFS_CLOSE => {
            let open_id = arg(msg, 1) as u32;
            let t = {
                let mut o = OPENS.lock();
                let i = o.iter().position(|x| x.id == open_id);
                i.map(|i| o.remove(i))
            };
            if let Some(t) = t {
                if let Some(a) = t.att {
                    if let Some(b) = backend() { (b.detach)(open_id, a.persist); }
                    if a.persist {
                        PERSIST.lock().push(Persist { ns: t.ns, name: a.name, tap: a.tap, owner: a.owner, group: a.group });
                    }
                }
            }
            reply(0)
        }
        vfs::VFS_READ => do_read(arg(msg, 1) as usize, arg(msg, 2) as usize, arg(msg, 4) as u32),
        vfs::VFS_WRITE => do_write(arg(msg, 1) as usize, arg(msg, 2) as usize, arg(msg, 4) as u32),
        vfs::VFS_POLL => {
            let open_id = arg(msg, 4) as u32;
            let (rev, seq) = {
                let o = OPENS.lock();
                match o.iter().find(|x| x.id == open_id) {
                    Some(t) if t.att.is_some() => (4u64 | if t.rq.is_empty() { 0 } else { 1 }, t.seq),
                    _ => (4u64 | 0x8, 0), // POLLOUT | POLLERR: not attached
                }
            };
            let mut m = Message::empty();
            m.data[0..8].copy_from_slice(&rev.to_le_bytes());
            m.data[8..16].copy_from_slice(&seq.to_le_bytes());
            m.data[24..32].copy_from_slice(&wake_tag(open_id).to_le_bytes());
            m.data[32] = 1;
            m
        }
        vfs::VFS_IOCTL => {
            match do_ioctl(arg(msg, 1) as u32, arg(msg, 2) as usize, arg(msg, 4) as u32) {
                Ok(v) => reply(v), Err(e) => err(e),
            }
        }
        _ => err(22),
    }
}

fn is_attached(open_id: u32) -> Result<(), i32> {
    match OPENS.lock().iter().find(|x| x.id == open_id) {
        Some(t) if t.att.is_some() => Ok(()),
        Some(_) => Err(77), // EBADFD
        None => Err(22),
    }
}

fn pop_frame(open_id: u32) -> Result<alloc::vec::Vec<u8>, i32> {
    let mut o = OPENS.lock();
    let Some(t) = o.iter_mut().find(|x| x.id == open_id) else { return Err(22) };
    if t.att.is_none() { return Err(77); }
    match t.rq.pop_front() { Some(f) => Ok(f), None => Err(11) }
}

fn do_read(buf: usize, count: usize, open_id: u32) -> Message {
    let f = match pop_frame(open_id) { Ok(f) => f, Err(e) => return err(e) };
    let n = f.len().min(count);
    if buf == 0 || !sched::with_current_address_space_mut(|a| a.write_user_buf(buf, &f[..n])).unwrap_or(false) {
        return err(14);
    }
    reply(n as i64)
}

/// readv(2) on a tun/tap fd: ONE frame, scattered over the iovecs (the rest
/// of the frame is dropped, like read(2) with a short buffer).
pub fn readv(open_id: u32, iov_ptr: usize, iovcnt: usize) -> Message {
    let mut iovs: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
    for i in 0..iovcnt.min(1024) {
        let (Some(b), Some(l)) = (rd::<usize>(iov_ptr + i * 16), rd::<usize>(iov_ptr + i * 16 + 8)) else { return err(14) };
        iovs.push((b, l));
    }
    let f = match pop_frame(open_id) { Ok(f) => f, Err(e) => return err(e) };
    let mut off = 0usize;
    for (b, l) in iovs {
        if off >= f.len() { break; }
        let k = l.min(f.len() - off);
        if k == 0 { continue; }
        if b == 0 || !sched::with_current_address_space_mut(|a| a.write_user_buf(b, &f[off..off + k])).unwrap_or(false) {
            return err(14);
        }
        off += k;
    }
    reply(off as i64)
}

/// writev(2) on a tun/tap fd: the gathered iovecs are ONE frame.
pub fn writev(open_id: u32, iov_ptr: usize, iovcnt: usize) -> Message {
    let mut data: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
    for i in 0..iovcnt.min(1024) {
        let (Some(b), Some(l)) = (rd::<usize>(iov_ptr + i * 16), rd::<usize>(iov_ptr + i * 16 + 8)) else { return err(14) };
        if l == 0 { continue; }
        if data.len() + l > 65535 { return err(22); }
        let at = data.len();
        data.resize(at + l, 0);
        if b == 0 || !sched::with_current_address_space(|a| a.read_user_buf(b, &mut data[at..])).unwrap_or(false) {
            return err(14);
        }
    }
    do_write_data(data, open_id)
}

fn do_write(buf: usize, count: usize, open_id: u32) -> Message {
    let pi = {
        let o = OPENS.lock();
        match o.iter().find(|x| x.id == open_id) {
            Some(t) => match t.att.as_ref() { Some(a) => a.pi, None => return err(77) },
            None => return err(22),
        }
    };
    if count > 65535 { return err(22); }
    let mut data = alloc::vec![0u8; count];
    if count > 0 && (buf == 0 || !sched::with_current_address_space(|a| a.read_user_buf(buf, &mut data)).unwrap_or(false)) {
        return err(14);
    }
    write_frame(data, pi, open_id)
}

fn do_write_data(data: alloc::vec::Vec<u8>, open_id: u32) -> Message {
    let pi = {
        let o = OPENS.lock();
        match o.iter().find(|x| x.id == open_id) {
            Some(t) => match t.att.as_ref() { Some(a) => a.pi, None => return err(77) },
            None => return err(22),
        }
    };
    write_frame(data, pi, open_id)
}

fn write_frame(data: alloc::vec::Vec<u8>, pi: bool, open_id: u32) -> Message {
    let count = data.len();
    let d = if pi {
        if data.len() < 4 { return err(22); }
        &data[4..]
    } else { &data[..] };
    let Some(b) = backend() else { return err(19) };
    match (b.input)(open_id, d) { Ok(()) => reply(count as i64), Err(e) => err(e) }
}

fn rd<T: Copy>(ptr: usize) -> Option<T> { sched::uaccess::read_user::<T>(ptr) }

fn nlen(n: &[u8; 16]) -> usize { n.iter().position(|&b| b == 0).unwrap_or(16) }

fn do_ioctl(cmd: u32, ptr: usize, open_id: u32) -> Result<i64, i32> {
    const TUNGETFEATURES: u32 = 0x800454cf;
    const TUNSETIFF: u32 = 0x400454ca;
    const TUNGETIFF: u32 = 0x800454d2;
    const TUNSETPERSIST: u32 = 0x400454cb;
    const TUNSETOWNER: u32 = 0x400454cc;
    const TUNSETGROUP: u32 = 0x400454ce;
    let ons = OPENS.lock().iter().find(|x| x.id == open_id).map(|x| x.ns.id()).ok_or(22)?;
    match cmd {
        TUNGETFEATURES => {
            let f = (IFF_TUN | IFF_TAP | IFF_NO_PI) as u32;
            if sched::uaccess::write_user(ptr, f) { Ok(0) } else { Err(14) }
        }
        TUNSETIFF => {
            let ifr = rd::<[u8; 40]>(ptr).ok_or(14)?;
            let flags = u16::from_le_bytes([ifr[16], ifr[17]]);
            if (flags & IFF_TUN != 0) == (flags & IFF_TAP != 0) { return Err(22); }
            if flags & !(IFF_TUN | IFF_TAP | IFF_NO_PI) != 0 { return Err(22); } // VNET_HDR, MULTI_QUEUE, ...
            let tap = flags & IFF_TAP != 0;
            if OPENS.lock().iter().any(|x| x.id == open_id && x.att.is_some()) { return Err(16); }
            let b = backend().ok_or(19)?; // ENODEV
            let mut name = [0u8; 16];
            name[..15].copy_from_slice(&ifr[..15]);
            let n = nlen(&name);
            if name[..n].iter().any(|&c| c == b'/' || c == b' ') { return Err(22); }
            // CAP_NET_ADMIN over the netns owner's user namespace.
            let capable = sched::caps::ns_capable_in(sched::ns::owner_userns(1, ons), CAP_NET_ADMIN);
            // Re-attach of a persistent interface by exact name.
            let (mut owner, mut group, mut persist) = (u32::MAX, u32::MAX, false);
            let pidx = if n > 0 && !name[..n].windows(2).any(|w| w == b"%d") {
                PERSIST.lock().iter().position(|p| p.ns.id() == ons && p.name[..nlen(&p.name)] == name[..n])
            } else { None };
            if let Some(i) = pidx {
                let (pt, po, pg) = { let p = PERSIST.lock(); (p[i].tap, p[i].owner, p[i].group) };
                if pt != tap { return Err(22); }
                let euid = sched::current_euid();
                if !capable && po != euid { return Err(1); }
                owner = po; group = pg; persist = true;
            } else if !capable { return Err(1); }
            let (_idx, fin) = (b.attach)(ons, &name, tap, open_id)?;
            if let Some(i) = pidx { PERSIST.lock().remove(i); }
            {
                let mut o = OPENS.lock();
                let Some(t) = o.iter_mut().find(|x| x.id == open_id) else { drop(o); (b.detach)(open_id, false); return Err(9) };
                t.att = Some(Attached { tap, pi: flags & IFF_NO_PI == 0, name: fin, persist, owner, group });
            }
            let mut out = ifr;
            out[..16].copy_from_slice(&fin);
            if !sched::uaccess::write_user(ptr, out) { return Err(14); }
            Ok(0)
        }
        TUNGETIFF => {
            let mut out = [0u8; 40];
            {
                let o = OPENS.lock();
                let a = o.iter().find(|x| x.id == open_id).and_then(|x| x.att.as_ref()).ok_or(77)?;
                out[..16].copy_from_slice(&a.name);
                let fl = (if a.tap { IFF_TAP } else { IFF_TUN }) | if a.pi { 0 } else { IFF_NO_PI };
                out[16..18].copy_from_slice(&fl.to_le_bytes());
            }
            if !sched::uaccess::write_user(ptr, out) { return Err(14); }
            Ok(0)
        }
        TUNSETPERSIST | TUNSETOWNER | TUNSETGROUP => {
            is_attached(open_id)?;
            if !sched::caps::ns_capable_in(sched::ns::owner_userns(1, ons), CAP_NET_ADMIN) { return Err(1); }
            let mut o = OPENS.lock();
            let a = o.iter_mut().find(|x| x.id == open_id).and_then(|x| x.att.as_mut()).ok_or(77)?;
            match cmd {
                TUNSETPERSIST => a.persist = ptr != 0,
                TUNSETOWNER => a.owner = ptr as u32,
                _ => a.group = ptr as u32,
            }
            Ok(0)
        }
        0x541B => { // FIONREAD
            is_attached(open_id)?;
            let n = OPENS.lock().iter().find(|x| x.id == open_id).and_then(|x| x.rq.front().map(|f| f.len())).unwrap_or(0) as i32;
            if sched::uaccess::write_user(ptr, n) { Ok(0) } else { Err(14) }
        }
        _ => Err(25),
    }
}
