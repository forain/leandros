//! AF_NETLINK / NETLINK_ROUTE sockets: the per-description reply queue and the
//! bind/send/recv/poll entry points `lib.rs` calls first for such a socket.
//! The messages themselves are built by `rtnetlink`.
//!
//! The socket table entry is an ordinary `Unbound { domain: AF_NETLINK }`
//! one (so close/dup/fork/SCM_RIGHTS keep working); its state lives here,
//! keyed by the open file description. A request is answered when it is sent:
//! the reply datagrams are queued and the socket becomes readable.

use super::*;
use crate::rtnetlink as rt;
use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;

const MSG_TRUNC_F: usize = 0x20;
const MAX_REQ: usize = 64 * 1024;
const MAX_QUEUED: usize = 64;
const MAX_QUEUED_NF: usize = 4096;

struct NlSock {
    pid: u32,
    /// Network namespace the socket was created in.
    ns: sched::ns::NsRef,
    groups: u32,
    /// Netlink protocol (NETLINK_ROUTE or NETLINK_NETFILTER).
    proto: usize,
    /// connect(2) destination (pid, groups); only the kernel (pid 0) is meaningful.
    peer: Option<(u32, u32)>,
    q: VecDeque<Vec<u8>>,
    /// Bumped on every enqueue: the poll edge sequence.
    gen: u64,
}

/// RTMGRP_* bits a NETLINK_ROUTE socket can bind (or NETLINK_ADD_MEMBERSHIP
/// `RTNLGRP_n` = bit n-1) to receive IPv6 address / route notifications.
const GRP_IPV6_IFADDR: u32 = 1 << 8;
const GRP_IPV6_ROUTE: u32 = 1 << 10;
const GRP_LINK: u32 = 1;
const GRP_IPV4_IFADDR: u32 = 1 << 4;
const GRP_IPV4_ROUTE: u32 = 1 << 6;
const GRP_V6: u32 = GRP_IPV6_IFADDR | GRP_IPV6_ROUTE | GRP_LINK | GRP_IPV4_IFADDR | GRP_IPV4_ROUTE;

/// Last-seen IPv6 addresses/routes per namespace that has subscribers; the
/// diff against the live tables is what produces RTM_NEW/DELADDR/ROUTE
/// multicasts, so every source (static, SLAAC, vif, tun) notifies alike.
static SNAP: Mutex<BTreeMap<u16, Snap>> = Mutex::new(BTreeMap::new());
/// Links with their IFLA_INET6_FLAGS, v6 addresses/routes, v4 routes.
#[derive(Clone)]
struct Snap { a6: Vec<rt::A6>, r6: Vec<rt::R6>, links: Vec<(rt::Link, u32)>, r4: Vec<rt::RouteEnt> }
static V6_SUBS: AtomicU32 = AtomicU32::new(0);

static NL: Mutex<BTreeMap<u32, NlSock>> = Mutex::new(BTreeMap::new());
static NL_EVER: AtomicU32 = AtomicU32::new(0);

/// A new netlink description `ofd` owned by process `tgid`.
pub fn create(ofd: u32, tgid: u32, netns: u16, proto: usize) {
    let mut m = NL.lock();
    m.retain(|&k, _| vfs::ofd::live(k));
    // Linux autobind: nl_portid is the pid as the creator sees it (task_tgid_vnr).
    // bwrap checks nlmsg_pid == getpid() inside its new pid namespace.
    let vn = sched::pidns::pidns_of(tgid);
    let portid = if vn != 0 { sched::pidns::to_v(vn, tgid) } else { tgid };
    m.insert(ofd, NlSock { pid: portid, ns: sched::ns::NsRef::get(netns), groups: 0, proto, peer: None, q: VecDeque::new(), gen: 0 });
    NL_EVER.store(1, Ordering::Release);
}

/// Drop the netlink sockets whose description is gone (every holder closed or
/// exited). Their namespace reference is the last thing keeping a dead netns
/// (and its veth ends) alive, so the reaper calls this before it looks for
/// namespaces that died; `create` alone only pruned on the next new socket.
pub fn prune() {
    if NL_EVER.load(Ordering::Acquire) == 0 { return; }
    let dead: Vec<NlSock> = {
        let mut m = NL.lock();
        let ids: Vec<u32> = m.keys().copied().filter(|&k| !vfs::ofd::live(k)).collect();
        ids.into_iter().filter_map(|k| m.remove(&k)).collect()
    };
    drop(dead); // the references drop with no netlink lock held
}

