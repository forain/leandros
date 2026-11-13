//! Per-interface IPv6 state of the NIC stacks (design-ipv6 L4): the link-local
//! address, scope-id mapping, on-link / default-router route selection and
//! `/proc/net/if_inet6`.
//!
//! The link-local address (fe80::/64, EUI-64 of the MAC) is added to the
//! smoltcp interface while the netif has a stack, is administratively up and
//! `disable_ipv6` is 0 for it (`sync`, called once per daemon pass). smoltcp
//! then answers neighbour solicitations for it and resolves peers by NDP.
//! Leaf locks only: `LL`, `PREFIXES`, `ROUTERS` are taken with no stack lock
//! held, except `sync`, which takes `LL` under one stack lock.

use alloc::vec::Vec;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::raw;
use smoltcp::wire::{IpProtocol, IpVersion};
use core::fmt::Write;
use smoltcp::wire::{IpAddress, IpCidr, Ipv6Address};
use spin::Mutex;

use drivers::netdev::{self, MAX_NETIF};
use crate::iface::{self, Sid, FIRST_DYN};

/// The link-local address installed on each netif's stack.
static LL: Mutex<[Option<[u8; 16]>; MAX_NETIF]> = Mutex::new([None; MAX_NETIF]);
/// On-link prefixes `(sid, prefix, len, metric)` besides the link-local one (SLAAC, static).
static PREFIXES: Mutex<Vec<(Sid, [u8; 16], u8, u32)>> = Mutex::new(Vec::new());
/// Default routers `(sid, router link-local, metric)`.
static ROUTERS: Mutex<Vec<(Sid, [u8; 16], u32)>> = Mutex::new(Vec::new());

/// Modified EUI-64 link-local address of a MAC (RFC 4291 appendix A).
pub(crate) fn eui64_ll(m: [u8; 6]) -> [u8; 16] {
    [0xfe, 0x80, 0, 0, 0, 0, 0, 0, m[0] ^ 2, m[1], m[2], 0xff, 0xfe, m[3], m[4], m[5]]
}

#[inline]
pub(crate) fn is_link_local(a: &[u8; 16]) -> bool { a[0] == 0xfe && a[1] & 0xc0 == 0x80 }

/// Does connecting / binding / sending to `ip` need a `sin6_scope_id`? (fe80::/10 unicast.)
pub(crate) fn needs_scope(ip: IpAddress) -> bool {
    matches!(ip, IpAddress::Ipv6(a) if is_link_local(&a.0))
}

/// The stack named by a scope id (an ifindex, `sid + 1`): EINVAL for 0 (Linux
/// `ip6_datagram_connect` / `inet6_bind` without a bound device), ENODEV for
/// an index that is no NIC of the namespace.
pub(crate) fn scope_sid(ns: u16, scope: u32) -> Result<Sid, i32> {
    if scope == 0 { return Err(-22); }
    let sid = scope - 1;
    if ns == 0 && sid >= 1 && sid < FIRST_DYN as u32 && crate::stack_for(sid as Sid).is_some() { Ok(sid as Sid) } else { Err(-19) }
}

/// The link-local address of netif `id`, while installed.
pub(crate) fn ll_of(id: u8) -> Option<[u8; 16]> { LL.lock().get(id as usize).copied().flatten() }

/// The NIC stack holding the v6 address `a` (link-local only until SLAAC / static addresses).
pub(crate) fn holder_of(a: Ipv6Address) -> Option<Sid> {
    let t = LL.lock();
    (0..MAX_NETIF).find(|&i| t[i] == Some(a.0)).map(|i| iface::sid_of(i as u8))
}

fn prefix_match(a: &[u8; 16], p: &[u8; 16], len: u8) -> bool {
    let full = (len / 8) as usize;
    if a[..full] != p[..full] { return false; }
    let rem = len % 8;
    rem == 0 || (a[full] ^ p[full]) >> (8 - rem) == 0
}

