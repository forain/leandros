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
const RTM_SETLINK: u16 = 19;
const RTM_DELLINK: u16 = 17;
const IFLA_LINK: u16 = 5;
const IFLA_NET_NS_PID: u16 = 19;
const IFLA_NET_NS_FD: u16 = 28;
const IFLA_INFO_DATA: u16 = 2;
const VETH_INFO_PEER: u16 = 1;
const RTM_DELADDR: u16 = 21;
const RTM_DELROUTE: u16 = 25;
const IFLA_TXQLEN: u16 = 13;
const IFLA_LINKINFO: u16 = 18;
const IFLA_MASTER: u16 = 10;
const IFLA_BR_FORWARD_DELAY: u16 = 1;
const IFLA_BR_AGEING_TIME: u16 = 4;
const IFLA_BR_STP_STATE: u16 = 5;
const IFLA_BR_VLAN_FILTERING: u16 = 7;
const NLA_F_NESTED: u16 = 0x8000;
const IFLA_INFO_KIND: u16 = 1;
const RTA_DST: u16 = 1;
const NLM_F_REPLACE: u16 = 0x100;
const NLM_F_CREATE: u16 = 0x400;
const RTM_GETADDR: u16 = 22;
const RTM_GETROUTE: u16 = 26;
const RTM_NEWROUTE: u16 = 24;
const RTM_GETNEIGH: u16 = 30;
const RTA_OIF: u16 = 4;
const RTA_GATEWAY: u16 = 5;
const RTA_PREFSRC: u16 = 7;
const RTA_PRIORITY: u16 = 6;
const RTA_TABLE: u16 = 15;
const NLM_F_ACK: u16 = 0x4;
const NLM_F_EXCL: u16 = 0x200;
const RT_SCOPE_LINK: u8 = 253;
const RTPROT_KERNEL: u8 = 2;
const RTPROT_BOOT: u8 = 3;
const RT_TABLE_MAIN: u8 = 254;
const RTN_UNICAST: u8 = 1;

const NLM_F_REQUEST: u16 = 0x1;
const NLM_F_MULTI: u16 = 0x2;
const NLM_F_DUMP: u16 = 0x300;
const AF_INET6: u8 = 10;

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
    /// A tun/tap device (IFLA_INFO_KIND "tun").
    pub tap: bool,
    /// A veth end (IFLA_INFO_KIND "veth"; also `tap`, i.e. dynamic and writable).
    pub veth: bool,
    /// Veth: the other end's ifindex (IFLA_LINK), else 0.
    pub peer: u32,
    /// The ifindex of the bridge this link is enslaved to (IFLA_MASTER), else 0.
    pub master: u32,
    /// A bridge (IFLA_INFO_KIND "bridge"): (ageing, forward_delay, stp_state, vlan_filtering).
    pub br: Option<(u32, u32, u32, u8)>,
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
    attr(&mut v, IFLA_TXQLEN, &1000u32.to_ne_bytes());
    if l.veth && l.peer != 0 { attr(&mut v, IFLA_LINK, &l.peer.to_ne_bytes()); }
    if l.master != 0 { attr(&mut v, IFLA_MASTER, &l.master.to_ne_bytes()); }
    if l.tap {
        let mut li = Vec::new();
        attr_str(&mut li, IFLA_INFO_KIND, if l.br.is_some() { "bridge" } else if l.veth { "veth" } else { "tun" });
        if let Some((ageing, fwd, stp, vlan)) = l.br {
            let mut d = Vec::new();
            attr(&mut d, IFLA_BR_FORWARD_DELAY, &fwd.to_ne_bytes());
            attr(&mut d, IFLA_BR_AGEING_TIME, &ageing.to_ne_bytes());
            attr(&mut d, IFLA_BR_STP_STATE, &stp.to_ne_bytes());
            attr(&mut d, IFLA_BR_VLAN_FILTERING, &[vlan]);
            attr(&mut li, IFLA_INFO_DATA | NLA_F_NESTED, &d);
        }
        attr(&mut v, IFLA_LINKINFO, &li);
    }
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

/// One entry of the main routing table, as the GETROUTE dump exports it.
#[derive(Clone, Debug, PartialEq)]
pub struct RouteEnt {
    pub dst: [u8; 4],
    pub prefix: u8,
    pub gw: Option<[u8; 4]>,
    pub oif: u32,
    pub prefsrc: Option<[u8; 4]>,
}

