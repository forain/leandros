//! Packet-level forwarding from an ns-0 bridge to a NIC (design-podnet3 L3).
//!
//! A container behind a bridge sends an IPv4 frame to the bridge MAC with a
//! destination that is not the bridge's own network. The daemon diverts it
//! here before the bridge's stack sees it ([`divert`]). It is forwarded only
//! when the Linux forward gate is open for the ingress bridge
//! (`sched::ns::forwarding(0, "<bridge>")`, set by `ip_forward=1`). It is then
//! source-NATed to the egress NIC's address and a reserved port (UDP) or id
//! (ICMP echo) and queued for the NIC pass ([`take_txq`]). Replies are matched
//! by the NIC rx hook ([`nic_rx`]), rewritten back and handed to the bridge
//! switch ([`drain_in`]), addressed to the container MAC recorded in the entry.
//!
//! Isolation: the only way out of a container is this path (bridge -> NIC) and
//! the only way back is a conntrack hit on a flow the container opened. A
//! frame whose destination is an address of the host itself (any ns-0 address,
//! loopback, broadcast, multicast) is never forwarded; nothing here ever
//! delivers into another netns or a host-local socket.
//!
//! Locks: `FWD` is a leaf. It is taken under a stack lock (NIC rx hook) and
//! takes only `VIFS`/`IFRT` leaf locks in the non-hook paths, never both
//! orders: the hook paths take `FWD` alone.
#![allow(dead_code)]

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::Mutex;

use crate::vifsel::{Sid, VifKind};

pub(crate) const PROTO_ICMP: u8 = 1;
pub(crate) const PROTO_TCP: u8 = 6;
pub(crate) const PROTO_UDP: u8 = 17;
pub(crate) const CT_CAP: usize = 1024;
const ARP_CAP: usize = 64;
const PEND_FRAMES: usize = 4;
pub(crate) const NAT_BASE: u16 = 61000;
pub(crate) const NAT_SPAN: u16 = 4000;
pub(crate) const UDP_NS: u64 = 30_000_000_000;
pub(crate) const UDP_REPLIED_NS: u64 = 180_000_000_000;
pub(crate) const ICMP_NS: u64 = 30_000_000_000;
pub(crate) const TCP_SYN_NS: u64 = 60_000_000_000;
pub(crate) const TCP_EST_NS: u64 = 7_200_000_000_000;
pub(crate) const TCP_FIN_NS: u64 = 60_000_000_000;
pub(crate) const TCP_TW_NS: u64 = 10_000_000_000;
pub(crate) const PEND_NS: u64 = 1_000_000_000;
pub(crate) const ARP_NS: u64 = 300_000_000_000;

struct Ct {
    proto: u8, nat: u16,
    cip: [u8; 4], cport: u16, cmac: [u8; 6], br: Sid, port: Sid, bmac: [u8; 6],
    rip: [u8; 4], rport: u16,
    nic: u8, nicip: [u8; 4], last: u64, replied: bool,
    /// TCP lite FSM: 0 SYN sent, 1 SYN-ACK seen, 2 established, 3 one side sent FIN, 4 closing (both FINs or RST: TIME_WAIT-lite).
    tst: u8, fins: u8,
    /// 0 = outbound SNAT flow, 1 = inbound DNAT flow (peer -> published port), 2 = hairpin (container -> local address, DNAT + masquerade), 3 = the same without masquerade (cross-network, source kept).
    kind: u8,
    /// Hairpin target: container ip/port, its bridge and port, its MAC and the bridge MAC.
    tip: [u8; 4], tport: u16, tbr: Sid, tps: Sid, tmac: [u8; 6], tbmac: [u8; 6],
}
/// A frame held on a learn miss. `br` is the bridge whose ARP reply releases it, `sbr` the bridge it arrived on.
struct Park { br: Sid, sbr: Sid, ip: [u8; 4], t: u64, port: Sid, frame: Vec<u8> }
/// A NIC frame held on a learn miss (host -> container DNAT); replayed through `nic_rx`.
pub(crate) struct NPark { pub(crate) br: Sid, pub(crate) ip: [u8; 16], pub(crate) v6: bool, pub(crate) nic: u8, pub(crate) t: u64, pub(crate) frame: Vec<u8> }
/// At most this many frames wait per unresolved next hop (Linux's neighbour queue is about 3).
pub(crate) const PARK_PER_IP: usize = 3;
const PARK_TOTAL: usize = 16;
/// Neighbour solicitation resend state for an unresolved next hop with parked frames (Linux mcast_solicit = 3).
pub(crate) struct Probe { br: Sid, ip: [u8; 16], t: u64, sent: u8, frame: Vec<u8> }
struct Lrn { br: Sid, ip: [u8; 4], mac: [u8; 6], port: Sid, t: u64 }
struct Arp { nic: u8, ip: [u8; 4], mac: [u8; 6], t: u64 }
struct Pend { nic: u8, ip: [u8; 4], t: u64, frames: Vec<Vec<u8>> }
pub(crate) struct St {
    pub(crate) ct6: Vec<crate::fwd6::Ct6>, pub(crate) nd: Vec<crate::fwd6::Nd>, pub(crate) pend6: Vec<crate::fwd6::Pend6>, pub(crate) l6: crate::fwd6::L6,
    ct: Vec<Ct>, arp: Vec<Arp>, lrn: Vec<Lrn>, pend: Vec<Pend>, park: Vec<Park>,
    pub(crate) npark: Vec<NPark>, nreplay: Vec<(u8, Vec<u8>)>, replay: Vec<(Sid, Sid, Vec<u8>)>, expired: Vec<(Sid, Sid, Vec<u8>)>, pub(crate) expired6: Vec<(Sid, Sid, Vec<u8>)>, probe: Vec<Probe>,
    pub(crate) txq: VecDeque<(u8, Vec<u8>)>, pub(crate) inq: VecDeque<(Sid, Sid, Vec<u8>)>,
    pub(crate) next: u16, pub(crate) swept: u64,
}
pub(crate) static FWD: Mutex<St> = Mutex::new(St { ct6: Vec::new(), nd: Vec::new(), pend6: Vec::new(), l6: crate::fwd6::L6::new(), ct: Vec::new(), arp: Vec::new(), lrn: Vec::new(), pend: Vec::new(), park: Vec::new(), npark: Vec::new(), nreplay: Vec::new(), replay: Vec::new(), expired: Vec::new(), expired6: Vec::new(), probe: Vec::new(),
    txq: VecDeque::new(), inq: VecDeque::new(), next: 0, swept: 0 });
/// Set at the first forwarded flow: the NIC rx hook costs one load until then.
pub(crate) static USED: AtomicBool = AtomicBool::new(false);
pub(crate) static TXQ_N: AtomicUsize = AtomicUsize::new(0);
pub(crate) static INQ_N: AtomicUsize = AtomicUsize::new(0);

/// Diagnostic counters (/proc/net/nic "fwd_stats:"): where a forwarded UDP/ICMP flow is lost.
/// Order: ing_udp ing_ttl ing_noroute ing_nft_drop ing_nopick emit arp_hit arp_miss pend_exp
/// arp_learn arp_flush nicrx_udp nicrx_udp_hit nicrx_udp_miss icmperr_rx
pub(crate) static STAT: [AtomicUsize; 35] = [const { AtomicUsize::new(0) }; 35];
/// icmp_error_rx rejection reasons live at STAT[15 + n]; the last quoted tuple that found no conntrack entry is kept here.
pub(crate) static LASTQ: [core::sync::atomic::AtomicU64; 2] = [core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0)];
fn rej(n: usize) -> bool { STAT[15 + n].fetch_add(1, Ordering::Relaxed); false }
pub(crate) const S_ING_UDP: usize = 0;
pub(crate) const S_ING_TTL: usize = 1;
pub(crate) const S_ING_NOROUTE: usize = 2;
pub(crate) const S_ING_NFT: usize = 3;
pub(crate) const S_ING_NOPICK: usize = 4;
pub(crate) const S_EMIT: usize = 5;
pub(crate) const S_ARP_HIT: usize = 6;
pub(crate) const S_ARP_MISS: usize = 7;
pub(crate) const S_PEND_EXP: usize = 8;
pub(crate) const S_ARP_LEARN: usize = 9;
pub(crate) const S_ARP_FLUSH: usize = 10;
pub(crate) const S_NICRX_UDP: usize = 11;
pub(crate) const S_NICRX_HIT: usize = 12;
pub(crate) const S_NICRX_MISS: usize = 13;
pub(crate) const S_ICMPERR_RX: usize = 14;
/// IPv6 (fwd6) counters, STAT[22..]: divert reached the NAT stage / no NIC route / no NIC MAC / emitted / nd hit / nd miss (NS sent) /
/// nd learned / nic_rx entered / nic_rx echo or L4 reply seen / reply key hit / reply key miss
pub(crate) const S6_NAT: usize = 22;
pub(crate) const S6_NOROUTE: usize = 23;
pub(crate) const S6_NOMAC: usize = 24;
pub(crate) const S6_EMIT: usize = 25;
pub(crate) const S6_ND_HIT: usize = 26;
pub(crate) const S6_ND_MISS: usize = 27;
pub(crate) const S6_ND_LEARN: usize = 28;
pub(crate) const S6_RX: usize = 29;
pub(crate) const S6_RX_L4: usize = 30;
pub(crate) const S6_RX_HIT: usize = 31;
pub(crate) const S6_RX_MISS: usize = 32;
pub(crate) const S_NONAT: usize = 33;
const S_PARKED: usize = 34;
#[inline] pub(crate) fn stat6(i: usize) { STAT[i].fetch_add(1, Ordering::Relaxed); }
#[inline] fn stat(i: usize) { STAT[i].fetch_add(1, Ordering::Relaxed); }