/// Route steps 4-5 of D7 for a non-link-local destination: longest on-link
/// prefix (ties to the lower metric), else the lowest-metric default router,
/// on stacks with carrier. `None` is ENETUNREACH.
pub(crate) fn route(dst: Ipv6Address) -> Option<Sid> {
    let up = |sid: Sid| sid >= 1 && sid < FIRST_DYN && iface::rt(sid - 1).carrier;
    let mut best: Option<(u8, u32, Sid)> = None;
    for &(sid, p, len, m) in PREFIXES.lock().iter() {
        if !up(sid) || !prefix_match(&dst.0, &p, len) { continue; }
        if best.map_or(true, |(bl, bm, _)| len > bl || (len == bl && m < bm)) { best = Some((len, m, sid)); }
    }
    if let Some((_, _, s)) = best { return Some(s); }
    ROUTERS.lock().iter().filter(|r| up(r.0)).min_by_key(|r| r.2).map(|r| r.0)
}

/// Can stack `sid` carry a new flow to `dst`?
pub(crate) fn can_route(sid: Sid, dst: Ipv6Address) -> bool {
    if sid < 1 || sid >= FIRST_DYN { return false; }
    if is_link_local(&dst.0) { return ll_of(sid - 1).is_some(); }
    route(dst) == Some(sid)
}

/// Replace the IPv4 address of `i` (`None` removes it), keeping every IPv6 one.
pub(crate) fn replace_v4(i: &mut smoltcp::iface::Interface, v4: Option<IpCidr>) {
    i.update_ip_addrs(|addrs| {
        let keep: Vec<IpCidr> = addrs.iter().copied().filter(|c| matches!(c.address(), IpAddress::Ipv6(_))).collect();
        addrs.clear();
        if let Some(c) = v4 { let _ = addrs.push(c); }
        for c in keep { let _ = addrs.push(c); }
    });
}

/// Bring a netif's link-local address in line with its state, once per
/// daemon pass: present while it has a stack, is admin up, has a MAC and
/// `disable_ipv6` is 0 (all or the interface).
pub(crate) fn sync(nif: netdev::Netif, admin_down: bool) {
    let Some(mac) = nif.mac() else { return };
    let disabled = admin_down || sched::ns::ipv6_disabled(0, nif.name().as_bytes());
    let want = if disabled { None } else { Some(eui64_ll(mac)) };
    let id = nif.id as usize;
    let mut stack = crate::stack_for(iface::sid_of(nif.id));
    let Some(ref mut s) = *stack else { return };
    let have = s.interface.ip_addrs().iter().any(|c| matches!(c.address(), IpAddress::Ipv6(a) if is_link_local(&a.0)));
    if have == want.is_some() && LL.lock()[id] == want { return; }
    s.interface.update_ip_addrs(|addrs| {
        let keep: Vec<IpCidr> = addrs.iter().copied()
            .filter(|c| !matches!(c.address(), IpAddress::Ipv6(a) if is_link_local(&a.0))).collect();
        addrs.clear();
        for c in keep { let _ = addrs.push(c); }
        if let Some(l) = want { let _ = addrs.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address(l)), 64)); }
    });
    LL.lock()[id] = want;
    drop(stack);
    iface::notify(nif.id);
}

/// `/proc/net/if_inet6`: `addr ifindex prefixlen scope flags name`, hex.
pub(crate) fn proc_if_inet6(out: &mut impl Write) {
    let _ = writeln!(out, "00000000000000000000000000000001 01 80 10 80       lo");
    for nif in netdev::iter() {
        let Some(a) = ll_of(nif.id) else { continue };
        for b in a { let _ = write!(out, "{:02x}", b); }
        let _ = writeln!(out, " {:02x} 40 20 80 {:>8}", nif.id as u32 + 2, nif.name());
        if let Some((g, _)) = SLAAC.lock()[nif.id as usize].addr {
            for b in g { let _ = write!(out, "{:02x}", b); }
            let _ = writeln!(out, " {:02x} 40 00 00 {:>8}", nif.id as u32 + 2, nif.name());
        }
    }
}

// ── Router discovery and SLAAC (design-ipv6 L5) ──────────────────────────────

const SEC: u64 = 1_000_000_000;
/// Router solicitations per link-up / loss of the router (Linux router_solicitations).
const RS_MAX: u8 = 3;
/// Interval between them (Linux router_solicitation_interval).
const RS_GAP: u64 = 4 * SEC;
const ONE_SHOT: u64 = u64::MAX;