/// The description of `fd` when it is a netlink socket.
pub fn of(pid: u32, fd: usize) -> Option<u32> {
    if NL_EVER.load(Ordering::Acquire) == 0 { return None; }
    let ofd = fd_ofd(pid, fd).filter(|&o| o != 0)?;
    if NL.lock().contains_key(&ofd) { Some(ofd) } else { None }
}

/// (readable, edge sequence) for poll.
pub fn poll_state(ofd: u32) -> (bool, u64) {
    NL.lock().get(&ofd).map_or((false, 0), |s| (!s.q.is_empty(), s.gen))
}

pub fn bind(ofd: u32, addr_ptr: usize, addrlen: usize) -> Message {
    if addrlen < 12 { return err_reply(-22); }
    let mut sa = [0u8; 12];
    if !ucopy_in(&mut sa, addr_ptr) { return err_reply(-14); }
    if u16::from_ne_bytes([sa[0], sa[1]]) as usize != rt::AF_NETLINK { return err_reply(-22); }
    let pid = u32::from_ne_bytes([sa[4], sa[5], sa[6], sa[7]]);
    let groups = u32::from_ne_bytes([sa[8], sa[9], sa[10], sa[11]]);
    if let Some(s) = NL.lock().get_mut(&ofd) {
        if pid != 0 { s.pid = pid; }
        s.groups = groups;
    }
    note_subs();
    ok_reply()
}

pub fn connect(ofd: u32, addr_ptr: usize, addrlen: usize) -> Message {
    if addrlen < 12 { return err_reply(-22); }
    let mut sa = [0u8; 12];
    if !ucopy_in(&mut sa, addr_ptr) { return err_reply(-14); }
    if u16::from_ne_bytes([sa[0], sa[1]]) as usize != rt::AF_NETLINK { return err_reply(-97); }
    let pid = u32::from_ne_bytes([sa[4], sa[5], sa[6], sa[7]]);
    let groups = u32::from_ne_bytes([sa[8], sa[9], sa[10], sa[11]]);
    if let Some(s) = NL.lock().get_mut(&ofd) { s.peer = Some((pid, groups)); }
    ok_reply()
}

pub fn getpeername(ofd: u32, addr_ptr: usize, addrlen_ptr: usize) -> Message {
    let Some(Some((pid, groups))) = NL.lock().get(&ofd).map(|s| s.peer) else { return err_reply(-107) };
    let sa = rt::sockaddr_nl(pid, groups);
    let cap = match sched::uaccess::read_user::<u32>(addrlen_ptr) { Some(c) => c as usize, None => return err_reply(-14) };
    if !ucopy_out(addr_ptr, &sa[..cap.min(12)]) || !wr_user(addrlen_ptr, 12u32) { return err_reply(-14); }
    ok_reply()
}

pub fn getsockname(ofd: u32, addr_ptr: usize, addrlen_ptr: usize) -> Message {
    let (pid, groups) = NL.lock().get(&ofd).map_or((0, 0), |s| (s.pid, s.groups));
    let sa = rt::sockaddr_nl(pid, groups);
    let cap = match sched::uaccess::read_user::<u32>(addrlen_ptr) { Some(c) => c as usize, None => return err_reply(-14) };
    if !ucopy_out(addr_ptr, &sa[..cap.min(12)]) || !wr_user(addrlen_ptr, 12u32) { return err_reply(-14); }
    ok_reply()
}

static NLTRACE: AtomicU8 = AtomicU8::new(0); // 0 unread, 1 off, 2 on

fn nltrace() -> bool {
    let mut v = NLTRACE.load(Ordering::Relaxed);
    if v == 0 {
        v = if drivers::nvidia_probe::cmdline_opt("net.nltrace") == Some(1) { 2 } else { 1 };
        NLTRACE.store(v, Ordering::Relaxed);
    }
    v == 2
}

fn trace_out(line: &str) {
    extern "C" { fn arch_serial_putc(c: u8); }
    for &b in line.as_bytes() { unsafe { arch_serial_putc(b); } }
    unsafe { arch_serial_putc(b'\n'); }
}

