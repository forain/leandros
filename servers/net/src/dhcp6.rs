//! DHCPv6 client of the NIC stacks (RFC 8415), the part of NetworkManager / systemd-networkd that
//! acts on a router advertisement's flags: the O flag starts a stateless Information-Request
//! (DNS servers and search list only), the M flag a stateful Solicit / Advertise / Request / Reply
//! exchange for an IA_NA address (/128, valid lifetime enforced; T1/T2 renewal is not done: the
//! lease is solicited again when it ends). Servers and domains join the RDNSS/DNSSL ones in
//! `/etc/resolv.conf` (`ip6::dns_publish`). The client talks UDP 546 -> [ff02::1:2]:547 through a
//! smoltcp UDP socket of the stack. State is in `/proc/net/dhcp6`.

use alloc::vec::Vec;
use core::fmt::Write;
use smoltcp::iface::SocketHandle;
use smoltcp::socket::udp;
use smoltcp::wire::{IpAddress, IpCidr, IpEndpoint, Ipv6Address};
use spin::Mutex;
use drivers::netdev::{self, MAX_NETIF};
use crate::iface;

const SEC: u64 = 1_000_000_000;
const MODE_NONE: u8 = 0;
const MODE_INFO: u8 = 1;
const MODE_FULL: u8 = 2;
// States.
const IDLE: u8 = 0;
const INFO_SENT: u8 = 1;
const SOLICIT: u8 = 2;
const REQUEST: u8 = 3;
const DONE: u8 = 4;
const GAVE_UP: u8 = 5;
/// Transmissions of one message before giving up (RFC 8415 IRT 1 s, doubling, capped at 16 s here).
const MAX_TRIES: u8 = 6;

#[derive(Clone)]
struct D6 {
    mode: u8,
    st: u8,
    xid: [u8; 3],
    tries: u8,
    /// Next (re)transmission.
    next: u64,
    /// Server identifier option body picked from the Advertise.
    server: Vec<u8>,
    /// Address offered / bound and its valid-lifetime end (`ONE_SHOT` = infinite).
    offer: Option<([u8; 16], u32, u32)>,
    bound: Option<([u8; 16], u64)>,
    dns: Vec<[u8; 16]>,
    dom: Vec<Vec<u8>>,
    replies: u32,
}
impl D6 {
    const fn new() -> D6 {
        D6 { mode: MODE_NONE, st: IDLE, xid: [0; 3], tries: 0, next: 0, server: Vec::new(), offer: None, bound: None,
             dns: Vec::new(), dom: Vec::new(), replies: 0 }
    }
}
static ST: Mutex<Vec<D6>> = Mutex::new(Vec::new());
static SOCK: Mutex<[Option<SocketHandle>; MAX_NETIF]> = Mutex::new([None; MAX_NETIF]);

fn with<R>(id: usize, f: impl FnOnce(&mut D6) -> R) -> R {
    let mut t = ST.lock();
    while t.len() <= id { t.push(D6::new()); }
    f(&mut t[id])
}

/// Add the DHCPv6 client socket to a fresh NIC stack.
pub(crate) fn attach(id: u8, set: &mut smoltcp::iface::SocketSet<'static>) {
    let rx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 4], alloc::vec![0u8; 4096]);
    let tx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 2], alloc::vec![0u8; 1024]);
    let mut s = udp::Socket::new(rx, tx);
    let _ = s.bind(546);
    SOCK.lock()[id as usize] = Some(set.add(s));
    with(id as usize, |d| *d = D6::new());
}

/// The RA's M/O flag byte (`icmp[5]`): M selects the stateful exchange, O the stateless one. The
/// first RA that asks starts the exchange; a mode that is already running is left alone.
pub(crate) fn ra_flags(id: usize, flags: u8, now: u64) {
    let want = if flags & 0x80 != 0 { MODE_FULL } else if flags & 0x40 != 0 { MODE_INFO } else { MODE_NONE };
    with(id, |d| {
        if want == MODE_NONE || want <= d.mode { return; }
        d.mode = want;
        d.st = IDLE;
        d.tries = 0;
        d.next = now;
    });
}

