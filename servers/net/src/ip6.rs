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
/// Addresses added over rtnetlink: `(ifindex, address, prefix length)`.
static STATIC_A: Mutex<Vec<(u32, [u8; 16], u8)>> = Mutex::new(Vec::new());
/// Routes added over rtnetlink.
static STATIC_R: Mutex<Vec<crate::rtnetlink::R6>> = Mutex::new(Vec::new());

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
    if ns == 0 && sid >= 1 && sid < FIRST_DYN as u32 && crate::stack_for(sid as Sid).is_some() { Ok(sid as Sid) }
    else if let Some(s) = crate::tunbe::sid_by_ifindex(ns, scope) { Ok(s) } else { Err(-19) }
}

/// The link-local address of netif `id`, while installed.
pub(crate) fn ll_of(id: u8) -> Option<[u8; 16]> { ll_cfg(id).filter(|a| dad_ok(id, a)) }
/// The link-local address configured on netif `id`, tentative or not.
pub(crate) fn ll_cfg(id: u8) -> Option<[u8; 16]> { LL.lock().get(id as usize).copied().flatten() }

/// The NIC stack holding the v6 address `a` (link-local only until SLAAC / static addresses).
pub(crate) fn holder_of(a: Ipv6Address) -> Option<Sid> {
    let t = LL.lock();
    (0..MAX_NETIF).find(|&i| t[i] == Some(a.0) && dad_ok(i as u8, &a.0)).map(|i| iface::sid_of(i as u8))
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
    best(&dst.0).filter(|b| b.2 >= 2).map(|b| (b.2 - 1) as Sid)
}

/// Longest-prefix match over the on-link prefixes, the static routes and the
/// default routers of the NICs with carrier: `(prefix length, metric, oif, gateway)`.
fn best(dst: &[u8; 16]) -> Option<(u8, u32, u32, Option<[u8; 16]>)> {
    let up = |sid: Sid| sid >= 1 && sid < FIRST_DYN && iface::rt(sid - 1).carrier;
    let mut best: Option<(u8, u32, u32, Option<[u8; 16]>)> = None;
    let mut take = |len: u8, m: u32, oif: u32, gw: Option<[u8; 16]>| {
        if best.map_or(true, |(bl, bm, _, _)| len > bl || (len == bl && m < bm)) { best = Some((len, m, oif, gw)); }
    };
    for &(sid, p, len, m) in PREFIXES.lock().iter() {
        if up(sid) && prefix_match(dst, &p, len) { take(len, m, sid as u32 + 1, None); }
    }
    for r in STATIC_R.lock().iter() {
        if r.oif >= 2 && up((r.oif - 1) as Sid) && prefix_match(dst, &r.dst, r.plen) { take(r.plen, r.metric, r.oif, r.gw); }
    }
    for &(sid, r, m) in ROUTERS.lock().iter() {
        if up(sid) { take(0, m, sid as u32 + 1, Some(r)); }
    }
    best
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
    let cfg = LL.lock()[id];
    if cfg == want && have == want.map_or(false, |l| dad_ok(nif.id, &l)) { return; }
    if let Some(old) = cfg { if Some(old) != want { dad_forget(nif.id, &old); } }
    let now = want.map_or(true, |l| dad_register(nif.id, nif.name().as_bytes(), l, 64, false));
    s.interface.update_ip_addrs(|addrs| {
        let keep: Vec<IpCidr> = addrs.iter().copied()
            .filter(|c| !matches!(c.address(), IpAddress::Ipv6(a) if is_link_local(&a.0))).collect();
        addrs.clear();
        for c in keep { let _ = addrs.push(c); }
        if let (Some(l), true) = (want, now) { let _ = addrs.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address(l)), 64)); }
    });
    LL.lock()[id] = want;
    drop(stack);
    iface::notify(nif.id);
}

/// `/proc/net/if_inet6`: `addr ifindex prefixlen scope flags name`, hex.
pub(crate) fn proc_if_inet6(out: &mut impl Write) {
    // A task in another netns sees that netns's own links (its lo and vifs), not the NICs.
    let ns = sched::ns::netns_of(sched::current_pid() as u32);
    if ns != 0 {
        if !lo_addrs6(ns).is_empty() { let _ = writeln!(out, "00000000000000000000000000000001 01 80 10 80       lo"); }
        crate::tunbe::proc_if_inet6(out, ns);
        return;
    }
    let _ = writeln!(out, "00000000000000000000000000000001 01 80 10 80       lo");
    for nif in netdev::iter() {
        let Some(a) = ll_cfg(nif.id) else { continue };
        for b in a { let _ = write!(out, "{:02x}", b); }
        let _ = writeln!(out, " {:02x} 40 20 {:02x} {:>8}", nif.id as u32 + 2, 0x80 | dad_flags(nif.id, &a), nif.name());
        let sl = SLAAC.lock()[nif.id as usize].addrs;
        for (g, _) in sl.iter().flatten() {
            for b in g { let _ = write!(out, "{:02x}", b); }
            let _ = writeln!(out, " {:02x} 40 00 {:02x} {:>8}", nif.id as u32 + 2, 0x80 | dad_flags(nif.id, g), nif.name());
        }
        if let Some(g) = crate::dhcp6::addr_of(nif.id) {
            for b in g { let _ = write!(out, "{:02x}", b); }
            let _ = writeln!(out, " {:02x} 80 00 80 {:>8}", nif.id as u32 + 2, nif.name());
        }
    }
    for &(idx, a, p) in STATIC_A.lock().iter() {
        let name = if idx == 1 { alloc::string::String::from("lo") } else { nic_of(idx).map(|n| alloc::string::String::from(n.name())).unwrap_or_default() };
        let fl = 0x80 | nic_of(idx).map_or(0, |n| dad_flags(n.id, &a));
        for b in a { let _ = write!(out, "{:02x}", b); }
        let _ = writeln!(out, " {:02x} {:02x} {:02x} {:02x} {:>8}", idx, p, if is_link_local(&a) { 0x20 } else { 0 }, fl, name);
    }
    crate::tunbe::proc_if_inet6(out, 0);
}