/// Everything a request is answered from. `lo_set(up)` applies an admin
/// up/down to the caller namespace's loopback and returns false when the
/// namespace cannot change it (the initial one).
pub struct Env<'a> {
    pub links: &'a [Link],
    pub routes: &'a [RouteEnt],
    pub route: &'a dyn Fn([u8; 4]) -> Option<RouteHit>,
    pub lo_set: &'a dyn Fn(bool) -> bool,
    /// Applies a parsed write to an interface of the caller's namespace;
    /// 0 or a positive errno.
    pub apply: &'a dyn Fn(RtOp) -> i32,
    /// The caller holds CAP_NET_ADMIN over the namespace (D7).
    pub admin: bool,
}

/// A rtnetlink write, parsed and validated by [`parse_write`].
#[derive(Clone, Debug, PartialEq)]
pub enum RtOp {
    SetLink { index: u32, up: Option<bool>, mtu: Option<u32>, mac: Option<[u8; 6]>, name: Option<String>, master: Option<u32> },
    AddAddr { index: u32, addr: [u8; 4], prefix: u8, excl: bool },
    DelAddr { index: u32, addr: [u8; 4], prefix: u8 },
    /// RTM_NEWLINK kind=veth with VETH_INFO_PEER.
    NewVeth { name: Option<String>, mtu: Option<u32>, mac: Option<[u8; 6]>, up: bool, master: Option<u32>, peer: VethPeer },
    /// RTM_NEWLINK kind=bridge.
    NewBridge { name: Option<String>, mtu: Option<u32>, mac: Option<[u8; 6]>, up: bool, opts: BrOpts },
    /// RTM_DELLINK of a veth end: removes both ends.
    DelLink { index: u32 },
    AddRoute { dst: [u8; 4], prefix: u8, gw: Option<[u8; 4]>, oif: u32, metric: u32, excl: bool, replace: bool },
    DelRoute { dst: [u8; 4], prefix: u8, gw: Option<[u8; 4]>, oif: u32, metric: Option<u32> },
}

/// Where a veth peer end goes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PeerNs { Same, Fd(i32), Pid(u32) }

#[derive(Clone, Debug, PartialEq)]
pub struct VethPeer { pub name: Option<String>, pub mtu: Option<u32>, pub mac: Option<[u8; 6]>, pub up: bool, pub ns: PeerNs, pub master: Option<u32> }

/// IFLA_BR_* options of a bridge create request.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct BrOpts { pub ageing: Option<u32>, pub fwd_delay: Option<u32>, pub stp: Option<u32>, pub vlan: Option<u8> }

fn a4(a: Option<&[u8]>) -> Option<[u8; 4]> {
    let a = a?;
    if a.len() == 4 { Some([a[0], a[1], a[2], a[3]]) } else { None }
}

/// Parse an `RTM_NEWADDR/DELADDR/NEWROUTE/DELROUTE` message into a [`RtOp`].
/// `Err` is a positive errno.
pub fn parse_write(msg: &[u8], ty: u16, flags: u16) -> Result<RtOp, i32> {
    match ty {
        RTM_NEWADDR | RTM_DELADDR => {
            if msg.len() < 24 { return Err(22); }
            if msg[16] != AF_INET { return Err(97); }
            let addr = a4(find_attr(msg, 8, IFA_LOCAL)).or_else(|| a4(find_attr(msg, 8, IFA_ADDRESS))).ok_or(22)?;
            let (prefix, index) = (msg[17], rd32(msg, 20));
            if prefix > 32 { return Err(22); }
            Ok(if ty == RTM_NEWADDR { RtOp::AddAddr { index, addr, prefix, excl: flags & NLM_F_EXCL != 0 } }
               else { RtOp::DelAddr { index, addr, prefix } })
        }
        _ => {
            if msg.len() < 28 { return Err(22); }
            if msg[16] != AF_INET { return Err(97); }
            let prefix = msg[17];
            if prefix > 32 { return Err(22); }
            let table = find_attr(msg, 12, RTA_TABLE).filter(|t| t.len() == 4).map_or(msg[20] as u32, |t| rd32(t, 0));
            if table != RT_TABLE_MAIN as u32 && table != 0 { return Err(95); }
            if msg[23] != RTN_UNICAST && ty == RTM_NEWROUTE { return Err(95); }
            let dst = if prefix == 0 { [0; 4] } else { a4(find_attr(msg, 12, RTA_DST)).ok_or(22)? };
            let m = mask(prefix);
            let dst = [dst[0] & m[0], dst[1] & m[1], dst[2] & m[2], dst[3] & m[3]];
            let gw = a4(find_attr(msg, 12, RTA_GATEWAY));
            let oif = find_attr(msg, 12, RTA_OIF).filter(|o| o.len() == 4).map_or(0, |o| rd32(o, 0));
            let prio = find_attr(msg, 12, RTA_PRIORITY).filter(|o| o.len() == 4).map(|o| rd32(o, 0));
            Ok(if ty == RTM_NEWROUTE {
                RtOp::AddRoute { dst, prefix, gw, oif, metric: prio.unwrap_or(0),
                                 excl: flags & NLM_F_EXCL != 0, replace: flags & NLM_F_REPLACE != 0 }
            } else { RtOp::DelRoute { dst, prefix, gw, oif, metric: prio } })
        }
    }
}