/// Attribute types at one level of `b` (nested ones as `t{..}` one level deep).
fn attr_list(b: &[u8], depth: u32) -> alloc::string::String {
    use alloc::format;
    let mut o = alloc::string::String::new();
    let mut p = 0usize;
    while p + 4 <= b.len() {
        let l = u16::from_ne_bytes([b[p], b[p + 1]]) as usize;
        let t = u16::from_ne_bytes([b[p + 2], b[p + 3]]);
        if l < 4 || p + l > b.len() { break; }
        let body = &b[p + 4..p + l];
        if !o.is_empty() { o.push(','); }
        if t & 0x8000 != 0 && depth == 0 {
            o.push_str(&format!("{}{{{}}}", t & 0x3fff, attr_list(body, 1)));
        } else if body.len() >= 1 && body.len() <= 16 && body.iter().take(body.len().saturating_sub(1)).all(|&c| (0x20..0x7f).contains(&c)) && body.last() == Some(&0) {
            o.push_str(&format!("{}=\"{}\"", t, core::str::from_utf8(&body[..body.len() - 1]).unwrap_or("?")));
        } else if body.len() == 4 {
            o.push_str(&format!("{}={}", t, u32::from_ne_bytes([body[0], body[1], body[2], body[3]])));
        } else {
            o.push_str(&format!("{}[{}]", t, body.len()));
        }
        p += (l + 3) & !3;
    }
    o
}

fn trace_req(data: &[u8], ns: u16, pid: u32) {
    use alloc::format;
    let mut off = 0usize;
    while off + 16 <= data.len() {
        let len = u32::from_ne_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]]) as usize;
        if len < 16 || off + len > data.len() { break; }
        let ty = u16::from_ne_bytes([data[off + 4], data[off + 5]]);
        let fl = u16::from_ne_bytes([data[off + 6], data[off + 7]]);
        let hdr = match ty { 16..=19 => 16, 20 | 21 => 8, 24 | 25 => 12, _ => 0 };
        let body = &data[off + 16..off + len];
        let fixed = body.get(..hdr.min(body.len())).unwrap_or(&[]);
        let attrs = if body.len() > hdr { attr_list(&body[hdr..], 0) } else { alloc::string::String::new() };
        let mut hx = alloc::string::String::new();
        for c in fixed { hx.push_str(&format!("{:02x}", c)); }
        trace_out(&format!("nltrace: ns={} pid={} REQ type={} flags={:#x} len={} fixed={} attrs=[{}]", ns, pid, ty, fl, len, hx, attrs));
        off += (len + 3) & !3;
    }
}

fn trace_rep(replies: &[Vec<u8>], ns: u16) {
    use alloc::format;
    for r in replies {
        if r.len() < 16 { continue; }
        let ty = u16::from_ne_bytes([r[4], r[5]]);
        if ty == 2 && r.len() >= 20 {
            trace_out(&format!("nltrace: ns={} REP ERROR errno={}", ns, i32::from_ne_bytes([r[16], r[17], r[18], r[19]])));
        } else {
            trace_out(&format!("nltrace: ns={} REP type={} len={}", ns, ty, r.len()));
        }
    }
}