// ── Duplicate address detection (RFC 4862) of the NIC addresses ──────────────
// The link-local, SLAAC and static addresses of a NIC start tentative: held out of
// the smoltcp interface while `dad_transmits` NS probes (from `::` to the
// solicited-node group, 1 s apart) go out and a further second passes. A NA for
// the target, or another node's NS from `::` for it, makes it DADFAILED (it stays
// listed, never used). `accept_dad` 0, `dad_transmits` 0 and IFA_F_NODAD skip it.

const DAD_MS: u64 = 1_000_000_000;
const IFA_F_TENTATIVE: u32 = 0x40;
const IFA_F_DADFAILED: u32 = 0x800;
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum DadSt { New, Probing(u8, u64), Failed }
struct NDad { id: u8, addr: [u8; 16], prefix: u8, st: DadSt }
/// Addresses not yet usable. An address with no entry is permanent.
static NDAD: Mutex<Vec<NDad>> = Mutex::new(Vec::new());

/// May `addr` of netif `id` be used (no pending or failed DAD)?
fn dad_ok(id: u8, addr: &[u8; 16]) -> bool { !NDAD.lock().iter().any(|d| d.id == id && d.addr == *addr) }
/// IFA_F_* of `addr` on netif `id`.
fn dad_flags(id: u8, addr: &[u8; 16]) -> u32 {
    match NDAD.lock().iter().find(|d| d.id == id && d.addr == *addr).map(|d| d.st) {
        None => 0,
        Some(DadSt::Failed) => IFA_F_TENTATIVE | IFA_F_DADFAILED,
        Some(_) => IFA_F_TENTATIVE,
    }
}
fn dad_forget(id: u8, addr: &[u8; 16]) { NDAD.lock().retain(|d| !(d.id == id && d.addr == *addr)); }
/// A new address: true when it can be installed at once, false when it starts tentative.
fn dad_register(id: u8, ifname: &[u8], addr: [u8; 16], prefix: u8, nodad: bool) -> bool {
    if nodad || sched::ns::ipv6_conf(0, ifname, "accept_dad") == 0 || sched::ns::ipv6_conf(0, ifname, "dad_transmits") == 0 {
        dad_forget(id, &addr);
        return true;
    }
    let mut t = NDAD.lock();
    if !t.iter().any(|d| d.id == id && d.addr == addr) { t.push(NDad { id, addr, prefix, st: DadSt::New }); }
    false
}