pub(crate) fn dump_stats(w: &mut dyn core::fmt::Write) {
    let _ = write!(w, "fwd_stats:");
    for (n, i) in ["ing_udp", "ing_ttl", "ing_noroute", "ing_nft_drop", "ing_nopick", "emit", "arp_hit", "arp_miss", "pend_exp",
                   "arp_learn", "arp_flush", "nicrx_udp", "nicrx_hit", "nicrx_miss", "icmperr_rx",
                   "rej_hdr", "rej_frag", "rej_qhdr", "rej_proto", "rej_noct", "rej_odst", "icmperr_queued", "v6_nat", "v6_noroute", "v6_nomac", "v6_emit", "v6_nd_hit", "v6_nd_miss", "v6_nd_learn", "v6_rx", "v6_rx_l4", "v6_rx_hit", "v6_rx_miss", "route_nonat", "parked"].iter().zip(STAT.iter()) {
        let _ = write!(w, " {}={}", n, i.load(Ordering::Relaxed));
    }
    let _ = writeln!(w);
    let (a, b) = (LASTQ[0].load(Ordering::Relaxed), LASTQ[1].load(Ordering::Relaxed));
    let _ = writeln!(w, "fwd_lastq: noct quoted {}.{}.{}.{}:{} -> {}.{}.{}.{}:{} proto {}",
        (a >> 56) as u8, (a >> 48) as u8, (a >> 40) as u8, (a >> 32) as u8, (a >> 16) as u16,
        (b >> 56) as u8, (b >> 48) as u8, (b >> 40) as u8, (b >> 32) as u8, (b >> 16) as u16, a as u8);
    let now = sched::monotonic_ns();
    let g = FWD.lock();
    let _ = writeln!(w, "fwd_state: ct={} ct6={} lrn={} lrn6={} park={} park6={} npark={} replay={} expired={} nreplay={} pend={} inq={} probe={} expired6={}",
        g.ct.len(), g.ct6.len(), g.lrn.len(), g.l6.lrn_len(), g.park.len(), g.l6.park_len(), g.npark.len(), g.replay.len(), g.expired.len(), g.nreplay.len(), g.pend.len(), g.inq.len(), g.probe.len(), g.expired6.len());
    for a in g.arp.iter() {
        let _ = writeln!(w, "fwd_arp: nic {} {}.{}.{}.{} {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} age_ms {}", a.nic, a.ip[0], a.ip[1], a.ip[2], a.ip[3],
            a.mac[0], a.mac[1], a.mac[2], a.mac[3], a.mac[4], a.mac[5], now.saturating_sub(a.t) / 1_000_000);
    }
    for q in g.pend.iter() {
        let _ = writeln!(w, "fwd_pend: nic {} {}.{}.{}.{} frames {} age_ms {}", q.nic, q.ip[0], q.ip[1], q.ip[2], q.ip[3], q.frames.len(), now.saturating_sub(q.t) / 1_000_000);
    }
    for e in g.ct.iter().filter(|e| e.proto != PROTO_TCP) {
        let _ = writeln!(w, "fwd_ct: proto {} nat {} {}.{}.{}.{}:{}->{}.{}.{}.{}:{} kind {} replied {} age_ms {} br {} port {}",
            e.proto, e.nat, e.cip[0], e.cip[1], e.cip[2], e.cip[3], e.cport, e.rip[0], e.rip[1], e.rip[2], e.rip[3], e.rport,
            e.kind, e.replied as u8, now.saturating_sub(e.last) / 1_000_000, e.br, e.port);
    }
    crate::fwd6::dump6(w, &g, now);
}

fn ct_ttl(e: &Ct) -> u64 {
    match e.proto {
        PROTO_UDP => if e.replied { UDP_REPLIED_NS } else { UDP_NS },
        PROTO_TCP => match e.tst { 0 | 1 => TCP_SYN_NS, 2 => TCP_EST_NS, 3 => TCP_FIN_NS, _ => TCP_TW_NS },
        _ => ICMP_NS,
    }
}

/// Advance the TCP lite FSM of `e` for a segment with `flags` (`from_ctr`: sent by the container).
fn tcp_step(e: &mut Ct, flags: u8, from_ctr: bool) { tcp_fsm(&mut e.tst, &mut e.fins, flags, from_ctr) }
pub(crate) fn tcp_fsm(tst: &mut u8, fins: &mut u8, flags: u8, from_ctr: bool) {
    const FIN: u8 = 1; const SYN: u8 = 2; const RST: u8 = 4; const ACK: u8 = 16;
    if flags & RST != 0 { *tst = 4; return; }
    if flags & SYN != 0 {
        if from_ctr && flags & ACK == 0 { if *tst >= 3 { *tst = 0; *fins = 0; } }
        else if !from_ctr && flags & ACK != 0 && *tst == 0 { *tst = 1; }
        return;
    }
    if from_ctr && *tst == 1 && flags & ACK != 0 { *tst = 2; }
    if flags & FIN != 0 && *tst >= 1 && *tst < 4 {
        *fins |= if from_ctr { 1 } else { 2 };
        *tst = if *fins == 3 { 4 } else { 3 };
    }
}

/// True while a live NAT entry holds `port` of the given protocol (TCP or UDP) on the host's NIC address.
pub(crate) fn reserved(tcp: bool, port: u16) -> bool {
    if !USED.load(Ordering::Relaxed) || port < NAT_BASE || port >= NAT_BASE + NAT_SPAN { return false; }
    let proto = if tcp { PROTO_TCP } else { PROTO_UDP };
    let now = sched::monotonic_ns();
    let g = FWD.lock();
    g.ct.iter().any(|e| e.proto == proto && e.nat == port && now.saturating_sub(e.last) < ct_ttl(e))
        || g.ct6.iter().any(|e| e.proto == proto && e.nat == port && now.saturating_sub(e.last) < crate::fwd6::ttl(e))
}

/// Live TCP NAT ports (the ones `reserved(true, _)` reports), for /proc/net/nic.
pub(crate) fn live_tcp_nat_ports(out: &mut alloc::vec::Vec<u16>) {
    let now = sched::monotonic_ns();
    for e in FWD.lock().ct.iter() {
        if e.proto == PROTO_TCP && now.saturating_sub(e.last) < ct_ttl(e) { out.push(e.nat); }
    }
}

/// One line per live TCP entry (diagnostics for failing tests): nat port, flow, kind, FSM state, age, ttl, bridge/port ids.
pub(crate) fn dump_tcp_ct(w: &mut dyn core::fmt::Write) {
    let now = sched::monotonic_ns();
    for e in FWD.lock().ct.iter() {
        if e.proto == PROTO_TCP && now.saturating_sub(e.last) < ct_ttl(e) {
            let _ = writeln!(w, "fwd_nat_ent: nat {} {}.{}.{}.{}:{}->{}.{}.{}.{}:{} kind {} tst {} fins {} age_ms {} ttl_ms {} br {} port {}",
                e.nat, e.cip[0], e.cip[1], e.cip[2], e.cip[3], e.cport, e.rip[0], e.rip[1], e.rip[2], e.rip[3], e.rport,
                e.kind, e.tst, e.fins, now.saturating_sub(e.last) / 1_000_000, ct_ttl(e) / 1_000_000, e.br, e.port);
        }
    }
}

/// Number of live conntrack entries (diagnostics).
pub(crate) fn ct_len() -> usize { let mut g = FWD.lock(); let now = sched::monotonic_ns(); sweep(&mut g, now); g.ct.len() + g.ct6.len() }

/// Drop every entry that belongs to bridge `sid` or entered through port `sid` (it went away).
pub(crate) fn forget(sid: Sid) {
    if !USED.load(Ordering::Relaxed) { return; }
    let mut g = FWD.lock();
    g.ct.retain(|e| e.br != sid && e.port != sid && e.tbr != sid && e.tps != sid);
    g.lrn.retain(|l| l.br != sid && l.port != sid);
    g.ct6.retain(|e| e.br != sid && e.port != sid && e.tbr != sid && e.tps != sid);
    g.park.retain(|q| q.br != sid && q.sbr != sid && q.port != sid);
    g.npark.retain(|q| q.br != sid);
    g.replay.retain(|q| q.0 != sid && q.1 != sid);
    g.expired.retain(|q| q.0 != sid && q.1 != sid);
    g.inq.retain(|q| q.0 != sid && q.1 != sid);
    g.l6.forget(sid);
    g.expired6.retain(|q| q.0 != sid && q.1 != sid);
    g.probe.retain(|q| q.br != sid);
}

// ---- checksums ---------------------------------------------------------------

fn sum(d: &[u8], mut acc: u32) -> u32 {
    let mut i = 0;
    while i + 1 < d.len() { acc += u16::from_be_bytes([d[i], d[i + 1]]) as u32; i += 2; }
    if i < d.len() { acc += (d[i] as u32) << 8; }
    acc
}
fn fold(mut a: u32) -> u16 {
    while a >> 16 != 0 { a = (a & 0xffff) + (a >> 16); }
    !(a as u16)
}
fn ip_csum(f: &mut [u8], ihl: usize) {
    f[24] = 0; f[25] = 0;
    let c = fold(sum(&f[14..14 + ihl], 0));
    f[24..26].copy_from_slice(&c.to_be_bytes());
}
fn udp_csum(f: &mut [u8], ihl: usize, ulen: usize) {
    let o = 14 + ihl;
    if f[o + 6] == 0 && f[o + 7] == 0 { return; } // sender used no checksum
    f[o + 6] = 0; f[o + 7] = 0;
    let mut a = sum(&f[26..34], 0) + PROTO_UDP as u32 + ulen as u32;
    a = sum(&f[o..o + ulen], a);
    let mut c = fold(a);
    if c == 0 { c = 0xffff; }
    f[o + 6..o + 8].copy_from_slice(&c.to_be_bytes());
}
fn tcp_csum(f: &mut [u8], ihl: usize, tlen: usize) {
    let o = 14 + ihl;
    f[o + 16] = 0; f[o + 17] = 0;
    let mut a = sum(&f[26..34], 0) + PROTO_TCP as u32 + tlen as u32;
    a = sum(&f[o..o + tlen], a);
    let c = fold(a);
    f[o + 16..o + 18].copy_from_slice(&c.to_be_bytes());
}
fn icmp_csum(f: &mut [u8], ihl: usize, ilen: usize) {
    let o = 14 + ihl;
    f[o + 2] = 0; f[o + 3] = 0;
    let c = fold(sum(&f[o..o + ilen], 0));
    f[o + 2..o + 4].copy_from_slice(&c.to_be_bytes());
}