/// Forget everything of `id` (link down, accept_ra off): the address, the servers.
pub(crate) fn reset(id: usize, s: &mut crate::NetStack) {
    let (had_addr, had_dns) = with(id, |d| {
        let r = (d.bound.map(|b| b.0), !d.dns.is_empty() || !d.dom.is_empty());
        *d = D6::new();
        r
    });
    if let Some(a) = had_addr { remove_addr(s, a); iface::notify(id as u8); }
    if had_dns { crate::ip6::dns_publish(); }
}

fn remove_addr(s: &mut crate::NetStack, a: [u8; 16]) {
    s.interface.update_ip_addrs(|v| {
        let keep: Vec<IpCidr> = v.iter().copied().filter(|c| c.address() != IpAddress::Ipv6(Ipv6Address(a))).collect();
        v.clear();
        for c in keep { let _ = v.push(c); }
    });
}

/// The DHCPv6 address of netif `id`.
pub(crate) fn addr_of(id: u8) -> Option<[u8; 16]> {
    ST.lock().get(id as usize).and_then(|d| d.bound.map(|b| b.0))
}
/// Every DHCPv6 name server, with its netif.
pub(crate) fn servers() -> Vec<(usize, [u8; 16])> {
    let mut v = Vec::new();
    for (i, d) in ST.lock().iter().enumerate() { for a in &d.dns { v.push((i, *a)); } }
    v
}
pub(crate) fn domains() -> Vec<Vec<u8>> {
    let mut v = Vec::new();
    for d in ST.lock().iter() { for x in &d.dom { v.push(x.clone()); } }
    v
}

fn opt(v: &mut Vec<u8>, code: u16, body: &[u8]) {
    v.extend_from_slice(&code.to_be_bytes());
    v.extend_from_slice(&(body.len() as u16).to_be_bytes());
    v.extend_from_slice(body);
}

/// Options of a DHCPv6 message body (after the 4-byte header) or of an IA option.
fn opts(mut b: &[u8]) -> Vec<(u16, &[u8])> {
    let mut v = Vec::new();
    while b.len() >= 4 {
        let c = u16::from_be_bytes([b[0], b[1]]);
        let l = u16::from_be_bytes([b[2], b[3]]) as usize;
        if 4 + l > b.len() { break; }
        v.push((c, &b[4..4 + l]));
        b = &b[4 + l..];
    }
    v
}

/// Domain search list option (RFC 1035 labels, no compression).
fn parse_domains(mut b: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    while !b.is_empty() {
        let mut name = Vec::new();
        loop {
            let Some(&l) = b.first() else { return out };
            let l = l as usize;
            if l == 0 { b = &b[1..]; break; }
            if l > 63 || 1 + l > b.len() { return out; }
            if !name.is_empty() { name.push(b'.'); }
            name.extend_from_slice(&b[1..1 + l]);
            b = &b[1 + l..];
        }
        if !name.is_empty() { out.push(name); }
    }
    out
}

fn build(msg: u8, d: &D6, mac: [u8; 6], iaid: u32) -> Vec<u8> {
    let mut p = alloc::vec![msg, d.xid[0], d.xid[1], d.xid[2]];
    let mut duid = alloc::vec![0u8, 3, 0, 1];
    duid.extend_from_slice(&mac);
    opt(&mut p, 1, &duid);
    if !d.server.is_empty() && msg != 1 { opt(&mut p, 2, &d.server); }
    if msg != 11 {
        let mut ia = Vec::new();
        ia.extend_from_slice(&iaid.to_be_bytes());
        ia.extend_from_slice(&[0u8; 8]);
        if msg == 3 {
            if let Some((a, pref, valid)) = d.offer {
                let mut ad = a.to_vec();
                ad.extend_from_slice(&pref.to_be_bytes());
                ad.extend_from_slice(&valid.to_be_bytes());
                opt(&mut ia, 5, &ad);
            }
        }
        opt(&mut p, 3, &ia);
    }
    opt(&mut p, 6, &[0, 23, 0, 24]);
    opt(&mut p, 8, &[0, 0]);
    p
}