/// The NS of a DAD probe for `addr`, a whole IPv6 packet.
fn dad_ns_packet(addr: &[u8; 16]) -> Vec<u8> {
    let dst = [0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0xff, addr[13], addr[14], addr[15]];
    let mut icmp = alloc::vec![135u8, 0, 0, 0, 0, 0, 0, 0];
    icmp.extend_from_slice(addr);
    let src = [0u8; 16];
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

/// Does `ip` (a whole IPv6 packet) claim a target for DAD purposes: a NA, or a NS from `::`?
fn dad_claim(ip: &[u8]) -> Option<[u8; 16]> {
    if ip.len() < 40 + 24 || ip[0] >> 4 != 6 || ip[6] != 58 { return None; }
    let unspec = ip[8..24].iter().all(|&b| b == 0);
    if !(ip[40] == 136 || (ip[40] == 135 && unspec)) { return None; }
    let mut t = [0u8; 16]; t.copy_from_slice(&ip[48..64]);
    Some(t)
}

/// What a received packet does to the pending addresses of `id`; true when one failed.
fn dad_rx(id: u8, ip: &[u8]) -> bool {
    let Some(t) = dad_claim(ip) else { return false };
    let mut hit = false;
    for d in NDAD.lock().iter_mut().filter(|d| d.id == id && d.addr == t && d.st != DadSt::Failed) { d.st = DadSt::Failed; hit = true; }
    hit
}

/// A received IPv6 packet (`ip` = past the Ethernet header) that is another node's DAD probe (a NS
/// from `::`) for a pending address of NIC `id`: that address failed.
pub(crate) fn dad_snoop(id: u8, ip: &[u8]) {
    if ip.len() >= 64 && ip[0] >> 4 == 6 && ip[6] == 58 && ip[40] == 135 && ip[8..24].iter().all(|&b| b == 0) { dad_rx(id, ip); }
}

/// One DAD step for NIC `id` (its stack lock held): probes due, addresses that passed.
/// `raw` is the stack's internal ICMPv6 socket. Returns ns until the next timed event.
fn dad_pass(nif: netdev::Netif, s: &mut crate::NetStack, raw: SocketHandle, now: u64) -> Option<u64> {
    let id = nif.id;
    if !iface::rt(id).carrier { return None; }
    let probes = sched::ns::ipv6_conf(0, nif.name().as_bytes(), "dad_transmits").clamp(1, 64) as u8;
    let mut next: Option<u64> = None;
    let mut done: Vec<([u8; 16], u8)> = Vec::new();
    {
        let mut t = NDAD.lock();
        for d in t.iter_mut().filter(|d| d.id == id) {
            let send = match d.st {
                DadSt::New => Some(0u8),
                DadSt::Probing(n, due) if now >= due && n < probes => Some(n),
                DadSt::Probing(_, due) if now >= due => { done.push((d.addr, d.prefix)); None }
                _ => None,
            };
            if let Some(n) = send {
                let sock = s.socket_set.get_mut::<raw::Socket>(raw);
                if sock.send_slice(&dad_ns_packet(&d.addr)).is_ok() { d.st = DadSt::Probing(n + 1, now + DAD_MS); }
                else { next = Some(1_000_000); }
            }
            if let DadSt::Probing(_, due) = d.st { if due > now { let w = due - now; next = Some(next.map_or(w, |x| x.min(w))); } }
        }
        t.retain(|d| !(d.id == id && done.iter().any(|x| x.0 == d.addr)));
    }
    if !done.is_empty() {
        s.interface.update_ip_addrs(|v| for &(a, p) in &done { let _ = v.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address(a)), p)); });
        iface::notify(id);
    }
    next
}

// ── Router discovery and SLAAC (design-ipv6 L5) ──────────────────────────────

const SEC: u64 = 1_000_000_000;
/// Router solicitations per link-up / loss of the router (Linux router_solicitations).
const RS_MAX: u8 = 3;
/// Interval between them (Linux router_solicitation_interval).
const RS_GAP: u64 = 4 * SEC;
const ONE_SHOT: u64 = u64::MAX;
/// SLAAC addresses kept per NIC.
const SLAAC_MAX: usize = 4;

/// What router advertisements told one netif.
#[derive(Clone, Copy)]
struct Slaac {
    /// SLAAC addresses (one per advertised prefix, like Linux) and when their valid lifetimes end.
    addrs: [Option<([u8; 16], u64)>; SLAAC_MAX],
    /// On-link prefix learned (with L flag): prefix, length, expiry.
    pfx: Option<([u8; 16], u8, u64)>,
    /// Default router (its link-local address) and when its lifetime ends.
    router: Option<([u8; 16], u64)>,
    rs_sent: u8,
    next_rs: u64,
}
impl Slaac { const NONE: Slaac = Slaac { addrs: [None; SLAAC_MAX], pfx: None, router: None, rs_sent: 0, next_rs: 0 }; }
/// Netifs that received a router advertisement (IF_RA_RCVD).
/// The MTU a router advertised (RA option 5) per netif, 0 = none. Linux ndisc_router_discovery:
/// ignored below IPV6_MIN_MTU (1280) or above the device MTU (1500 here), else it becomes the
/// interface's IPv6 MTU (cnf.mtu6).
static RA_MTU: Mutex<[u32; MAX_NETIF]> = Mutex::new([0; MAX_NETIF]);
const DEV_MTU: u32 = 1500;
fn ra_mtu_ok(v: u32) -> Option<u32> { if (1280..=DEV_MTU).contains(&v) { Some(v) } else { None } }
/// The IPv6 MTU an RA set on the NIC stack `sid`, if any.
pub(crate) fn ra_mtu(sid: crate::Sid) -> Option<u32> {
    let id = (sid as usize).checked_sub(1)?;
    if id >= MAX_NETIF { return None; }
    match RA_MTU.lock()[id] { 0 => None, m => Some(m) }
}
/// RDNSS (RFC 8106 option 25) servers and DNSSL (option 31) search domains learned from RAs:
/// (netif, address, expiry) / (netif, domain, expiry). Linux's kernel only hands the options to
/// userspace (NetworkManager / systemd-networkd write resolv.conf); here the net server stands
/// in for that manager and feeds `/etc/resolv.conf` (vfs::set_ra_dns), v4 DHCP servers first.
static RDNSS: Mutex<Vec<(usize, [u8; 16], u64)>> = Mutex::new(Vec::new());
static DNSSL: Mutex<Vec<(usize, Vec<u8>, u64)>> = Mutex::new(Vec::new());