/// What router advertisements told one netif.
#[derive(Clone, Copy)]
struct Slaac {
    /// SLAAC address and when its valid lifetime ends.
    addr: Option<([u8; 16], u64)>,
    /// On-link prefix learned (with L flag): prefix, length, expiry.
    pfx: Option<([u8; 16], u8, u64)>,
    /// Default router (its link-local address) and when its lifetime ends.
    router: Option<([u8; 16], u64)>,
    rs_sent: u8,
    next_rs: u64,
}
impl Slaac { const NONE: Slaac = Slaac { addr: None, pfx: None, router: None, rs_sent: 0, next_rs: 0 }; }
static SLAAC: Mutex<[Slaac; MAX_NETIF]> = Mutex::new([Slaac::NONE; MAX_NETIF]);
/// The internal raw ICMPv6 socket of each NIC stack.
static RAW: Mutex<[Option<SocketHandle>; MAX_NETIF]> = Mutex::new([None; MAX_NETIF]);

/// Add the internal ICMPv6 raw socket to a fresh NIC stack.
pub(crate) fn attach(id: u8, set: &mut smoltcp::iface::SocketSet<'static>) {
    let rx = raw::PacketBuffer::new(alloc::vec![raw::PacketMetadata::EMPTY; 8], alloc::vec![0u8; 4096]);
    let tx = raw::PacketBuffer::new(alloc::vec![raw::PacketMetadata::EMPTY; 2], alloc::vec![0u8; 256]);
    let h = set.add(raw::Socket::new(IpVersion::Ipv6, IpProtocol::Icmpv6, rx, tx));
    RAW.lock()[id as usize] = Some(h);
    SLAAC.lock()[id as usize] = Slaac::NONE;
}

fn csum6(src: &[u8; 16], dst: &[u8; 16], p: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut add = |b: &[u8]| { for c in b.chunks(2) { sum += ((c[0] as u32) << 8) | *c.get(1).unwrap_or(&0) as u32; } };
    add(src); add(dst);
    add(&(p.len() as u32).to_be_bytes());
    add(&[0, 0, 0, 58]);
    add(p);
    while sum >> 16 != 0 { sum = (sum & 0xffff) + (sum >> 16); }
    !(sum as u16)
}

fn rs_packet(src: [u8; 16], mac: [u8; 6]) -> Vec<u8> {
    let dst = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2];
    let mut icmp = alloc::vec![133u8, 0, 0, 0, 0, 0, 0, 0, 1, 1];
    icmp.extend_from_slice(&mac);
    let c = csum6(&src, &dst, &icmp);
    icmp[2..4].copy_from_slice(&c.to_be_bytes());
    let mut p = alloc::vec![0x60u8, 0, 0, 0];
    p.extend_from_slice(&(icmp.len() as u16).to_be_bytes());
    p.extend_from_slice(&[58, 255]);
    p.extend_from_slice(&src);
    p.extend_from_slice(&dst);
    p.extend_from_slice(&icmp);
    p
}

fn life(secs: u32, now: u64) -> u64 { if secs == u32::MAX { ONE_SHOT } else { now.saturating_add(secs as u64 * SEC) } }

/// Drop everything SLAAC installed on `id` (stack lock held by the caller).
fn withdraw(id: usize, s: &mut crate::NetStack) {
    let st = core::mem::replace(&mut SLAAC.lock()[id], Slaac::NONE);
    let sid = iface::sid_of(id as u8);
    if let Some((a, _)) = st.addr {
        s.interface.update_ip_addrs(|v| {
            let keep: Vec<IpCidr> = v.iter().copied().filter(|c| c.address() != IpAddress::Ipv6(Ipv6Address(a))).collect();
            v.clear();
            for c in keep { let _ = v.push(c); }
        });
    }
    if st.router.is_some() { s.interface.routes_mut().remove_default_ipv6_route(); }
    PREFIXES.lock().retain(|e| e.0 != sid);
    ROUTERS.lock().retain(|e| e.0 != sid);
    if st.addr.is_some() || st.router.is_some() { iface::notify(id as u8); }
}

