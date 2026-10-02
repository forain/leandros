//! `/proc/net/*` in the Linux formats, so stock tools can read the network
//! state: `route` (busybox/net-tools `route -n`, `netstat -r`, and anything
//! that looks for the default gateway the way those do), `dev` (per-interface
//! counters: `ifconfig`, `netstat -i`, bottom's network graph), `tcp`, `udp`,
//! `unix` (`netstat`, `ss` without netlink) plus header-only `tcp6`, `udp6`,
//! `raw`, `raw6` so a tool that reads the whole family finds every file.
//!
//! The VFS calls [`generate`] (registered with `vfs::set_proc_net_gen`) on
//! every open and parks the bytes in an ephemeral snapshot, like the rest of
//! /proc. Nothing here holds a VFS lock. Lock order is the net server's own:
//! SOCK_TABLES alone (snapshotted, then released), then BOUND_PATHS alone,
//! then one stack at a time.
//!
//! Field notes (all as Linux prints them on a little-endian machine):
//! - IPv4 addresses are `%08X` of the network-order word, i.e. the four octets
//!   read as a little-endian u32: 10.0.2.2 is `0202000A`. Ports are host order.
//! - `inode` is the socket's open-file-description id (shared by dup/fork
//!   copies, like an inode), not a real inode number: there is no sockfs.
//! - `uid` is the effective uid of the process whose table holds the socket.

use alloc::vec::Vec;
use core::fmt::Write;
use core::sync::atomic::Ordering;

use smoltcp::socket::{tcp, udp, AnySocket};
use smoltcp::wire::{IpAddress, IpEndpoint};

use super::*;

/// The NIC's IPv4 configuration as the stack currently uses it: the static
/// pre-lease 10.0.2.15/24 via 10.0.2.2 set by `init`, replaced by the DHCP
/// lease. `None` with no NIC.
#[derive(Clone, Copy)]
pub(crate) struct IfCfg { pub addr: [u8; 4], pub prefix: u8, pub gateway: Option<[u8; 4]> }

pub(crate) static IFCFG: Mutex<Option<IfCfg>> = Mutex::new(None);

/// eth0 counters, bumped by the virtio-net device wrapper.
pub(crate) static RX_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub(crate) static RX_PKTS:  core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub(crate) static RX_DROP:  core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub(crate) static TX_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub(crate) static TX_PKTS:  core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub(crate) static TX_DROP:  core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The interface name everything here reports for the virtio-net NIC.
const IFNAME: &str = "eth0";

/// `name` is the part after "/proc/net/". False for a file this module does
/// not generate (the VFS then falls back to its static table).
pub fn generate(name: &[u8], out: &mut Vec<u8>) -> bool {
    let mut w = Out(out);
    match name {
        b"route" => route(&mut w),
        b"dev"   => dev(&mut w),
        b"tcp"   => inet(&mut w, true),
        b"udp"   => inet(&mut w, false),
        b"unix"  => unix(&mut w),
        b"tcp6" | b"udp6" => {
            let _ = w.write_str("  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n");
        }
        b"raw" | b"raw6" => {
            let _ = w.write_str("  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n");
        }
        _ => return false,
    }
    true
}

struct Out<'a>(&'a mut Vec<u8>);

impl Write for Out<'_> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

impl Out<'_> {
    /// Pad the current line to `width` columns and end it, like seq_pad().
    fn pad_line(&mut self, line_start: usize, width: usize) {
        let len = self.0.len() - line_start;
        for _ in len..width { self.0.push(b' '); }
        self.0.push(b'\n');
    }
}

fn hex_ip(o: [u8; 4]) -> u32 { u32::from_le_bytes(o) }

fn mask_of(prefix: u8) -> [u8; 4] {
    let m: u32 = if prefix == 0 { 0 } else { u32::MAX << (32 - prefix.min(32) as u32) };
    m.to_be_bytes()
}