/// A request arrived: queue the replies and wake pollers. Returns the length.
pub fn send(ofd: u32, data: &[u8]) -> Message {
    let (pid, ns, proto) = match NL.lock().get(&ofd) { Some(s) => (s.pid, s.ns.id(), s.proto), None => return err_reply(-9) };
    if proto == crate::nfnl::NETLINK_NETFILTER {
        let admin = sched::caps::ns_capable_in(sched::ns::owner_userns(1, ns), 12);
        let replies = crate::nfnl::handle(ns, pid, admin, data);
        if nltrace() {
            use alloc::format;
            for (tag, d) in core::iter::once(("REQ", data)).chain(replies.iter().map(|r| ("REP", &r[..]))) {
                let mut hx = alloc::string::String::new();
                for c in d.iter().take(300) { hx.push_str(&format!("{:02x}", c)); }
                trace_out(&format!("nfnl {} {} {}", tag, d.len(), hx));
            }
        }
        let mut m = NL.lock();
        if let Some(s) = m.get_mut(&ofd) {
            for r in replies { if s.q.len() >= MAX_QUEUED_NF { break; } s.q.push_back(r); }
            s.gen += 1;
        }
        drop(m);
        sock_state_moved(&[sock_state_key(ofd)]);
        return val_reply(data.len() as u64);
    }
    crate::vif::daemon_pass(); // reap veth pairs of namespaces that died
    let links = rt::current_links(ns);
    let routes = rt::current_routes(ns);
    let lo_set = |up: bool| { if ns == 0 { false } else { sched::ns::set_lo_admin_up(ns, up); true } };
    let (mut addrs6, mut routes6) = if ns == 0 { (crate::ip6::addrs6(), crate::ip6::routes6()) } else { (crate::ip6::lo_addrs6(ns), alloc::vec::Vec::new()) };
    addrs6.extend(crate::tunbe::addrs6(ns));
    routes6.extend(crate::tunbe::routes6(ns));
    let env = rt::Env { addrs6: &addrs6, routes6: &routes6, route6: &|d| if ns == 0 { crate::ip6::lookup6(d) } else { None }, links: &links, routes: &routes, route: &|d| if ns == 0 { rt::route_lookup(d) } else { None }, lo_set: &lo_set,
        apply: &|op| crate::tunbe::rtnl_apply(ns, op),
        admin: sched::caps::ns_capable_in(sched::ns::owner_userns(1, ns), 12) };
    let tr = nltrace();
    if tr { trace_req(data, ns, pid); }
    let replies = rt::handle_request_env(data, pid, &env);
    if tr { trace_rep(&replies, ns); }
    if !replies.is_empty() {
        let mut m = NL.lock();
        if let Some(s) = m.get_mut(&ofd) {
            for r in replies {
                if s.q.len() >= MAX_QUEUED { break; }
                s.q.push_back(r);
            }
            s.gen += 1;
        }
    }
    sock_state_moved(&[sock_state_key(ofd)]);
    poll_notify();
    val_reply(data.len() as u64)
}

pub fn send_buf(ofd: u32, buf_ptr: usize, len: usize) -> Message {
    if len > MAX_REQ { return err_reply(-90); }
    let mut data = alloc::vec![0u8; len];
    if !ucopy_in(&mut data, buf_ptr) { return err_reply(-14); }
    send(ofd, &data)
}

pub fn send_msg(ofd: u32, msghdr_ptr: usize) -> Message {
    let iov_ptr = match rd_usize(msghdr_ptr + 16) { Ok(v) => v, Err(_) => return err_reply(-14) };
    let iovcnt = match rd_usize(msghdr_ptr + 24) { Ok(v) => v, Err(_) => return err_reply(-14) };
    let mut data = Vec::new();
    for i in 0..iovcnt.min(UIO_MAXIOV) {
        let b = match rd_usize(iov_ptr + i * 16) { Ok(v) => v, Err(_) => return err_reply(-14) };
        let l = match rd_usize(iov_ptr + i * 16 + 8) { Ok(v) => v, Err(_) => return err_reply(-14) };
        if data.len() + l > MAX_REQ { return err_reply(-90); }
        let at = data.len();
        data.resize(at + l, 0);
        if !ucopy_in(&mut data[at..], b) { return err_reply(-14); }
    }
    send(ofd, &data)
}

/// The next datagram (a copy; `peek` leaves it queued), or -EAGAIN.
fn next(ofd: u32, peek: bool) -> Result<Vec<u8>, i32> {
    let mut m = NL.lock();
    let s = m.get_mut(&ofd).ok_or(-9)?;
    if peek { s.q.front().cloned().ok_or(-11) } else { s.q.pop_front().ok_or(-11) }
}

pub fn recv(ofd: u32, buf_ptr: usize, len: usize, addr_ptr: usize, addrlen_ptr: usize, flags: usize) -> Message {
    let d = match next(ofd, flags & MSG_PEEK != 0) { Ok(d) => d, Err(e) => return err_reply(e) };
    let n = d.len().min(len);
    if n > 0 && !ucopy_out(buf_ptr, &d[..n]) { return err_reply(-14); }
    if addr_ptr != 0 && addrlen_ptr != 0 {
        let sa = rt::sockaddr_nl(0, 0);
        let cap = sched::uaccess::read_user::<u32>(addrlen_ptr).unwrap_or(0) as usize;
        if !ucopy_out(addr_ptr, &sa[..cap.min(12)]) || !wr_user(addrlen_ptr, 12u32) { return err_reply(-14); }
    }
    val_reply(if flags & MSG_TRUNC_F != 0 { d.len() } else { n } as u64)
}

