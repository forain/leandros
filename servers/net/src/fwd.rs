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

const PROTO_ICMP: u8 = 1;
const PROTO_TCP: u8 = 6;
const PROTO_UDP: u8 = 17;
const CT_CAP: usize = 1024;
const ARP_CAP: usize = 64;
const PEND_FRAMES: usize = 4;
const NAT_BASE: u16 = 61000;
const NAT_SPAN: u16 = 4000;
const UDP_NS: u64 = 30_000_000_000;
const UDP_REPLIED_NS: u64 = 180_000_000_000;
const ICMP_NS: u64 = 30_000_000_000;
const TCP_SYN_NS: u64 = 60_000_000_000;
const TCP_EST_NS: u64 = 7_200_000_000_000;
const TCP_FIN_NS: u64 = 60_000_000_000;
const TCP_TW_NS: u64 = 10_000_000_000;
const PEND_NS: u64 = 1_000_000_000;
const ARP_NS: u64 = 300_000_000_000;

struct Ct {
    proto: u8, nat: u16,
    cip: [u8; 4], cport: u16, cmac: [u8; 6], br: Sid, port: Sid, bmac: [u8; 6],
    rip: [u8; 4], rport: u16,
    nic: u8, nicip: [u8; 4], last: u64, replied: bool,
    /// TCP lite FSM: 0 SYN sent, 1 SYN-ACK seen, 2 established, 3 one side sent FIN, 4 closing (both FINs or RST: TIME_WAIT-lite).
    tst: u8, fins: u8,
}
struct Arp { nic: u8, ip: [u8; 4], mac: [u8; 6], t: u64 }
struct Pend { nic: u8, ip: [u8; 4], t: u64, frames: Vec<Vec<u8>> }
struct St {
    ct: Vec<Ct>, arp: Vec<Arp>, pend: Vec<Pend>,
    txq: VecDeque<(u8, Vec<u8>)>, inq: VecDeque<(Sid, Sid, Vec<u8>)>,
    next: u16, swept: u64,
}
static FWD: Mutex<St> = Mutex::new(St { ct: Vec::new(), arp: Vec::new(), pend: Vec::new(),
    txq: VecDeque::new(), inq: VecDeque::new(), next: 0, swept: 0 });
/// Set at the first forwarded flow: the NIC rx hook costs one load until then.
static USED: AtomicBool = AtomicBool::new(false);
static TXQ_N: AtomicUsize = AtomicUsize::new(0);
static INQ_N: AtomicUsize = AtomicUsize::new(0);

fn ct_ttl(e: &Ct) -> u64 {
    match e.proto {
        PROTO_UDP => if e.replied { UDP_REPLIED_NS } else { UDP_NS },
        PROTO_TCP => match e.tst { 0 | 1 => TCP_SYN_NS, 2 => TCP_EST_NS, 3 => TCP_FIN_NS, _ => TCP_TW_NS },
        _ => ICMP_NS,
    }
}

/// Advance the TCP lite FSM of `e` for a segment with `flags` (`from_ctr`: sent by the container).
fn tcp_step(e: &mut Ct, flags: u8, from_ctr: bool) {
    const FIN: u8 = 1; const SYN: u8 = 2; const RST: u8 = 4; const ACK: u8 = 16;
    if flags & RST != 0 { e.tst = 4; return; }
    if flags & SYN != 0 {
        if from_ctr && flags & ACK == 0 { if e.tst >= 3 { e.tst = 0; e.fins = 0; } }
        else if !from_ctr && flags & ACK != 0 && e.tst == 0 { e.tst = 1; }
        return;
    }
    if from_ctr && e.tst == 1 && flags & ACK != 0 { e.tst = 2; }
    if flags & FIN != 0 && e.tst >= 1 && e.tst < 4 {
        e.fins |= if from_ctr { 1 } else { 2 };
        e.tst = if e.fins == 3 { 4 } else { 3 };
    }
}