fn v4(a: IpAddress) -> [u8; 4] {
    #[allow(unreachable_patterns)]
    match a { IpAddress::Ipv4(x) => x.0, _ => [0; 4] }
}

/// /proc/net/route: the main table, which is the default route plus the
/// lease's on-link subnet (the loopback stack's 127/8 is in Linux's local
/// table, not here).
fn route(w: &mut Out) {
    let start = w.0.len();
    let _ = w.write_str("Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT");
    w.pad_line(start, 127);
    let cfg = match *IFCFG.lock() { Some(c) => c, None => return };
    const RTF_UP: u32 = 0x1;
    const RTF_GATEWAY: u32 = 0x2;
    if let Some(gw) = cfg.gateway {
        let start = w.0.len();
        let _ = write!(w, "{}\t{:08X}\t{:08X}\t{:04X}\t0\t0\t0\t{:08X}\t0\t0\t0",
                       IFNAME, 0, hex_ip(gw), RTF_UP | RTF_GATEWAY, 0);
        w.pad_line(start, 127);
    }
    let mask = mask_of(cfg.prefix);
    let mut net = cfg.addr;
    for i in 0..4 { net[i] &= mask[i]; }
    let start = w.0.len();
    let _ = write!(w, "{}\t{:08X}\t{:08X}\t{:04X}\t0\t0\t0\t{:08X}\t0\t0\t0",
                   IFNAME, hex_ip(net), 0, RTF_UP, hex_ip(mask));
    w.pad_line(start, 127);
}

/// /proc/net/dev. lo's traffic never leaves smoltcp's Loopback phy and is not
/// counted; eth0 counts frames at the virtio-net boundary.
fn dev(w: &mut Out) {
    let _ = w.write_str("Inter-|   Receive                                                |  Transmit\n");
    let _ = w.write_str(" face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n");
    let mut line = |name: &str, rb: u64, rp: u64, rd: u64, tb: u64, tp: u64, td: u64| {
        let _ = write!(w, "{:>6}: {:7} {:7} {:4} {:4} {:4} {:5} {:10} {:9} {:8} {:7} {:4} {:4} {:4} {:5} {:7} {:10}\n",
                       name, rb, rp, 0, rd, 0, 0, 0, 0, tb, tp, 0, td, 0, 0, 0, 0);
    };
    line("lo", 0, 0, 0, 0, 0, 0);
    if IFCFG.lock().is_some() {
        line(IFNAME,
             RX_BYTES.load(Ordering::Relaxed), RX_PKTS.load(Ordering::Relaxed), RX_DROP.load(Ordering::Relaxed),
             TX_BYTES.load(Ordering::Relaxed), TX_PKTS.load(Ordering::Relaxed), TX_DROP.load(Ordering::Relaxed));
    }
}

/// One socket-table entry, copied out so no table lock is held while the
/// stacks are consulted.
#[derive(Clone, Copy)]
struct Snap { pid: u32, state: SockState, sock_type: u8, domain: u8, bound_port: u16, ofd: u32 }

/// Every live entry of `domain`, one per open file description (a socket
/// shared by fork or dup appears once, under the first table holding it).
fn snapshot(domain: usize) -> Vec<Snap> {
    let mut v: Vec<Snap> = Vec::new();
    let tbls = SOCK_TABLES.lock();
    for t in tbls.iter() {
        if !t.in_use { continue; }
        for e in t.socks.iter() {
            if !e.in_use || e.domain as usize != domain { continue; }
            if e.ofd != 0 && v.iter().any(|s| s.ofd == e.ofd) { continue; }
            v.push(Snap { pid: t.pid, state: e.state, sock_type: e.sock_type,
                          domain: e.domain, bound_port: e.bound_port, ofd: e.ofd });
        }
    }
    v
}

/// Linux's TCP_* state numbers.
fn tcp_st(s: tcp::State) -> u8 {
    use tcp::State::*;
    match s {
        Established => 0x01, SynSent => 0x02, SynReceived => 0x03, FinWait1 => 0x04,
        FinWait2 => 0x05, TimeWait => 0x06, Closed => 0x07, CloseWait => 0x08,
        LastAck => 0x09, Listen => 0x0A, Closing => 0x0B,
    }
}

