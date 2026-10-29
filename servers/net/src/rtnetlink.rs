//! Minimal NETLINK_ROUTE: the message encoding and the SIOCGIF* ioctls.
//!
//! Firefox's nICEr (WebRTC ICE gathering), musl `getifaddrs`/`if_nameindex`
//! and glibc's `getifaddrs` learn the local addresses through an
//! `RTM_GETLINK`/`RTM_GETADDR` dump (or `SIOCGIFCONF`). This file is the pure
//! part: it turns a list of `Link`s into the reply datagrams and answers the
//! `struct ifreq` ioctls. The socket plumbing (bind/send/recv/poll) is in
//! `lib.rs`. No kernel dependency here, so `rustc --test rtnetlink.rs` runs
//! the unit tests on the host.

use alloc::string::String;
use alloc::vec::Vec;

#[cfg(test)]
extern crate alloc;

pub const AF_NETLINK: usize = 16;
pub const NETLINK_ROUTE: usize = 0;

const NLMSG_ERROR: u16 = 2;
const NLMSG_DONE: u16 = 3;
const RTM_NEWLINK: u16 = 16;
const RTM_GETLINK: u16 = 18;
const RTM_NEWADDR: u16 = 20;
const RTM_GETADDR: u16 = 22;
const RTM_GETROUTE: u16 = 26;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETNEIGH: u16 = 30;
const RTA_DST: u16 = 1;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PREFSRC: u16 = 7;
const RTA_TABLE: u16 = 15;
const RT_TABLE_MAIN: u8 = 254;
const RTN_UNICAST: u8 = 1;

const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_MULTI: u16 = 0x2;
const NLM_F_DUMP: u16 = 0x300;

const IFLA_ADDRESS: u16 = 1;
const IFLA_BROADCAST: u16 = 2;
const IFLA_IFNAME: u16 = 3;
const IFLA_MTU: u16 = 4;
const IFLA_OPERSTATE: u16 = 16;
const IFA_ADDRESS: u16 = 1;
const IFA_LOCAL: u16 = 2;
const IFA_LABEL: u16 = 3;
const IFA_BROADCAST: u16 = 4;

pub const IFF_UP: u32 = 0x1;
pub const IFF_BROADCAST: u32 = 0x2;
pub const IFF_LOOPBACK: u32 = 0x8;
pub const IFF_RUNNING: u32 = 0x40;
pub const IFF_MULTICAST: u32 = 0x1000;
pub const IFF_LOWER_UP: u32 = 0x10000;

const AF_UNSPEC: u8 = 0;
const AF_INET: u8 = 2;
const ARPHRD_ETHER: u16 = 1;
const ARPHRD_LOOPBACK: u16 = 772;
const RT_SCOPE_UNIVERSE: u8 = 0;
const RT_SCOPE_HOST: u8 = 254;

/// Datagrams are kept under nICEr's 4 KiB read buffer.
const MAX_DGRAM: usize = 3584;

/// One network interface as the netlink and ioctl layers see it.
#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub index: u32,
    pub name: String,
    pub mac: [u8; 6],
    pub mtu: u32,
    pub up: bool,
    pub running: bool,
    pub loopback: bool,
    /// IPv4 address and prefix length, when configured.
    pub addr: Option<([u8; 4], u8)>,
}

impl Link {
    /// `ifi_flags` / `SIOCGIFFLAGS` bits.
    pub fn flags(&self) -> u32 {
        let mut f = 0;
        if self.up { f |= IFF_UP; }
        if self.running { f |= IFF_RUNNING | IFF_LOWER_UP; }
        if self.loopback { f |= IFF_LOOPBACK; } else { f |= IFF_BROADCAST | IFF_MULTICAST; }
        f
    }
    fn arphrd(&self) -> u16 { if self.loopback { ARPHRD_LOOPBACK } else { ARPHRD_ETHER } }
    fn broadcast(&self) -> Option<[u8; 4]> {
        if self.loopback { return None; }
        let (a, p) = self.addr?;
        let m = mask(p);
        Some([a[0] | !m[0], a[1] | !m[1], a[2] | !m[2], a[3] | !m[3]])
    }
}

pub fn mask(prefix: u8) -> [u8; 4] {
    let m = if prefix == 0 { 0u32 } else { u32::MAX << (32 - prefix.min(32) as u32) };
    m.to_be_bytes()
}