/// Apply one router advertisement (`ip` is the whole IPv6 packet) to netif `id`.
fn handle_ra(id: usize, s: &mut crate::NetStack, ip: &[u8], now: u64, autoconf: bool) {
    if ip.len() < 40 + 16 || ip[0] >> 4 != 6 || ip[6] != 58 || ip[7] != 255 { return; }
    let mut src = [0u8; 16]; src.copy_from_slice(&ip[8..24]);
    let icmp = &ip[40..];
    if icmp[0] != 134 || icmp[1] != 0 || !is_link_local(&src) { return; }
    let sid = iface::sid_of(id as u8);
    let rlife = u16::from_be_bytes([icmp[6], icmp[7]]) as u32;
    let iid: [u8; 8] = match LL.lock()[id] { Some(l) => l[8..16].try_into().unwrap(), None => return };
    let mut changed = false;
    iface::serial(&[b"[NET] ra: ", alloc::format!("id={} router lifetime {} s\r\n", id, rlife).as_bytes()]);
    // Options.
    let mut o = &icmp[16..];
    while o.len() >= 8 {
        let l = o[1] as usize * 8;
        if l == 0 || l > o.len() { break; }
        if o[0] == 3 && l == 32 && o[2] == 64 {
            let (flags, valid, pref) = (o[3], u32::from_be_bytes(o[4..8].try_into().unwrap()), u32::from_be_bytes(o[8..12].try_into().unwrap()));
            let mut pfx = [0u8; 16]; pfx.copy_from_slice(&o[16..32]);
            if !is_link_local(&pfx) && pref <= valid {
                if flags & 0x80 != 0 {
                    let mut t = SLAAC.lock();
                    changed |= t[id].pfx.map_or(true, |p| p.0 != pfx);
                    t[id].pfx = if valid == 0 { None } else { Some((pfx, 64, life(valid, now))) };
                    drop(t);
                    PREFIXES.lock().retain(|e| !(e.0 == sid && e.1 == pfx));
                    if valid != 0 { PREFIXES.lock().push((sid, pfx, 64, 256)); }
                }
                if flags & 0x40 != 0 && autoconf {
                    let mut a = pfx; a[8..].copy_from_slice(&iid);
                    let mut t = SLAAC.lock();
                    if valid == 0 {
                        if t[id].addr.map_or(false, |x| x.0 == a) { drop(t); withdraw_addr(id, s); changed = true; }
                    } else if t[id].addr.map_or(true, |x| x.0 == a) {
                        if t[id].addr.is_none() {
                            s.interface.update_ip_addrs(|v| { let _ = v.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address(a)), 64)); });
                            changed = true;
                        }
                        t[id].addr = Some((a, life(valid, now)));
                    }
                }
            }
        }
        o = &o[l..];
    }
    // Router.
    let mut t = SLAAC.lock();
    if rlife == 0 {
        if t[id].router.take().is_some() { drop(t); s.interface.routes_mut().remove_default_ipv6_route(); ROUTERS.lock().retain(|e| e.0 != sid); changed = true; }
    } else {
        let new = t[id].router.map_or(true, |r| r.0 != src);
        t[id].router = Some((src, now + rlife as u64 * SEC));
        drop(t);
        if new {
            let _ = s.interface.routes_mut().add_default_ipv6_route(Ipv6Address(src));
            ROUTERS.lock().retain(|e| e.0 != sid);
            ROUTERS.lock().push((sid, src, 1024));
            changed = true;
        }
    }
    if changed { iface::notify(id as u8); }
}

fn withdraw_addr(id: usize, s: &mut crate::NetStack) {
    let a = SLAAC.lock()[id].addr.take();
    if let Some((a, _)) = a {
        s.interface.update_ip_addrs(|v| {
            let keep: Vec<IpCidr> = v.iter().copied().filter(|c| c.address() != IpAddress::Ipv6(Ipv6Address(a))).collect();
            v.clear();
            for c in keep { let _ = v.push(c); }
        });
    }
}