/// Parse an RDNSS option body (`o` = the whole option, length already checked to be a multiple
/// of 8): (lifetime, addresses). None when malformed (RFC 8106: length >= 3 and odd).
fn parse_rdnss(o: &[u8]) -> Option<(u32, Vec<[u8; 16]>)> {
    if o.len() < 24 || (o.len() / 8) % 2 == 0 { return None; }
    let life = u32::from_be_bytes(o[4..8].try_into().unwrap());
    Some((life, o[8..].chunks_exact(16).map(|c| c.try_into().unwrap()).collect()))
}

/// Parse a DNSSL option: (lifetime, dotted domains). Compression is not allowed in DNSSL, a
/// malformed name ends the list.
fn parse_dnssl(o: &[u8]) -> Option<(u32, Vec<Vec<u8>>)> {
    if o.len() < 16 { return None; }
    let life = u32::from_be_bytes(o[4..8].try_into().unwrap());
    let mut out = Vec::new();
    let mut p = 8;
    while p < o.len() && o[p] != 0 {
        let mut name = Vec::new();
        loop {
            let l = *o.get(p)? as usize;
            if l == 0 { p += 1; break; }
            if l > 63 || p + 1 + l > o.len() { return Some((life, out)); }
            if !name.is_empty() { name.push(b'.'); }
            name.extend_from_slice(&o[p + 1..p + 1 + l]);
            p += 1 + l;
        }
        if !name.is_empty() { out.push(name); }
    }
    Some((life, out))
}

/// Install/refresh (lifetime 0 deletes) RDNSS addresses of `id`; true if the set changed.
fn rdnss_update(id: usize, life: u32, addrs: &[[u8; 16]], now: u64) -> bool {
    let mut t = RDNSS.lock();
    let mut ch = false;
    for a in addrs {
        if a[0] == 0xff || *a == [0; 16] { continue; } // link-local ones are listed `addr%ifname`
        let pos = t.iter().position(|e| e.0 == id && e.1 == *a);
        match (pos, life) {
            (Some(i), 0) => { t.remove(i); ch = true; }
            (Some(i), _) => t[i].2 = life_of(life, now),
            (None, 0) => {}
            (None, _) => { t.push((id, *a, life_of(life, now))); ch = true; }
        }
    }
    ch
}
fn dnssl_update(id: usize, life: u32, doms: &[Vec<u8>], now: u64) -> bool {
    let mut t = DNSSL.lock();
    let mut ch = false;
    for d in doms {
        let pos = t.iter().position(|e| e.0 == id && e.1 == *d);
        match (pos, life) {
            (Some(i), 0) => { t.remove(i); ch = true; }
            (Some(i), _) => t[i].2 = life_of(life, now),
            (None, 0) => {}
            (None, _) => { t.push((id, d.clone(), life_of(life, now))); ch = true; }
        }
    }
    ch
}
fn life_of(secs: u32, now: u64) -> u64 { if secs == u32::MAX { ONE_SHOT } else { now.saturating_add(secs as u64 * SEC) } }

/// Drop expired (or, with `all`, every) RDNSS/DNSSL entry of `id`; true if anything went.
fn dns_expire(id: usize, now: u64, all: bool) -> bool {
    let mut a = RDNSS.lock();
    let n = a.len();
    a.retain(|e| e.0 != id || (!all && e.2 > now));
    let mut d = DNSSL.lock();
    let m = d.len();
    d.retain(|e| e.0 != id || (!all && e.2 > now));
    a.len() != n || d.len() != m
}
fn dns_next(id: usize) -> Option<u64> {
    RDNSS.lock().iter().filter(|e| e.0 == id).map(|e| e.2)
        .chain(DNSSL.lock().iter().filter(|e| e.0 == id).map(|e| e.2)).filter(|&e| e != ONE_SHOT).min()
}
/// Hand the merged v6 resolver state (RDNSS, then DHCPv6) to /etc/resolv.conf. A link-local
/// server carries its interface as the scope (`fe80::1%eth0`, as NetworkManager writes it).
pub(crate) fn dns_publish() {
    let mut servers: Vec<([u8; 16], alloc::string::String)> = Vec::new();
    let mut add = |id: usize, a: [u8; 16]| {
        let scope = if is_link_local(&a) { alloc::string::String::from(nic_name(id)) } else { alloc::string::String::new() };
        if !servers.iter().any(|e| e.0 == a && e.1 == scope) { servers.push((a, scope)); }
    };
    for e in RDNSS.lock().iter() { add(e.0, e.1); }
    for (id, a) in crate::dhcp6::servers() { add(id, a); }
    let sv: Vec<([u8; 16], &[u8])> = servers.iter().map(|e| (e.0, e.1.as_bytes())).collect();
    let mut search: Vec<u8> = Vec::new();
    let mut doms: Vec<Vec<u8>> = DNSSL.lock().iter().map(|e| e.1.clone()).collect();
    doms.extend(crate::dhcp6::domains());
    for d in doms.iter() {
        if search.split(|&b| b == b' ').any(|w| w == &d[..]) { continue; }
        if search.len() + d.len() + 1 > 200 { break; }
        if !search.is_empty() { search.push(b' '); }
        search.extend_from_slice(d);
    }
    vfs::set_ra_dns(&sv, &search);
}

