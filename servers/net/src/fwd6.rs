//! IPv6 NAT66 for a bridge -> NIC path (design-ipv6 L9, phase 1: outbound masquerade).
//!
//! Mirrors `fwd.rs`: a container behind an ns-0 bridge sends an IPv6 frame to the bridge MAC with a
//! destination off the bridge's prefixes. Gated by `ipv6/conf/{all,<bridge>}/forwarding`
//! (`sched::ns::forwarding6`), it is source-translated to the NIC's global address and a reserved
//! port (TCP/UDP) or echo id (ICMPv6), and queued for the NIC pass (`fwd::FWD.txq`). Replies are
//! matched in the NIC rx hook and handed to the bridge switch through `fwd::FWD.inq`.
//! Never forwarded: link-local, multicast, unspecified and any address of this host. Neighbor
//! resolution on the NIC is a small NS/NA cache (`Nd`). ICMPv6 errors for forwarded flows and
//! extension headers are not handled (design-ipv6 D13).
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::fwd::{self, FWD, PROTO_TCP, PROTO_UDP};
use crate::vifsel::Sid;

const PROTO_ICMP6: u8 = 58;

pub(crate) struct Ct6 {
    pub(crate) proto: u8, pub(crate) nat: u16,
    cip: [u8; 16], cport: u16, cmac: [u8; 6], pub(crate) br: Sid, pub(crate) port: Sid, bmac: [u8; 6],
    rip: [u8; 16], rport: u16,
    nic: u8, nicip: [u8; 16], pub(crate) last: u64, replied: bool, tst: u8, fins: u8,
    /// 0 = outbound SNAT flow, 1 = inbound DNAT flow (peer -> published port), 2 = hairpin (container -> host address, DNAT + masquerade).
    kind: u8,
    /// Hairpin target: container ip/port, its bridge and port, its MAC and the bridge MAC.
    tip: [u8; 16], tport: u16, pub(crate) tbr: Sid, pub(crate) tps: Sid, tmac: [u8; 6], tbmac: [u8; 6],
}
impl Ct6 {
    const BLANK: Ct6 = Ct6 { proto: 0, nat: 0, cip: [0; 16], cport: 0, cmac: [0; 6], br: crate::vifsel::NO_PEER, port: crate::vifsel::NO_PEER, bmac: [0; 6],
        rip: [0; 16], rport: 0, nic: 0, nicip: [0; 16], last: 0, replied: false, tst: 0, fins: 0,
        kind: 0, tip: [0; 16], tport: 0, tbr: crate::vifsel::NO_PEER, tps: crate::vifsel::NO_PEER, tmac: [0; 6], tbmac: [0; 6] };
}
struct Lrn6 { br: Sid, ip: [u8; 16], mac: [u8; 6], port: Sid, t: u64 }
struct Park6 { br: Sid, sbr: Sid, ip: [u8; 16], t: u64, port: Sid, frame: Vec<u8> }
pub(crate) struct L6 { lrn: Vec<Lrn6>, park: Vec<Park6> }
impl L6 { pub(crate) const fn new() -> L6 { L6 { lrn: Vec::new(), park: Vec::new() } }
    pub(crate) fn forget(&mut self, sid: Sid) {
        self.lrn.retain(|l| l.br != sid && l.port != sid);
        self.park.retain(|q| q.br != sid && q.sbr != sid && q.port != sid);
    }
    pub(crate) fn lrn_len(&self) -> usize { self.lrn.len() }
    pub(crate) fn park_len(&self) -> usize { self.park.len() } }
pub(crate) struct Nd { nic: u8, ip: [u8; 16], mac: [u8; 6], t: u64 }
pub(crate) struct Pend6 { nic: u8, ip: [u8; 16], t: u64, frames: Vec<Vec<u8>> }

static USED6: AtomicBool = AtomicBool::new(false);

pub(crate) fn ttl(e: &Ct6) -> u64 {
    match e.proto {
        PROTO_UDP => if e.replied { fwd::UDP_REPLIED_NS } else { fwd::UDP_NS },
        PROTO_TCP => match e.tst { 0 | 1 => fwd::TCP_SYN_NS, 2 => fwd::TCP_EST_NS, 3 => fwd::TCP_FIN_NS, _ => fwd::TCP_TW_NS },
        _ => fwd::ICMP_NS,
    }
}

pub(crate) fn sweep6(st: &mut fwd::St, now: u64) {
    st.ct6.retain(|e| now.saturating_sub(e.last) < ttl(e));
    st.pend6.retain(|q| now.saturating_sub(q.t) < fwd::PEND_NS);
    st.nd.retain(|a| now.saturating_sub(a.t) < fwd::ARP_NS);
    let mut i = 0;
    while i < st.l6.park.len() {
        if now.saturating_sub(st.l6.park[i].t) >= fwd::PEND_NS * 2 {
            let q = st.l6.park.remove(i);
            if st.expired6.len() < 16 { st.expired6.push((q.sbr, q.port, q.frame)); }
        } else { i += 1; }
    }
}

fn sum(d: &[u8], mut acc: u32) -> u32 {
    let mut i = 0;
    while i + 1 < d.len() { acc += u16::from_be_bytes([d[i], d[i + 1]]) as u32; i += 2; }
    if i < d.len() { acc += (d[i] as u32) << 8; }
    acc
}
fn fold(mut a: u32) -> u16 { while a >> 16 != 0 { a = (a & 0xffff) + (a >> 16); } !(a as u16) }