/// One daemon pass of router discovery for a NIC stack (its lock held by the
/// caller): receive RAs, expire what ran out, send RS. Returns the ns until
/// the next timed event, if any.
pub(crate) fn slaac_pass(nif: netdev::Netif, s: &mut crate::NetStack, admin_down: bool, now: u64) -> Option<u64> {
    let id = nif.id as usize;
    let h = RAW.lock()[id]?;
    let on = !admin_down && LL.lock()[id].is_some() && iface::rt(nif.id).carrier
        && sched::ns::ipv6_conf(0, nif.name().as_bytes(), "accept_ra") != 0;
    if !on {
        let sock = s.socket_set.get_mut::<raw::Socket>(h);
        let mut b = [0u8; 1500];
        while sock.can_recv() { let _ = sock.recv_slice(&mut b); }
        withdraw(id, s);
        return None;
    }
    let autoconf = sched::ns::ipv6_conf(0, nif.name().as_bytes(), "autoconf") != 0;
    let mut pkt = [0u8; 1500];
    loop {
        let sock = s.socket_set.get_mut::<raw::Socket>(h);
        if !sock.can_recv() { break; }
        let Ok(n) = sock.recv_slice(&mut pkt) else { break };
        handle_ra(id, s, &pkt[..n], now, autoconf);
    }
    // Expiry.
    let st = SLAAC.lock()[id];
    if st.addr.map_or(false, |a| a.1 <= now) { withdraw_addr(id, s); iface::notify(nif.id); }
    if st.router.map_or(false, |r| r.1 <= now) {
        SLAAC.lock()[id].router = None;
        s.interface.routes_mut().remove_default_ipv6_route();
        ROUTERS.lock().retain(|e| e.0 != iface::sid_of(nif.id));
        iface::notify(nif.id);
    }
    if st.pfx.map_or(false, |p| p.2 <= now) {
        SLAAC.lock()[id].pfx = None;
        PREFIXES.lock().retain(|e| e.0 != iface::sid_of(nif.id));
    }
    let st = SLAAC.lock()[id];
    if st.router.is_none() && st.addr.is_none() {
        if st.rs_sent < RS_MAX && now >= st.next_rs {
            if let (Some(ll), Some(mac)) = (LL.lock()[id], nif.mac()) {
                let p = rs_packet(ll, mac);
                let sock = s.socket_set.get_mut::<raw::Socket>(h);
                if sock.send_slice(&p).is_ok() {
                    let mut t = SLAAC.lock();
                    t[id].rs_sent += 1;
                    t[id].next_rs = now + RS_GAP;
                }
            }
        }
        let t = SLAAC.lock();
        if t[id].rs_sent < RS_MAX { return Some(t[id].next_rs.saturating_sub(now).max(1_000_000)); }
        return None;
    }
    let mut next = ONE_SHOT;
    for e in [st.addr.map(|a| a.1), st.router.map(|r| r.1), st.pfx.map(|p| p.2)].into_iter().flatten() { next = next.min(e); }
    if next == ONE_SHOT { None } else { Some(next.saturating_sub(now).max(1_000_000)) }
}

/// `/proc/net/ipv6_route`: `dest plen src slen nexthop metric refcnt use flags dev`.
pub(crate) fn proc_ipv6_route(out: &mut impl Write) {
    let hex = |o: &mut dyn Write, a: &[u8; 16]| { for b in a { let _ = write!(o, "{:02x}", b); } };
    let z = [0u8; 16];
    let nm = |sid: Sid| netdev::iter().find(|n| iface::sid_of(n.id) == sid).map(|n| alloc::string::String::from(n.name()));
    for nif in netdev::iter() {
        if ll_of(nif.id).is_some() {
            let mut a = [0u8; 16]; a[0] = 0xfe; a[1] = 0x80;
            hex(out, &a); let _ = write!(out, " 40 "); hex(out, &z); let _ = write!(out, " 00 "); hex(out, &z);
            let _ = writeln!(out, " 00000100 00000000 00000000 00000001 {:>8}", nif.name());
        }
    }
    for &(sid, p, len, m) in PREFIXES.lock().iter() {
        hex(out, &p); let _ = write!(out, " {:02x} ", len); hex(out, &z); let _ = write!(out, " 00 "); hex(out, &z);
        let _ = writeln!(out, " {:08x} 00000000 00000000 00000001 {:>8}", m, nm(sid).unwrap_or_default());
    }
    for &(sid, r, m) in ROUTERS.lock().iter() {
        hex(out, &z); let _ = write!(out, " 00 "); hex(out, &z); let _ = write!(out, " 00 "); hex(out, &r);
        let _ = writeln!(out, " {:08x} 00000000 00000000 00000003 {:>8}", m, nm(sid).unwrap_or_default());
    }
}