fn put16(v: &mut Vec<u8>, x: u16) { v.extend_from_slice(&x.to_ne_bytes()); }
fn put32(v: &mut Vec<u8>, x: u32) { v.extend_from_slice(&x.to_ne_bytes()); }
fn rd16(b: &[u8], o: usize) -> u16 { u16::from_ne_bytes([b[o], b[o + 1]]) }
fn rd32(b: &[u8], o: usize) -> u32 { u32::from_ne_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]]) }

fn pad4(v: &mut Vec<u8>) { while v.len() % 4 != 0 { v.push(0); } }

fn attr(v: &mut Vec<u8>, ty: u16, data: &[u8]) {
    put16(v, (4 + data.len()) as u16);
    put16(v, ty);
    v.extend_from_slice(data);
    pad4(v);
}

fn attr_str(v: &mut Vec<u8>, ty: u16, s: &str) {
    let mut d = Vec::from(s.as_bytes());
    d.push(0);
    attr(v, ty, &d);
}

/// Start a message: header with a length to be patched by `end_msg`.
fn begin_msg(v: &mut Vec<u8>, ty: u16, flags: u16, seq: u32, pid: u32) -> usize {
    let start = v.len();
    put32(v, 0);
    put16(v, ty);
    put16(v, flags);
    put32(v, seq);
    put32(v, pid);
    start
}

fn end_msg(v: &mut Vec<u8>, start: usize) {
    pad4(v);
    let len = (v.len() - start) as u32;
    v[start..start + 4].copy_from_slice(&len.to_ne_bytes());
}

fn link_msg(l: &Link, seq: u32, pid: u32) -> Vec<u8> {
    let mut v = Vec::new();
    let s = begin_msg(&mut v, RTM_NEWLINK, NLM_F_MULTI, seq, pid);
    v.push(AF_UNSPEC);
    v.push(0);
    put16(&mut v, l.arphrd());
    put32(&mut v, l.index);
    put32(&mut v, l.flags());
    put32(&mut v, 0);
    attr_str(&mut v, IFLA_IFNAME, &l.name);
    attr(&mut v, IFLA_ADDRESS, &l.mac);
    attr(&mut v, IFLA_BROADCAST, if l.loopback { &[0u8; 6] } else { &[0xff; 6] });
    attr(&mut v, IFLA_MTU, &l.mtu.to_ne_bytes());
    // IF_OPER_UNKNOWN for lo, IF_OPER_UP / IF_OPER_DOWN otherwise.
    attr(&mut v, IFLA_OPERSTATE, &[if l.loopback { 0 } else if l.running { 6 } else { 2 }]);
    end_msg(&mut v, s);
    v
}

fn addr_msg(l: &Link, a: [u8; 4], prefix: u8, seq: u32, pid: u32) -> Vec<u8> {
    let mut v = Vec::new();
    let s = begin_msg(&mut v, RTM_NEWADDR, NLM_F_MULTI, seq, pid);
    v.push(AF_INET);
    v.push(prefix);
    v.push(0x80); // IFA_F_PERMANENT
    v.push(if l.loopback { RT_SCOPE_HOST } else { RT_SCOPE_UNIVERSE });
    put32(&mut v, l.index);
    attr(&mut v, IFA_ADDRESS, &a);
    attr(&mut v, IFA_LOCAL, &a);
    attr_str(&mut v, IFA_LABEL, &l.name);
    if let Some(b) = l.broadcast() { attr(&mut v, IFA_BROADCAST, &b); }
    end_msg(&mut v, s);
    v
}

fn done_msg(seq: u32, pid: u32) -> Vec<u8> {
    let mut v = Vec::new();
    let s = begin_msg(&mut v, NLMSG_DONE, NLM_F_MULTI, seq, pid);
    put32(&mut v, 0);
    end_msg(&mut v, s);
    v
}

/// NLMSG_ERROR: `errno` is a positive error number, 0 is an ACK. Echoes the
/// request header.
fn error_msg(errno: i32, req: &[u8], seq: u32, pid: u32) -> Vec<u8> {
    let mut v = Vec::new();
    let s = begin_msg(&mut v, NLMSG_ERROR, 0, seq, pid);
    v.extend_from_slice(&(-errno).to_ne_bytes());
    v.extend_from_slice(&req[..16.min(req.len())]);
    end_msg(&mut v, s);
    v
}