// ---- parsing -----------------------------------------------------------------

struct Pkt { flags: u8, ihl: usize, proto: u8, src: [u8; 4], dst: [u8; 4], sport: u16, dport: u16, l4len: usize }

/// A well-formed unfragmented IPv4 UDP datagram or ICMP echo (request when
/// `reply` is false, reply when true). The frame is trimmed to the IP length.
fn parse(f: &mut Vec<u8>, reply: bool) -> Option<Pkt> {
    if f.len() < 34 || f[12] != 0x08 || f[13] != 0x00 || f[14] >> 4 != 4 { return None; }
    let ihl = (f[14] & 15) as usize * 4;
    let tot = u16::from_be_bytes([f[16], f[17]]) as usize;
    if ihl < 20 || tot < ihl || 14 + tot > f.len() { return None; }
    if u16::from_be_bytes([f[20], f[21]]) & 0x3fff != 0 { return None; } // fragment
    f.truncate(14 + tot);
    let proto = f[23];
    let l4len = tot - ihl;
    let o = 14 + ihl;
    let (sport, dport) = match proto {
        PROTO_UDP => {
            if l4len < 8 { return None; }
            let ul = u16::from_be_bytes([f[o + 4], f[o + 5]]) as usize;
            if ul < 8 || ul > l4len { return None; }
            (u16::from_be_bytes([f[o], f[o + 1]]), u16::from_be_bytes([f[o + 2], f[o + 3]]))
        }
        PROTO_TCP => {
            if l4len < 20 { return None; }
            let doff = (f[o + 12] >> 4) as usize * 4;
            if doff < 20 || doff > l4len { return None; }
            (u16::from_be_bytes([f[o], f[o + 1]]), u16::from_be_bytes([f[o + 2], f[o + 3]]))
        }
        PROTO_ICMP => {
            if l4len < 8 || f[o] != if reply { 0 } else { 8 } || f[o + 1] != 0 { return None; }
            let id = u16::from_be_bytes([f[o + 4], f[o + 5]]);
            (id, id)
        }
        _ => return None,
    };
    let flags = if proto == PROTO_TCP { f[o + 13] } else { 0 };
    Some(Pkt { flags, ihl, proto, src: [f[26], f[27], f[28], f[29]], dst: [f[30], f[31], f[32], f[33]], sport, dport, l4len })
}

fn finish(f: &mut [u8], p: &Pkt) {
    ip_csum(f, p.ihl);
    match p.proto {
        PROTO_TCP => tcp_csum(f, p.ihl, p.l4len),
        PROTO_UDP => { let ul = u16::from_be_bytes([f[14 + p.ihl + 4], f[14 + p.ihl + 5]]) as usize; udp_csum(f, p.ihl, ul) }
        _ => icmp_csum(f, p.ihl, p.l4len),
    }
}

// ---- policy helpers ------------------------------------------------------------

fn mask(p: u8) -> u32 { if p == 0 { 0 } else { u32::MAX << (32 - p.min(32) as u32) } }

/// True when `d` is an address of this host (netns 0): never forwarded.
pub(crate) fn host_local(d: [u8; 4]) -> bool {
    if d[0] == 127 || d[0] >= 224 || d == [0; 4] { return true; }
    if crate::iface::is_nic_addr(d) { return true; }
    let v = crate::vif::VIFS.lock();
    v.iter().flatten().any(|x| x.netns == 0 && x.addrs.iter().any(|&(a, p)| a == d
        || (p < 31 && u32::from_be_bytes(a) | !mask(p) == u32::from_be_bytes(d))))
}

/// Bridge `br`'s rx queue, before its stack runs: forward what is not for the
/// bridge's own network, leave the rest.
pub(crate) fn divert(br: Sid, rx: VecDeque<Vec<u8>>, rxsrc: VecDeque<Sid>) -> VecDeque<Vec<u8>> {
    // Cheap pre-check: any unicast IPv4 frame to the bridge MAC at all?
    let info = {
        let v = crate::vif::VIFS.lock();
        v[br as usize].as_ref().filter(|b| b.kind == VifKind::Bridge && b.netns == 0)
            .map(|b| (b.mac, b.name, b.addrs.clone()))
    };
    let Some((bmac, name, addrs)) = info else { return rx };
    let n = name.iter().position(|&c| c == 0).unwrap_or(16);
    let on4 = sched::ns::forwarding(0, &name[..n]);
    let on6 = sched::ns::forwarding6(0, &name[..n]);
    if !on4 && !on6 { return rx; }
    let mut keep = VecDeque::with_capacity(rx.len());
    let aligned = rxsrc.len() == rx.len();
    let nft = crate::nfnl::has_tables(0);
    let mut work: VecDeque<(Sid, Vec<u8>)> = rx.into_iter().enumerate()
        .map(|(i, f)| (if aligned { rxsrc[i] } else { crate::vifsel::NO_PEER }, f)).collect();
    {
        let mut st = FWD.lock();
        if !st.replay.is_empty() || !st.park.is_empty() {
            expire_park(&mut st, sched::monotonic_ns());
            let mut i = 0;
            while i < st.replay.len() { if st.replay[i].0 == br { let (_, p, f) = st.replay.remove(i); work.push_back((p, f)); } else { i += 1; } }
        }
    }
    while let Some((port, mut f)) = work.pop_front() {
        if (nft || on4 || on6) && port != crate::vifsel::NO_PEER && port != br {
            if let Some(ip) = learn(br, port, bmac, &f) { requeue(br, unpark(br, ip), &mut work); }
            if let Some(ip) = crate::fwd6::learn_c(br, port, bmac, &f) { requeue(br, crate::fwd6::unpark(br, ip), &mut work); }
        }
        if on6 && f.len() >= 54 && f[12] == 0x86 && f[13] == 0xdd && f[0..6] == bmac && port != crate::vifsel::NO_PEER && port != br {
            match crate::fwd6::divert(br, port, bmac, f) { Some(back) => keep.push_back(back), None => {} }
            continue;
        }
        if !on4 || f.len() < 34 || f[0..6] != bmac || f[12] != 0x08 || f[13] != 0 {
            keep.push_back(f);
            continue;
        }
        let dst = [f[30], f[31], f[32], f[33]];
        let src = [f[26], f[27], f[28], f[29]];
        if nft && port != crate::vifsel::NO_PEER && port != br && (f[23] == PROTO_TCP || f[23] == PROTO_UDP)
            && addrs.iter().any(|&(a, p)| a != src && u32::from_be_bytes(a) & mask(p) == u32::from_be_bytes(src) & mask(p)) {
            if let Some(p) = parse(&mut f, false) {
                if reply_in(br, port, &mut f, &p) || hair(br, port, bmac, &mut f, &p) { continue; }
            }
        }
        if nft && port != crate::vifsel::NO_PEER && port != br && f[23] == PROTO_ICMP && icmp_error_type(&f) && hair_icmp_err(br, &f) { continue; }
        // The bridge's own network (and its broadcast) stays with the stack.
        if addrs.iter().any(|&(a, p)| u32::from_be_bytes(a) & mask(p) == u32::from_be_bytes(dst) & mask(p)) {
            keep.push_back(f);
            continue;
        }
        // Anti-spoof: the source must belong to the bridge's network, and not be the bridge itself.
        let srcok = addrs.iter().any(|&(a, p)| a != src && u32::from_be_bytes(a) & mask(p) == u32::from_be_bytes(src) & mask(p));
        if !srcok || host_local(dst) { continue; }
        if port == crate::vifsel::NO_PEER || port == br { continue; }
        // Another ns-0 bridge's network: route as a plain IP router, no NAT (ip_forward on both sides).
        if let Some(t) = bridge_for(dst) {
            if t.br != br { route_l3(br, port, &t, f); continue; }
        }
        let Some(p) = parse(&mut f, false) else { continue };
        ingress(br, port, bmac, f, p);
    }
    keep
}