fn fresh_xid(id: usize, now: u64) -> [u8; 3] {
    let mut x = now ^ ((id as u64 + 1) << 40) ^ 0x9e37_79b9_7f4a_7c15;
    x ^= x >> 29; x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9); x ^= x >> 32;
    [x as u8, (x >> 8) as u8, (x >> 16) as u8]
}

fn valid_addr(a: &[u8; 16]) -> bool { a[0] != 0xff && *a != [0; 16] && !(a[0] == 0xfe && a[1] & 0xc0 == 0x80) }

/// One step of netif `nif`'s client (its stack lock held): replies, state changes, sends, lease end.
/// Returns the ns until the next timed event.
pub(crate) fn pass(nif: netdev::Netif, s: &mut crate::NetStack, now: u64) -> Option<u64> {
    let id = nif.id as usize;
    let h = SOCK.lock()[id]?;
    let mac = nif.mac()?;
    let mut dgrams: Vec<Vec<u8>> = Vec::new();
    {
        let sock = s.socket_set.get_mut::<udp::Socket>(h);
        let mut b = [0u8; 1500];
        while sock.can_recv() { match sock.recv_slice(&mut b) { Ok((n, _)) => dgrams.push(b[..n].to_vec()), Err(_) => break } }
    }
    let iaid = nif.id as u32 + 2;
    let mut dns_changed = false;
    let mut install: Option<([u8; 16], u64)> = None;
    let mut drop_addr: Option<[u8; 16]> = None;
    for g in &dgrams {
        if g.len() < 4 { continue; }
        let (ty, xid) = (g[0], [g[1], g[2], g[3]]);
        let o = opts(&g[4..]);
        with(id, |d| {
            if d.xid != xid || d.st == IDLE || d.st == DONE || d.st == GAVE_UP { return; }
            // Our own client identifier must be echoed (RFC 8415 16.x).
            if let Some((_, c)) = o.iter().find(|x| x.0 == 1) { if c.len() != 10 || c[4..] != mac { return; } }
            let status_bad = o.iter().any(|x| x.0 == 13 && x.1.len() >= 2 && (x.1[0] != 0 || x.1[1] != 0));
            let ia_addr = o.iter().filter(|x| x.0 == 3 && x.1.len() >= 12).find_map(|x| {
                opts(&x.1[12..]).into_iter().find(|y| y.0 == 5 && y.1.len() >= 24).map(|y| {
                    let a: [u8; 16] = y.1[..16].try_into().unwrap();
                    (a, u32::from_be_bytes(y.1[16..20].try_into().unwrap()), u32::from_be_bytes(y.1[20..24].try_into().unwrap()))
                })
            });
            match (d.st, ty) {
                (SOLICIT, 2) if !status_bad => {
                    let (Some((_, sid)), Some(a)) = (o.iter().find(|x| x.0 == 2), ia_addr) else { return };
                    if !valid_addr(&a.0) || a.2 == 0 { return; }
                    d.server = sid.to_vec();
                    d.offer = Some(a);
                    d.st = REQUEST;
                    d.tries = 0;
                    d.xid = fresh_xid(id, now ^ 1);
                    d.next = now;
                }
                (INFO_SENT, 7) | (REQUEST, 7) | (SOLICIT, 7) => {
                    d.replies += 1;
                    if status_bad { d.st = GAVE_UP; return; }
                    for x in &o {
                        if x.0 == 23 { d.dns = x.1.chunks_exact(16).map(|c| c.try_into().unwrap()).collect(); dns_changed = true; }
                        if x.0 == 24 { d.dom = parse_domains(x.1); dns_changed = true; }
                    }
                    if d.mode == MODE_FULL {
                        let Some(a) = ia_addr else { d.st = GAVE_UP; return };
                        if !valid_addr(&a.0) || a.2 == 0 { d.st = GAVE_UP; return; }
                        let end = if a.2 == u32::MAX { u64::MAX } else { now.saturating_add(a.2 as u64 * SEC) };
                        if let Some(old) = d.bound { if old.0 != a.0 { drop_addr = Some(old.0); } }
                        d.bound = Some((a.0, end));
                        install = Some((a.0, end));
                    }
                    d.st = DONE;
                    crate::iface::serial(&[b"[NET] dhcp6: reply on ", nif.name().as_bytes(), b"\r\n"]);
                }
                _ => {}
            }
        });
    }
    if let Some(a) = drop_addr { remove_addr(s, a); }
    if let Some((a, _)) = install {
        s.interface.update_ip_addrs(|v| {
            if !v.iter().any(|c| c.address() == IpAddress::Ipv6(Ipv6Address(a))) {
                let _ = v.push(IpCidr::new(IpAddress::Ipv6(Ipv6Address(a)), 128));
            }
        });
        iface::notify(nif.id);
    }
    // Lease end: drop the address and solicit again.
    let expired = with(id, |d| match d.bound { Some((a, e)) if e <= now => { d.bound = None; d.st = IDLE; d.tries = 0; d.next = now; Some(a) } _ => None });
    if let Some(a) = expired { remove_addr(s, a); iface::notify(nif.id); }
    if dns_changed { crate::ip6::dns_publish(); }

    // Transmission.
    let go = with(id, |d| {
        if d.mode == MODE_NONE || matches!(d.st, DONE | GAVE_UP) || now < d.next { return None; }
        if d.st == IDLE {
            d.xid = fresh_xid(id, now);
            d.st = if d.mode == MODE_FULL { SOLICIT } else { INFO_SENT };
            d.tries = 0;
        }
        if d.tries >= MAX_TRIES { d.st = GAVE_UP; return None; }
        let msg = match d.st { SOLICIT => 1, REQUEST => 3, _ => 11 };
        let p = build(msg, d, mac, iaid);
        d.tries += 1;
        d.next = now + (SEC << (d.tries - 1).min(4));
        Some(p)
    });
    if let Some(p) = go {
        let dst = IpEndpoint::new(IpAddress::Ipv6(Ipv6Address([0xff, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 2])), 547);
        let sock = s.socket_set.get_mut::<udp::Socket>(h);
        if sock.send_slice(&p, dst).is_err() { with(id, |d| { d.tries -= 1; d.next = now + 100_000_000; }); }
    }
    with(id, |d| {
        if d.mode == MODE_NONE || matches!(d.st, GAVE_UP) { return None; }
        let mut next = if d.st == DONE { u64::MAX } else { d.next };
        if let Some((_, e)) = d.bound { next = next.min(e); }
        if next == u64::MAX { None } else { Some(next.saturating_sub(now).max(1_000_000)) }
    })
}

/// `/proc/net/dhcp6`: `ifname mode state xid tries replies address dns...`.
pub(crate) fn proc_dhcp6(out: &mut impl Write) {
    let t = ST.lock();
    for nif in netdev::iter() {
        let Some(d) = t.get(nif.id as usize) else { continue };
        let mode = ["none", "info", "stateful"][d.mode as usize];
        let st = ["idle", "info-sent", "solicit", "request", "done", "gave-up"][d.st as usize];
        let _ = write!(out, "{} {} {} {:02x}{:02x}{:02x} {} {} ", nif.name(), mode, st, d.xid[0], d.xid[1], d.xid[2], d.tries, d.replies);
        match d.bound { Some((a, _)) => { for b in a { let _ = write!(out, "{:02x}", b); } } None => { let _ = write!(out, "-"); } }
        for a in &d.dns { let _ = write!(out, " "); for b in a { let _ = write!(out, "{:02x}", b); } }
        let _ = writeln!(out);
    }
}