/// Pack `msgs` into datagrams of at most `MAX_DGRAM` bytes.
fn pack(msgs: Vec<Vec<u8>>, out: &mut Vec<Vec<u8>>) {
    let mut cur: Vec<u8> = Vec::new();
    for m in msgs {
        if !cur.is_empty() && cur.len() + m.len() > MAX_DGRAM {
            out.push(core::mem::take(&mut cur));
        }
        cur.extend_from_slice(&m);
    }
    if !cur.is_empty() { out.push(cur); }
}

/// Answer the netlink request(s) in `req`: the reply datagrams, in order.
/// `pid` is the socket's nl_pid, echoed as `nlmsg_pid`.
pub fn handle_request(req: &[u8], links: &[Link], pid: u32) -> Vec<Vec<u8>> {
    handle_request_r(req, links, pid, &|_| None)
}

/// Route lookup result: output ifindex and the gateway when off-link.
pub type RouteHit = (u32, Option<[u8; 4]>);

/// RTM_GETROUTE reply: one RTM_NEWROUTE for `dst` via `hit`.
fn route_msg(dst: [u8; 4], hit: RouteHit, links: &[Link], seq: u32, pid: u32) -> Vec<u8> {
    let mut v = Vec::new();
    let s = begin_msg(&mut v, RTM_NEWROUTE, 0, seq, pid);
    v.extend_from_slice(&[AF_INET, 32, 0, 0, RT_TABLE_MAIN, if hit.1.is_some() { 4 } else { 2 },
                          RT_SCOPE_UNIVERSE, RTN_UNICAST]);
    put32(&mut v, 0); // rtm_flags
    attr(&mut v, RTA_TABLE, &(RT_TABLE_MAIN as u32).to_ne_bytes());
    attr(&mut v, RTA_DST, &dst);
    attr(&mut v, RTA_OIF, &hit.0.to_ne_bytes());
    if let Some(a) = links.iter().find(|l| l.index == hit.0).and_then(|l| l.addr) {
        attr(&mut v, RTA_PREFSRC, &a.0);
    }
    if let Some(g) = hit.1 { attr(&mut v, RTA_GATEWAY, &g); }
    end_msg(&mut v, s);
    v
}

/// The RTA_DST of an rtmsg request body, for AF_INET.
fn req_dst(msg: &[u8]) -> Option<[u8; 4]> {
    if msg.len() < 28 || msg[16] != AF_INET { return None; }
    let mut o = 28;
    while o + 4 <= msg.len() {
        let l = rd16(msg, o) as usize;
        if l < 4 || o + l > msg.len() { break; }
        if rd16(msg, o + 2) & 0x3fff == RTA_DST && l >= 8 {
            return Some([msg[o + 4], msg[o + 5], msg[o + 6], msg[o + 7]]);
        }
        o += (l + 3) & !3;
    }
    None
}

/// [`handle_request`] with a route lookup for non-dump RTM_GETROUTE.
pub fn handle_request_r(req: &[u8], links: &[Link], pid: u32,
                        route: &dyn Fn([u8; 4]) -> Option<RouteHit>) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut off = 0usize;
    while off + 16 <= req.len() {
        let len = rd32(req, off) as usize;
        if len < 16 || off + len > req.len() { break; }
        let ty = rd16(req, off + 4);
        let flags = rd16(req, off + 6);
        let seq = rd32(req, off + 8);
        let msg = &req[off..off + len];
        let dump = flags & NLM_F_DUMP != 0; // Linux: ROOT or MATCH alone is a dump
        match ty {
            RTM_GETLINK if dump => {
                let msgs = links.iter().map(|l| link_msg(l, seq, pid)).collect();
                pack(msgs, &mut out);
                out.push(done_msg(seq, pid));
            }
            RTM_GETADDR if dump => {
                let msgs = links.iter()
                    .filter_map(|l| l.addr.map(|(a, p)| addr_msg(l, a, p, seq, pid)))
                    .collect();
                pack(msgs, &mut out);
                out.push(done_msg(seq, pid));
            }
            // No routes are exported: an empty dump.
            RTM_GETROUTE if dump => out.push(done_msg(seq, pid)),
            RTM_GETROUTE => match req_dst(msg).and_then(|d| route(d).map(|h| (d, h))) {
                Some((d, h)) => out.push(route_msg(d, h, links, seq, pid)),
                None => out.push(error_msg(101, msg, seq, pid)), // ENETUNREACH
            },
            RTM_GETNEIGH if dump => out.push(done_msg(seq, pid)),
            // Control messages (NLMSG_NOOP..DONE) are ignored, as in Linux.
            t if t < 16 => {}
            _ if flags & NLM_F_REQUEST != 0 => out.push(error_msg(95, msg, seq, pid)), // EOPNOTSUPP
            _ => {}
        }
        off += (len + 3) & !3;
    }
    out
}