/// Routed forward (no NAT) of an IPv4 frame from one ns-0 bridge into another's network: FORWARD hook,
/// TTL, then the destination's learned MAC (a miss sends an ARP request and drops this packet;
/// the sender's retransmit finds the entry). The source address is kept.
fn route_l3(sbr: Sid, sport: Sid, t: &Tgt, mut f: Vec<u8>) {
    let n = t.name.iter().position(|&c| c == 0).unwrap_or(16);
    if !sched::ns::forwarding(0, &t.name[..n]) { return; }
    if f[22] <= 1 { stat(S_ING_TTL); ttl_exceeded4(sbr, sport, &f); return; }
    let ihl = ((f[14] & 0xf) as usize) * 4;
    if ihl < 20 || f.len() < 14 + ihl { return; }
    if crate::nfnl::has_tables(0) {
        let l4 = f[23];
        let o = 14 + ihl;
        let (sport, dport, est) = if (l4 == PROTO_TCP || l4 == PROTO_UDP) && f.len() >= o + 14 {
            (u16::from_be_bytes([f[o], f[o + 1]]), u16::from_be_bytes([f[o + 2], f[o + 3]]), l4 == PROTO_TCP && f[o + 13] & 0x12 != 0x02)
        } else { (0, 0, false) };
        let mut m = crate::nfnl::Pkt { rel: false, est, l4, src: [f[26], f[27], f[28], f[29]], dst: [f[30], f[31], f[32], f[33]], sport, dport, oif: t.name, mark: 0, local: false, v6: None };
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { stat(S_ING_NFT); return; }
    }
    let dip = [f[30], f[31], f[32], f[33]];
    let mut st = FWD.lock();
    USED.store(true, Ordering::Relaxed);
    let Some((tmac, tport)) = st.lrn.iter().find(|l| l.br == t.br && l.ip == dip).map(|l| (l.mac, l.port)) else {
        // Hold the packet (a few per next hop) until the ARP reply teaches us the MAC; `divert` replays it.
        if st.park.iter().filter(|q| q.br == t.br && q.ip == dip).count() < PARK_PER_IP && st.park.len() < PARK_TOTAL {
            stat(S_PARKED);
            st.park.push(Park { br: t.br, sbr, ip: dip, t: sched::monotonic_ns(), port: sport, frame: f });
        }
        let r = arp_request(t.mac, t.addr, dip);
        { let mut w = [0u8; 16]; w[..4].copy_from_slice(&dip); add_probe(&mut st, t.br, w, &r); }
        st.inq.push_back((t.br, crate::vifsel::NO_PEER, r)); INQ_N.fetch_add(1, Ordering::Relaxed);
        drop(st);
        drivers::netdev::kick();
        return;
    };
    f[22] -= 1;
    ip_csum(&mut f, ihl);
    f[0..6].copy_from_slice(&tmac);
    f[6..12].copy_from_slice(&t.mac);
    if st.inq.len() < 256 { st.inq.push_back((t.br, tport, f)); INQ_N.fetch_add(1, Ordering::Relaxed); }
    drop(st);
    drivers::netdev::kick();
}

fn ingress(br: Sid, port: Sid, bmac: [u8; 6], mut f: Vec<u8>, p: Pkt) {
    if p.proto == PROTO_UDP { stat(S_ING_UDP); }
    if f[22] <= 1 { stat(S_ING_TTL); ttl_exceeded4(br, port, &f); return; } // TTL exhausted
    let Some((nic, nh, nicip)) = crate::iface::nic_route(p.dst) else { stat(S_ING_NOROUTE); return };
    let Some(nicmac) = drivers::netdev::get(nic as usize).and_then(|n| n.mac()) else { return };
    let now = sched::monotonic_ns();
    // Nftables is loaded but no nat/postrouting chain exists: nothing may masquerade, so the packet is
    // routed to the NIC subnet with its source address kept (Linux ip_forward without a NAT rule).
    // A postrouting chain whose verdict for this packet is not masquerade (netavark's internal networks, or a
    // subnet without a masquerade rule) behaves the same way: Linux forwards it unchanged if FORWARD accepts.
    let nomasq = crate::nfnl::has_tables(0) && {
        if !crate::nfnl::has_hook(0, crate::nfnl::H_POST) { true } else {
            let mut oif = [0u8; 16]; oif[..4].copy_from_slice(b"eth0");
            let rp = if p.proto == PROTO_ICMP { 0 } else { p.dport };
            let mut m = crate::nfnl::Pkt { rel: false, est: false, l4: p.proto, src: p.src, dst: p.dst, sport: p.sport, dport: rp, oif, mark: 0, local: false, v6: None };
            crate::nfnl::eval(0, crate::nfnl::H_POST, &mut m).1 != crate::nfnl::Nat::Masq
        }
    };
    let cmac: [u8; 6] = f[6..12].try_into().unwrap();
    let rport = if p.proto == PROTO_ICMP { 0 } else { p.dport };
    let mut st = FWD.lock();
    USED.store(true, Ordering::Relaxed);
    sweep(&mut st, now);
    let found = st.ct.iter_mut().find(|e| e.kind == 0 && e.proto == p.proto && e.br == br && e.port == port && e.cip == p.src && e.cport == p.sport
        && e.rip == p.dst && e.rport == rport).map(|e| { e.last = now; e.cmac = cmac; (e.nat, e.replied, e.nicip) });
    let (nat, snat) = match found {
        Some((n, rep, sip)) => {
            // Established flows traverse the filter hook too, so a rule added mid-flow bites.
            if crate::nfnl::has_tables(0) {
                let mut oif = [0u8; 16]; oif[..4].copy_from_slice(b"eth0");
                let mut m = crate::nfnl::Pkt { rel: false, est: rep, l4: p.proto, src: p.src, dst: p.dst, sport: p.sport, dport: rport, oif, mark: 0, local: false, v6: None };
                if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { stat(S_ING_NFT); return; }
            }
            (n, sip)
        }
        None => {
            // A TCP flow starts with a bare SYN only.
            if p.proto == PROTO_TCP && p.flags & 0x12 != 0x02 { return; }
            drop(st);
            // With a nat/postrouting chain loaded, outbound flows need FORWARD to accept and a masquerade
            // verdict (netavark: an internal network has none, so a leaked ip_forward=1 cannot SNAT it).
            if nomasq || crate::nfnl::has_hook(0, crate::nfnl::H_POST) {
                let mut oif = [0u8; 16]; oif[..4].copy_from_slice(b"eth0");
                let mut m = crate::nfnl::Pkt { rel: false, est: false, l4: p.proto, src: p.src, dst: p.dst, sport: p.sport, dport: rport, oif, mark: 0, local: false, v6: None };
                if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { stat(S_ING_NFT); return; }
            }
            if nomasq {
                // Identity entry: conntrack tracks the flow though nothing is rewritten (nicip = container address,
                // nat = container port), so replies and ICMP errors match it like a masqueraded one.
                let mut g = FWD.lock();
                if g.ct.len() >= CT_CAP { return; }
                g.ct.push(Ct { proto: p.proto, nat: p.sport, cip: p.src, cport: p.sport, cmac, br, port, bmac, rip: p.dst,
                    rport, nic, nicip: p.src, last: now, replied: false, tst: 0, fins: 0,
                    kind: 0, tip: [0; 4], tport: 0, tbr: crate::vifsel::NO_PEER, tps: crate::vifsel::NO_PEER, tmac: [0; 6], tbmac: [0; 6] });
                st = g;
                stat(S_NONAT);
                (p.sport, p.src)
            } else {
            let mut pick = None;
            let claimed = if crate::nfnl::has_tables(0) { crate::nfnl::dnat_ports(0) } else { Vec::new() };
            for _ in 0..16 {
                let mut g = FWD.lock();
                if g.ct.len() >= CT_CAP { return; }
                let mut cand = None;
                for _ in 0..NAT_SPAN {
                    let c = NAT_BASE + g.next % NAT_SPAN;
                    g.next = g.next.wrapping_add(1);
                    if !g.ct.iter().any(|e| e.nat == c && e.proto == p.proto) && !claimed.iter().any(|&(a, b)| c >= a && c <= b) { cand = Some(c); break; }
                }
                let Some(c) = cand else { return };
                // Reserve first, then ask the host's port table: a bind racing with us
                // sees the entry (bind_conflict) or we see its socket.
                g.ct.push(Ct { proto: p.proto, nat: c, cip: p.src, cport: p.sport, cmac, br, port, bmac, rip: p.dst,
                    rport, nic, nicip, last: now, replied: false, tst: 0, fins: 0,
                    kind: 0, tip: [0; 4], tport: 0, tbr: crate::vifsel::NO_PEER, tps: crate::vifsel::NO_PEER, tmac: [0; 6], tbmac: [0; 6] });
                drop(g);
                if p.proto == PROTO_ICMP || !crate::host_port_busy(p.proto == PROTO_TCP, c) { pick = Some(c); break; }
                let mut g = FWD.lock();
                if let Some(i) = g.ct.iter().position(|e| e.nat == c && e.proto == p.proto && e.cip == p.src && e.cport == p.sport) { g.ct.swap_remove(i); }
            }
            let Some(n) = pick else { stat(S_ING_NOPICK); return };
            st = FWD.lock();
            (n, nicip)
            }
        }
    };
    if p.proto == PROTO_TCP {
        if let Some(e) = st.ct.iter_mut().find(|e| e.nat == nat && e.proto == PROTO_TCP && e.kind == 0 && e.cip == p.src && e.cport == p.sport) { tcp_step(e, p.flags, true); }
    }
    // Rewrite: TTL, source address, source port / echo id.
    f[22] -= 1;
    f[26..30].copy_from_slice(&snat);
    let o = 14 + p.ihl;
    if p.proto != PROTO_ICMP { f[o..o + 2].copy_from_slice(&nat.to_be_bytes()); }
    else { f[o + 4..o + 6].copy_from_slice(&nat.to_be_bytes()); }
    finish(&mut f, &p);
    f[6..12].copy_from_slice(&nicmac);
    emit_nic(&mut st, now, nic, nh, nicip, nicmac, f);
    drop(st);
    drivers::netdev::kick();
}

fn emit_nic(st: &mut St, now: u64, nic: u8, nh: [u8; 4], nicip: [u8; 4], nicmac: [u8; 6], mut f: Vec<u8>) {
    stat(S_EMIT);
    match st.arp.iter().find(|a| a.nic == nic && a.ip == nh && now.saturating_sub(a.t) < ARP_NS).map(|a| a.mac) {
        Some(m) => { stat(S_ARP_HIT); f[0..6].copy_from_slice(&m); push_tx(st, nic, f); }
        None => {
            stat(S_ARP_MISS);
            let first = match st.pend.iter_mut().find(|q| q.nic == nic && q.ip == nh) {
                Some(q) => { if q.frames.len() < PEND_FRAMES { q.frames.push(f); } false }
                None => { st.pend.push(Pend { nic, ip: nh, t: now, frames: alloc::vec![f] }); true }
            };
            if first { push_tx(st, nic, arp_request(nicmac, nicip, nh)); }
        }
    }
}