pub fn recv_msg(ofd: u32, msghdr_ptr: usize, flags: usize) -> Message {
    let rd = |o: usize| rd_usize(msghdr_ptr + o);
    let (name_ptr, iov_ptr, iovcnt) = match (rd(0), rd(16), rd(24)) {
        (Ok(a), Ok(b), Ok(c)) => (a, b, c), _ => return err_reply(-14),
    };
    let d = match next(ofd, flags & MSG_PEEK != 0) { Ok(d) => d, Err(e) => return err_reply(e) };
    let mut off = 0usize;
    for i in 0..iovcnt.min(UIO_MAXIOV) {
        if off >= d.len() { break; }
        let (b, l) = match (rd_usize(iov_ptr + i * 16), rd_usize(iov_ptr + i * 16 + 8)) {
            (Ok(b), Ok(l)) => (b, l), _ => return err_reply(-14),
        };
        let k = l.min(d.len() - off);
        if k > 0 && !ucopy_out(b, &d[off..off + k]) { return err_reply(-14); }
        off += k;
    }
    if name_ptr != 0 {
        let cap = sched::uaccess::read_user::<u32>(msghdr_ptr + 8).unwrap_or(0) as usize;
        let sa = rt::sockaddr_nl(0, 0);
        if !ucopy_out(name_ptr, &sa[..cap.min(12)]) || !wr_user(msghdr_ptr + 8, 12u32) { return err_reply(-14); }
    }
    let truncated = off < d.len();
    unsafe { write_msg_tail(msghdr_ptr, 0, if truncated { MSG_TRUNC } else { 0 }); }
    val_reply(if flags & MSG_TRUNC_F != 0 { d.len() } else { off } as u64)
}

/// NETLINK_ADD_MEMBERSHIP (1) / NETLINK_DROP_MEMBERSHIP (2) on a netlink socket.
pub fn membership(ofd: u32, optname: usize, grp: u32) -> Message {
    if grp == 0 || grp > 31 { return err_reply(-22); }
    if let Some(s) = NL.lock().get_mut(&ofd) {
        match optname {
            1 => s.groups |= 1 << (grp - 1),
            2 => s.groups &= !(1 << (grp - 1)),
            _ => {}
        }
    }
    note_subs();
    ok_reply()
}

/// Recount the IPv6 subscribers and baseline the namespaces that gained one.
fn note_subs() {
    let nss: Vec<u16> = NL.lock().values().filter(|s| s.proto == rt::NETLINK_ROUTE && s.groups & GRP_V6 != 0).map(|s| s.ns.id()).collect();
    V6_SUBS.store(nss.len() as u32, Ordering::Release);
    let mut snap = SNAP.lock();
    snap.retain(|k, _| nss.contains(k));
    drop(snap);
    for ns in nss {
        if !SNAP.lock().contains_key(&ns) { let cur = live6(ns); SNAP.lock().insert(ns, cur); }
    }
}

fn live6(ns: u16) -> Snap {
    let (mut a, mut r) = if ns == 0 { (crate::ip6::addrs6(), crate::ip6::routes6()) } else { (crate::ip6::lo_addrs6(ns), Vec::new()) };
    a.extend(crate::tunbe::addrs6(ns));
    r.extend(crate::tunbe::routes6(ns));
    let links = rt::current_links(ns).into_iter().map(|l| { let f = rt::inet6_flags(&a, l.index); (l, f) }).collect();
    Snap { a6: a, r6: r, links, r4: rt::current_routes(ns) }
}

/// A link compared without its IPv4 address (that is an address event).
fn link_key(l: &rt::Link) -> rt::Link { let mut k = l.clone(); k.addr = None; k }

fn with_hdr(mut m: Vec<u8>, ty: u16) -> Vec<u8> {
    m[4..6].copy_from_slice(&ty.to_ne_bytes());
    m[6..8].copy_from_slice(&0u16.to_ne_bytes()); // not part of a dump
    m
}