/// A `struct sockaddr_nl` (12 bytes).
pub fn sockaddr_nl(pid: u32, groups: u32) -> [u8; 12] {
    let mut sa = [0u8; 12];
    sa[0..2].copy_from_slice(&(AF_NETLINK as u16).to_ne_bytes());
    sa[4..8].copy_from_slice(&pid.to_ne_bytes());
    sa[8..12].copy_from_slice(&groups.to_ne_bytes());
    sa
}

// ── ioctls on a `struct ifreq` (40 bytes: name[16] + 24-byte union) ──────────

pub const SIOCGIFNAME: u32 = 0x8910;
pub const SIOCGIFCONF: u32 = 0x8912;
pub const SIOCGIFFLAGS: u32 = 0x8913;
pub const SIOCGIFADDR: u32 = 0x8915;
pub const SIOCGIFBRDADDR: u32 = 0x8919;
pub const SIOCGIFNETMASK: u32 = 0x891b;
pub const SIOCGIFMTU: u32 = 0x8921;
pub const SIOCGIFHWADDR: u32 = 0x8927;
pub const SIOCGIFINDEX: u32 = 0x8933;
pub const SIOCETHTOOL: u32 = 0x8946;
pub const SIOCGIWRATE: u32 = 0x8b21;

pub const IFREQ_LEN: usize = 40;

/// True for the ioctls `ifreq_ioctl` / `ifconf` answer (including the ones
/// answered EOPNOTSUPP).
pub fn is_if_ioctl(cmd: u32) -> bool {
    matches!(cmd, SIOCGIFNAME | SIOCGIFCONF | SIOCGIFFLAGS | SIOCGIFADDR | SIOCGIFBRDADDR
        | SIOCGIFNETMASK | SIOCGIFMTU | SIOCGIFHWADDR | SIOCGIFINDEX | SIOCETHTOOL | SIOCGIWRATE)
}

fn sockaddr_in(ifr: &mut [u8; IFREQ_LEN], ip: [u8; 4]) {
    ifr[16..40].fill(0);
    ifr[16..18].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    ifr[20..24].copy_from_slice(&ip);
}

/// Answer one SIOCGIF* request in place. 0 or a negative errno.
pub fn ifreq_ioctl(cmd: u32, ifr: &mut [u8; IFREQ_LEN], links: &[Link]) -> i32 {
    if cmd == SIOCETHTOOL || cmd == SIOCGIWRATE { return -95; } // EOPNOTSUPP
    let link = if cmd == SIOCGIFNAME {
        let idx = i32::from_ne_bytes([ifr[16], ifr[17], ifr[18], ifr[19]]);
        links.iter().find(|l| l.index as i32 == idx)
    } else {
        let n = ifr[..16].iter().position(|&b| b == 0).unwrap_or(16);
        links.iter().find(|l| l.name.as_bytes() == &ifr[..n])
    };
    let Some(l) = link else { return -19 }; // ENODEV
    match cmd {
        SIOCGIFNAME => {
            ifr[..16].fill(0);
            let nb = l.name.as_bytes();
            let n = nb.len().min(15);
            ifr[..n].copy_from_slice(&nb[..n]);
        }
        SIOCGIFINDEX => {
            ifr[16..40].fill(0);
            ifr[16..20].copy_from_slice(&(l.index as i32).to_ne_bytes());
        }
        SIOCGIFFLAGS => {
            ifr[16..40].fill(0);
            ifr[16..18].copy_from_slice(&((l.flags() & 0xffff) as u16).to_ne_bytes());
        }
        SIOCGIFMTU => {
            ifr[16..40].fill(0);
            ifr[16..20].copy_from_slice(&(l.mtu as i32).to_ne_bytes());
        }
        SIOCGIFHWADDR => {
            ifr[16..40].fill(0);
            ifr[16..18].copy_from_slice(&l.arphrd().to_ne_bytes());
            ifr[18..24].copy_from_slice(&l.mac);
        }
        SIOCGIFADDR => match l.addr {
            Some((a, _)) => sockaddr_in(ifr, a),
            None => return -99, // EADDRNOTAVAIL
        },
        SIOCGIFNETMASK => match l.addr {
            Some((_, p)) => sockaddr_in(ifr, mask(p)),
            None => return -99,
        },
        SIOCGIFBRDADDR => match l.broadcast() {
            Some(b) => sockaddr_in(ifr, b),
            None => return -99,
        },
        _ => return -25,
    }
    0
}