pub(crate) fn push_tx(st: &mut St, nic: u8, f: Vec<u8>) {
    if st.txq.len() >= 256 { return; }
    st.txq.push_back((nic, f));
    TXQ_N.fetch_add(1, Ordering::Relaxed);
}

fn arp_request(mac: [u8; 6], ip: [u8; 4], target: [u8; 4]) -> Vec<u8> {
    let mut r = alloc::vec![0u8; 42];
    r[0..6].copy_from_slice(&[0xff; 6]);
    r[6..12].copy_from_slice(&mac);
    r[12..14].copy_from_slice(&[0x08, 0x06]);
    r[14..20].copy_from_slice(&[0, 1, 8, 0, 6, 4]);
    r[20..22].copy_from_slice(&[0, 1]);
    r[22..28].copy_from_slice(&mac);
    r[28..32].copy_from_slice(&ip);
    r[38..42].copy_from_slice(&target);
    r
}

pub(crate) fn ct4_len(st: &St) -> usize { st.ct.len() }
pub(crate) fn v4_nat_busy(st: &St, c: u16, proto: u8) -> bool { st.ct.iter().any(|e| e.nat == c && e.proto == proto) }
pub(crate) fn sweep_pub(st: &mut St, now: u64) { sweep(st, now) }
fn sweep(st: &mut St, now: u64) {
    if now.saturating_sub(st.swept) < 1_000_000_000 { return; }
    st.swept = now;
    st.ct.retain(|e| now.saturating_sub(e.last) < ct_ttl(e));
    let pn = st.pend.iter().filter(|q| now.saturating_sub(q.t) >= PEND_NS).count();
    if pn > 0 { STAT[S_PEND_EXP].fetch_add(pn, Ordering::Relaxed); }
    st.pend.retain(|q| now.saturating_sub(q.t) < PEND_NS);
    st.arp.retain(|a| now.saturating_sub(a.t) < ARP_NS);
    st.lrn.retain(|l| now.saturating_sub(l.t) < ARP_NS);
    expire_park(st, now);
    crate::fwd6::sweep6(st, now);
}

// ---- NIC side --------------------------------------------------------------------

/// NIC rx hook, before smoltcp. Learns ARP and un-NATs replies of forwarded
/// flows. True = the frame was taken and must not reach the NIC's stack.
pub(crate) fn nic_rx(nic: u8, buf: &[u8]) -> bool {
    if buf.len() >= 54 && buf[12] == 0x86 && buf[13] == 0xdd { return crate::fwd6::nic_rx(nic, buf); }
    if !USED.load(Ordering::Relaxed) || buf.len() < 34 { return false; }
    if buf[12] == 0x08 && buf[13] == 0x06 { arp_learn(nic, buf); return false; }
    if buf[12] == 0x08 && buf[13] == 0x00 && buf[23] == PROTO_ICMP && icmp_error_type(buf) {
        stat(S_ICMPERR_RX);
        return icmp_error_rx(nic, buf);
    }
    if buf[12] != 0x08 || buf[13] != 0x00 || (buf[23] != PROTO_UDP && buf[23] != PROTO_ICMP && buf[23] != PROTO_TCP) { return false; }
    if buf[23] == PROTO_UDP { stat(S_NICRX_UDP); }
    let mut st = FWD.lock();
    let any_nft = st.ct.is_empty() && crate::nfnl::has_tables(0);
    if st.ct.is_empty() && !any_nft { return false; }
    let mut f = buf.to_vec();
    let Some(p) = parse(&mut f, buf[23] == PROTO_ICMP) else { return false };
    let now = sched::monotonic_ns();
    let key = |e: &Ct| e.nic == nic && e.proto == p.proto && e.nicip == p.dst && e.nat == p.dport
        && e.rip == p.src && (p.proto == PROTO_ICMP || e.rport == p.sport) && e.kind < 2;
    if !st.ct.iter().any(|e| key(e)) {
        match new_in(&mut st, nic, &p, now, buf) { 0 => return false, 1 => return true, _ => {} }
    }
    let Some(e) = st.ct.iter_mut().find(|e| key(e)) else { if p.proto == PROTO_UDP { stat(S_NICRX_MISS); } return false };
    if p.proto == PROTO_UDP { stat(S_NICRX_HIT); }
    e.last = now; e.replied = true;
    if p.proto == PROTO_TCP { tcp_step(e, p.flags, e.kind == 1); }
    let (cip, cport, cmac, bmac, br, port) = (e.cip, e.cport, e.cmac, e.bmac, e.br, e.port);
    if crate::nfnl::has_tables(0) {
        let mut m = crate::nfnl::Pkt { rel: false, est: true, l4: p.proto, src: p.src, dst: cip, sport: p.sport, dport: cport, oif: br_name(br), mark: 0, local: false, v6: None };
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { stat(S_ING_NFT); return true; }
    }
    if f[22] <= 1 { return true; }
    f[22] -= 1;
    f[30..34].copy_from_slice(&cip);
    let o = 14 + p.ihl;
    if p.proto != PROTO_ICMP { f[o + 2..o + 4].copy_from_slice(&cport.to_be_bytes()); }
    else { f[o + 4..o + 6].copy_from_slice(&cport.to_be_bytes()); }
    finish(&mut f, &p);
    f[0..6].copy_from_slice(&cmac);
    f[6..12].copy_from_slice(&bmac);
    if st.inq.len() < 256 { st.inq.push_back((br, port, f)); INQ_N.fetch_add(1, Ordering::Relaxed); }
    drop(st);
    drivers::netdev::kick();
    true
}

/// True for an ICMP destination-unreachable / time-exceeded / parameter-problem
/// message in an IPv4 frame (the ones that quote the datagram that caused them).
fn icmp_error_type(b: &[u8]) -> bool {
    let ihl = (b[14] & 15) as usize * 4;
    b.len() >= 14 + ihl + 1 && matches!(b[14 + ihl], 3 | 11 | 12)
}

/// Un-NAT an ICMP error that quotes a forwarded flow (RFC 5508): the quoted
/// header names NIC address + NAT port as the source, which is looked up in
/// conntrack and rewritten to the container, as are the outer destination and
/// both checksums. True = taken (also when dropped); false = not ours.
fn icmp_error_rx(nic: u8, buf: &[u8]) -> bool {
    let ihl = (buf[14] & 15) as usize * 4;
    let tot = u16::from_be_bytes([buf[16], buf[17]]) as usize;
    if ihl < 20 || tot < ihl + 8 + 20 || 14 + tot > buf.len() { return rej(0); }
    if u16::from_be_bytes([buf[20], buf[21]]) & 0x3fff != 0 { return rej(1); }
    let i = 14 + ihl + 8; // quoted IP header
    if buf[i] >> 4 != 4 { return rej(2); }
    let iihl = (buf[i] & 15) as usize * 4;
    if iihl < 20 || 14 + tot < i + iihl + 8 { return rej(2); }
    let proto = buf[i + 9];
    if proto != PROTO_UDP && proto != PROTO_TCP && proto != PROTO_ICMP { return rej(3); }
    let qsrc = [buf[i + 12], buf[i + 13], buf[i + 14], buf[i + 15]];
    let qdst = [buf[i + 16], buf[i + 17], buf[i + 18], buf[i + 19]];
    let l = i + iihl;
    let (qsport, qdport) = if proto == PROTO_ICMP {
        let id = u16::from_be_bytes([buf[l + 4], buf[l + 5]]);
        (id, 0)
    } else {
        (u16::from_be_bytes([buf[l], buf[l + 1]]), u16::from_be_bytes([buf[l + 2], buf[l + 3]]))
    };
    let mut st = FWD.lock();
    let now = sched::monotonic_ns();
    let Some(e) = st.ct.iter_mut().find(|e| e.nic == nic && e.proto == proto && e.nicip == qsrc && e.nat == qsport
        && e.rip == qdst && (proto == PROTO_ICMP || e.rport == qdport)) else {
        LASTQ[0].store((u32::from_be_bytes(qsrc) as u64) << 32 | (qsport as u64) << 16 | proto as u64, Ordering::Relaxed);
        LASTQ[1].store((u32::from_be_bytes(qdst) as u64) << 32 | (qdport as u64) << 16, Ordering::Relaxed);
        return rej(4);
    };
    if buf[30..34] != e.nicip { return rej(5); }
    stat(21);
    e.last = now;
    let (cip, cport, cmac, bmac, br, port) = (e.cip, e.cport, e.cmac, e.bmac, e.br, e.port);
    // ct state related: the error quotes a tracked flow; the FORWARD hook sees it like any forwarded packet.
    if crate::nfnl::has_tables(0) {
        let mut m = crate::nfnl::Pkt { rel: true, est: true, l4: PROTO_ICMP, src: [buf[26], buf[27], buf[28], buf[29]], dst: cip, sport: 0, dport: 0, oif: br_name(br), mark: 0, local: false, v6: None };
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { stat(S_ING_NFT); return true; }
    }
    let mut f = buf[..14 + tot].to_vec();
    if f[22] <= 1 { return true; }
    f[22] -= 1;
    f[30..34].copy_from_slice(&cip);
    f[i + 12..i + 16].copy_from_slice(&cip);
    if proto == PROTO_ICMP { f[l + 4..l + 6].copy_from_slice(&cport.to_be_bytes()); }
    else { f[l..l + 2].copy_from_slice(&cport.to_be_bytes()); }
    // Quoted IP header checksum, then the ICMP message, then the outer header.
    f[i + 10] = 0; f[i + 11] = 0;
    let c = fold(sum(&f[i..i + iihl], 0));
    f[i + 10..i + 12].copy_from_slice(&c.to_be_bytes());
    icmp_csum(&mut f, ihl, tot - ihl);
    ip_csum(&mut f, ihl);
    f[0..6].copy_from_slice(&cmac);
    f[6..12].copy_from_slice(&bmac);
    if st.inq.len() < 256 { st.inq.push_back((br, port, f)); INQ_N.fetch_add(1, Ordering::Relaxed); }
    drop(st);
    drivers::netdev::kick();
    true
}

