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
use super::iface;

/// Net daemon counters (`netd:` line of /proc/net/nic): passes, the most
/// frames one pass took from the NICs, targeted vs broadcast readiness wakes.
pub(crate) static NETD_PASSES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub(crate) static NETD_MAX_RX: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub(crate) static NETD_TAG_WAKES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub(crate) static NETD_BCAST_WAKES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Receive buffer of the last TCP socket created (its advertised window).
pub(crate) static TCP_RXBUF_LAST: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

fn ip4(o: [u8; 4]) -> alloc::string::String { alloc::format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]) }

/// /proc/net/nic (LeandrOS-specific): one section per netif, headed by an
/// `[ifname]` line, in registry order (wired NICs first, so `eth0`'s keys are
/// the first match for a reader that ignores sections): the driver's
/// `key: value` facts, then the interface's routing, DHCP and traffic state.
/// Then the daemon's counters. `driver: none` without a NIC.
fn nic(w: &mut Out) {
    if drivers::netdev::count() == 0 {
        let _ = w.write_str("driver: none\nrealtek: none\nintel: none\n");
    }
    let def = iface::snapshot().iter().find(|i| i.default_route).map(|i| i.id);
    for nif in drivers::netdev::iter() {
        let id = nif.id as usize;
        let _ = writeln!(w, "[{}]", nif.name());
        let mut s = alloc::string::String::new();
        nif.info_lines(&mut s);
        let _ = w.write_str(&s);
        let _ = writeln!(w, "ifname: {}", nif.name());
        let r = iface::rt(nif.id);
        let _ = writeln!(w, "kind: {}", match nif.kind { drivers::netdev::Kind::Wired => "wired", drivers::netdev::Kind::Wifi => "wifi" });
        let _ = writeln!(w, "ifindex: {}", nif.ifindex());
        let _ = writeln!(w, "carrier: {}", if r.carrier { "up" } else { "down" });
        let _ = writeln!(w, "metric: {}", r.metric);
        match r.cfg {
            Some(c) => { let _ = writeln!(w, "ipv4: {}/{}", ip4(c.addr), c.prefix); }
            None => { let _ = writeln!(w, "ipv4: -"); }
        }
        let _ = writeln!(w, "default_route: {}", if def == Some(nif.id) { "yes" } else { "no" });
        let d = iface::DHCP[id].lock();
        match &d.lease {
            Some((a, p, r, dns, t)) => {
                let _ = writeln!(w, "dhcp: bound");
                let _ = writeln!(w, "address: {}/{}", ip4(*a), p);
                let _ = writeln!(w, "gateway: {}", r.map(ip4).unwrap_or_else(|| alloc::string::String::from("-")));
                let list: alloc::vec::Vec<alloc::string::String> = dns.iter().map(|x| ip4(*x)).collect();
                let _ = writeln!(w, "dns: {}", if list.is_empty() { alloc::string::String::from("-") } else { list.join(" ") });
                let _ = writeln!(w, "lease_age_s: {}", sched::ticks().saturating_sub(*t) / 100);
            }
            None => {
                let _ = writeln!(w, "dhcp: searching");
            }
        }
        let _ = writeln!(w, "dhcp_restarts: {} leases: {} lost: {}", d.restarts, d.leases, d.lost);
        drop(d);
        let st = &iface::STATS[id];
        let _ = writeln!(w, "stack_rx: {} packets {} bytes {} dropped {} weak_host", st.rx_pkts.load(Ordering::Relaxed),
                         st.rx_bytes.load(Ordering::Relaxed), st.rx_drop.load(Ordering::Relaxed),
                         st.rx_weak_host.load(Ordering::Relaxed));
        let _ = writeln!(w, "stack_tx: {} packets {} bytes {} dropped {} held", st.tx_pkts.load(Ordering::Relaxed),
                         st.tx_bytes.load(Ordering::Relaxed), st.tx_drop.load(Ordering::Relaxed),
                         st.tx_held.load(Ordering::Relaxed));
    }
    let _ = writeln!(w, "netd: passes {} max_rx_per_pass {} wakes tagged {} broadcast {} rx_irq {} virtio_rx_irqs {} tcp_rx_buf {} netctl_wakes {}",
                     NETD_PASSES.load(Ordering::Relaxed), NETD_MAX_RX.load(Ordering::Relaxed),
                     NETD_TAG_WAKES.load(Ordering::Relaxed), NETD_BCAST_WAKES.load(Ordering::Relaxed),
                     if drivers::netdev::iter().any(|n| n.rx_irq_driven()) { "on" } else { "off" },
                     (0..4).map(drivers::virtio_net::rx_irq_count).sum::<u64>(),
                     TCP_RXBUF_LAST.load(Ordering::Relaxed),
                     crate::netctl::WAKES.load(Ordering::Relaxed));
    let r = &smoltcp::socket::tcp::RECOVERY_STATS;
    let _ = writeln!(w, "tcp_recovery: fast_retransmit {} rto {} partial_ack {} dup_acks {}",
                     r[0].load(Ordering::Relaxed), r[1].load(Ordering::Relaxed),
                     r[2].load(Ordering::Relaxed), r[3].load(Ordering::Relaxed));
    let n = &smoltcp::iface::NEIGHBOR_STATS;
    let _ = writeln!(w, "neighbor: unresolved {} stale_sends {} refresh_requests {} refreshed_by_traffic {}",
                     n[0].load(Ordering::Relaxed), n[1].load(Ordering::Relaxed),
                     n[2].load(Ordering::Relaxed), n[3].load(Ordering::Relaxed));
}

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
        b"nic"   => nic(&mut w),
        b"igmp"  => {
            let mut t = alloc::string::String::new();
            crate::mcast::proc_igmp(&mut t);
            let _ = w.write_str(&t);
        }
        b"wifi"  => {
            let mut t = alloc::string::String::new();
            drivers::iwlwifi::scan_lines(&mut t);
            let _ = w.write_str(&t);
        }
        b"tcp6" | b"udp6" => {
            let _ = w.write_str("  sl  local_address                         remote_address                        st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n");
        }
        b"raw" => raw(&mut w),
        b"raw6" => {
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

/// /proc/net/route: the main table, which is each interface's default route
/// plus its lease's on-link subnet (the loopback stack's 127/8 is in Linux's
/// local table, not here).
fn route(w: &mut Out) {
    let start = w.0.len();
    let _ = w.write_str("Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT");
    w.pad_line(start, 127);
    const RTF_UP: u32 = 0x1;
    const RTF_GATEWAY: u32 = 0x2;
    // Lowest metric first, so a reader taking the first default route (as
    // `nettest` and most scripts do) gets the one new flows use. A netif
    // without carrier is listed last: it takes no new flows.
    let mut nifs: Vec<_> = drivers::netdev::iter().map(|n| (n, iface::rt(n.id))).collect();
    nifs.sort_by_key(|(n, r)| (!r.carrier, r.metric, n.id));
    for (nif, r) in nifs {
        let cfg = match r.cfg { Some(c) => c, None => continue };
        let name = nif.name();
        let metric = r.metric;
        if let Some(gw) = cfg.gateway {
            let start = w.0.len();
            let _ = write!(w, "{}\t{:08X}\t{:08X}\t{:04X}\t0\t0\t{}\t{:08X}\t0\t0\t0",
                           name, 0, hex_ip(gw), RTF_UP | RTF_GATEWAY, metric, 0);
            w.pad_line(start, 127);
        }
        let mask = mask_of(cfg.prefix);
        let mut net = cfg.addr;
        for i in 0..4 { net[i] &= mask[i]; }
        let start = w.0.len();
        let _ = write!(w, "{}\t{:08X}\t{:08X}\t{:04X}\t0\t0\t{}\t{:08X}\t0\t0\t0",
                       name, hex_ip(net), 0, RTF_UP, metric, hex_ip(mask));
        w.pad_line(start, 127);
    }
}

/// /proc/net/dev. lo's traffic never leaves smoltcp's Loopback phy and is not
/// counted; a netif counts frames at the driver boundary.
fn dev(w: &mut Out) {
    let _ = w.write_str("Inter-|   Receive                                                |  Transmit\n");
    let _ = w.write_str(" face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed\n");
    let mut line = |name: &str, rb: u64, rp: u64, rd: u64, tb: u64, tp: u64, td: u64| {
        let _ = write!(w, "{:>6}: {:7} {:7} {:4} {:4} {:4} {:5} {:10} {:9} {:8} {:7} {:4} {:4} {:4} {:5} {:7} {:10}\n",
                       name, rb, rp, 0, rd, 0, 0, 0, 0, tb, tp, 0, td, 0, 0, 0, 0);
    };
    line("lo", 0, 0, 0, 0, 0, 0);
    for nif in drivers::netdev::iter() {
        let st = &iface::STATS[nif.id as usize];
        line(nif.name(),
             st.rx_bytes.load(Ordering::Relaxed), st.rx_pkts.load(Ordering::Relaxed), st.rx_drop.load(Ordering::Relaxed),
             st.tx_bytes.load(Ordering::Relaxed), st.tx_pkts.load(Ordering::Relaxed), st.tx_drop.load(Ordering::Relaxed));
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
fn tcp_info(sid: Sid, h: SocketHandle) -> Option<(tcp::State, Option<IpEndpoint>, Option<IpEndpoint>, usize, usize)> {
    let stack = stack_for(sid);
    let s = stack.as_ref()?;
    let (_, sock) = s.socket_set.iter().find(|(hh, _)| *hh == h)?;
    let t = tcp::Socket::downcast(sock)?;
    Some((t.state(), t.local_endpoint(), t.remote_endpoint(), t.send_queue(), t.recv_queue()))
}

fn udp_local(sid: Sid, h: SocketHandle) -> Option<([u8; 4], u16)> {
    let stack = stack_for(sid);
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
            // rx_queue of a listener: connections waiting for accept(), as
            // on Linux.
            SockState::InetListening { lid, local } if tcp_wanted => Some(InetRow {
                local: (v4(local.addr), local.port), remote: ([0; 4], 0), st: 0x0A,
                txq: 0, rxq: crate::listen::backlog(lid).1, uid, inode: s.ofd }),
            SockState::InetConnected { socket_handle, sid, .. } if tcp_wanted => {
                tcp_info(sid, socket_handle).map(|(st, l, r, tx, rx)| InetRow {
                    local: ep4(l), remote: ep4(r), st: tcp_st(st), txq: tx, rxq: rx, uid, inode: s.ofd })
            }
            SockState::InetBound { local_endpoint, .. } if !tcp_wanted => Some(InetRow {
                local: (v4(local_endpoint.addr), local_endpoint.port), remote: ([0; 4], 0),
                st: 0x07, txq: 0, rxq: 0, uid, inode: s.ofd }),
            SockState::InetConnected { socket_handle, remote_endpoint, sid } if !tcp_wanted => {
                let local = udp_local(sid, socket_handle).unwrap_or(([0; 4], s.bound_port));
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
        for sid in 0..MAX_STACKS as Sid {
            let handles: Vec<SocketHandle> = match stack_for(sid).as_ref() {
                Some(s) => s.orphans.iter().map(|&(h, _)| h).collect(),
                None => continue,
            };
            for h in handles {
                if let Some((st, l, r, tx, rx)) = tcp_info(sid, h) {
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

/// /proc/net/raw: the AF_INET SOCK_RAW sockets. The only raw protocol is
/// ICMP, so each row's local "port" is IPPROTO_ICMP (1), as on Linux; st 07
/// (TCP_CLOSE) because a raw socket is never connected here.
fn raw(w: &mut Out) {
    let _ = w.write_str("  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode ref pointer drops\n");
    let mut sl = 0;
    for s in snapshot(AF_INET) {
        if s.sock_type != SOCK_RAW as u8 { continue; }
        if !matches!(s.state, SockState::IcmpUnbound | SockState::IcmpBound { .. }) { continue; }
        let _ = writeln!(w, "{:4}: {:08X}:{:04X} {:08X}:{:04X} 07 00000000:00000000 00:00000000 00000000 {:5} {:8} {} 2 0000000000000000 0",
                         sl, 0, 1, 0, 0, sched::euid_of(s.pid), 0, s.ofd);
        sl += 1;
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