/// SIOCGIFCONF: one `ifreq` (name + AF_INET address) per configured IPv4
/// address, at most `cap` bytes' worth (whole entries only).
pub fn ifconf(links: &[Link], cap: usize) -> Vec<u8> {
    let mut out = Vec::new();
    for l in links {
        let Some((a, _)) = l.addr else { continue };
        if out.len() + IFREQ_LEN > cap { break; }
        let mut ifr = [0u8; IFREQ_LEN];
        let nb = l.name.as_bytes();
        let n = nb.len().min(15);
        ifr[..n].copy_from_slice(&nb[..n]);
        sockaddr_in(&mut ifr, a);
        out.extend_from_slice(&ifr);
    }
    out
}

/// The interface table, as `/proc/net/dev` sees it: lo, then every netif
/// (`ifindex = id + 2`).
#[cfg(not(test))]
pub fn route_lookup(d: [u8; 4]) -> Option<RouteHit> {
    use smoltcp::wire::{IpAddress, Ipv4Address};
    let sid = super::iface::route_select(IpAddress::Ipv4(Ipv4Address::from_bytes(&d)))?;
    let gw = super::iface::snapshot().into_iter().find(|s| s.id as u32 + 1 == sid as u32)
        .and_then(|s| match (s.addr, s.gateway) {
            (Some((a, p)), Some(g)) if !super::iface::IfCfg { addr: a, prefix: p, gateway: None }.on_link(d) => Some(g),
            _ => None,
        });
    Some((sid as u32 + 1, gw))
}

