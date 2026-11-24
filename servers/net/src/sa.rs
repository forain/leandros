//! The only place that reads and writes inet sockaddrs (`sockaddr_in`,
//! `sockaddr_in6`). Endpoints are native inside the server (`IpAddress` is
//! v4 or v6); an AF_INET6 socket sees a v4 endpoint as `::ffff:a.b.c.d`.
//! Every function touches user memory: call with NO server lock held.

use smoltcp::wire::{IpAddress, IpEndpoint, Ipv4Address, Ipv6Address};
use crate::{AF_INET, AF_INET6};

pub const SIN_LEN: usize = 16;
pub const SIN6_LEN: usize = 28;
/// Linux SIN6_LEN_RFC2133: the shortest acceptable sockaddr_in6.
const SIN6_MIN: usize = 24;

pub struct SockAddr {
    pub ip: IpAddress,
    pub port: u16,
    pub scope: u32,
}

/// Decode a v6 address; `::ffff:a.b.c.d` becomes the v4 address.
pub fn from_v6(b: [u8; 16]) -> IpAddress {
    if b[..10] == [0u8; 10] && b[10] == 0xff && b[11] == 0xff {
        IpAddress::Ipv4(Ipv4Address([b[12], b[13], b[14], b[15]]))
    } else {
        IpAddress::Ipv6(Ipv6Address(b))
    }
}

/// Parse the sockaddr at `ptr` (`len` bytes). The family inside the
/// sockaddr decides the layout; `Err(-97)` when it is neither AF_INET nor
/// AF_INET6, `Err(-22)` when too short, `Err(-14)` on a fault.
pub fn parse(ptr: usize, len: usize) -> Result<SockAddr, i32> {
    if len < 2 { return Err(-22); }
    let fam = sched::uaccess::read_user::<u16>(ptr).ok_or(-14)? as usize;
    match fam {
        AF_INET => {
            if len < 8 { return Err(-22); }
            let mut b = [0u8; 8];
            if !crate::ucopy_in(&mut b, ptr) { return Err(-14); }
            Ok(SockAddr {
                ip: IpAddress::Ipv4(Ipv4Address([b[4], b[5], b[6], b[7]])),
                port: u16::from_be_bytes([b[2], b[3]]),
                scope: 0,
            })
        }
        AF_INET6 => {
            if len < SIN6_MIN { return Err(-22); }
            let mut b = [0u8; SIN6_MIN];
            if !crate::ucopy_in(&mut b, ptr) { return Err(-14); }
            let mut a = [0u8; 16];
            a.copy_from_slice(&b[8..24]);
            let scope = if len >= SIN6_LEN {
                sched::uaccess::read_user::<u32>(ptr + 24).ok_or(-14)?
            } else { 0 };
            Ok(SockAddr { ip: from_v6(a), port: u16::from_be_bytes([b[2], b[3]]), scope })
        }
        _ => Err(-97),
    }
}

/// Parse and check the address family against the socket's domain (bind,
/// connect: Linux EAFNOSUPPORT for a sockaddr of the other family).
pub fn parse_for(domain: usize, ptr: usize, len: usize) -> Result<SockAddr, i32> {
    let a = parse(ptr, len)?;
    let fam = sched::uaccess::read_user::<u16>(ptr).ok_or(-14)? as usize;
    if fam != domain { return Err(-97); }
    Ok(a)
}

/// Build the sockaddr for `ep` as seen by a socket of `domain`.
pub fn encode(domain: usize, ip: IpAddress, port: u16, scope: u32) -> ([u8; SIN6_LEN], usize) {
    let mut sa = [0u8; SIN6_LEN];
    // Linux fills sin6_scope_id for a link-local peer (fe80::/10): the interface it was heard on.
    let scope = match ip {
        IpAddress::Ipv6(a) if scope == 0 && domain == AF_INET6 && a.0[0] == 0xfe && a.0[1] & 0xc0 == 0x80 => crate::ip6::ll_scope_of(&a.0),
        _ => scope,
    };
    if domain == AF_INET6 {
        sa[0..2].copy_from_slice(&(AF_INET6 as u16).to_ne_bytes());
        sa[2..4].copy_from_slice(&port.to_be_bytes());
        match ip {
            IpAddress::Ipv6(a) => sa[8..24].copy_from_slice(&a.0),
            IpAddress::Ipv4(a) if a.0 == [0; 4] => {}
            IpAddress::Ipv4(a) => {
                sa[18] = 0xff; sa[19] = 0xff;
                sa[20..24].copy_from_slice(&a.0);
            }
        }
        sa[24..28].copy_from_slice(&scope.to_ne_bytes());
        (sa, SIN6_LEN)
    } else {
        sa[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
        sa[2..4].copy_from_slice(&port.to_be_bytes());
        if let IpAddress::Ipv4(a) = ip { sa[4..8].copy_from_slice(&a.0); }
        (sa, SIN_LEN)
    }
}

/// Store the sockaddr for `ep` and its length. `addrlen_ptr` is in/out: its
/// incoming value caps the bytes written, it always gets the full length.
pub fn write(domain: usize, addr_ptr: usize, addrlen_ptr: usize, ep: IpEndpoint) -> bool {
    let (sa, n) = encode(domain, ep.addr, ep.port, 0);
    let cap = match sched::uaccess::read_user::<u32>(addrlen_ptr) { Some(c) => c as usize, None => return false };
    crate::ucopy_out(addr_ptr, &sa[..cap.min(n)]) && sched::uaccess::write_user(addrlen_ptr, n as u32)
}