/// Upper-layer checksum with the IPv6 pseudo-header (src, dst, length, next header) over `f[54..54+len]`.
fn csum6(f: &mut [u8], proto: u8, len: usize) {
    let off = match proto { PROTO_TCP => 16, PROTO_UDP => 6, _ => 2 };
    f[54 + off] = 0; f[54 + off + 1] = 0;
    let mut a = sum(&f[22..54], 0) + len as u32 + proto as u32;
    a = sum(&f[54..54 + len], a);
    let mut c = fold(a);
    if c == 0 && proto == PROTO_UDP { c = 0xffff; }
    f[54 + off..54 + off + 2].copy_from_slice(&c.to_be_bytes());
}

struct P6 { proto: u8, flags: u8, src: [u8; 16], dst: [u8; 16], sport: u16, dport: u16, l4len: usize }

/// A well-formed IPv6 TCP/UDP segment or ICMPv6 echo (request or reply per `reply`), no extension headers.
fn parse(f: &mut Vec<u8>, reply: bool) -> Option<P6> {
    if f.len() < 54 || f[12] != 0x86 || f[13] != 0xdd || f[14] >> 4 != 6 { return None; }
    let plen = u16::from_be_bytes([f[18], f[19]]) as usize;
    if 54 + plen > f.len() { return None; }
    f.truncate(54 + plen);
    let proto = f[20];
    let o = 54;
    let (sport, dport) = match proto {
        PROTO_UDP => {
            if plen < 8 { return None; }
            let ul = u16::from_be_bytes([f[o + 4], f[o + 5]]) as usize;
            if ul < 8 || ul > plen { return None; }
            (u16::from_be_bytes([f[o], f[o + 1]]), u16::from_be_bytes([f[o + 2], f[o + 3]]))
        }
        PROTO_TCP => {
            if plen < 20 { return None; }
            let doff = (f[o + 12] >> 4) as usize * 4;
            if doff < 20 || doff > plen { return None; }
            (u16::from_be_bytes([f[o], f[o + 1]]), u16::from_be_bytes([f[o + 2], f[o + 3]]))
        }
        PROTO_ICMP6 => {
            if plen < 8 || f[o] != if reply { 129 } else { 128 } || f[o + 1] != 0 { return None; }
            let id = u16::from_be_bytes([f[o + 4], f[o + 5]]);
            (id, id)
        }
        _ => return None,
    };
    let flags = if proto == PROTO_TCP { f[o + 13] } else { 0 };
    Some(P6 { proto, flags, src: f[22..38].try_into().unwrap(), dst: f[38..54].try_into().unwrap(), sport, dport, l4len: plen })
}

fn prefix_match(a: &[u8; 16], b: &[u8; 16], len: u8) -> bool {
    let full = (len / 8) as usize;
    if a[..full] != b[..full] { return false; }
    let rem = len % 8;
    rem == 0 || (a[full] ^ b[full]) >> (8 - rem) == 0
}

fn finish(f: &mut [u8], p: &P6) { csum6(f, p.proto, p.l4len); }

/// Bridge `br`, port `port`: an IPv6 frame addressed to the bridge MAC. `Some(frame)` = leave it to the
/// bridge's stack (on-link, ND, host-local, non-forwardable); `None` = consumed (forwarded or dropped).