fn route_ent_msg(r: &RouteEnt, seq: u32, pid: u32) -> Vec<u8> {
    let mut v = Vec::new();
    let s = begin_msg(&mut v, RTM_NEWROUTE, NLM_F_MULTI, seq, pid);
    v.extend_from_slice(&[AF_INET, r.prefix, 0, 0, RT_TABLE_MAIN,
        if r.gw.is_some() { RTPROT_BOOT } else { RTPROT_KERNEL },
        if r.gw.is_some() { RT_SCOPE_UNIVERSE } else { RT_SCOPE_LINK }, RTN_UNICAST]);
    put32(&mut v, 0);
    attr(&mut v, RTA_TABLE, &(RT_TABLE_MAIN as u32).to_ne_bytes());
    if r.prefix != 0 { attr(&mut v, RTA_DST, &r.dst); }
    if let Some(g) = r.gw { attr(&mut v, RTA_GATEWAY, &g); }
    attr(&mut v, RTA_OIF, &r.oif.to_ne_bytes());
    attr(&mut v, RTA_PRIORITY, &0u32.to_ne_bytes());
    if let Some(p) = r.prefsrc { attr(&mut v, RTA_PREFSRC, &p); }
    end_msg(&mut v, s);
    v
}

/// The attribute `ty` of `msg` (an rtnl message whose fixed part is `fixed`
/// bytes after the 16-byte header).
fn find_attr(msg: &[u8], fixed: usize, ty: u16) -> Option<&[u8]> {
    nla_find(msg.get(16 + fixed..)?, ty)
}

/// The attribute `ty` in a bare attribute list (a nested attribute's body).
fn nla_find(b: &[u8], ty: u16) -> Option<&[u8]> {
    let mut o = 0;
    while o + 4 <= b.len() {
        let l = rd16(b, o) as usize;
        if l < 4 || o + l > b.len() { break; }
        if rd16(b, o + 2) & 0x3fff == ty { return Some(&b[o + 4..o + l]); }
        o += (l + 3) & !3;
    }
    None
}

fn nla_name(a: Option<&[u8]>) -> Option<String> {
    let n = a?;
    let n = &n[..n.iter().position(|&b| b == 0).unwrap_or(n.len())];
    core::str::from_utf8(n).ok().map(String::from)
}