static RA_SEEN: Mutex<[bool; MAX_NETIF]> = Mutex::new([false; MAX_NETIF]);
/// Whether a router solicitation was ever sent on netif `id` (IF_RS_SENT).
pub(crate) fn rs_was_sent(id: usize) -> bool { id < MAX_NETIF && (SLAAC.lock()[id].rs_sent > 0 || RA_SEEN.lock()[id]) }
pub(crate) fn ra_rcvd(id: usize) -> bool { id < MAX_NETIF && RA_SEEN.lock()[id] }
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
    crate::dhcp6::attach(id, set);
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
    RA_MTU.lock()[id] = 0;
    crate::dhcp6::reset(id, s);
    if dns_expire(id, 0, true) { dns_publish(); }
    let sid = iface::sid_of(id as u8);
    for (a, _) in st.addrs.iter().flatten().copied() {
        dad_forget(id as u8, &a);
        s.interface.update_ip_addrs(|v| {
            let keep: Vec<IpCidr> = v.iter().copied().filter(|c| c.address() != IpAddress::Ipv6(Ipv6Address(a))).collect();
            v.clear();
            for c in keep { let _ = v.push(c); }
        });
    }
    if st.router.is_some() { s.interface.routes_mut().remove_default_ipv6_route(); }
    PREFIXES.lock().retain(|e| e.0 != sid);
    ROUTERS.lock().retain(|e| e.0 != sid);
    if st.addrs.iter().any(|a| a.is_some()) || st.router.is_some() { iface::notify(id as u8); }
}

/// Apply one router advertisement (`ip` is the whole IPv6 packet) to netif `id`.
fn handle_ra(id: usize, s: &mut crate::NetStack, ip: &[u8], now: u64, autoconf: bool) {
    if ip.len() < 40 + 16 || ip[0] >> 4 != 6 || ip[6] != 58 || ip[7] != 255 { return; }
    let mut src = [0u8; 16]; src.copy_from_slice(&ip[8..24]);
    let icmp = &ip[40..];
    if icmp[0] != 134 || icmp[1] != 0 || !is_link_local(&src) { return; }
    RA_SEEN.lock()[id] = true;
    crate::dhcp6::ra_flags(id, icmp[5], now);
    let sid = iface::sid_of(id as u8);
    let rlife = u16::from_be_bytes([icmp[6], icmp[7]]) as u32;
    let iid: [u8; 8] = match LL.lock()[id] { Some(l) => l[8..16].try_into().unwrap(), None => return };
    let mut changed = false;
    let mut dns_changed = false;
    iface::serial(&[b"[NET] ra: ", alloc::format!("id={} router lifetime {} s\r\n", id, rlife).as_bytes()]);
    // Options.
    let mut o = &icmp[16..];
    while o.len() >= 8 {
        let l = o[1] as usize * 8;
        if l == 0 || l > o.len() { break; }
        if o[0] == 5 && l == 8 {
            if let Some(m) = ra_mtu_ok(u32::from_be_bytes(o[4..8].try_into().unwrap())) {
                let mut r = RA_MTU.lock();
                if r[id] != m { r[id] = m; changed = true; }
            }
        }
        if o[0] == 25 {
            if let Some((life, a)) = parse_rdnss(&o[..l]) { dns_changed |= rdnss_update(id, life, &a, now); }
        }
        if o[0] == 31 {
            if let Some((life, d)) = parse_dnssl(&o[..l]) { dns_changed |= dnssl_update(id, life, &d, now); }
        }
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
                    let have = t[id].addrs.iter().position(|x| x.map_or(false, |x| x.0 == a));
                    if valid == 0 {
                        if have.is_some() { drop(t); withdraw_addr(id, s, &a); changed = true; }
                    } else if let Some(i) = have {
                        t[id].addrs[i] = Some((a, life(valid, now)));
                    } else if let Some(i) = t[id].addrs.iter().position(|x| x.is_none()) {
                        if dad_register(id as u8, nic_name(id).as_bytes(), a, 64, false) {
                            s.interface.update_ip_addrs(|v| { let _ = v.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address(a)), 64)); });
                        }
                        changed = true;
                        t[id].addrs[i] = Some((a, life(valid, now)));
                    }
                }
            }
        }
        o = &o[l..];
    }
    if dns_changed { dns_publish(); }
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