/// Multicast the IPv6 address / route changes since the last call to the
/// subscribed sockets. Cheap when nobody listens; called from the net daemon
/// loop and right after a request that may have changed state.
pub fn poll_notify() {
    if V6_SUBS.load(Ordering::Acquire) == 0 { return; }
    let nss: Vec<u16> = SNAP.lock().keys().copied().collect();
    for ns in nss {
        let cur = live6(ns);
        let old = match SNAP.lock().get(&ns) { Some(x) => x.clone(), None => continue };
        let (olda, oldr) = (&old.a6, &old.r6);
        let same_links = old.links.len() == cur.links.len()
            && old.links.iter().zip(&cur.links).all(|(o, c)| link_key(&o.0) == link_key(&c.0) && o.0.addr == c.0.addr && o.1 == c.1);
        if cur.a6 == old.a6 && cur.r6 == old.r6 && cur.r4 == old.r4 && same_links { continue; }
        let name = |i: u32| cur.links.iter().chain(old.links.iter()).find(|l| l.0.index == i).map_or(alloc::string::String::new(), |l| l.0.name.clone());
        let mut ev: Vec<(u32, Vec<u8>)> = Vec::new();
        // Links: DELLINK for the gone, NEWLINK for the new or changed (flags, state, IFLA_INET6_FLAGS).
        for (o, f) in &old.links { if !cur.links.iter().any(|c| c.0.index == o.index) { ev.push((GRP_LINK, with_hdr(rt::link_msg(o, *f, 0, 0), 17))); } }
        for (c, f) in &cur.links {
            let was = old.links.iter().find(|o| o.0.index == c.index);
            if was.map_or(true, |o| link_key(&o.0) != link_key(c) || o.1 != *f) { ev.push((GRP_LINK, with_hdr(rt::link_msg(c, *f, 0, 0), 16))); }
        }
        // IPv4 addresses.
        for (o, _) in &old.links {
            if let Some((a, p)) = o.addr {
                if !cur.links.iter().any(|c| c.0.index == o.index && c.0.addr == o.addr) { ev.push((GRP_IPV4_IFADDR, with_hdr(rt::addr_msg(o, a, p, 0, 0), 21))); }
            }
        }
        for (c, _) in &cur.links {
            if let Some((a, p)) = c.addr {
                if !old.links.iter().any(|o| o.0.index == c.index && o.0.addr == c.addr) { ev.push((GRP_IPV4_IFADDR, with_hdr(rt::addr_msg(c, a, p, 0, 0), 20))); }
            }
        }
        for r in &old.r4 { if !cur.r4.contains(r) { ev.push((GRP_IPV4_ROUTE, with_hdr(rt::route_ent_msg(r, 0, 0), 25))); } }
        for r in &cur.r4 { if !old.r4.contains(r) { ev.push((GRP_IPV4_ROUTE, with_hdr(rt::route_ent_msg(r, 0, 0), 24))); } }
        // IPv6: a flag change of a kept address (tentative -> permanent) re-announces it.
        for a in olda { if !cur.a6.iter().any(|c| c.index == a.index && c.addr == a.addr && c.prefix == a.prefix) { ev.push((GRP_IPV6_IFADDR, with_hdr(rt::addr6_msg(a, &name(a.index), 0, 0), 21))); } }
        for a in &cur.a6 { if !olda.contains(a) { ev.push((GRP_IPV6_IFADDR, with_hdr(rt::addr6_msg(a, &name(a.index), 0, 0), 20))); } }
        for r in oldr { if !cur.r6.contains(r) { ev.push((GRP_IPV6_ROUTE, with_hdr(rt::route6_msg(r, 0, 0, false, None), 25))); } }
        for r in &cur.r6 { if !oldr.contains(r) { ev.push((GRP_IPV6_ROUTE, with_hdr(rt::route6_msg(r, 0, 0, false, None), 24))); } }
        SNAP.lock().insert(ns, cur);
        let mut keys = Vec::new();
        {
            let mut m = NL.lock();
            for (&ofd, s) in m.iter_mut() {
                if s.proto != rt::NETLINK_ROUTE || s.ns.id() != ns { continue; }
                let mut got = false;
                for (g, msg) in &ev {
                    if s.groups & g != 0 && s.q.len() < MAX_QUEUED { s.q.push_back(msg.clone()); got = true; }
                }
                if got { s.gen += 1; keys.push(sock_state_key(ofd)); }
            }
        }
        if !keys.is_empty() { sock_state_moved(&keys); }
    }
}
