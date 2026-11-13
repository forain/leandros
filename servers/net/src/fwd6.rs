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
}
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
pub static D6: [core::sync::atomic::AtomicU32; 64] = [const { core::sync::atomic::AtomicU32::new(0) }; 64];
fn dbg(i: usize) { D6[i].fetch_add(1, Ordering::Relaxed); }

pub(crate) fn divert(br: Sid, port: Sid, bmac: [u8; 6], mut f: Vec<u8>) -> Option<Vec<u8>> {
    dbg(0);
    let dst: [u8; 16] = f[38..54].try_into().unwrap();
    let src: [u8; 16] = f[22..38].try_into().unwrap();
    if dst[0] == 0xff || (dst[0] == 0xfe && dst[1] & 0xc0 == 0x80) || dst == [0; 16] || crate::ip6::is_host6(&dst) { return Some(f); }
    let addrs = crate::tunbe::addrs6_of(br);
    if addrs.iter().any(|&(a, p)| prefix_match(&a, &dst, p)) { return Some(f); }
    // Anti-spoof: a global source inside one of the bridge's prefixes, never the bridge itself.
    if !addrs.iter().any(|&(a, p)| a[0] != 0xfe && a != src && prefix_match(&a, &src, p)) { return { dbg(1); None }; }
    let Some(p) = parse(&mut f, false) else { return { dbg(2); None } };
    if f[21] <= 1 { return { dbg(3); None }; }
    let Some((nic, nh, nicip)) = crate::ip6::nic_route6(p.dst) else { return { dbg(4); None } };
    let Some(nicmac) = drivers::netdev::get(nic as usize).and_then(|n| n.mac()) else { return { dbg(5); None } };
    let now = sched::monotonic_ns();
    let cmac: [u8; 6] = f[6..12].try_into().unwrap();
    let rport = if p.proto == PROTO_ICMP6 { 0 } else { p.dport };
    let mut st = FWD.lock();
    USED6.store(true, Ordering::Relaxed);
    fwd::USED.store(true, Ordering::Relaxed);
    fwd::sweep_pub(&mut st, now);
    let found = st.ct6.iter_mut().find(|e| e.proto == p.proto && e.br == br && e.port == port && e.cip == p.src && e.cport == p.sport
        && e.rip == p.dst && e.rport == rport).map(|e| { e.last = now; e.cmac = cmac; e.nat });
    let nat = match found {
        Some(n) => n,
        None => {
            if p.proto == PROTO_TCP && p.flags & 0x12 != 0x02 { return { dbg(6); None }; }
            drop(st);
            if crate::nfnl::has_hook(0, crate::nfnl::H_POST) {
                let mut oif = [0u8; 16]; oif[..4].copy_from_slice(b"eth0");
                let mut m = crate::nfnl::Pkt { l4: p.proto, src: [0; 4], dst: [0; 4], sport: p.sport, dport: rport, oif, mark: 0, local: false, v6: Some((p.src, p.dst)) };
                if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return { dbg(7); None }; }
                if crate::nfnl::eval(0, crate::nfnl::H_POST, &mut m).1 != crate::nfnl::Nat::Masq { return { dbg(8); None }; }
            }
            let mut pick = None;
            let claimed = if crate::nfnl::has_tables(0) { crate::nfnl::dnat_ports(0) } else { Vec::new() };
            for _ in 0..16 {
                let mut g = FWD.lock();
                if fwd::ct4_len(&g) + g.ct6.len() >= fwd::CT_CAP { return { dbg(9); None }; }
                let mut cand = None;
                for _ in 0..fwd::NAT_SPAN {
                    let c = fwd::NAT_BASE + g.next % fwd::NAT_SPAN;
                    g.next = g.next.wrapping_add(1);
                    let v4 = fwd::v4_nat_busy(&g, c, if p.proto == PROTO_ICMP6 { 1 } else { p.proto });
                    if !v4 && !g.ct6.iter().any(|e| e.nat == c && e.proto == p.proto) && !claimed.iter().any(|&(a, b)| c >= a && c <= b) { cand = Some(c); break; }
                }
                let Some(c) = cand else { return { dbg(10); None } };
                g.ct6.push(Ct6 { proto: p.proto, nat: c, cip: p.src, cport: p.sport, cmac, br, port, bmac, rip: p.dst, rport,
                    nic, nicip, last: now, replied: false, tst: 0, fins: 0 });
                drop(g);
                if p.proto == PROTO_ICMP6 || !crate::host_port_busy(p.proto == PROTO_TCP, c) { pick = Some(c); break; }
                let mut g = FWD.lock();
                if let Some(i) = g.ct6.iter().position(|e| e.nat == c && e.proto == p.proto && e.cip == p.src && e.cport == p.sport) { g.ct6.swap_remove(i); }
            }
            let Some(n) = pick else { return { dbg(11); None } };
            st = FWD.lock();
            n
        }
    };
    if p.proto == PROTO_TCP {
        if let Some(e) = st.ct6.iter_mut().find(|e| e.nat == nat && e.proto == PROTO_TCP) { fwd::tcp_fsm(&mut e.tst, &mut e.fins, p.flags, true); }
    }
    dbg(30);
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
    match st.nd.iter().find(|a| a.nic == nic && a.ip == nh && now.saturating_sub(a.t) < fwd::ARP_NS).map(|a| a.mac) {
        Some(m) => { f[0..6].copy_from_slice(&m); fwd::push_tx(st, nic, f); }
        None => {
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
    if let Some(a) = st.nd.iter_mut().find(|a| a.nic == nic && a.ip == ip) { a.mac = mac; a.t = now; }
    else if st.nd.len() < 64 { st.nd.push(Nd { nic, ip, mac, t: now }); }
    if let Some(i) = st.pend6.iter().position(|q| q.nic == nic && q.ip == ip) {
        let q = st.pend6.swap_remove(i);
        for mut f in q.frames { f[0..6].copy_from_slice(&mac); fwd::push_tx(st, nic, f); }
    }
}

/// NIC rx hook for IPv6 frames. True = taken (a reply of a forwarded flow).
pub(crate) fn nic_rx(nic: u8, buf: &[u8]) -> bool {
    dbg(38);
    if !USED6.load(Ordering::Relaxed) { return { dbg(41); false }; }
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
            return { dbg(42); false };
        }
        if ty != 129 { return { dbg(43); false }; }
    } else if buf[20] != PROTO_TCP && buf[20] != PROTO_UDP { return { dbg(44); false }; }
    dbg(39);
    let mut st = FWD.lock();
    if st.ct6.is_empty() { return { dbg(45); false }; }
    let mut f = buf.to_vec();
    let Some(p) = parse(&mut f, buf[20] == PROTO_ICMP6) else { return { dbg(46); false } };
    let now = sched::monotonic_ns();
    let Some(e) = st.ct6.iter_mut().find(|e| e.nic == nic && e.proto == p.proto && e.nicip == p.dst && e.nat == p.dport
        && e.rip == p.src && (p.proto == PROTO_ICMP6 || e.rport == p.sport)) else { return { dbg(47); false } };
    e.last = now; e.replied = true;
    if p.proto == PROTO_TCP { fwd::tcp_fsm(&mut e.tst, &mut e.fins, p.flags, false); }
    let (cip, cport, cmac, bmac, br, port) = (e.cip, e.cport, e.cmac, e.bmac, e.br, e.port);
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