fn withdraw_addr(id: usize, s: &mut crate::NetStack, a: &[u8; 16]) {
    let a = *a;
    let mut t = SLAAC.lock();
    let had = t[id].addrs.iter_mut().find(|x| x.map_or(false, |x| x.0 == a)).map(|x| x.take()).is_some();
    drop(t);
    if had {
        dad_forget(id as u8, &a);
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
    // Everything the internal socket received: DAD conflicts first, then the RAs.
    let mut pkts: Vec<Vec<u8>> = Vec::new();
    {
        let sock = s.socket_set.get_mut::<raw::Socket>(h);
        let mut b = [0u8; 1500];
        while sock.can_recv() { match sock.recv_slice(&mut b) { Ok(n) => pkts.push(b[..n].to_vec()), Err(_) => break } }
    }
    for p in &pkts { dad_rx(nif.id, p); }
    let dad = dad_pass(nif, s, h, now);
    let rs = slaac_inner(nif, s, admin_down, now, h, &pkts);
    let d6 = crate::dhcp6::pass(nif, s, now);
    [dad, rs, d6].into_iter().flatten().min()
}

fn slaac_inner(nif: netdev::Netif, s: &mut crate::NetStack, admin_down: bool, now: u64, h: SocketHandle, pkts: &[Vec<u8>]) -> Option<u64> {
    let id = nif.id as usize;
    let on = !admin_down && ll_of(nif.id).is_some() && iface::rt(nif.id).carrier
        && sched::ns::ipv6_conf(0, nif.name().as_bytes(), "accept_ra") != 0;
    if !on {
        withdraw(id, s);
        return None;
    }
    let autoconf = sched::ns::ipv6_conf(0, nif.name().as_bytes(), "autoconf") != 0;
    for p in pkts { handle_ra(id, s, p, now, autoconf); }
    if dns_expire(id, now, false) { dns_publish(); }
    let st = SLAAC.lock()[id];
    for (a, _) in st.addrs.iter().flatten().filter(|a| a.1 <= now) { withdraw_addr(id, s, a); iface::notify(nif.id); }
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
    if st.router.is_none() && st.addrs.iter().all(|a| a.is_none()) {
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
    for e in st.addrs.iter().flatten().map(|a| a.1).chain([st.router.map(|r| r.1), st.pfx.map(|p| p.2)].into_iter().flatten()) { next = next.min(e); }
    if let Some(e) = dns_next(id) { next = next.min(e); }
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
    for r in STATIC_R.lock().iter() {
        hex(out, &r.dst); let _ = write!(out, " {:02x} ", r.plen); hex(out, &z); let _ = write!(out, " 00 ");
        hex(out, &r.gw.unwrap_or(z));
        let _ = writeln!(out, " {:08x} 00000000 00000000 {:08x} {:>8}", r.metric, if r.gw.is_some() { 3 } else { 1 }, nm(r.oif.wrapping_sub(1) as Sid).unwrap_or_default());
    }
}

// ── rtnetlink view and writes (design-ipv6 L6) ───────────────────────────────

use crate::rtnetlink::{A6, R6, RtOp};

fn scope_of(a: &[u8; 16]) -> u8 { if is_link_local(a) { 253 } else { 0 } }

/// The interface index a link-local address `a` belongs to or was heard on: the NIC that owns
/// it, else the vif that does, else the first NIC with a link-local address (0 when none).
pub(crate) fn ll_scope_of(a: &[u8; 16]) -> u32 {
    let mut first = 0;
    for nif in netdev::iter() {
        let Some(l) = ll_cfg(nif.id) else { continue };
        if &l == a { return nif.id as u32 + 2; }
        if first == 0 { first = nif.id as u32 + 2; }
    }
    first
}

/// Is `a` a static address on `lo`? Such an address is local: it is delivered through the loopback.
pub(crate) fn lo_static(a: &Ipv6Address) -> bool {
    STATIC_A.lock().iter().any(|e| e.0 == 1 && e.1 == a.0)
}

/// `::1` of a namespace's loopback while it is up.
pub(crate) fn lo_addrs6(ns: u16) -> Vec<A6> {
    if sched::ns::lo_admin_up(ns) && !sched::ns::ipv6_disabled(ns, b"lo") {
        alloc::vec![A6 { index: 1, addr: LOOPBACK, prefix: 128, scope: 254, flags: 0x80 }]
    } else { Vec::new() }
}

const LOOPBACK: [u8; 16] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];

/// Every IPv6 address of the initial namespace.
pub(crate) fn addrs6() -> Vec<A6> {
    let mut v = alloc::vec![A6 { index: 1, addr: LOOPBACK, prefix: 128, scope: 254, flags: 0x80 }];
    for nif in netdev::iter() {
        let idx = nif.id as u32 + 2;
        if let Some(a) = ll_cfg(nif.id) { v.push(A6 { index: idx, addr: a, prefix: 64, scope: 253, flags: 0x80 | dad_flags(nif.id, &a) }); }
        let sl = SLAAC.lock()[nif.id as usize].addrs;
        for (g, _) in sl.iter().flatten().copied() { v.push(A6 { index: idx, addr: g, prefix: 64, scope: 0, flags: 0x80 | dad_flags(nif.id, &g) }); }
        if let Some(g) = crate::dhcp6::addr_of(nif.id) { v.push(A6 { index: idx, addr: g, prefix: 128, scope: 0, flags: 0x80 }); }
    }
    for &(idx, a, p) in STATIC_A.lock().iter() { v.push(A6 { index: idx, addr: a, prefix: p, scope: scope_of(&a), flags: 0x80 | nic_of(idx).map_or(0, |n| dad_flags(n.id, &a)) }); }
    v
}

/// Every IPv6 route of the initial namespace (the `/proc/net/ipv6_route` table).
pub(crate) fn routes6() -> Vec<R6> {
    let mut v = Vec::new();
    for nif in netdev::iter() {
        if ll_of(nif.id).is_some() {
            let mut a = [0u8; 16]; a[0] = 0xfe; a[1] = 0x80;
            v.push(R6 { dst: a, plen: 64, gw: None, oif: nif.id as u32 + 2, metric: 256, proto: 2 });
        }
    }
    for &(sid, p, len, m) in PREFIXES.lock().iter() { v.push(R6 { dst: p, plen: len, gw: None, oif: sid as u32 + 1, metric: m, proto: 2 }); }
    for &(sid, r, m) in ROUTERS.lock().iter() { v.push(R6 { dst: [0; 16], plen: 0, gw: Some(r), oif: sid as u32 + 1, metric: m, proto: 9 }); }
    for r in STATIC_R.lock().iter() { v.push(r.clone()); }
    v
}

/// RTM_GETROUTE for one destination: `(oif, gateway)`.
pub(crate) fn lookup6(dst: [u8; 16]) -> Option<(u32, Option<[u8; 16]>)> {
    if dst == LOOPBACK || lo_static(&Ipv6Address(dst)) { return Some((1, None)); }
    if is_link_local(&dst) {
        return netdev::iter().find(|n| ll_of(n.id).is_some()).map(|n| (n.id as u32 + 2, None));
    }
    best(&dst).map(|b| (b.2, b.3))
}

fn nic_name(id: usize) -> alloc::string::String { netdev::get(id).map(|n| alloc::string::String::from(n.name())).unwrap_or_default() }

fn nic_of(index: u32) -> Option<netdev::Netif> { netdev::iter().find(|n| n.id as u32 + 2 == index) }

fn mask16(a: &[u8; 16], len: u8) -> [u8; 16] {
    let mut o = *a;
    for (i, b) in o.iter_mut().enumerate() {
        let keep = (len as u32).saturating_sub(i as u32 * 8).min(8);
        *b &= if keep == 0 { 0 } else { 0xffu8 << (8 - keep) };
    }
    o
}

/// Apply an IPv6 rtnetlink write; 0 or a positive errno.
pub(crate) fn apply(ns: u16, op: RtOp) -> i32 {
    if ns != 0 { return 95; }
    match op {
        RtOp::AddAddr6 { index, addr, prefix, flags, excl, replace } => {
            if index != 1 && nic_of(index).is_none() { return 19; }
            if addr == [0; 16] || addr[0] == 0xff { return 22; }
            if is_link_local(&addr) && prefix != 64 && index == 1 { return 22; }
            {
                let mut t = STATIC_A.lock();
                if t.iter().any(|e| e.0 == index && e.1 == addr && e.2 == prefix) { return if replace && !excl { 0 } else { 17 }; }
                if index == 1 && (addr == LOOPBACK || t.iter().any(|e| e.1 == addr)) { return 17; }
                t.push((index, addr, prefix));
            }
            if let Some(nif) = nic_of(index) {
                let sid = iface::sid_of(nif.id);
                let now = dad_register(nif.id, nif.name().as_bytes(), addr, prefix, flags & crate::rtnetlink::IFA_F_NODAD != 0);
                if let (Some(ref mut s), true) = (&mut *crate::stack_for(sid), now) {
                    s.interface.update_ip_addrs(|v| { let _ = v.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address(addr)), prefix)); });
                }
                if flags & crate::rtnetlink::IFA_F_NOPREFIXROUTE == 0 && prefix < 128 {
                    let p = mask16(&addr, prefix);
                    let mut t = PREFIXES.lock();
                    if !t.iter().any(|e| e.0 == sid && e.1 == p && e.2 == prefix) { t.push((sid, p, prefix, 256)); }
                }
                iface::notify(nif.id);
            }
            0
        }
        RtOp::DelAddr6 { index, addr, prefix } => {
            {
                let mut t = STATIC_A.lock();
                let Some(i) = t.iter().position(|e| e.0 == index && e.1 == addr && (prefix == 0 || e.2 == prefix)) else { return 99 };
                t.remove(i);
            }
            if let Some(nif) = nic_of(index) {
                let sid = iface::sid_of(nif.id);
                dad_forget(nif.id, &addr);
                if let Some(ref mut s) = *crate::stack_for(sid) {
                    s.interface.update_ip_addrs(|v| {
                        let keep: Vec<IpCidr> = v.iter().copied().filter(|c| c.address() != IpAddress::Ipv6(Ipv6Address(addr))).collect();
                        v.clear();
                        for c in keep { let _ = v.push(c); }
                    });
                }
                let p = mask16(&addr, prefix);
                let others = STATIC_A.lock().iter().any(|e| e.0 == index && e.2 == prefix && mask16(&e.1, prefix) == p);
                if !others { PREFIXES.lock().retain(|e| !(e.0 == sid && e.1 == p && e.2 == prefix && e.3 == 256)); }
                iface::notify(nif.id);
            }
            0
        }
        RtOp::AddRoute6 { dst, plen, gw, oif, metric, excl, replace } => {
            let mut oif = oif;
            if oif == 0 {
                match gw.and_then(|g| best(&g)) { Some(b) => oif = b.2, None => return 22 }
            }
            if oif != 1 && nic_of(oif).is_none() { return 19; }
            {
                let mut t = STATIC_R.lock();
                if let Some(i) = t.iter().position(|r| r.dst == dst && r.plen == plen && r.oif == oif && r.metric == metric) {
                    if replace && !excl { t.remove(i); } else { return 17; }
                }
                t.push(R6 { dst, plen, gw, oif, metric, proto: 3 });
            }
            sync_static_routes(oif);
            0
        }
        RtOp::DelRoute6 { dst, plen, gw, oif, metric } => {
            let removed = {
                let mut t = STATIC_R.lock();
                let Some(i) = t.iter().position(|r| r.dst == dst && r.plen == plen && (oif == 0 || r.oif == oif)
                    && (gw.is_none() || r.gw == gw) && metric.map_or(true, |m| r.metric == m)) else { return 3 };
                t.remove(i)
            };
            sync_static_routes(removed.oif);
            0
        }
        _ => 95,
    }
}