/// An ICMP error from a hairpin target (or a hop in front of it) quoting the DNATed packet it received: it is
/// un-NATed like any related error of a tracked flow (outer destination, quoted source and destination),
/// run through FORWARD as `ct state related` and handed to the client. True = taken (also when dropped).
fn hair_icmp_err(br: Sid, buf: &[u8]) -> bool {
    let ihl = (buf[14] & 15) as usize * 4;
    let tot = u16::from_be_bytes([buf[16], buf[17]]) as usize;
    if ihl < 20 || tot < ihl + 8 + 20 || 14 + tot > buf.len() { return false; }
    let i = 14 + ihl + 8;
    if buf[i] >> 4 != 4 { return false; }
    let iihl = (buf[i] & 15) as usize * 4;
    let proto = buf[i + 9];
    if iihl < 20 || 14 + tot < i + iihl + 8 || (proto != PROTO_UDP && proto != PROTO_TCP) { return false; }
    let qsrc = [buf[i + 12], buf[i + 13], buf[i + 14], buf[i + 15]];
    let qdst = [buf[i + 16], buf[i + 17], buf[i + 18], buf[i + 19]];
    let l = i + iihl;
    let (qsp, qdp) = (u16::from_be_bytes([buf[l], buf[l + 1]]), u16::from_be_bytes([buf[l + 2], buf[l + 3]]));
    let odst = [buf[30], buf[31], buf[32], buf[33]];
    let osrc = [buf[26], buf[27], buf[28], buf[29]];
    let mut st = FWD.lock();
    let Some(e) = st.ct.iter().find(|e| (e.kind == 2 || e.kind == 3) && e.proto == proto && e.tbr == br && e.tip == qdst && e.tport == qdp
        && if e.kind == 2 { e.nicip == qsrc && e.nat == qsp && odst == e.nicip } else { e.cip == qsrc && e.cport == qsp && odst == e.cip }) else { return false };
    let (cip, cport, cmac, bmac, cbr, cport_sid, rip, rport, tip) = (e.cip, e.cport, e.cmac, e.bmac, e.br, e.port, e.rip, e.rport, e.tip);
    if crate::nfnl::has_tables(0) {
        let mut m = crate::nfnl::Pkt { rel: true, est: true, l4: PROTO_ICMP, src: osrc, dst: cip, sport: 0, dport: 0, oif: br_name(cbr), mark: 0, local: false, v6: None };
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { stat(S_ING_NFT); return true; }
    }
    let mut f = buf[..14 + tot].to_vec();
    if f[22] <= 1 { return true; }
    f[22] -= 1;
    f[30..34].copy_from_slice(&cip);
    if osrc == tip { f[26..30].copy_from_slice(&rip); }
    f[i + 12..i + 16].copy_from_slice(&cip);
    f[i + 16..i + 20].copy_from_slice(&rip);
    f[l..l + 2].copy_from_slice(&cport.to_be_bytes());
    f[l + 2..l + 4].copy_from_slice(&rport.to_be_bytes());
    f[i + 10] = 0; f[i + 11] = 0;
    let c = fold(sum(&f[i..i + iihl], 0));
    f[i + 10..i + 12].copy_from_slice(&c.to_be_bytes());
    icmp_csum(&mut f, ihl, tot - ihl);
    ip_csum(&mut f, ihl);
    f[0..6].copy_from_slice(&cmac);
    f[6..12].copy_from_slice(&bmac);
    if st.inq.len() < 256 { st.inq.push_back((cbr, cport_sid, f)); INQ_N.fetch_add(1, Ordering::Relaxed); }
    drop(st);
    drivers::netdev::kick();
    true
}

fn arp_learn(nic: u8, b: &[u8]) {
    if b.len() < 42 || b[14..20] != [0, 1, 8, 0, 6, 4] || (b[21] != 1 && b[21] != 2) { return; }
    let mac: [u8; 6] = b[22..28].try_into().unwrap();
    let ip: [u8; 4] = b[28..32].try_into().unwrap();
    if mac[0] & 1 != 0 || ip == [0; 4] { return; }
    // Only an on-link neighbour of that NIC is cached.
    let Some(cfg) = crate::iface::rt(nic).cfg else { return };
    if !cfg.on_link(ip) { return; }
    let now = sched::monotonic_ns();
    stat(S_ARP_LEARN);
    let mut st = FWD.lock();
    if let Some(a) = st.arp.iter_mut().find(|a| a.nic == nic && a.ip == ip) { a.mac = mac; a.t = now; }
    else {
        if st.arp.len() >= ARP_CAP { st.arp.remove(0); }
        st.arp.push(Arp { nic, ip, mac, t: now });
    }
    if let Some(i) = st.pend.iter().position(|q| q.nic == nic && q.ip == ip) {
        let q = st.pend.remove(i);
        STAT[S_ARP_FLUSH].fetch_add(q.frames.len(), Ordering::Relaxed);
        for mut f in q.frames { f[0..6].copy_from_slice(&mac); push_tx(&mut st, nic, f); }
        drop(st);
        drivers::netdev::kick();
    }
}

/// Frames the forwarder wants on the wire of NIC `nic` (NIC pass).
pub(crate) fn take_txq(nic: u8) -> Vec<Vec<u8>> {
    if TXQ_N.load(Ordering::Relaxed) == 0 { return Vec::new(); }
    let mut st = FWD.lock();
    let mut out = Vec::new();
    let mut i = 0;
    while i < st.txq.len() {
        if st.txq[i].0 == nic { out.push(st.txq.remove(i).unwrap().1); TXQ_N.fetch_sub(1, Ordering::Relaxed); } else { i += 1; }
    }
    out
}

/// Un-NATed replies for the bridge switch (after the NIC pass).
pub(crate) fn drain_in() {
    if USED.load(Ordering::Relaxed) {
        let mut st = FWD.lock(); sweep(&mut st, sched::monotonic_ns()); // expiry timer
        let nr: Vec<(u8, Vec<u8>)> = st.nreplay.drain(..).collect();
        let ex: Vec<(Sid, Sid, Vec<u8>)> = st.expired.drain(..).collect();
        let ex6: Vec<(Sid, Sid, Vec<u8>)> = st.expired6.drain(..).collect();
        drop(st);
        for (sbr, port, f) in ex { unreach4(sbr, port, &f); }
        for (sbr, port, f) in ex6 { crate::fwd6::unreach6(sbr, port, &f); }
        for (nic, f) in nr { nic_rx(nic, &f); } // NIC frames released by a learned neighbour
    }
    if INQ_N.load(Ordering::Relaxed) == 0 { return; }
    let q: Vec<(Sid, Sid, Vec<u8>)> = { let mut st = FWD.lock(); INQ_N.store(0, Ordering::Relaxed); st.inq.drain(..).collect() };
    for (br, port, f) in q {
        if port == crate::vifsel::NO_PEER { crate::l2::forward(br, br, alloc::vec![f]); } else { crate::l2::deliver(br, port, &f); }
    }
}

// ---- nftables NAT on new flows (podnat L2) ----------------------------------------

/// Learn the IP -> (MAC, port) of whoever sent an IPv4 or ARP frame into bridge `br`.
fn learn(br: Sid, port: Sid, bmac: [u8; 6], f: &[u8]) -> Option<[u8; 4]> {
    if f.len() < 34 { return None; }
    let (ip, mac): ([u8; 4], [u8; 6]) = if f[12] == 0x08 && f[13] == 0x06 && f.len() >= 42 && f[14..20] == [0, 1, 8, 0, 6, 4] {
        (f[28..32].try_into().unwrap(), f[22..28].try_into().unwrap())
    } else if f[12] == 0x08 && f[13] == 0 {
        (f[26..30].try_into().unwrap(), f[6..12].try_into().unwrap())
    } else { return None };
    if ip == [0; 4] || mac[0] & 1 != 0 || mac == bmac { return None; }
    let now = sched::monotonic_ns();
    let mut st = FWD.lock();
    if let Some(l) = st.lrn.iter_mut().find(|l| l.br == br && l.ip == ip) { l.mac = mac; l.port = port; l.t = now; }
    else { if st.lrn.len() >= 256 { st.lrn.remove(0); } st.lrn.push(Lrn { br, ip, mac, port, t: now }); }
    Some(ip)
}

/// Move parked frames older than the timeout to `expired` (ICMP host unreachable is sent from `divert`).
fn expire_park(st: &mut St, now: u64) {
    let mut i = 0;
    while i < st.park.len() {
        if now.saturating_sub(st.park[i].t) >= PEND_NS * 2 {
            let q = st.park.remove(i);
            if st.expired.len() < PARK_TOTAL { st.expired.push((q.sbr, q.port, q.frame)); }
        } else { i += 1; }
    }
    st.npark.retain(|q| now.saturating_sub(q.t) < PEND_NS * 2);
    // Resend the solicitation up to three times in all, spread over the park timeout (Linux mcast_solicit = 3).
    let mut i = 0;
    while i < st.probe.len() {
        let age = now.saturating_sub(st.probe[i].t);
        if age >= PEND_NS * 2 { st.probe.remove(i); continue; }
        if (st.probe[i].sent as u64) < 3 && age >= st.probe[i].sent as u64 * PEND_NS * 2 / 3 {
            let (br, f) = (st.probe[i].br, st.probe[i].frame.clone());
            st.probe[i].sent += 1;
            st.inq.push_back((br, crate::vifsel::NO_PEER, f)); INQ_N.fetch_add(1, Ordering::Relaxed);
        }
        i += 1;
    }
}