pub(crate) fn divert(br: Sid, port: Sid, bmac: [u8; 6], mut f: Vec<u8>) -> Option<Vec<u8>> {
    let dst: [u8; 16] = f[38..54].try_into().unwrap();
    let src: [u8; 16] = f[22..38].try_into().unwrap();
    if dst[0] == 0xff || (dst[0] == 0xfe && dst[1] & 0xc0 == 0x80) || dst == [0; 16] { return Some(f); }
    let addrs = crate::tunbe::addrs6_of(br);
    if crate::nfnl::has_tables(0) && (f[20] == PROTO_TCP || f[20] == PROTO_UDP)
        && addrs.iter().any(|&(a, pl)| a[0] != 0xfe && a != src && prefix_match(&a, &src, pl)) {
        if let Some(p) = parse(&mut f, false) {
            if reply_in6(br, port, &mut f, &p) || hair6(br, port, bmac, &mut f, &p) { return None; }
        }
    }
    if crate::ip6::is_host6(&dst) { return Some(f); }
    if addrs.iter().any(|&(a, p)| prefix_match(&a, &dst, p)) { return Some(f); }
    // Anti-spoof: a global source inside one of the bridge's prefixes, never the bridge itself.
    if !addrs.iter().any(|&(a, p)| a[0] != 0xfe && a != src && prefix_match(&a, &src, p)) { return None; }
    let Some(p) = parse(&mut f, false) else { return None };
    // Another ns-0 bridge's prefix: route as a plain IPv6 router, no NAT (forwarding on both sides).
    let rt = bridge_for6(p.dst).filter(|t| t.br != br);
    if let Some(t) = &rt {
        let n = t.name.iter().position(|&c| c == 0).unwrap_or(16);
        if !sched::ns::forwarding6(0, &t.name[..n]) { return None; }
    }
    if f[21] <= 1 {
        if let Some(&(ba, _)) = addrs.iter().find(|&&(a, pl)| a[0] != 0xfe && a != src && prefix_match(&a, &src, pl)) { time_exceeded(br, port, bmac, ba, &f); }
        return None;
    }
    if let Some(t) = rt { route6_l3(br, port, &p, &t, f); return None; }
    let Some((nic, nh, nicip)) = crate::ip6::nic_route6(p.dst) else { fwd::stat6(fwd::S6_NOROUTE); return None };
    let Some(nicmac) = drivers::netdev::get(nic as usize).and_then(|n| n.mac()) else { fwd::stat6(fwd::S6_NOMAC); return None };
    fwd::stat6(fwd::S6_NAT);
    let now = sched::monotonic_ns();
    let cmac: [u8; 6] = f[6..12].try_into().unwrap();
    let rport = if p.proto == PROTO_ICMP6 { 0 } else { p.dport };
    let mut st = FWD.lock();
    USED6.store(true, Ordering::Relaxed);
    fwd::USED.store(true, Ordering::Relaxed);
    fwd::sweep_pub(&mut st, now);
    let found = st.ct6.iter_mut().find(|e| e.kind == 0 && e.proto == p.proto && e.br == br && e.port == port && e.cip == p.src && e.cport == p.sport
        && e.rip == p.dst && e.rport == rport).map(|e| { e.last = now; e.cmac = cmac; (e.nat, e.replied) });
    let nat = match found {
        Some((n, rep)) => {
            // Established flows traverse the filter hook too, so a rule added mid-flow bites.
            if crate::nfnl::has_tables(0) {
                let mut oif = [0u8; 16]; oif[..4].copy_from_slice(b"eth0");
                let mut m = crate::nfnl::Pkt { rel: false, est: rep, l4: p.proto, src: [0; 4], dst: [0; 4], sport: p.sport, dport: rport, oif, mark: 0, local: false, v6: Some((p.src, p.dst)) };
                if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return None; }
            }
            n
        }
        None => {
            if p.proto == PROTO_TCP && p.flags & 0x12 != 0x02 { return None; }
            drop(st);
            if crate::nfnl::has_hook(0, crate::nfnl::H_POST) {
                let mut oif = [0u8; 16]; oif[..4].copy_from_slice(b"eth0");
                let mut m = crate::nfnl::Pkt { rel: false, est: false, l4: p.proto, src: [0; 4], dst: [0; 4], sport: p.sport, dport: rport, oif, mark: 0, local: false, v6: Some((p.src, p.dst)) };
                if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return None; }
                if crate::nfnl::eval(0, crate::nfnl::H_POST, &mut m).1 != crate::nfnl::Nat::Masq { return None; }
            }
            let mut pick = None;
            let claimed = if crate::nfnl::has_tables(0) { crate::nfnl::dnat_ports(0) } else { Vec::new() };
            for _ in 0..16 {
                let mut g = FWD.lock();
                if fwd::ct4_len(&g) + g.ct6.len() >= fwd::CT_CAP { return None; }
                let mut cand = None;
                for _ in 0..fwd::NAT_SPAN {
                    let c = fwd::NAT_BASE + g.next % fwd::NAT_SPAN;
                    g.next = g.next.wrapping_add(1);
                    let v4 = fwd::v4_nat_busy(&g, c, if p.proto == PROTO_ICMP6 { 1 } else { p.proto });
                    if !v4 && !g.ct6.iter().any(|e| e.nat == c && e.proto == p.proto) && !claimed.iter().any(|&(a, b)| c >= a && c <= b) { cand = Some(c); break; }
                }
                let Some(c) = cand else { return None };
                g.ct6.push(Ct6 { proto: p.proto, nat: c, cip: p.src, cport: p.sport, cmac, br, port, bmac, rip: p.dst, rport,
                    nic, nicip, last: now, replied: false, tst: 0, fins: 0, ..Ct6::BLANK });
                drop(g);
                if p.proto == PROTO_ICMP6 || !crate::host_port_busy(p.proto == PROTO_TCP, c) { pick = Some(c); break; }
                let mut g = FWD.lock();
                if let Some(i) = g.ct6.iter().position(|e| e.nat == c && e.proto == p.proto && e.cip == p.src && e.cport == p.sport) { g.ct6.swap_remove(i); }
            }
            let Some(n) = pick else { return None };
            st = FWD.lock();
            n
        }
    };
    if p.proto == PROTO_TCP {
        if let Some(e) = st.ct6.iter_mut().find(|e| e.nat == nat && e.proto == PROTO_TCP) { fwd::tcp_fsm(&mut e.tst, &mut e.fins, p.flags, true); }
    }
    f[21] -= 1;
    f[22..38].copy_from_slice(&nicip);
    if p.proto != PROTO_ICMP6 { f[54..56].copy_from_slice(&nat.to_be_bytes()); } else { f[58..60].copy_from_slice(&nat.to_be_bytes()); }
    finish(&mut f, &p);
    f[6..12].copy_from_slice(&nicmac);
    emit(&mut st, now, nic, nh, nicip, nicmac, f);
    drop(st);
    drivers::netdev::kick();
    None
}

fn emit(st: &mut fwd::St, now: u64, nic: u8, nh: [u8; 16], nicip: [u8; 16], nicmac: [u8; 6], mut f: Vec<u8>) {
    fwd::stat6(fwd::S6_EMIT);
    match st.nd.iter().find(|a| a.nic == nic && a.ip == nh && now.saturating_sub(a.t) < fwd::ARP_NS).map(|a| a.mac) {
        Some(m) => { fwd::stat6(fwd::S6_ND_HIT); f[0..6].copy_from_slice(&m); fwd::push_tx(st, nic, f); }
        None => {
            fwd::stat6(fwd::S6_ND_MISS);
            let first = match st.pend6.iter_mut().find(|q| q.nic == nic && q.ip == nh) {
                Some(q) => { if q.frames.len() < 4 { q.frames.push(f); } false }
                None => { st.pend6.push(Pend6 { nic, ip: nh, t: now, frames: alloc::vec![f] }); true }
            };
            if first {
                let src = if nh[0] == 0xfe { crate::ip6::ll_of(nic).unwrap_or(nicip) } else { nicip };
                fwd::push_tx(st, nic, ns_frame(nicmac, src, nh));
            }
        }
    }
}