/// Mirror the gateway routes of NIC `oif` into its smoltcp interface.
fn sync_static_routes(oif: u32) {
    let Some(nif) = nic_of(oif) else { return };
    let mine: Vec<R6> = STATIC_R.lock().iter().filter(|r| r.oif == oif && r.gw.is_some()).cloned().collect();
    if let Some(ref mut s) = *crate::stack_for(iface::sid_of(nif.id)) {
        s.interface.routes_mut().update(|t| {
            t.retain(|r| !(matches!(r.cidr.address(), IpAddress::Ipv6(_)) && r.cidr.prefix_len() != 0));
            for r in &mine {
                let _ = t.push(smoltcp::iface::Route {
                    cidr: IpCidr::new(IpAddress::Ipv6(Ipv6Address(r.dst)), r.plen),
                    via_router: IpAddress::Ipv6(Ipv6Address(r.gw.unwrap())),
                    preferred_until: None, expires_at: None,
                });
            }
        });
    }
    iface::notify(nif.id);
}

/// The NIC's global v6 address (SLAAC, else a static one with scope global).
pub(crate) fn nic_global(id: u8) -> Option<[u8; 16]> {
    let sl = SLAAC.lock()[id as usize].addrs;
    if let Some((g, _)) = sl.iter().flatten().copied().find(|(g, _)| dad_ok(id, g)) { return Some(g); }
    let idx = id as u32 + 2;
    STATIC_A.lock().iter().find(|&&(i, a, _)| i == idx && !is_link_local(&a) && a[0] != 0xff && dad_ok(id, &a)).map(|&(_, a, _)| a)
}