struct InetRow { local: ([u8; 4], u16), remote: ([u8; 4], u16), st: u8, txq: usize, rxq: usize, uid: u32, inode: u32 }

/// `(state, local, remote, send_queue, recv_queue)` of the TCP socket behind
/// `h`, found without `SocketSet::get` (which panics on a stale handle).
fn tcp_info(lo: bool, h: SocketHandle) -> Option<(tcp::State, Option<IpEndpoint>, Option<IpEndpoint>, usize, usize)> {
    let stack = stack_for(lo);
    let s = stack.as_ref()?;
    let (_, sock) = s.socket_set.iter().find(|(hh, _)| *hh == h)?;
    let t = tcp::Socket::downcast(sock)?;
    Some((t.state(), t.local_endpoint(), t.remote_endpoint(), t.send_queue(), t.recv_queue()))
}

fn udp_local(lo: bool, h: SocketHandle) -> Option<([u8; 4], u16)> {
    let stack = stack_for(lo);
    let s = stack.as_ref()?;
    let (_, sock) = s.socket_set.iter().find(|(hh, _)| *hh == h)?;
    let u = udp::Socket::downcast(sock)?;
    let ep = u.endpoint();
    Some((ep.addr.map(v4).unwrap_or([0; 4]), ep.port))
}

fn ep4(e: Option<IpEndpoint>) -> ([u8; 4], u16) {
    e.map(|e| (v4(e.addr), e.port)).unwrap_or(([0; 4], 0))
}

/// /proc/net/tcp (`tcp`) or /proc/net/udp.
fn inet(w: &mut Out, tcp_wanted: bool) {
    let start = w.0.len();
    if tcp_wanted {
        let _ = w.write_str("  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode");
        w.pad_line(start, 149);
    } else {
        let _ = w.write_str("   sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops");
        w.pad_line(start, 127);
    }
    let want_type = if tcp_wanted { SOCK_STREAM } else { SOCK_DGRAM } as u8;
    let mut rows: Vec<InetRow> = Vec::new();
    for s in snapshot(AF_INET) {
        if s.sock_type != want_type { continue; }
        let uid = sched::euid_of(s.pid);
        let row = match s.state {
            SockState::InetListening { local, .. } if tcp_wanted => Some(InetRow {
                local: (v4(local.addr), local.port), remote: ([0; 4], 0), st: 0x0A,
                txq: 0, rxq: 0, uid, inode: s.ofd }),
            SockState::InetConnected { socket_handle, lo, .. } if tcp_wanted => {
                tcp_info(lo, socket_handle).map(|(st, l, r, tx, rx)| InetRow {
                    local: ep4(l), remote: ep4(r), st: tcp_st(st), txq: tx, rxq: rx, uid, inode: s.ofd })
            }
            SockState::InetBound { local_endpoint, .. } if !tcp_wanted => Some(InetRow {
                local: (v4(local_endpoint.addr), local_endpoint.port), remote: ([0; 4], 0),
                st: 0x07, txq: 0, rxq: 0, uid, inode: s.ofd }),
            SockState::InetConnected { socket_handle, remote_endpoint, lo } if !tcp_wanted => {
                let local = udp_local(lo, socket_handle).unwrap_or(([0; 4], s.bound_port));
                Some(InetRow { local, remote: ep4(remote_endpoint),
                               st: if remote_endpoint.is_some() { 0x01 } else { 0x07 },
                               txq: 0, rxq: 0, uid, inode: s.ofd })
            }
            _ => None,
        };
        if let Some(r) = row { rows.push(r); }
    }
    if tcp_wanted {
        // Connections whose fd is closed but which are still shutting down
        // (FIN_WAIT, LAST_ACK, ...): Linux lists them too, owned by nobody.
        for lo in [false, true] {
            let handles: Vec<SocketHandle> = match stack_for(lo).as_ref() {
                Some(s) => s.orphans.iter().map(|&(h, _)| h).collect(),
                None => continue,
            };
            for h in handles {
                if let Some((st, l, r, tx, rx)) = tcp_info(lo, h) {
                    rows.push(InetRow { local: ep4(l), remote: ep4(r), st: tcp_st(st),
                                        txq: tx, rxq: rx, uid: 0, inode: 0 });
                }
            }
        }
    }
    for (sl, r) in rows.iter().enumerate() {
        let start = w.0.len();
        if tcp_wanted {
            let _ = write!(w, "{:4}: {:08X}:{:04X} {:08X}:{:04X} {:02X} {:08X}:{:08X} 00:00000000 00000000 {:5} {:8} {} 1 0000000000000000 100 0 0 10 0",
                           sl, hex_ip(r.local.0), r.local.1, hex_ip(r.remote.0), r.remote.1, r.st,
                           r.txq, r.rxq, r.uid, 0, r.inode);
            w.pad_line(start, 149);
        } else {
            let _ = write!(w, "{:5}: {:08X}:{:04X} {:08X}:{:04X} {:02X} {:08X}:{:08X} 00:00000000 00000000 {:5} {:8} {} 2 0000000000000000 0",
                           sl, hex_ip(r.local.0), r.local.1, hex_ip(r.remote.0), r.remote.1, r.st,
                           r.txq, r.rxq, r.uid, 0, r.inode);
            w.pad_line(start, 127);
        }
    }
}