/// Neighbor solicitation for `target`, to its solicited-node group.
/// Routed forward (no NAT) of an IPv6 frame from one ns-0 bridge into another's prefix: FORWARD hook, hop limit,
/// then the destination's learned MAC; a miss parks the packet (a few per next hop) and sends an NS, `fwd::divert`
/// replays it on the NA and `sweep6` answers the sender with address unreachable after the timeout. The source is kept.
fn route6_l3(sbr: Sid, sport: Sid, p: &P6, t: &Tgt6, mut f: Vec<u8>) {
    if crate::nfnl::has_tables(0) {
        let est = p.proto == PROTO_TCP && p.flags & 0x12 != 0x02;
        let rport = if p.proto == PROTO_ICMP6 { 0 } else { p.dport };
        let mut m = crate::nfnl::Pkt { rel: false, est, l4: p.proto, src: [0; 4], dst: [0; 4], sport: p.sport, dport: rport, oif: t.name, mark: 0, local: false, v6: Some((p.src, p.dst)) };
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return; }
    }
    let mut st = FWD.lock();
    USED6.store(true, Ordering::Relaxed);
    fwd::USED.store(true, Ordering::Relaxed);
    let Some((tmac, tport)) = st.l6.lrn.iter().find(|l| l.br == t.br && l.ip == p.dst).map(|l| (l.mac, l.port)) else {
        if st.l6.park.iter().filter(|q| q.br == t.br && q.ip == p.dst).count() < fwd::PARK_PER_IP && st.l6.park.len() < 16 {
            st.l6.park.push(Park6 { br: t.br, sbr, ip: p.dst, t: sched::monotonic_ns(), port: sport, frame: f });
        }
        ns_into(&mut st, t, p.dst);
        drop(st);
        drivers::netdev::kick();
        return;
    };
    f[21] -= 1;
    f[0..6].copy_from_slice(&tmac);
    f[6..12].copy_from_slice(&t.mac);
    if st.inq.len() < 256 { st.inq.push_back((t.br, tport, f)); fwd::INQ_N.fetch_add(1, Ordering::Relaxed); }
    drop(st);
    drivers::netdev::kick();
}

fn ns_frame(mac: [u8; 6], src: [u8; 16], target: [u8; 16]) -> Vec<u8> {
    let mut f = alloc::vec![0u8; 14 + 40 + 32];
    f[0..6].copy_from_slice(&[0x33, 0x33, 0xff, target[13], target[14], target[15]]);
    f[6..12].copy_from_slice(&mac);
    f[12] = 0x86; f[13] = 0xdd;
    f[14] = 0x60; f[18..20].copy_from_slice(&32u16.to_be_bytes()); f[20] = PROTO_ICMP6; f[21] = 255;
    f[22..38].copy_from_slice(&src);
    f[38] = 0xff; f[39] = 0x02; f[49] = 1; f[50] = 0xff; f[51..54].copy_from_slice(&target[13..16]);
    f[54] = 135;
    f[62..78].copy_from_slice(&target);
    f[78] = 1; f[79] = 1; f[80..86].copy_from_slice(&mac);
    csum6(&mut f, PROTO_ICMP6, 32);
    f
}

fn learn(st: &mut fwd::St, nic: u8, ip: [u8; 16], mac: [u8; 6], now: u64) {
    fwd::stat6(fwd::S6_ND_LEARN);
    if let Some(a) = st.nd.iter_mut().find(|a| a.nic == nic && a.ip == ip) { a.mac = mac; a.t = now; }
    else if st.nd.len() < 64 { st.nd.push(Nd { nic, ip, mac, t: now }); }
    if let Some(i) = st.pend6.iter().position(|q| q.nic == nic && q.ip == ip) {
        let q = st.pend6.swap_remove(i);
        for mut f in q.frames { f[0..6].copy_from_slice(&mac); fwd::push_tx(st, nic, f); }
    }
}