/// The end-specific attributes of a veth request (the main message or the
/// VETH_INFO_PEER body after its ifinfomsg): (name, mtu, mac, up, ns).
fn veth_end(b: &[u8], ifi_flags: u32, ifi_change: u32) -> Result<VethPeer, i32> {
    let mtu = match nla_find(b, IFLA_MTU) { Some(m) if m.len() >= 4 => Some(rd32(m, 0)), Some(_) => return Err(22), None => None };
    let mac = match nla_find(b, IFLA_ADDRESS) {
        Some(a) if a.len() == 6 => Some([a[0], a[1], a[2], a[3], a[4], a[5]]), Some(_) => return Err(22), None => None,
    };
    let ns = if let Some(f) = nla_find(b, IFLA_NET_NS_FD) {
        if f.len() < 4 { return Err(22); }
        PeerNs::Fd(rd32(f, 0) as i32)
    } else if let Some(p) = nla_find(b, IFLA_NET_NS_PID) {
        if p.len() < 4 { return Err(22); }
        PeerNs::Pid(rd32(p, 0))
    } else { PeerNs::Same };
    let master = match nla_find(b, IFLA_MASTER) { Some(m) if m.len() >= 4 => Some(rd32(m, 0)), Some(_) => return Err(22), None => None };
    Ok(VethPeer { name: nla_name(nla_find(b, IFLA_IFNAME)), mtu, mac, up: ifi_change & IFF_UP != 0 && ifi_flags & IFF_UP != 0, ns, master })
}

/// RTM_NEWLINK with NLM_F_CREATE naming a link kind. `None` = not a create
/// request for a kind handled here; `Some(errno)` = handled.
fn create_link(msg: &[u8], flags: u16, env: &Env) -> Option<i32> {
    if flags & NLM_F_CREATE == 0 { return None; }
    let li = find_attr(msg, 16, IFLA_LINKINFO)?;
    let kind = nla_find(li, IFLA_INFO_KIND)?;
    let is_br = cstr_eq(kind, "bridge");
    if !cstr_eq(kind, "veth") && !is_br { return None; }
    let name = nla_name(find_attr(msg, 16, IFLA_IFNAME));
    if let Some(n) = name.as_ref() { if env.links.iter().any(|l| l.name == *n) { return Some(17); } }
    let (f, c) = (rd32(msg, 24), rd32(msg, 28));
    if is_br {
        let me = match veth_end(&msg[16 + 16..], f, c) { Ok(e) => e, Err(e) => return Some(e) };
        let mut opts = BrOpts::default();
        if let Some(d) = nla_find(li, IFLA_INFO_DATA) {
            let u32a = |t: u16| nla_find(d, t).filter(|a| a.len() == 4).map(|a| rd32(a, 0));
            opts.ageing = u32a(IFLA_BR_AGEING_TIME);
            opts.fwd_delay = u32a(IFLA_BR_FORWARD_DELAY);
            opts.stp = u32a(IFLA_BR_STP_STATE);
            opts.vlan = nla_find(d, IFLA_BR_VLAN_FILTERING).filter(|a| a.len() == 1).map(|a| a[0]);
        }
        return Some((env.apply)(RtOp::NewBridge { name, mtu: me.mtu, mac: me.mac, up: me.up, opts }));
    }
    let me = match veth_end(&msg[16 + 16..], f, c) { Ok(e) => e, Err(e) => return Some(e) };
    // VETH_INFO_PEER: a full ifinfomsg, then the peer's attributes.
    let Some(data) = nla_find(li, IFLA_INFO_DATA) else { return Some(22) };
    let peer = match nla_find(data, VETH_INFO_PEER) {
        Some(p) if p.len() >= 16 => match veth_end(&p[16..], rd32(p, 8), rd32(p, 12)) { Ok(e) => e, Err(e) => return Some(e) },
        Some(_) => return Some(22),
        None => VethPeer { name: None, mtu: None, mac: None, up: false, ns: PeerNs::Same, master: None },
    };
    if let (Some(a), PeerNs::Same) = (name.as_ref(), peer.ns) { if peer.name.as_ref() == Some(a) { return Some(17); } }
    Some((env.apply)(RtOp::NewVeth { name, mtu: me.mtu, mac: me.mac, up: me.up, master: me.master, peer }))
}

fn cstr_eq(a: &[u8], name: &str) -> bool {
    let n = a.iter().position(|&b| b == 0).unwrap_or(a.len());
    &a[..n] == name.as_bytes()
}

/// The link a GETLINK/NEWLINK request names (ifi_index, else IFLA_IFNAME).
fn target_link<'a>(msg: &[u8], links: &'a [Link]) -> Option<&'a Link> {
    if msg.len() < 32 { return None; }
    let idx = rd32(msg, 20);
    if idx != 0 { return links.iter().find(|l| l.index == idx); }
    let n = find_attr(msg, 16, IFLA_IFNAME)?;
    links.iter().find(|l| cstr_eq(n, &l.name))
}