/// True while a live NAT entry holds `port` of the given protocol (TCP or UDP) on the host's NIC address.
pub(crate) fn reserved(tcp: bool, port: u16) -> bool {
    if !USED.load(Ordering::Relaxed) || port < NAT_BASE || port >= NAT_BASE + NAT_SPAN { return false; }
    let proto = if tcp { PROTO_TCP } else { PROTO_UDP };
    let now = sched::monotonic_ns();
    FWD.lock().ct.iter().any(|e| e.proto == proto && e.nat == port && now.saturating_sub(e.last) < ct_ttl(e))
}

/// Live TCP NAT ports (the ones `reserved(true, _)` reports), for /proc/net/nic.
pub(crate) fn live_tcp_nat_ports(out: &mut alloc::vec::Vec<u16>) {
    let now = sched::monotonic_ns();
    for e in FWD.lock().ct.iter() {
        if e.proto == PROTO_TCP && now.saturating_sub(e.last) < ct_ttl(e) { out.push(e.nat); }
    }
}

/// Number of live conntrack entries (diagnostics).
pub(crate) fn ct_len() -> usize { FWD.lock().ct.len() }

/// Drop every entry that belongs to bridge `sid` or entered through port `sid` (it went away).
pub(crate) fn forget(sid: Sid) {
    if !USED.load(Ordering::Relaxed) { return; }
    FWD.lock().ct.retain(|e| e.br != sid && e.port != sid);
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
fn host_local(d: [u8; 4]) -> bool {
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
    let on = sched::ns::forwarding(0, &name[..n]);
    if !on { return rx; }
    let mut keep = VecDeque::with_capacity(rx.len());
    let aligned = rxsrc.len() == rx.len();
    for (i, mut f) in rx.into_iter().enumerate() {
        let port = if aligned { rxsrc[i] } else { crate::vifsel::NO_PEER };
        if f.len() < 34 || f[0..6] != bmac || f[12] != 0x08 || f[13] != 0 {
            keep.push_back(f);
            continue;
        }
        let dst = [f[30], f[31], f[32], f[33]];
        let src = [f[26], f[27], f[28], f[29]];
        // The bridge's own network (and its broadcast) stays with the stack.
        if addrs.iter().any(|&(a, p)| u32::from_be_bytes(a) & mask(p) == u32::from_be_bytes(dst) & mask(p)) {
            keep.push_back(f);
            continue;
        }
        // Anti-spoof: the source must belong to the bridge's network, and not be the bridge itself.
        let srcok = addrs.iter().any(|&(a, p)| a != src && u32::from_be_bytes(a) & mask(p) == u32::from_be_bytes(src) & mask(p));
        if !srcok || host_local(dst) { continue; }
        let Some(p) = parse(&mut f, false) else { continue };
        if port == crate::vifsel::NO_PEER || port == br { continue; }
        ingress(br, port, bmac, f, p);
    }
    keep
}

fn ingress(br: Sid, port: Sid, bmac: [u8; 6], mut f: Vec<u8>, p: Pkt) {
    if f[22] <= 1 { return; } // TTL exhausted
    let Some((nic, nh, nicip)) = crate::iface::nic_route(p.dst) else { return };
    let Some(nicmac) = drivers::netdev::get(nic as usize).and_then(|n| n.mac()) else { return };
    let now = sched::monotonic_ns();
    let cmac: [u8; 6] = f[6..12].try_into().unwrap();
    let rport = if p.proto == PROTO_ICMP { 0 } else { p.dport };
    let mut st = FWD.lock();
    USED.store(true, Ordering::Relaxed);
    sweep(&mut st, now);
    let found = st.ct.iter_mut().find(|e| e.proto == p.proto && e.br == br && e.port == port && e.cip == p.src && e.cport == p.sport
        && e.rip == p.dst && e.rport == rport).map(|e| { e.last = now; e.cmac = cmac; e.nat });
    let nat = match found {
        Some(n) => n,
        None => {
            // A TCP flow starts with a bare SYN only.
            if p.proto == PROTO_TCP && p.flags & 0x12 != 0x02 { return; }
            drop(st);
            let mut pick = None;
            for _ in 0..16 {
                let mut g = FWD.lock();
                if g.ct.len() >= CT_CAP { return; }
                let mut cand = None;
                for _ in 0..NAT_SPAN {
                    let c = NAT_BASE + g.next % NAT_SPAN;
                    g.next = g.next.wrapping_add(1);
                    if !g.ct.iter().any(|e| e.nat == c && e.proto == p.proto) { cand = Some(c); break; }
                }
                let Some(c) = cand else { return };
                // Reserve first, then ask the host's port table: a bind racing with us
                // sees the entry (bind_conflict) or we see its socket.
                g.ct.push(Ct { proto: p.proto, nat: c, cip: p.src, cport: p.sport, cmac, br, port, bmac, rip: p.dst,
                    rport, nic, nicip, last: now, replied: false, tst: 0, fins: 0 });
                drop(g);
                if p.proto == PROTO_ICMP || !crate::host_port_busy(p.proto == PROTO_TCP, c) { pick = Some(c); break; }
                let mut g = FWD.lock();
                if let Some(i) = g.ct.iter().position(|e| e.nat == c && e.proto == p.proto && e.cip == p.src && e.cport == p.sport) { g.ct.swap_remove(i); }
            }
            let Some(n) = pick else { return };
            st = FWD.lock();
            n
        }
    };
    if p.proto == PROTO_TCP {
        if let Some(e) = st.ct.iter_mut().find(|e| e.nat == nat && e.proto == PROTO_TCP) { tcp_step(e, p.flags, true); }
    }
    // Rewrite: TTL, source address, source port / echo id.
    f[22] -= 1;
    f[26..30].copy_from_slice(&nicip);
    let o = 14 + p.ihl;
    if p.proto != PROTO_ICMP { f[o..o + 2].copy_from_slice(&nat.to_be_bytes()); }
    else { f[o + 4..o + 6].copy_from_slice(&nat.to_be_bytes()); }
    finish(&mut f, &p);
    f[6..12].copy_from_slice(&nicmac);
    match st.arp.iter().find(|a| a.nic == nic && a.ip == nh && now.saturating_sub(a.t) < ARP_NS).map(|a| a.mac) {
        Some(m) => { f[0..6].copy_from_slice(&m); push_tx(&mut st, nic, f); }
        None => {
            let first = match st.pend.iter_mut().find(|q| q.nic == nic && q.ip == nh) {
                Some(q) => { if q.frames.len() < PEND_FRAMES { q.frames.push(f); } false }
                None => { st.pend.push(Pend { nic, ip: nh, t: now, frames: alloc::vec![f] }); true }
            };
            if first { push_tx(&mut st, nic, arp_request(nicmac, nicip, nh)); }
        }
    }
    drop(st);
    drivers::netdev::kick();
}

fn push_tx(st: &mut St, nic: u8, f: Vec<u8>) {
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

fn sweep(st: &mut St, now: u64) {
    if now.saturating_sub(st.swept) < 1_000_000_000 { return; }
    st.swept = now;
    st.ct.retain(|e| now.saturating_sub(e.last) < ct_ttl(e));
    st.pend.retain(|q| now.saturating_sub(q.t) < PEND_NS);
    st.arp.retain(|a| now.saturating_sub(a.t) < ARP_NS);
}

// ---- NIC side --------------------------------------------------------------------

/// NIC rx hook, before smoltcp. Learns ARP and un-NATs replies of forwarded
/// flows. True = the frame was taken and must not reach the NIC's stack.
pub(crate) fn nic_rx(nic: u8, buf: &[u8]) -> bool {
    if !USED.load(Ordering::Relaxed) || buf.len() < 34 { return false; }
    if buf[12] == 0x08 && buf[13] == 0x06 { arp_learn(nic, buf); return false; }
    if buf[12] == 0x08 && buf[13] == 0x00 && buf[23] == PROTO_ICMP && icmp_error_type(buf) {
        return icmp_error_rx(nic, buf);
    }
    if buf[12] != 0x08 || buf[13] != 0x00 || (buf[23] != PROTO_UDP && buf[23] != PROTO_ICMP && buf[23] != PROTO_TCP) { return false; }
    let mut st = FWD.lock();
    if st.ct.is_empty() { return false; }
    let mut f = buf.to_vec();
    let Some(p) = parse(&mut f, buf[23] == PROTO_ICMP) else { return false };
    let now = sched::monotonic_ns();
    let Some(e) = st.ct.iter_mut().find(|e| e.nic == nic && e.proto == p.proto && e.nicip == p.dst && e.nat == p.dport
        && e.rip == p.src && (p.proto == PROTO_ICMP || e.rport == p.sport)) else { return false };
    e.last = now; e.replied = true;
    if p.proto == PROTO_TCP { tcp_step(e, p.flags, false); }
    let (cip, cport, cmac, bmac, br, port) = (e.cip, e.cport, e.cmac, e.bmac, e.br, e.port);
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
    if ihl < 20 || tot < ihl + 8 + 20 || 14 + tot > buf.len() { return false; }
    if u16::from_be_bytes([buf[20], buf[21]]) & 0x3fff != 0 { return false; }
    let i = 14 + ihl + 8; // quoted IP header
    if buf[i] >> 4 != 4 { return false; }
    let iihl = (buf[i] & 15) as usize * 4;
    if iihl < 20 || 14 + tot < i + iihl + 8 { return false; }
    let proto = buf[i + 9];
    if proto != PROTO_UDP && proto != PROTO_TCP && proto != PROTO_ICMP { return false; }
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
        && e.rip == qdst && (proto == PROTO_ICMP || e.rport == qdport)) else { return false };
    if buf[30..34] != e.nicip { return false; }
    e.last = now;
    let (cip, cport, cmac, bmac, br, port) = (e.cip, e.cport, e.cmac, e.bmac, e.br, e.port);
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

fn arp_learn(nic: u8, b: &[u8]) {
    if b.len() < 42 || b[14..20] != [0, 1, 8, 0, 6, 4] || (b[21] != 1 && b[21] != 2) { return; }
    let mac: [u8; 6] = b[22..28].try_into().unwrap();
    let ip: [u8; 4] = b[28..32].try_into().unwrap();
    if mac[0] & 1 != 0 || ip == [0; 4] { return; }
    // Only an on-link neighbour of that NIC is cached.
    let Some(cfg) = crate::iface::rt(nic).cfg else { return };
    if !cfg.on_link(ip) { return; }
    let now = sched::monotonic_ns();
    let mut st = FWD.lock();
    if let Some(a) = st.arp.iter_mut().find(|a| a.nic == nic && a.ip == ip) { a.mac = mac; a.t = now; }
    else {
        if st.arp.len() >= ARP_CAP { st.arp.remove(0); }
        st.arp.push(Arp { nic, ip, mac, t: now });
    }
    if let Some(i) = st.pend.iter().position(|q| q.nic == nic && q.ip == ip) {
        let q = st.pend.remove(i);
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
    if USED.load(Ordering::Relaxed) { let mut st = FWD.lock(); sweep(&mut st, sched::monotonic_ns()); } // expiry timer
    if INQ_N.load(Ordering::Relaxed) == 0 { return; }
    let q: Vec<(Sid, Sid, Vec<u8>)> = { let mut st = FWD.lock(); INQ_N.store(0, Ordering::Relaxed); st.inq.drain(..).collect() };
    for (br, port, f) in q { crate::l2::deliver(br, port, &f); }
}