/// NIC rx hook for IPv6 frames. True = taken (a reply of a forwarded flow).
pub(crate) fn nic_rx(nic: u8, buf: &[u8]) -> bool {
    fwd::stat6(fwd::S6_RX);
    if !USED6.load(Ordering::Relaxed) && !crate::nfnl::has_tables(0) { return false; }
    let plen = u16::from_be_bytes([buf[18], buf[19]]) as usize;
    if buf[20] == PROTO_ICMP6 && plen >= 24 && buf.len() >= 54 + 24 {
        let ty = buf[54];
        if ty == 136 || ty == 135 {
            let now = sched::monotonic_ns();
            let mut st = FWD.lock();
            if ty == 136 {
                let tgt: [u8; 16] = buf[62..78].try_into().unwrap();
                let mac = if plen >= 32 && buf.len() >= 86 && buf[78] == 2 && buf[79] == 1 { buf[80..86].try_into().unwrap() } else { buf[6..12].try_into().unwrap() };
                learn(&mut st, nic, tgt, mac, now);
            } else if plen >= 32 && buf.len() >= 86 && buf[78] == 1 && buf[79] == 1 && buf[22..38] != [0; 16] {
                learn(&mut st, nic, buf[22..38].try_into().unwrap(), buf[80..86].try_into().unwrap(), now);
            }
            drop(st);
            drivers::netdev::kick();
            return false;
        }
        if (1..=4).contains(&ty) { return icmp6_err_rx(nic, buf); }
        if ty != 129 { return false; }
    } else if buf[20] == PROTO_ICMP6 {
        // Short ICMPv6 (plen < 24): only an echo reply (8-byte header + <16 bytes of data, e.g. a bare ping) is of interest.
        if plen < 8 || buf.len() < 54 + 8 || buf[54] != 129 { return false; }
    } else if buf[20] != PROTO_TCP && buf[20] != PROTO_UDP { return false; }
    fwd::stat6(fwd::S6_RX_L4);
    let mut st = FWD.lock();
    if st.ct6.is_empty() && !crate::nfnl::has_tables(0) { return false; }
    let mut f = buf.to_vec();
    let Some(p) = parse(&mut f, buf[20] == PROTO_ICMP6) else { return false };
    let now = sched::monotonic_ns();
    let key = |e: &Ct6| e.nic == nic && e.proto == p.proto && e.nicip == p.dst && e.nat == p.dport
        && e.rip == p.src && (p.proto == PROTO_ICMP6 || e.rport == p.sport) && e.kind < 2;
    if p.proto != PROTO_ICMP6 && !st.ct6.iter().any(|e| key(e)) {
        match new_in6(&mut st, nic, &p, now, buf) { 0 => return false, 1 => return true, _ => {} }
    }
    let Some(e) = st.ct6.iter_mut().find(|e| key(e)) else { fwd::stat6(fwd::S6_RX_MISS); return false };
    fwd::stat6(fwd::S6_RX_HIT);
    e.last = now; e.replied = true;
    if p.proto == PROTO_TCP { fwd::tcp_fsm(&mut e.tst, &mut e.fins, p.flags, e.kind == 1); }
    let (cip, cport, cmac, bmac, br, port) = (e.cip, e.cport, e.cmac, e.bmac, e.br, e.port);
    if crate::nfnl::has_tables(0) {
        let mut m = crate::nfnl::Pkt { rel: false, est: true, l4: p.proto, src: [0; 4], dst: [0; 4], sport: p.sport, dport: cport, oif: fwd::br_name(br), mark: 0, local: false, v6: Some((p.src, cip)) };
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return true; }
    }
    if f[21] <= 1 { return true; }
    f[21] -= 1;
    f[38..54].copy_from_slice(&cip);
    if p.proto != PROTO_ICMP6 { f[56..58].copy_from_slice(&cport.to_be_bytes()); } else { f[58..60].copy_from_slice(&cport.to_be_bytes()); }
    finish(&mut f, &p);
    f[0..6].copy_from_slice(&cmac);
    f[6..12].copy_from_slice(&bmac);
    if st.inq.len() < 256 { st.inq.push_back((br, port, f)); fwd::INQ_N.fetch_add(1, Ordering::Relaxed); }
    drop(st);
    drivers::netdev::kick();
    true
}

// ---- DNAT66: published ports (design-ipv6 L9 phase 2) --------------------------------

/// Learn the IP -> (MAC, port) of whoever sent an IPv6 frame into bridge `br`.
pub(crate) fn learn_c(br: Sid, port: Sid, bmac: [u8; 6], f: &[u8]) -> Option<[u8; 16]> {
    if f.len() < 54 || f[12] != 0x86 || f[13] != 0xdd { return None; }
    let ip: [u8; 16] = f[22..38].try_into().unwrap();
    let mac: [u8; 6] = f[6..12].try_into().unwrap();
    if ip == [0; 16] || ip[0] == 0xff || mac[0] & 1 != 0 || mac == bmac { return None; }
    let now = sched::monotonic_ns();
    let mut st = FWD.lock();
    if let Some(l) = st.l6.lrn.iter_mut().find(|l| l.br == br && l.ip == ip) { l.mac = mac; l.port = port; l.t = now; }
    else { if st.l6.lrn.len() >= 256 { st.l6.lrn.remove(0); } st.l6.lrn.push(Lrn6 { br, ip, mac, port, t: now }); }
    Some(ip)
}

/// Frames parked on a learn miss for `ip` on `br`, oldest first.
pub(crate) fn unpark(br: Sid, ip: [u8; 16]) -> Vec<(Sid, Sid, Vec<u8>)> {
    let now = sched::monotonic_ns();
    let mut st = FWD.lock();
    st.l6.park.retain(|q| now.saturating_sub(q.t) < fwd::PEND_NS * 2);
    fwd::take_npark(&mut st, br, ip, true, now);
    let mut out = Vec::new();
    let mut i = 0;
    while i < st.l6.park.len() {
        if st.l6.park[i].br == br && st.l6.park[i].ip == ip { let q = st.l6.park.remove(i); out.push((q.sbr, q.port, q.frame)); } else { i += 1; }
    }
    drop(st);
    if !out.is_empty() { drivers::netdev::kick(); }
    out
}

struct Tgt6 { br: Sid, mac: [u8; 6], name: [u8; 16], addr: [u8; 16] }

/// The ns-0 bridge whose global IPv6 prefix holds `ip`.
fn bridge_for6(ip: [u8; 16]) -> Option<Tgt6> {
    let n = crate::vif::VIFS.lock().len();
    for i in 0..n {
        let (mac, name) = {
            let v = crate::vif::VIFS.lock();
            match v[i].as_ref().filter(|b| b.kind == crate::vifsel::VifKind::Bridge && b.netns == 0) { Some(b) => (b.mac, b.name), None => continue }
        };
        let addrs = crate::tunbe::addrs6_of(i as Sid);
        if let Some(&(a, _)) = addrs.iter().find(|&&(a, pl)| a[0] != 0xfe && a != ip && prefix_match(&a, &ip, pl)) {
            return Some(Tgt6 { br: i as Sid, mac, name, addr: a });
        }
    }
    None
}

fn is_new_flow(p: &P6) -> bool { p.proto == PROTO_UDP || (p.proto == PROTO_TCP && p.flags & 0x12 == 0x02) }

