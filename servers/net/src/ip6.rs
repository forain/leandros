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
    }
}
