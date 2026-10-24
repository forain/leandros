//! Reachability probe for the kernel's trial-boot watchdog (`kernel/src/netwatch.rs`).
//!
//! One ICMP echo socket owned by the net server (ident `IDENT`), on the stack
//! the target routes through. Each `probe()` call collects any echo reply that
//! arrived since the previous call and sends the next request. Sending to the
//! default gateway makes smoltcp ARP for it first, so "replied" means the
//! gateway answered ARP and ping: the data path works, not just the lease.
//!
//! `disarm()` (`NETCTL_NETWATCH_DISARM`, issued by `espctl promote`) tells the
//! watchdog to stop for the rest of this boot.

use core::sync::atomic::{AtomicBool, Ordering};
use smoltcp::iface::SocketHandle;
use smoltcp::socket::icmp;
use smoltcp::wire::{IpAddress, Ipv4Address};
use spin::Mutex;

use crate::iface::{self, Sid};

const IDENT: u16 = 0x4E57; // "NW"

struct Probe { sid: Sid, h: SocketHandle, seq: u16 }
static PROBE: Mutex<Option<Probe>> = Mutex::new(None);
static DISARMED: AtomicBool = AtomicBool::new(false);

/// Stop the watchdog for the rest of this boot (the running trial was promoted).
pub fn disarm() { DISARMED.store(true, Ordering::Release); }
pub fn disarmed() -> bool { DISARMED.load(Ordering::Acquire) }

/// The default route's gateway, if any netif has one.
pub fn default_gateway() -> Option<[u8; 4]> {
    let s = iface::snapshot();
    s.iter().find(|i| i.default_route && i.gateway.is_some()).or_else(|| s.iter().find(|i| i.gateway.is_some()))
        .and_then(|i| i.gateway)
}

fn echo_request(seq: u16) -> [u8; 24] {
    let mut p = [0u8; 24];
    p[0] = 8; // echo request, code 0
    p[4..6].copy_from_slice(&IDENT.to_be_bytes());
    p[6..8].copy_from_slice(&seq.to_be_bytes());
    p[8..24].copy_from_slice(b"leandros-netwtch");
    let mut sum: u32 = 0;
    for c in p.chunks(2) { sum += u16::from_be_bytes([c[0], c[1]]) as u32; }
    while sum > 0xFFFF { sum = (sum & 0xFFFF) + (sum >> 16); }
    p[2..4].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    p
}

/// One probe step toward `dest`: true when an echo reply from `dest` arrived
/// since the previous call. Then sends the next echo request.
pub fn probe(dest: [u8; 4]) -> bool {
    let ip = IpAddress::Ipv4(Ipv4Address::new(dest[0], dest[1], dest[2], dest[3]));
    let sid = match iface::route_select(ip) { Some(s) if s != iface::LO => s, _ => return false };
    let mut pg = PROBE.lock();
    if let Some(p) = pg.as_ref() {
        if p.sid != sid {
            if let Some(s) = crate::stack_for(p.sid).as_mut() { s.socket_set.remove(p.h); }
            *pg = None;
        }
    }
    let mut stack = crate::stack_for(sid);
    let s = match stack.as_mut() { Some(s) => s, None => return false };
    if pg.is_none() {
        let rx = icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; 4], alloc::vec![0; 512]);
        let tx = icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; 4], alloc::vec![0; 512]);
        let mut sock = icmp::Socket::new(rx, tx);
        if sock.bind(icmp::Endpoint::Ident(IDENT)).is_err() { return false; }
        *pg = Some(Probe { sid, h: s.socket_set.add(sock), seq: 0 });
    }
    let p = pg.as_mut().unwrap();
    let sock = s.socket_set.get_mut::<icmp::Socket>(p.h);
    let mut got = false;
    while let Ok((d, from)) = sock.recv() {
        if from == ip && d.len() >= 8 && d[0] == 0 && d[4..6] == IDENT.to_be_bytes() { got = true; }
    }
    p.seq = p.seq.wrapping_add(1);
    let _ = sock.send_slice(&echo_request(p.seq), ip);
    drop(stack);
    drop(pg);
    drivers::netdev::kick();
    got
}