/// /proc/net/unix. Path is the bound address of a listener (`@name` for the
/// abstract namespace); connected ends show none, since which listener
/// accepted them is not recorded per end.
fn unix(w: &mut Out) {
    let _ = w.write_str("Num       RefCount Protocol Flags    Type St Inode Path\n");
    const SS_UNCONNECTED: u8 = 1;
    const SS_CONNECTING: u8 = 2;
    const SS_CONNECTED: u8 = 3;
    const SO_ACCEPTCON: u32 = 0x0001_0000;
    let snaps = snapshot(AF_UNIX);
    let mut rows: Vec<(u32, u16, u8, u32, Option<usize>)> = Vec::new(); // flags, type, st, inode, bound_idx
    for s in snaps.iter() {
        let ty = s.sock_type as u16;
        let row = match s.state {
            SockState::UnixListening { bound_idx } => (SO_ACCEPTCON, ty, SS_UNCONNECTED, s.ofd, Some(bound_idx)),
            SockState::UnixConnected { .. } => (0, ty, SS_CONNECTED, s.ofd, None),
            SockState::UnixPendingAccept { .. } => (0, ty, SS_CONNECTING, s.ofd, None),
            _ => (0, if ty == 0 { s.domain as u16 } else { ty }, SS_UNCONNECTED, s.ofd, None),
        };
        rows.push(row);
    }
    let bound = BOUND_PATHS.lock();
    for (flags, ty, st, inode, bidx) in rows {
        let _ = write!(w, "0000000000000000: {:08X} {:08X} {:08X} {:04X} {:02X} {:5}", 2, 0, flags, ty, st, inode);
        if let Some(b) = bidx.and_then(|i| bound.get(i)).filter(|b| b.in_use) {
            let n = b.path_len.min(PATH_MAX);
            let _ = w.write_str(" ");
            if b.is_abstract {
                w.0.push(b'@');
                // Abstract names are length-delimited; drop the leading NUL.
                let body = if n > 0 && b.path[0] == 0 { &b.path[1..n] } else { &b.path[..n] };
                w.0.extend(body.iter().map(|&c| if c == 0 { b'@' } else { c }));
            } else {
                let end = b.path[..n].iter().position(|&c| c == 0).unwrap_or(n);
                w.0.extend_from_slice(&b.path[..end]);
            }
        }
        w.0.push(b'\n');
    }
}