/// RTM_NEWLINK change request. 0 = acked, else a positive errno.
fn new_link(msg: &[u8], flags_nl: u16, env: &Env) -> i32 {
    if let Some(e) = create_link(msg, flags_nl, env) { return e; }
    let Some(t) = target_link(msg, env.links) else { return 19 }; // ENODEV
    let flags = rd32(msg, 24);
    let change = rd32(msg, 28);
    if t.tap {
        let up = if change & IFF_UP != 0 { Some(flags & IFF_UP != 0) } else { None };
        let mtu = match find_attr(msg, 16, IFLA_MTU) {
            Some(m) if m.len() >= 4 => Some(rd32(m, 0)), Some(_) => return 22, None => None,
        };
        let mac = match find_attr(msg, 16, IFLA_ADDRESS) {
            Some(a) if a.len() == 6 => Some([a[0], a[1], a[2], a[3], a[4], a[5]]), Some(_) => return 22, None => None,
        };
        let name = find_attr(msg, 16, IFLA_IFNAME).and_then(|n| {
            let n = &n[..n.iter().position(|&b| b == 0).unwrap_or(n.len())];
            core::str::from_utf8(n).ok().map(String::from)
        }).filter(|n| *n != t.name);
        let master = match find_attr(msg, 16, IFLA_MASTER) { Some(m) if m.len() >= 4 => Some(rd32(m, 0)), Some(_) => return 22, None => None };
        return (env.apply)(RtOp::SetLink { index: t.index, up, mtu, mac, name, master });
    }
    if let Some(m) = find_attr(msg, 16, IFLA_MTU) {
        if m.len() < 4 || rd32(m, 0) != t.mtu { return 95; }
    }
    if let Some(n) = find_attr(msg, 16, IFLA_IFNAME) {
        if !cstr_eq(n, &t.name) { return 95; }
    }
    if let Some(a) = find_attr(msg, 16, IFLA_ADDRESS) {
        if a != &t.mac[..] { return 95; }
    }
    if change & IFF_UP != 0 {
        let want = flags & IFF_UP != 0;
        if want != t.up {
            if !(t.loopback && (env.lo_set)(want)) { return 95; }
        }
    }
    0
}

/// RTM_NEWADDR: only the address lo already carries is accepted.
fn new_addr(msg: &[u8], flags: u16, env: &Env) -> i32 {
    if msg.len() < 24 || msg[16] != AF_INET { return 97; } // EAFNOSUPPORT
    let idx = rd32(msg, 20);
    let Some(t) = env.links.iter().find(|l| l.index == idx) else { return 19 };
    if t.tap {
        return match parse_write(msg, RTM_NEWADDR, flags) { Ok(op) => (env.apply)(op), Err(e) => e };
    }
    let a = find_attr(msg, 8, IFA_LOCAL).or_else(|| find_attr(msg, 8, IFA_ADDRESS));
    match (a, t.addr) {
        (Some(a), Some((cur, p))) if a.len() == 4 && a == &cur[..] && msg[17] == p => {
            if flags & NLM_F_EXCL != 0 { 17 } else { 0 } // EEXIST
        }
        _ => 95,
    }
}

/// [`handle_request`] with a route lookup for non-dump RTM_GETROUTE.
pub fn handle_request_r(req: &[u8], links: &[Link], pid: u32,
                        route: &dyn Fn([u8; 4]) -> Option<RouteHit>) -> Vec<Vec<u8>> {
    handle_request_env(req, pid, &Env { links, routes: &[], route, lo_set: &|_| false, apply: &|_| 95, admin: true })
}