fn rewrite_dst(f: &mut [u8], ip: [u8; 16], port: u16) {
    f[38..54].copy_from_slice(&ip);
    f[56..58].copy_from_slice(&port.to_be_bytes());
}

fn ns_into(st: &mut fwd::St, t: &Tgt6, target: [u8; 16]) {
    let r = ns_frame(t.mac, t.addr, target);
    fwd::add_probe(st, t.br, target, &r);
    st.inq.push_back((t.br, crate::vifsel::NO_PEER, r));
    fwd::INQ_N.fetch_add(1, Ordering::Relaxed);
}

/// A new packet from the NIC to an address of this host: PREROUTING DNAT, then FORWARD must accept.
/// 0 = not ours (pass to the stack), 1 = dropped, 2 = entry created.
fn new_in6(st: &mut fwd::St, nic: u8, p: &P6, now: u64, frame: &[u8]) -> u8 {
    if !is_new_flow(p) || p.dst[0] == 0xff || !crate::ip6::is_host6(&p.dst) { return 0; }
    let mut m = crate::nfnl::Pkt { rel: false, est: false, l4: p.proto, src: [0; 4], dst: [0; 4], sport: p.sport, dport: p.dport, oif: [0; 16], mark: 0, local: true, v6: Some((p.src, p.dst)) };
    let (acc, nat) = crate::nfnl::eval(0, crate::nfnl::H_PRE, &mut m);
    let crate::nfnl::Nat::Dnat6(cip, cport) = nat else { return if acc { 0 } else { 1 } };
    let cport = if cport == 0 { p.dport } else { cport };
    let Some(t) = bridge_for6(cip) else { return 1 };
    m.v6 = Some((p.src, cip)); m.dport = cport; m.oif = t.name;
    if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return 1; }
    if fwd::ct4_len(st) + st.ct6.len() >= fwd::CT_CAP || st.ct6.iter().filter(|e| e.kind == 1).count() >= fwd::CT_CAP / 2 { return 1; }
    let Some((cmac, port)) = st.l6.lrn.iter().find(|l| l.br == t.br && l.ip == cip).map(|l| (l.mac, l.port)) else {
        fwd::park_nic(st, t.br, cip, true, nic, now, frame);
        ns_into(st, &t, cip);
        return 1;
    };
    USED6.store(true, Ordering::Relaxed);
    fwd::USED.store(true, Ordering::Relaxed);
    st.ct6.push(Ct6 { proto: p.proto, nat: p.dport, cip, cport, cmac, br: t.br, port, bmac: t.mac, rip: p.src, rport: p.sport,
        nic, nicip: p.dst, last: now, replied: false, tst: 0, fins: 0, kind: 1, ..Ct6::BLANK });
    2
}

/// A container's packet answering an inbound flow: source becomes nicip:H, out of the NIC.
fn reply_in6(br: Sid, port: Sid, f: &mut Vec<u8>, p: &P6) -> bool {
    let mut st = FWD.lock();
    let now = sched::monotonic_ns();
    let Some(e) = st.ct6.iter_mut().find(|e| e.kind == 1 && e.proto == p.proto && e.br == br && e.port == port
        && e.cip == p.src && e.cport == p.sport && e.rip == p.dst && e.rport == p.dport) else { return false };
    e.last = now; e.replied = true;
    if p.proto == PROTO_TCP { fwd::tcp_fsm(&mut e.tst, &mut e.fins, p.flags, false); }
    let (nic, nicip, nat) = (e.nic, e.nicip, e.nat);
    if crate::nfnl::has_tables(0) {
        let mut oif = [0u8; 16]; oif[..4].copy_from_slice(b"eth0");
        let mut m = crate::nfnl::Pkt { rel: false, est: true, l4: p.proto, src: [0; 4], dst: [0; 4], sport: p.sport, dport: p.dport, oif, mark: 0, local: false, v6: Some((p.src, p.dst)) };
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return true; }
    }
    if f[21] <= 1 { return true; }
    let Some((_, nh, _)) = crate::ip6::nic_route6(p.dst) else { return true };
    let Some(nicmac) = drivers::netdev::get(nic as usize).and_then(|n| n.mac()) else { return true };
    f[21] -= 1;
    f[22..38].copy_from_slice(&nicip);
    f[54..56].copy_from_slice(&nat.to_be_bytes());
    finish(f, p);
    f[6..12].copy_from_slice(&nicmac);
    fwd::USED.store(true, Ordering::Relaxed);
    emit(&mut st, now, nic, nh, nicip, nicmac, core::mem::take(f));
    drop(st);
    drivers::netdev::kick();
    true
}