/// Egress for a forwarded v6 packet: `(nic id, next hop, source address)`. Only NICs (never a dynamic link).
pub(crate) fn nic_route6(dst: [u8; 16]) -> Option<(u8, [u8; 16], [u8; 16])> {
    let (oif, gw) = lookup6(dst)?;
    if oif < 2 { return None; }
    let id = (oif - 2) as u8;
    if (id as usize) >= netdev::count() { return None; }
    let src = nic_global(id)?;
    Some((id, gw.unwrap_or(dst), src))
}

/// True when `a` is an address of this host's initial namespace (NIC, loopback, any ns-0 link).
pub(crate) fn is_host6(a: &[u8; 16]) -> bool {
    if addrs6().iter().any(|x| &x.addr == a) { return true; }
    let v = crate::vif::VIFS.lock();
    v.iter().flatten().any(|x| x.netns == 0 && x.addrs6.iter().any(|&(p, _)| &p == a))
}

#[cfg(test)]
mod ra_mtu_tests {
    use super::*;
    #[test]
    fn rdnss_dnssl_parse() {
        let mut o = alloc::vec![25u8, 3, 0, 0, 0, 0, 0x0e, 0x10];
        let mut a = [0u8; 16]; a[0] = 0xfe; a[1] = 0xc0; a[15] = 3;
        o.extend_from_slice(&a);
        o.extend_from_slice(&[0x20, 1, 0xd, 0xb8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 9]);
        let (l, v) = parse_rdnss(&o).unwrap();
        assert_eq!((l, v.len(), v[0]), (3600, 2, a));
        assert!(parse_rdnss(&o[..16]).is_none());
        assert!(parse_rdnss(&o[..8]).is_none());
        // DNSSL "lan.example" + "x.org", padded to 8.
        let mut d = alloc::vec![31u8, 0, 0, 0, 0, 0, 0, 60];
        d.extend_from_slice(b"\x03lan\x07example\x00\x01x\x03org\x00");
        while d.len() % 8 != 0 { d.push(0); }
        d[1] = (d.len() / 8) as u8;
        let (l, v) = parse_dnssl(&d).unwrap();
        assert_eq!(l, 60);
        assert_eq!(v, alloc::vec![b"lan.example".to_vec(), b"x.org".to_vec()]);
    }
    #[test]
    fn ra_mtu_bounds() {
        assert_eq!(ra_mtu_ok(1279), None);
        assert_eq!(ra_mtu_ok(1280), Some(1280));
        assert_eq!(ra_mtu_ok(1492), Some(1492));
        assert_eq!(ra_mtu_ok(1500), Some(1500));
        assert_eq!(ra_mtu_ok(1501), None);
    }
}