/// Remember an ARP request / NS sent for a parked frame so it is resent until the park times out.
pub(crate) fn add_probe(st: &mut St, br: Sid, ip: [u8; 16], frame: &[u8]) {
    if st.probe.iter().any(|q| q.br == br && q.ip == ip) || st.probe.len() >= PARK_TOTAL { return; }
    st.probe.push(Probe { br, ip, t: sched::monotonic_ns(), sent: 1, frame: frame.to_vec() });
}

/// Park a NIC frame whose container next hop has no learned MAC; `take_npark` releases it.
pub(crate) fn park_nic(st: &mut St, br: Sid, ip: [u8; 16], v6: bool, nic: u8, now: u64, frame: &[u8]) {
    if st.npark.iter().filter(|q| q.br == br && q.ip == ip && q.v6 == v6).count() < PARK_PER_IP && st.npark.len() < PARK_TOTAL {
        stat(S_PARKED);
        st.npark.push(NPark { br, ip, v6, nic, t: now, frame: frame.to_vec() });
    }
}

/// Queue the NIC frames waiting for `ip` on `br` for replay through `nic_rx` (see `drain_in`).
pub(crate) fn take_npark(st: &mut St, br: Sid, ip: [u8; 16], v6: bool, now: u64) {
    st.npark.retain(|q| now.saturating_sub(q.t) < PEND_NS * 2);
    st.probe.retain(|q| !(q.br == br && q.ip == ip));
    let mut i = 0;
    while i < st.npark.len() {
        if st.npark[i].br == br && st.npark[i].ip == ip && st.npark[i].v6 == v6 { let q = st.npark.remove(i); st.nreplay.push((q.nic, q.frame)); } else { i += 1; }
    }
}

/// Frames parked on a learn miss for `ip` on `br`, oldest first: (source bridge, source port, frame).
fn unpark(br: Sid, ip: [u8; 4]) -> Vec<(Sid, Sid, Vec<u8>)> {
    let now = sched::monotonic_ns();
    let mut st = FWD.lock();
    expire_park(&mut st, now);
    let mut wide = [0u8; 16]; wide[..4].copy_from_slice(&ip);
    take_npark(&mut st, br, wide, false, now);
    let mut out = Vec::new();
    let mut i = 0;
    while i < st.park.len() {
        if st.park[i].br == br && st.park[i].ip == ip { let q = st.park.remove(i); out.push((q.sbr, q.port, q.frame)); } else { i += 1; }
    }
    drop(st);
    if !out.is_empty() { drivers::netdev::kick(); }
    out
}

/// Replay released frames: ones that arrived on this bridge go back to the front of its work queue,
/// others are handed to their own bridge's next `divert` pass.
pub(crate) fn requeue(br: Sid, list: Vec<(Sid, Sid, Vec<u8>)>, work: &mut VecDeque<(Sid, Vec<u8>)>) {
    let mut other = false;
    for (sbr, port, f) in list.into_iter().rev() {
        if sbr == br { work.push_front((port, f)); } else { FWD.lock().replay.push((sbr, port, f)); other = true; }
    }
    if other { drivers::netdev::kick(); }
}

/// A parked packet whose next hop never resolved: Linux answers the sender with ICMP host unreachable.
fn unreach4(sbr: Sid, port: Sid, f: &[u8]) { err4(sbr, port, f, 3, 1) }

/// Hop limit exhausted on `f` (a container's packet): ICMP time exceeded back to it from the bridge address.
fn ttl_exceeded4(sbr: Sid, port: Sid, f: &[u8]) { err4(sbr, port, f, 11, 0) }

/// A locally generated ICMP error about the forwarded packet `f`. It leaves through the OUTPUT hook as
/// `ct state related` (the error quotes a tracked packet), exactly like Linux's icmp_send.
fn err4(sbr: Sid, port: Sid, f: &[u8], ty: u8, code: u8) {
    if f.len() < 34 { return; }
    let info = {
        let v = crate::vif::VIFS.lock();
        v[sbr as usize].as_ref().filter(|b| b.kind == VifKind::Bridge && b.netns == 0).map(|b| (b.mac, b.addrs.clone()))
    };
    let Some((bmac, addrs)) = info else { return };
    let src = [f[26], f[27], f[28], f[29]];
    let a = addrs.iter().find(|&&(a, p)| a != src && u32::from_be_bytes(a) & mask(p) == u32::from_be_bytes(src) & mask(p)).map(|x| x.0);
    let Some(a) = a else { return };
    if crate::nfnl::has_tables(0) {
        let mut m = crate::nfnl::Pkt { rel: true, est: true, l4: PROTO_ICMP, src: a, dst: src, sport: 0, dport: 0, oif: br_name(sbr), mark: 0, local: false, v6: None };
        if !crate::nfnl::eval(0, crate::nfnl::H_OUT, &mut m).0 { stat(S_ING_NFT); return; }
    }
    if let Some(r) = host_icmp_err(bmac, a, f, ty, code) {
        let mut st = FWD.lock();
        st.inq.push_back((sbr, port, r)); INQ_N.fetch_add(1, Ordering::Relaxed);
        drop(st);
        drivers::netdev::kick();
    }
}

/// ICMP error (`ty`/`code`) for `f` (an Ethernet+IPv4 frame), from the bridge address `baddr`.
fn host_icmp_err(bmac: [u8; 6], baddr: [u8; 4], f: &[u8], ty: u8, code: u8) -> Option<Vec<u8>> {
    let ihl = ((f[14] & 0xf) as usize) * 4;
    if ihl < 20 || f.len() < 14 + ihl + 8 || (f[23] == PROTO_ICMP && icmp_error_type(f)) { return None; }
    let q = &f[14..14 + ihl + 8];
    let mut r = alloc::vec![0u8; 14 + 20 + 8 + q.len()];
    r[0..6].copy_from_slice(&f[6..12]);
    r[6..12].copy_from_slice(&bmac);
    r[12..14].copy_from_slice(&[0x08, 0x00]);
    r[14] = 0x45; r[15] = 0xc0;
    let tl = (20 + 8 + q.len()) as u16;
    r[16..18].copy_from_slice(&tl.to_be_bytes());
    r[22] = 64; r[23] = PROTO_ICMP;
    r[26..30].copy_from_slice(&baddr);
    r[30..34].copy_from_slice(&f[26..30]);
    ip_csum(&mut r, 20);
    r[34] = ty; r[35] = code;
    r[42..].copy_from_slice(q);
    let c = fold(sum(&r[34..], 0));
    r[36..38].copy_from_slice(&c.to_be_bytes());
    Some(r)
}

struct Tgt { br: Sid, mac: [u8; 6], name: [u8; 16], addr: [u8; 4] }

/// The ns-0 bridge whose subnet holds `ip`.
fn bridge_for(ip: [u8; 4]) -> Option<Tgt> {
    let v = crate::vif::VIFS.lock();
    for (i, x) in v.iter().enumerate() {
        let Some(b) = x else { continue };
        if b.kind != VifKind::Bridge || b.netns != 0 { continue; }
        if let Some(&(a, p)) = b.addrs.iter().find(|&&(a, p)| a != ip && u32::from_be_bytes(a) & mask(p) == u32::from_be_bytes(ip) & mask(p)) {
            let _ = p;
            let mut name = [0u8; 16];
            let n = b.name.len().min(16);
            name[..n].copy_from_slice(&b.name[..n]);
            return Some(Tgt { br: i as Sid, mac: b.mac, name, addr: a });
        }
    }
    None
}

/// Zero-padded interface name of ns-0 bridge `br` (for `meta oifname`).
pub(crate) fn br_name(br: Sid) -> [u8; 16] {
    let v = crate::vif::VIFS.lock();
    let mut o = [0u8; 16];
    if let Some(b) = v.get(br as usize).and_then(|x| x.as_ref()) { let n = b.name.len().min(16); o[..n].copy_from_slice(&b.name[..n]); }
    o
}

fn rewrite_dst(f: &mut Vec<u8>, p: &Pkt, ip: [u8; 4], port: u16) {
    f[30..34].copy_from_slice(&ip);
    let o = 14 + p.ihl;
    f[o + 2..o + 4].copy_from_slice(&port.to_be_bytes());
}

fn is_new_flow(p: &Pkt) -> bool { p.proto == PROTO_UDP || (p.proto == PROTO_TCP && p.flags & 0x12 == 0x02) }