/// Hairpin: container -> an address of this host with a DNAT rule. The target gets the packet from the
/// egress bridge's own address (masquerade) and a NAT port.
fn hair6(br: Sid, port: Sid, bmac: [u8; 6], f: &mut Vec<u8>, p: &P6) -> bool {
    let now = sched::monotonic_ns();
    let mut st = FWD.lock();
    let mut hit = st.ct6.iter().position(|e| e.kind == 2 && e.proto == p.proto && e.br == br && e.port == port
        && e.cip == p.src && e.cport == p.sport && e.rip == p.dst && e.rport == p.dport);
    let mut rev = false;
    if hit.is_none() {
        hit = st.ct6.iter().position(|e| e.kind == 2 && e.proto == p.proto && e.tbr == br && e.tip == p.src && e.tport == p.sport
            && e.nicip == p.dst && e.nat == p.dport);
        rev = hit.is_some();
    }
    if let Some(i) = hit {
        if crate::nfnl::has_tables(0) {
            // Established hairpin flows traverse FORWARD too (post-DNAT addresses, the target's bridge as oif).
            let e = &st.ct6[i];
            let mut m = if !rev {
                crate::nfnl::Pkt { rel: false, est: e.replied, l4: p.proto, src: [0; 4], dst: [0; 4], sport: p.sport, dport: e.tport, oif: fwd::br_name(e.tbr), mark: 0, local: false, v6: Some((p.src, e.tip)) }
            } else {
                crate::nfnl::Pkt { rel: false, est: true, l4: p.proto, src: [0; 4], dst: [0; 4], sport: e.tport, dport: e.cport, oif: fwd::br_name(e.br), mark: 0, local: false, v6: Some((e.tip, e.cip)) }
            };
            if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return true; }
        }
    }
    if hit.is_none() {
        if !is_new_flow(p) || !crate::ip6::is_host6(&p.dst) || fwd::ct4_len(&st) + st.ct6.len() >= fwd::CT_CAP { return false; }
        let mut m = crate::nfnl::Pkt { rel: false, est: false, l4: p.proto, src: [0; 4], dst: [0; 4], sport: p.sport, dport: p.dport, oif: [0; 16], mark: 0, local: true, v6: Some((p.src, p.dst)) };
        let (_, nat) = crate::nfnl::eval(0, crate::nfnl::H_PRE, &mut m);
        let crate::nfnl::Nat::Dnat6(cip, cport) = nat else { return false };
        let cport = if cport == 0 { p.dport } else { cport };
        let Some(t) = bridge_for6(cip) else { return false };
        m.v6 = Some((p.src, cip)); m.dport = cport; m.oif = t.name;
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return true; }
        if crate::nfnl::eval(0, crate::nfnl::H_POST, &mut m).1 != crate::nfnl::Nat::Masq { return false; }
        let Some((tmac, tps)) = st.l6.lrn.iter().find(|l| l.br == t.br && l.ip == cip).map(|l| (l.mac, l.port)) else {
            if st.l6.park.iter().filter(|q| q.br == t.br && q.ip == cip).count() < fwd::PARK_PER_IP && st.l6.park.len() < 16 {
                // Hold the SYN until the NA teaches us the MAC; `fwd::divert` replays it (also across bridges).
                let fr = core::mem::take(f);
                st.l6.park.push(Park6 { br: t.br, sbr: br, ip: cip, t: now, port, frame: fr });
            }
            ns_into(&mut st, &t, cip);
            return true;
        };
        let claimed = crate::nfnl::dnat_ports(0);
        let mut cand = None;
        for _ in 0..fwd::NAT_SPAN {
            let c = fwd::NAT_BASE + st.next % fwd::NAT_SPAN;
            st.next = st.next.wrapping_add(1);
            if !st.ct6.iter().any(|e| e.nat == c && e.proto == p.proto) && !claimed.iter().any(|&(a, b)| c >= a && c <= b) { cand = Some(c); break; }
        }
        let Some(nat) = cand else { return true };
        let cmac: [u8; 6] = f[6..12].try_into().unwrap();
        st.ct6.push(Ct6 { proto: p.proto, nat, cip: p.src, cport: p.sport, cmac, br, port, bmac, rip: p.dst, rport: p.dport,
            nic: 0, nicip: t.addr, last: now, replied: false, tst: 0, fins: 0,
            kind: 2, tip: cip, tport: cport, tbr: t.br, tps, tmac, tbmac: t.mac });
        hit = Some(st.ct6.len() - 1);
    }
    USED6.store(true, Ordering::Relaxed);
    fwd::USED.store(true, Ordering::Relaxed);
    let i = hit.unwrap();
    if f[21] <= 1 { return true; }
    f[21] -= 1;
    let e = &mut st.ct6[i];
    e.last = now;
    if p.proto == PROTO_TCP { fwd::tcp_fsm(&mut e.tst, &mut e.fins, p.flags, !rev); }
    let (dbr, dsid);
    if !rev {
        f[22..38].copy_from_slice(&e.nicip);
        f[54..56].copy_from_slice(&e.nat.to_be_bytes());
        rewrite_dst(f, e.tip, e.tport);
        f[0..6].copy_from_slice(&e.tmac); f[6..12].copy_from_slice(&e.tbmac);
        dbr = e.tbr; dsid = e.tps;
    } else {
        e.replied = true;
        f[22..38].copy_from_slice(&e.rip);
        f[54..56].copy_from_slice(&e.rport.to_be_bytes());
        rewrite_dst(f, e.cip, e.cport);
        f[0..6].copy_from_slice(&e.cmac); f[6..12].copy_from_slice(&e.bmac);
        dbr = e.br; dsid = e.port;
    }
    finish(f, p);
    if st.inq.len() < 256 { st.inq.push_back((dbr, dsid, core::mem::take(f))); fwd::INQ_N.fetch_add(1, Ordering::Relaxed); }
    drop(st);
    drivers::netdev::kick();
    true
}

// ---- ICMPv6 errors for forwarded flows (design-ipv6 L9 phase 2, B) ----------------------

/// Hop limit exhausted on `f` (the container's packet): Time Exceeded back to it from the bridge address `ba`.
fn time_exceeded(br: Sid, port: Sid, bmac: [u8; 6], ba: [u8; 16], f: &[u8]) { icmp6_err(br, port, bmac, ba, f, 3, 0) }