/// Answer the netlink request(s) in `req` from `env`.
pub fn handle_request_env(req: &[u8], pid: u32, env: &Env) -> Vec<Vec<u8>> {
    let links = env.links;
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut off = 0usize;
    while off + 16 <= req.len() {
        let len = rd32(req, off) as usize;
        if len < 16 || off + len > req.len() { break; }
        let ty = rd16(req, off + 4);
        let flags = rd16(req, off + 6);
        let seq = rd32(req, off + 8);
        let msg = &req[off..off + len];
        // Linux: ROOT or MATCH alone is a dump, but only for GET requests (RTM_GET* = 2 mod 4);
        // on NEW/DEL the same bits are REPLACE/EXCL and must not read as a dump.
        let dump = flags & NLM_F_DUMP != 0 && ty >= 16 && ty & 3 == 2;
        let fam = if len > 16 { msg[16] } else { AF_UNSPEC };
        let mut ack = flags & NLM_F_ACK != 0 && !dump;
        match ty {
            RTM_GETLINK if dump => {
                // IFLA_MASTER filter: only the links enslaved to that bridge.
                let filt = if len >= 32 { find_attr(msg, 16, IFLA_MASTER).filter(|m| m.len() >= 4).map(|m| rd32(m, 0)) } else { None };
                let msgs = links.iter().filter(|l| filt.map_or(true, |m| l.master == m)).map(|l| link_msg(l, seq, pid)).collect();
                pack(msgs, &mut out);
                out.push(done_msg(seq, pid));
            }
            RTM_GETLINK => match target_link(msg, links) {
                Some(l) => {
                    let mut m = link_msg(l, seq, pid);
                    m[6..8].copy_from_slice(&0u16.to_ne_bytes()); // not multipart
                    out.push(m);
                }
                None => { out.push(error_msg(19, msg, seq, pid)); ack = false; } // ENODEV
            },
            RTM_GETADDR if dump => {
                let want_idx = if len >= 24 { rd32(msg, 20) } else { 0 };
                let msgs = links.iter()
                    .filter(|l| want_idx == 0 || l.index == want_idx)
                    .filter(|_| fam == AF_UNSPEC || fam == AF_INET)
                    .filter_map(|l| l.addr.map(|(a, p)| addr_msg(l, a, p, seq, pid)))
                    .collect();
                pack(msgs, &mut out);
                out.push(done_msg(seq, pid));
            }
            RTM_GETROUTE if dump => {
                let msgs = env.routes.iter()
                    .filter(|_| fam == AF_UNSPEC || fam == AF_INET)
                    .map(|r| route_ent_msg(r, seq, pid)).collect();
                pack(msgs, &mut out);
                out.push(done_msg(seq, pid));
            }
            RTM_GETROUTE => match req_dst(msg).and_then(|d| (env.route)(d).map(|h| (d, h))) {
                Some((d, h)) => out.push(route_msg(d, h, links, seq, pid)),
                None => { out.push(error_msg(101, msg, seq, pid)); ack = false; } // ENETUNREACH
            },
            RTM_GETNEIGH if dump => out.push(done_msg(seq, pid)),
            RTM_NEWLINK | RTM_SETLINK | RTM_DELLINK | RTM_NEWADDR | RTM_DELADDR | RTM_NEWROUTE | RTM_DELROUTE
                if flags & NLM_F_REQUEST != 0 && !env.admin => {
                out.push(error_msg(1, msg, seq, pid)); ack = false; // EPERM
            }
            RTM_DELLINK if flags & NLM_F_REQUEST != 0 => {
                let e = if len < 32 { 22 } else {
                    match target_link(msg, links) { Some(t) if t.veth || t.br.is_some() => (env.apply)(RtOp::DelLink { index: t.index }), Some(_) => 95, None => 19 }
                };
                if e != 0 { out.push(error_msg(e, msg, seq, pid)); ack = false; }
            }
            RTM_NEWLINK | RTM_SETLINK if flags & NLM_F_REQUEST != 0 => {
                let e = if len < 32 { 22 } else { new_link(msg, flags, env) };
                if e != 0 { out.push(error_msg(e, msg, seq, pid)); ack = false; }
            }
            RTM_NEWADDR if flags & NLM_F_REQUEST != 0 => {
                let e = new_addr(msg, flags, env);
                if e != 0 { out.push(error_msg(e, msg, seq, pid)); ack = false; }
            }
            RTM_DELADDR if flags & NLM_F_REQUEST != 0 => {
                let e = match parse_write(msg, ty, flags) {
                    Ok(op @ RtOp::DelAddr { index, .. }) if links.iter().any(|l| l.index == index && l.tap) => (env.apply)(op),
                    Ok(RtOp::DelAddr { index, .. }) if !links.iter().any(|l| l.index == index) => 19,
                    Ok(_) => 95,
                    Err(e) => e,
                };
                if e != 0 { out.push(error_msg(e, msg, seq, pid)); ack = false; }
            }
            RTM_NEWROUTE | RTM_DELROUTE if flags & NLM_F_REQUEST != 0 => {
                let e = match parse_write(msg, ty, flags) { Ok(op) => (env.apply)(op), Err(e) => e };
                if e != 0 { out.push(error_msg(e, msg, seq, pid)); ack = false; }
            }
            // Control messages (NLMSG_NOOP..DONE) are ignored, as in Linux.
            t if t < 16 => ack = false,
            _ if flags & NLM_F_REQUEST != 0 => { out.push(error_msg(95, msg, seq, pid)); ack = false; } // EOPNOTSUPP
            _ => ack = false,
        }
        if ack { out.push(error_msg(0, msg, seq, pid)); }
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
pub const SIOCSIFFLAGS: u32 = 0x8914;
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
    matches!(cmd, SIOCGIFNAME | SIOCGIFCONF | SIOCGIFFLAGS | SIOCSIFFLAGS | 0x8916 | 0x891c | 0x8922 | SIOCGIFADDR | SIOCGIFBRDADDR
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
    let sid = super::iface::route_select(0, IpAddress::Ipv4(Ipv4Address::from_bytes(&d)))?;
    let gw = super::iface::snapshot().into_iter().find(|s| s.id as u32 + 1 == sid as u32)
        .and_then(|s| match (s.addr, s.gateway) {
            (Some((a, p)), Some(g)) if !super::iface::IfCfg { addr: a, prefix: p, gateway: None }.on_link(d) => Some(g),
            _ => None,
        });
    Some((sid as u32 + 1, gw))
}

/// The links `ns` sees. The initial namespace has `lo` and every NIC; any
/// other has only its own `lo`, whose admin state is recorded by
/// `RTM_NEWLINK` (no stack behind it yet).
#[cfg(not(test))]
pub fn current_links(ns: u16) -> Vec<Link> {
    let mut v = Vec::new();
    if ns != 0 {
        let up = sched::ns::lo_admin_up(ns);
        v.push(Link {
            index: 1, name: String::from("lo"), mac: [0; 6], mtu: 65536,
            up, running: up, tap: false, veth: false, peer: 0, master: 0, br: None, loopback: true,
            addr: if up { Some(([127, 0, 0, 1], 8)) } else { None },
        });
        v.extend(super::tunbe::links(ns));
        return v;
    }
    v.push(Link {
        index: 1, name: String::from("lo"), mac: [0; 6], mtu: 65536,
        up: true, running: true, tap: false, veth: false, peer: 0, master: 0, br: None, loopback: true, addr: Some(([127, 0, 0, 1], 8)),
    });
    for s in super::iface::snapshot() {
        v.push(Link {
            index: s.id as u32 + 2,
            name: s.name.clone(),
            mac: s.mac.unwrap_or([0; 6]),
            mtu: 1500,
            up: s.admin_up,
            running: s.carrier && s.admin_up,
            tap: false, veth: false, peer: 0, master: 0, br: None, loopback: false,
            addr: if s.admin_up { s.addr } else { None },
        });
    }
    v.extend(super::tunbe::links(0));
    v
}

/// The main routing table `ns` exports: per configured NIC its connected
/// route and, when set, the default route. Other namespaces have none.
#[cfg(not(test))]
pub fn current_routes(ns: u16) -> Vec<RouteEnt> {
    let mut v = Vec::new();
    if ns != 0 { return super::tunbe::routes(ns); }
    for s in super::iface::snapshot() {
        let Some((a, p)) = s.addr else { continue };
        if !s.admin_up { continue; }
        let m = mask(p);
        let net = [a[0] & m[0], a[1] & m[1], a[2] & m[2], a[3] & m[3]];
        let oif = s.id as u32 + 2;
        v.push(RouteEnt { dst: net, prefix: p, gw: None, oif, prefsrc: Some(a) });
        if let Some(g) = s.gateway {
            v.push(RouteEnt { dst: [0; 4], prefix: 0, gw: Some(g), oif, prefsrc: None });
        }
    }
    v.extend(super::tunbe::routes(0));
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    fn links() -> Vec<Link> {
        alloc::vec![
            Link { index: 1, name: "lo".into(), mac: [0; 6], mtu: 65536, up: true, running: true,
                   tap: false, veth: false, peer: 0, master: 0, br: None, loopback: true, addr: Some(([127, 0, 0, 1], 8)) },
            Link { index: 2, name: "eth0".into(), mac: [0x52, 0x54, 0, 0x12, 0x34, 0x56], mtu: 1500,
                   up: true, running: true, tap: false, veth: false, peer: 0, master: 0, br: None, loopback: false, addr: Some(([10, 0, 2, 15], 24)) },
            Link { index: 3, name: "wifi0".into(), mac: [2; 6], mtu: 1500, up: true, running: false,
                   tap: false, veth: false, peer: 0, master: 0, br: None, loopback: false, addr: None },
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
                           up: true, running: true, tap: false, veth: false, peer: 0, master: 0, br: None, loopback: false, addr: Some(([10, 0, 0, 1], 24)) });
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
    fn ifinfo_req(ty: u16, flags: u16, index: u32, ifflags: u32, change: u32) -> Vec<u8> {
        let mut v = Vec::new();
        put32(&mut v, 32);
        put16(&mut v, ty);
        put16(&mut v, flags);
        put32(&mut v, 7);
        put32(&mut v, 0);
        v.extend_from_slice(&[0, 0, 0, 0]);
        put32(&mut v, index);
        put32(&mut v, ifflags);
        put32(&mut v, change);
        v
    }

    fn run(req: &[u8], ls: &[Link], routes: &[RouteEnt], lo: bool) -> Vec<(u16, Vec<u8>)> {
        let env = Env { links: ls, routes, route: &|_| None, lo_set: &|_| lo, apply: &|_| 95, admin: true };
        split(&handle_request_env(req, 1, &env).concat())
    }

    #[test]
    fn route_dump_lists_connected_and_default() {
        let routes = alloc::vec![
            RouteEnt { dst: [10, 0, 2, 0], prefix: 24, gw: None, oif: 2, prefsrc: Some([10, 0, 2, 15]) },
            RouteEnt { dst: [0; 4], prefix: 0, gw: Some([10, 0, 2, 2]), oif: 2, prefsrc: None },
        ];
        let m = run(&req(RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP, 1), &links(), &routes, false);
        assert_eq!(m.len(), 3);
        assert_eq!(m[0].0, RTM_NEWROUTE);
        assert_eq!(m[2].0, NLMSG_DONE);
        assert_eq!(m[1].1[1], 0); // default: dst_len 0
        assert_eq!(&attrs(&m[1].1[12..]).iter().find(|a| a.0 == RTA_GATEWAY).unwrap().1[..], &[10, 0, 2, 2]);
        // AF_INET6 dump: DONE only
        let mut r6 = req(RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP, 1);
        r6[16] = AF_INET6;
        assert_eq!(run(&r6, &links(), &routes, false).len(), 1);
    }

    #[test]
    fn getlink_by_index_and_ack() {
        let ls = links();
        let m = run(&ifinfo_req(RTM_GETLINK, NLM_F_REQUEST | NLM_F_ACK, 3, 0, 0), &ls, &[], false);
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].0, RTM_NEWLINK);
        assert_eq!(rd32(&m[0].1, 4), 3);
        assert_eq!(m[1].0, NLMSG_ERROR);
        assert_eq!(rd32(&m[1].1, 0), 0);
        let m = run(&ifinfo_req(RTM_GETLINK, NLM_F_REQUEST, 9, 0, 0), &ls, &[], false);
        assert_eq!((m.len(), m[0].0, rd32(&m[0].1, 0) as i32), (1, NLMSG_ERROR, -19));
    }

    #[test]
    fn newlink_lo_up_acked_only_when_allowed() {
        let mut ls = links();
        ls[0].up = false;
        let up = ifinfo_req(RTM_NEWLINK, NLM_F_REQUEST | NLM_F_ACK, 1, IFF_UP, IFF_UP);
        let m = run(&up, &ls, &[], true);
        assert_eq!((m.len(), rd32(&m[0].1, 0)), (1, 0));
        let m = run(&up, &ls, &[], false);
        assert_eq!(rd32(&m[0].1, 0) as i32, -95);
        // already up: no-op ack even where lo cannot be changed
        let m = run(&up, &links(), &[], false);
        assert_eq!(rd32(&m[0].1, 0), 0);
    }
}