/// A new packet from the NIC to an address of this host: PREROUTING DNAT, then FORWARD must accept.
/// Creates an In entry. 0 = not ours (pass to the stack), 1 = dropped, 2 = entry created.
fn new_in(st: &mut St, nic: u8, p: &Pkt, now: u64, frame: &[u8]) -> u8 {
    if !is_new_flow(p) || !crate::iface::is_nic_addr(p.dst) { return 0; }
    let mut m = crate::nfnl::Pkt { rel: false, est: false, l4: p.proto, src: p.src, dst: p.dst, sport: p.sport, dport: p.dport, oif: [0; 16], mark: 0, local: true, v6: None };
    let (acc, nat) = crate::nfnl::eval(0, crate::nfnl::H_PRE, &mut m);
    let crate::nfnl::Nat::Dnat(cip, cport) = nat else { return if acc { 0 } else { 1 } };
    let cport = if cport == 0 { p.dport } else { cport };
    let Some(t) = bridge_for(cip) else { return 1 };
    m.dst = cip; m.dport = cport; m.oif = t.name;
    if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return 1; }
    if st.ct.len() >= CT_CAP || st.ct.iter().filter(|e| e.kind == 1).count() >= CT_CAP / 2 { return 1; }
    let Some((cmac, port)) = st.lrn.iter().find(|l| l.br == t.br && l.ip == cip).map(|l| (l.mac, l.port)) else {
        // Unknown MAC: ask for it and hold the frame; the ARP reply replays it (else the peer's retransmit finds it).
        let mut wide = [0u8; 16]; wide[..4].copy_from_slice(&cip);
        park_nic(st, t.br, wide, false, nic, now, frame);
        let r = arp_request(t.mac, t.addr, cip);
        add_probe(st, t.br, wide, &r);
        st.inq.push_back((t.br, crate::vifsel::NO_PEER, r)); INQ_N.fetch_add(1, Ordering::Relaxed);
        return 1;
    };
    st.ct.push(Ct { proto: p.proto, nat: p.dport, cip, cport, cmac, br: t.br, port, bmac: t.mac, rip: p.src, rport: p.sport,
        nic, nicip: p.dst, last: now, replied: false, tst: 0, fins: 0,
        kind: 1, tip: [0; 4], tport: 0, tbr: crate::vifsel::NO_PEER, tps: crate::vifsel::NO_PEER, tmac: [0; 6], tbmac: [0; 6] });
    2
}

/// A container's packet answering an In flow: source becomes nicip:H, out of the NIC.
fn reply_in(br: Sid, port: Sid, f: &mut Vec<u8>, p: &Pkt) -> bool {
    if !USED.load(Ordering::Relaxed) && !crate::nfnl::has_tables(0) { return false; }
    let mut st = FWD.lock();
    let now = sched::monotonic_ns();
    let Some(e) = st.ct.iter_mut().find(|e| e.kind == 1 && e.proto == p.proto && e.br == br && e.port == port
        && e.cip == p.src && e.cport == p.sport && e.rip == p.dst && e.rport == p.dport) else { return false };
    e.last = now; e.replied = true;
    if p.proto == PROTO_TCP { tcp_step(e, p.flags, false); }
    let (nic, nicip, nat) = (e.nic, e.nicip, e.nat);
    if crate::nfnl::has_tables(0) {
        let mut oif = [0u8; 16]; oif[..4].copy_from_slice(b"eth0");
        let mut m = crate::nfnl::Pkt { rel: false, est: true, l4: p.proto, src: p.src, dst: p.dst, sport: p.sport, dport: p.dport, oif, mark: 0, local: false, v6: None };
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { stat(S_ING_NFT); return true; }
    }
    if f[22] <= 1 { return true; }
    let Some((_, nh, _)) = crate::iface::nic_route(p.dst) else { return true };
    let Some(nicmac) = drivers::netdev::get(nic as usize).and_then(|n| n.mac()) else { return true };
    f[22] -= 1;
    f[26..30].copy_from_slice(&nicip);
    let o = 14 + p.ihl;
    f[o..o + 2].copy_from_slice(&nat.to_be_bytes());
    finish(f, p);
    f[6..12].copy_from_slice(&nicmac);
    USED.store(true, Ordering::Relaxed);
    emit_nic(&mut st, now, nic, nh, nicip, nicmac, core::mem::take(f));
    drop(st);
    drivers::netdev::kick();
    true
}

/// Hairpin: container -> an address of this host with a DNAT rule. The target gets the packet from the
/// egress bridge's own address (masquerade, as netavark's SETMARK rules ask) and a NAT port.
fn hair(br: Sid, port: Sid, bmac: [u8; 6], f: &mut Vec<u8>, p: &Pkt) -> bool {
    let now = sched::monotonic_ns();
    let mut st = FWD.lock();
    let mut hit = st.ct.iter().position(|e| (e.kind == 2 || e.kind == 3) && e.proto == p.proto && e.br == br && e.port == port
        && e.cip == p.src && e.cport == p.sport && e.rip == p.dst && e.rport == p.dport);
    let mut rev = false;
    if hit.is_none() {
        hit = st.ct.iter().position(|e| e.proto == p.proto && e.tbr == br && e.tip == p.src && e.tport == p.sport
            && ((e.kind == 2 && e.nicip == p.dst && e.nat == p.dport) || (e.kind == 3 && e.cip == p.dst && e.cport == p.dport)));
        rev = hit.is_some();
    }
    let existed = hit.is_some();
    if hit.is_none() {
        if !is_new_flow(p) || !host_local(p.dst) || st.ct.len() >= CT_CAP { return false; }
        let mut m = crate::nfnl::Pkt { rel: false, est: false, l4: p.proto, src: p.src, dst: p.dst, sport: p.sport, dport: p.dport, oif: [0; 16], mark: 0, local: true, v6: None };
        let (_, nat) = crate::nfnl::eval(0, crate::nfnl::H_PRE, &mut m);
        let crate::nfnl::Nat::Dnat(cip, cport) = nat else { return false };
        let cport = if cport == 0 { p.dport } else { cport };
        let Some(t) = bridge_for(cip) else { return false };
        m.dst = cip; m.dport = cport; m.oif = t.name;
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { return true; }
        // Masquerade (netavark SETMARK: same-network clients) rewrites the source; an unmarked
        // cross-network flow is DNATed and routed with the client's own address (kind 3).
        let masq = crate::nfnl::eval(0, crate::nfnl::H_POST, &mut m).1 == crate::nfnl::Nat::Masq;
        let Some((tmac, tps)) = st.lrn.iter().find(|l| l.br == t.br && l.ip == cip).map(|l| (l.mac, l.port)) else {
            if st.park.iter().filter(|q| q.br == t.br && q.ip == cip).count() < PARK_PER_IP && st.park.len() < PARK_TOTAL {
                // Hold the SYN until the ARP reply teaches us the MAC; `divert` replays it (also across bridges).
                stat(S_PARKED);
                let fr = core::mem::take(f);
                st.park.push(Park { br: t.br, sbr: br, ip: cip, t: now, port, frame: fr });
            }
            let r = arp_request(t.mac, t.addr, cip);
            { let mut w = [0u8; 16]; w[..4].copy_from_slice(&cip); add_probe(&mut st, t.br, w, &r); }
            st.inq.push_back((t.br, crate::vifsel::NO_PEER, r)); INQ_N.fetch_add(1, Ordering::Relaxed);
            drop(st);
            drivers::netdev::kick(); // else the ARP request waits for the next housekeeping tick (~450 ms)
            return true;
        };
        let claimed = crate::nfnl::dnat_ports(0);
        let mut cand = None;
        if !masq { cand = Some(0); } else {
        for _ in 0..NAT_SPAN {
            let c = NAT_BASE + st.next % NAT_SPAN;
            st.next = st.next.wrapping_add(1);
            if !st.ct.iter().any(|e| e.nat == c && e.proto == p.proto) && !claimed.iter().any(|&(a, b)| c >= a && c <= b) { cand = Some(c); break; }
        } }
        let Some(nat) = cand else { return true };
        let cmac: [u8; 6] = f[6..12].try_into().unwrap();
        st.ct.push(Ct { proto: p.proto, nat, cip: p.src, cport: p.sport, cmac, br, port, bmac, rip: p.dst, rport: p.dport,
            nic: 0, nicip: t.addr, last: now, replied: false, tst: 0, fins: 0,
            kind: if masq { 2 } else { 3 }, tip: cip, tport: cport, tbr: t.br, tps, tmac, tbmac: t.mac });
        hit = Some(st.ct.len() - 1);
    }
    USED.store(true, Ordering::Relaxed);
    let i = hit.unwrap();
    if existed && crate::nfnl::has_tables(0) {
        // Established hairpin flows traverse FORWARD too (post-DNAT addresses, the target's bridge as oif).
        let e = &st.ct[i];
        let mut m = if !rev {
            crate::nfnl::Pkt { rel: false, est: e.replied, l4: p.proto, src: p.src, dst: e.tip, sport: p.sport, dport: e.tport, oif: br_name(e.tbr), mark: 0, local: false, v6: None }
        } else {
            crate::nfnl::Pkt { rel: false, est: true, l4: p.proto, src: e.tip, dst: e.cip, sport: e.tport, dport: e.cport, oif: br_name(e.br), mark: 0, local: false, v6: None }
        };
        if !crate::nfnl::eval(0, crate::nfnl::H_FWD, &mut m).0 { stat(S_ING_NFT); return true; }
    }
    if f[22] <= 1 { return true; }
    f[22] -= 1;
    let o = 14 + p.ihl;
    let e = &mut st.ct[i];
    e.last = now;
    if p.proto == PROTO_TCP { tcp_step(e, p.flags, !rev); }
    let (dbr, dport_sid);
    if !rev {
        if e.kind == 2 {
            f[26..30].copy_from_slice(&e.nicip);
            f[o..o + 2].copy_from_slice(&e.nat.to_be_bytes());
        }
        rewrite_dst(f, p, e.tip, e.tport);
        f[0..6].copy_from_slice(&e.tmac); f[6..12].copy_from_slice(&e.tbmac);
        dbr = e.tbr; dport_sid = e.tps;
    } else {
        e.replied = true;
        f[26..30].copy_from_slice(&e.rip);
        f[o..o + 2].copy_from_slice(&e.rport.to_be_bytes());
        rewrite_dst(f, p, e.cip, e.cport);
        f[0..6].copy_from_slice(&e.cmac); f[6..12].copy_from_slice(&e.bmac);
        dbr = e.br; dport_sid = e.port;
    }
    finish(f, p);
    if st.inq.len() < 256 { st.inq.push_back((dbr, dport_sid, core::mem::take(f))); INQ_N.fetch_add(1, Ordering::Relaxed); }
    drop(st);
    drivers::netdev::kick();
    true
}