/// A parked IPv6 packet whose next hop never answered the NS: ICMPv6 destination unreachable, address unreachable.
pub(crate) fn unreach6(sbr: Sid, port: Sid, f: &[u8]) {
    if f.len() < 54 || f[20] == PROTO_ICMP6 { return; }
    let mac = { let v = crate::vif::VIFS.lock(); v[sbr as usize].as_ref().filter(|b| b.kind == crate::vifsel::VifKind::Bridge && b.netns == 0).map(|b| b.mac) };
    let Some(mac) = mac else { return };
    let src: [u8; 16] = f[22..38].try_into().unwrap();
    let Some(&(a, _)) = crate::tunbe::addrs6_of(sbr).iter().find(|&&(a, pl)| a[0] != 0xfe && a != src && prefix_match(&a, &src, pl)) else { return };
    icmp6_err(sbr, port, mac, a, f, 1, 3);
}

fn icmp6_err(br: Sid, port: Sid, bmac: [u8; 6], ba: [u8; 16], f: &[u8], ty: u8, code: u8) {
    let inv = &f[14..f.len().min(14 + 1232)];
    let mut r = alloc::vec![0u8; 14 + 40 + 8 + inv.len()];
    r[0..6].copy_from_slice(&f[6..12]);
    r[6..12].copy_from_slice(&bmac);
    r[12] = 0x86; r[13] = 0xdd;
    r[14] = 0x60;
    r[18..20].copy_from_slice(&((8 + inv.len()) as u16).to_be_bytes());
    r[20] = PROTO_ICMP6; r[21] = 64;
    r[22..38].copy_from_slice(&ba);
    r[38..54].copy_from_slice(&f[22..38]);
    r[54] = ty; r[55] = code;
    r[62..].copy_from_slice(inv);
    let n = 8 + inv.len();
    csum6(&mut r, PROTO_ICMP6, n);
    let mut st = FWD.lock();
    if st.inq.len() < 256 { st.inq.push_back((br, port, r)); fwd::INQ_N.fetch_add(1, Ordering::Relaxed); }
    drop(st);
    drivers::netdev::kick();
}

/// An ICMPv6 error (1 unreachable, 2 too big, 3 time exceeded, 4 parameter) from the NIC about a forwarded flow:
/// the embedded packet is rewritten back to the container's addresses and the error handed to the bridge.
fn icmp6_err_rx(nic: u8, buf: &[u8]) -> bool {
    if buf.len() < 54 + 8 + 40 + 8 { return false; }
    let ip = 62;
    let proto = buf[ip + 6];
    if proto != PROTO_TCP && proto != PROTO_UDP { return false; }
    let isrc: [u8; 16] = buf[ip + 8..ip + 24].try_into().unwrap();
    let idst: [u8; 16] = buf[ip + 24..ip + 40].try_into().unwrap();
    let isp = u16::from_be_bytes([buf[ip + 40], buf[ip + 41]]);
    let idp = u16::from_be_bytes([buf[ip + 42], buf[ip + 43]]);
    let mut st = FWD.lock();
    let Some(e) = st.ct6.iter().find(|e| e.kind == 0 && e.nic == nic && e.proto == proto && e.nicip == isrc && e.nat == isp
        && e.rip == idst && e.rport == idp && buf[38..54] == e.nicip) else { return false };
    let (cip, cport, cmac, bmac, br, port) = (e.cip, e.cport, e.cmac, e.bmac, e.br, e.port);
    if crate::nfnl::has_tables(0) {
        let osrc: [u8; 16] = buf[22..38].try_into().unwrap();
        let mut m = crate::nfnl::Pkt { rel: true, est: true, l4: PROTO_ICMP6, src: [0; 4], dst: [0; 4], sport: 0, dport: 0, oif: fwd::br_name(br), mark: 0, local: false, v6: Some((osrc, cip)) };
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return true; }
    }
    let mut f = buf.to_vec();
    let plen = u16::from_be_bytes([f[18], f[19]]) as usize;
    if 54 + plen > f.len() || plen < 8 + 40 + 8 { return false; }
    f.truncate(54 + plen);
    f[38..54].copy_from_slice(&cip);
    f[ip + 8..ip + 24].copy_from_slice(&cip);
    f[ip + 40..ip + 42].copy_from_slice(&cport.to_be_bytes());
    csum6(&mut f, PROTO_ICMP6, plen);
    f[0..6].copy_from_slice(&cmac);
    f[6..12].copy_from_slice(&bmac);
    if st.inq.len() < 256 { st.inq.push_back((br, port, f)); fwd::INQ_N.fetch_add(1, Ordering::Relaxed); }
    drop(st);
    drivers::netdev::kick();
    true
}

/// /proc/net/nic: the NAT66 conntrack table and the NIC neighbour cache.
pub(crate) fn dump6(w: &mut dyn core::fmt::Write, g: &fwd::St, now: u64) {
    use core::fmt::Write as _;
    let h = |a: &[u8; 16]| { let mut s = alloc::string::String::new(); for i in 0..8 { let _ = write!(s, "{}{:x}", if i > 0 { ":" } else { "" }, u16::from_be_bytes([a[2 * i], a[2 * i + 1]])); } s };
    for e in g.ct6.iter() {
        let _ = writeln!(w, "fwd_ct6: proto {} nat {} cip {} cport {} rip {} rport {} nicip {} nic {} kind {} replied {} age_ms {}",
            e.proto, e.nat, h(&e.cip), e.cport, h(&e.rip), e.rport, h(&e.nicip), e.nic, e.kind, e.replied as u8, now.saturating_sub(e.last) / 1_000_000);
    }
    for a in g.nd.iter() {
        let _ = writeln!(w, "fwd_nd: nic {} {} {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} age_ms {}", a.nic, h(&a.ip),
            a.mac[0], a.mac[1], a.mac[2], a.mac[3], a.mac[4], a.mac[5], now.saturating_sub(a.t) / 1_000_000);
    }
}