#[cfg(not(test))]
pub fn current_links() -> Vec<Link> {
    let mut v = Vec::new();
    v.push(Link {
        index: 1, name: String::from("lo"), mac: [0; 6], mtu: 65536,
        up: true, running: true, loopback: true, addr: Some(([127, 0, 0, 1], 8)),
    });
    for s in super::iface::snapshot() {
        v.push(Link {
            index: s.id as u32 + 2,
            name: s.name.clone(),
            mac: s.mac.unwrap_or([0; 6]),
            mtu: 1500,
            up: s.admin_up,
            running: s.carrier && s.admin_up,
            loopback: false,
            addr: if s.admin_up { s.addr } else { None },
        });
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links() -> Vec<Link> {
        alloc::vec![
            Link { index: 1, name: "lo".into(), mac: [0; 6], mtu: 65536, up: true, running: true,
                   loopback: true, addr: Some(([127, 0, 0, 1], 8)) },
            Link { index: 2, name: "eth0".into(), mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56], mtu: 1500,
                   up: true, running: true, loopback: false, addr: Some(([10, 0, 2, 15], 24)) },
            Link { index: 3, name: "wifi0".into(), mac: [2; 6], mtu: 1500, up: true, running: false,
                   loopback: false, addr: None },
        ]
    }

    fn req(ty: u16, flags: u16, seq: u32) -> Vec<u8> {
        let mut v = Vec::new();
        put32(&mut v, 20);
        put16(&mut v, ty);
        put16(&mut v, flags);
        put32(&mut v, seq);
        put32(&mut v, 0);
        v.extend_from_slice(&[0, 0, 0, 0]);
        v
    }

    /// Split a datagram into (type, payload) pairs, checking lengths.
    fn split(d: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let mut o = 0;
        let mut r = Vec::new();
        while o < d.len() {
            let l = rd32(d, o) as usize;
            assert!(l >= 16 && o + l <= d.len() && l % 4 == 0);
            r.push((rd16(d, o + 4), d[o + 16..o + l].to_vec()));
            o += l;
        }
        r
    }

    fn attrs(p: &[u8]) -> Vec<(u16, Vec<u8>)> {
        let mut o = 0;
        let mut r = Vec::new();
        while o + 4 <= p.len() {
            let l = rd16(p, o) as usize;
            r.push((rd16(p, o + 2), p[o + 4..o + l].to_vec()));
            o += (l + 3) & !3;
        }
        r
    }

    #[test]
    fn getaddr_dump() {
        let out = handle_request(&req(RTM_GETADDR, 0x301, 7), &links(), 42);
        assert_eq!(out.len(), 2);
        let m = split(&out[0]);
        assert_eq!(m.len(), 2); // lo + eth0; wifi0 has no address
        assert_eq!(m[1].0, RTM_NEWADDR);
        assert_eq!(&m[1].1[..8], &[2, 24, 0x80, 0, 2, 0, 0, 0]);
        let a = attrs(&m[1].1[8..]);
        assert_eq!(a[0], (IFA_ADDRESS, alloc::vec![10, 0, 2, 15]));
        assert_eq!(a[1], (IFA_LOCAL, alloc::vec![10, 0, 2, 15]));
        assert_eq!(a[2], (IFA_LABEL, b"eth0\0".to_vec()));
        assert_eq!(a[3], (IFA_BROADCAST, alloc::vec![10, 0, 2, 255]));
        assert_eq!(m[0].1[3], RT_SCOPE_HOST);
        let d = split(&out[1]);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].0, NLMSG_DONE);
        assert_eq!(rd32(&out[1], 8), 7); // seq echoed
        assert_eq!(rd32(&out[1], 12), 42); // pid
    }

    #[test]
    fn getlink_dump() {
        let out = handle_request(&req(RTM_GETLINK, 0x301, 1), &links(), 1);
        let m = split(&out[0]);
        assert_eq!(m.len(), 3);
        let eth = &m[1].1;
        assert_eq!(rd16(eth, 2), ARPHRD_ETHER);
        assert_eq!(rd32(eth, 4), 2);
        let fl = rd32(eth, 8);
        assert!(fl & IFF_UP != 0 && fl & IFF_RUNNING != 0 && fl & IFF_BROADCAST != 0 && fl & IFF_LOOPBACK == 0);
        let a = attrs(&eth[16..]);
        assert_eq!(a[0], (IFLA_IFNAME, b"eth0\0".to_vec()));
        assert_eq!(a[1].0, IFLA_ADDRESS);
        assert_eq!(a[3], (IFLA_MTU, 1500u32.to_ne_bytes().to_vec()));
        assert_eq!(a[4], (IFLA_OPERSTATE, alloc::vec![6]));
        assert_eq!(rd32(&m[0].1, 8) & IFF_LOOPBACK, IFF_LOOPBACK);
        assert_eq!(rd16(&m[0].1, 2), ARPHRD_LOOPBACK);
        // wifi0: no carrier
        assert_eq!(rd32(&m[2].1, 8) & IFF_RUNNING, 0);
    }

    fn route_req(dst: [u8; 4]) -> Vec<u8> {
        let mut v = Vec::new();
        put32(&mut v, 0);
        put16(&mut v, RTM_GETROUTE);
        put16(&mut v, 1);
        put32(&mut v, 11);
        put32(&mut v, 0);
        v.extend_from_slice(&[AF_INET, 32, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        attr(&mut v, RTA_DST, &dst);
        let l = v.len() as u32;
        v[0..4].copy_from_slice(&l.to_ne_bytes());
        v
    }

    #[test]
    fn getaddr_root_request_flags_is_a_dump() {
        let out = handle_request(&req(RTM_GETADDR, 0x101, 3), &links(), 4);
        assert_eq!(split(&out[0])[0].0, RTM_NEWADDR);
    }

    #[test]
    fn getroute_single_reply_and_neigh_done() {
        let hit = |d: [u8; 4]| if d[0] == 8 { Some((2u32, Some([192, 168, 2, 1]))) } else { None };
        let out = handle_request_r(&route_req([8, 8, 8, 8]), &links(), 42, &hit);
        assert_eq!(out.len(), 1);
        let m = split(&out[0]);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].0, RTM_NEWROUTE);
        assert_eq!(rd16(&out[0], 6), 0);
        assert_eq!(rd32(&out[0], 8), 11);
        assert_eq!(rd32(&out[0], 12), 42);
        let a = attrs(&m[0].1[12..]);
        let get = |t| a.iter().find(|x| x.0 == t).map(|x| x.1.clone());
        assert_eq!(get(RTA_OIF).unwrap(), 2u32.to_ne_bytes());
        assert_eq!(get(RTA_GATEWAY).unwrap(), vec![192, 168, 2, 1]);
        assert_eq!(get(RTA_DST).unwrap(), vec![8, 8, 8, 8]);
        assert!(get(RTA_PREFSRC).is_some() && get(RTA_TABLE).is_some());
        let e = handle_request_r(&route_req([9, 9, 9, 9]), &links(), 42, &hit);
        let m = split(&e[0]);
        assert_eq!(m[0].0, NLMSG_ERROR);
        assert_eq!(i32::from_ne_bytes(m[0].1[..4].try_into().unwrap()), -101);
        let n = handle_request(&req(RTM_GETNEIGH, 0x301, 5), &links(), 1);
        assert_eq!(split(&n[0])[0].0, NLMSG_DONE);
    }

    #[test]
    fn unknown_request_is_error() {
        let out = handle_request(&req(60, 1, 9), &links(), 5);
        assert_eq!(out.len(), 1);
        let m = split(&out[0]);
        assert_eq!(m[0].0, NLMSG_ERROR);
        assert_eq!(i32::from_ne_bytes(m[0].1[..4].try_into().unwrap()), -95);
        assert_eq!(rd32(&m[0].1, 4 + 8), 9); // echoed request seq
    }

    #[test]
    fn many_links_split_datagrams() {
        let mut ls = Vec::new();
        for i in 0..100 {
            ls.push(Link { index: i + 1, name: alloc::format!("eth{}", i), mac: [1; 6], mtu: 1500,
                           up: true, running: true, loopback: false, addr: Some(([10, 0, 0, 1], 24)) });
        }
        let out = handle_request(&req(RTM_GETLINK, 0x301, 1), &ls, 1);
        assert!(out.len() > 2);
        assert!(out.iter().all(|d| d.len() <= MAX_DGRAM + 128));
        let total: usize = out.iter().map(|d| split(d).len()).sum();
        assert_eq!(total, 101);
    }

    #[test]
    fn ioctls() {
        let ls = links();
        let mut ifr = [0u8; IFREQ_LEN];
        ifr[..4].copy_from_slice(b"eth0");
        assert_eq!(ifreq_ioctl(SIOCGIFADDR, &mut ifr, &ls), 0);
        assert_eq!(&ifr[20..24], &[10, 0, 2, 15]);
        assert_eq!(ifreq_ioctl(SIOCGIFNETMASK, &mut ifr, &ls), 0);
        assert_eq!(&ifr[20..24], &[255, 255, 255, 0]);
        assert_eq!(ifreq_ioctl(SIOCGIFINDEX, &mut ifr, &ls), 0);
        assert_eq!(rd32(&ifr, 16), 2);
        assert_eq!(ifreq_ioctl(SIOCGIFFLAGS, &mut ifr, &ls), 0);
        assert!(rd16(&ifr, 16) as u32 & IFF_UP != 0);
        assert_eq!(ifreq_ioctl(SIOCGIFMTU, &mut ifr, &ls), 0);
        assert_eq!(rd32(&ifr, 16), 1500);
        assert_eq!(ifreq_ioctl(SIOCGIFHWADDR, &mut ifr, &ls), 0);
        assert_eq!(&ifr[18..24], &[0x52, 0x54, 0, 0x12, 0x34, 0x56]);
        let mut ifr2 = [0u8; IFREQ_LEN];
        ifr2[16] = 3;
        assert_eq!(ifreq_ioctl(SIOCGIFNAME, &mut ifr2, &ls), 0);
        assert_eq!(&ifr2[..6], b"wifi0\0");
        ifr2[..5].copy_from_slice(b"nope0");
        assert_eq!(ifreq_ioctl(SIOCGIFFLAGS, &mut ifr2, &ls), -19);
        assert_eq!(ifreq_ioctl(SIOCETHTOOL, &mut ifr2, &ls), -95);
        let c = ifconf(&ls, 4096);
        assert_eq!(c.len(), 2 * IFREQ_LEN);
        assert_eq!(&c[..3], b"lo\0");
        assert_eq!(ifconf(&ls, 40).len(), 40);
    }
}
