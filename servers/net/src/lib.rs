//! Net server — smoltcp and AF_UNIX sockets.

#![no_std]

extern crate alloc;

pub mod nftables;

use ipc::Message;
use spin::Mutex;
use smoltcp::iface::{Config, Interface, SocketSet, SocketHandle};
use smoltcp::socket::{tcp, udp, icmp};
use smoltcp::time::Instant;
use smoltcp::wire::{EthernetAddress, HardwareAddress, IpAddress, IpCidr, IpEndpoint};

// ── Protocol tag constants ────────────────────────────────────────────────────

pub const NET_SOCKET:     u64 = 0x30;
pub const NET_BIND:       u64 = 0x31;
pub const NET_LISTEN:     u64 = 0x32;
pub const NET_ACCEPT:     u64 = 0x33;
pub const NET_CONNECT:    u64 = 0x34;
pub const NET_SEND:       u64 = 0x35;
pub const NET_RECV:       u64 = 0x36;
pub const NET_SENDMSG:    u64 = 0x37;
pub const NET_RECVMSG:    u64 = 0x38;
pub const NET_SHUTDOWN:   u64 = 0x39;
pub const NET_GETSOCKNAME: u64 = 0x3A;
pub const NET_GETPEERNAME: u64 = 0x3B;
pub const NET_SOCKETPAIR: u64 = 0x3C;
pub const NET_SETSOCKOPT: u64 = 0x3D;
pub const NET_GETSOCKOPT: u64 = 0x3E;
pub const NET_CLOSE_ALL:  u64 = 0x3F;
pub const NET_CLOSE:      u64 = 0x40;
pub const NET_POLL:       u64 = 0x41;
pub const NET_DUP:        u64 = 0x42;
pub const NET_FORK_DUP:   u64 = 0x43;
pub const NET_EXEC_CLOEXEC: u64 = 0x44;
pub const NET_SETFL:      u64 = 0x45;
pub const NET_GETFL:      u64 = 0x46;
pub const NET_SETFD:      u64 = 0x47;
pub const NET_GETFD:      u64 = 0x48;

const POLLIN:  u64 = 0x0001;
const POLLOUT: u64 = 0x0004;
const POLLHUP: u64 = 0x0010;

// ── SCM_RIGHTS / cmsg (Linux ABI, 64-bit) ─────────────────────────────────────
const SOL_SOCKET:       i32   = 1;
const SCM_RIGHTS:       i32   = 1;
const SO_ERROR:         usize = 4;
const SO_PEERCRED:      usize = 17;
const ENOPROTOOPT:      i32   = 92;
const MSG_CTRUNC:       i32   = 0x08;
const MSG_CMSG_CLOEXEC: usize = 0x4000_0000;
/// Linux SCM_MAX_FD: at most this many fds may ride one message.
const SCM_MAX_FD:       usize = 253;
/// `sizeof(struct cmsghdr)` on 64-bit: size_t len(8) + int level(4) + int type(4).
const CMSG_HDR_LEN:     usize = 16;
/// Bound on how much of a user control buffer we parse/emit (fits SCM_MAX_FD
/// fds plus the header, rounded up).
const MAX_CONTROL:      usize = CMSG_HDR_LEN + SCM_MAX_FD * 4 + 8;

#[inline]
fn cmsg_align(n: usize) -> usize { (n + 7) & !7 }

// ── Constants ─────────────────────────────────────────────────────────────────

pub const AF_UNIX:    usize = 1;
pub const AF_INET:    usize = 2;
pub const SOCK_STREAM: usize = 1;
pub const SOCK_DGRAM:  usize = 2;
pub const SOCK_RAW:    usize = 3;

pub const IPPROTO_ICMP: usize = 1;

pub const SOCK_FD_BASE: usize = 0x100;
/// One past the last socket fd. Socket fds occupy [SOCK_FD_BASE, SOCK_FD_END) =
/// [0x100, 0x300); this stays below EPOLL_FD_BASE (0x400) and — with the dormant
/// TTY_FD_BASE relocated to 0x1000 — is disjoint from every other fd range.
pub const SOCK_FD_END: usize = SOCK_FD_BASE + MAX_SOCKS;

const MAX_PROCS:   usize = 64;
/// Per-process socket fd cap. Raised 16→512 for a COSMIC-class workload (a
/// compositor holds a socket per client + the bus + internal socketpairs).
const MAX_SOCKS:   usize = 512;
/// Connection-pair pool. Raised 32→256 (K1 acceptance: 64 socketpairs + 32
/// listener connections concurrently; headroom for the desktop session).
const MAX_CONNS:   usize = 256;
/// Bound-address pool (abstract + pathname listeners). Raised 16→512.
const MAX_BOUND:   usize = 512;
const RING_SIZE:   usize = 4096;
const PATH_MAX:    usize = 108;
/// Per-direction in-flight SCM_RIGHTS fd cap. A sender that would push a
/// connection's queued-but-undelivered fd count past this fails sendmsg with
/// ETOOMANYREFS rather than growing the PendingFdBatch queue without bound.
const QUEUED_FD_CAP: usize = 1024;
/// ETOOMANYREFS — too many in-flight SCM_RIGHTS references.
const ETOOMANYREFS: i32 = -109;

// ── Message helpers ───────────────────────────────────────────────────────────

#[inline]
fn arg(msg: &Message, n: usize) -> u64 {
    let off = n * 8;
    u64::from_le_bytes(msg.data[off..off + 8].try_into().unwrap_or([0u8; 8]))
}

fn make_reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.data[0..8].copy_from_slice(&(v as u64).to_le_bytes());
    m
}

fn ok_reply()        -> Message { make_reply(0) }
fn err_reply(e: i32) -> Message { make_reply(e as i64) }
fn val_reply(v: u64) -> Message { make_reply(v as i64) }

/// NET_POLL reply: revents in data[0..8], the connection edge-seq in
/// data[8..16], and data[16] = 1 when the seq is meaningful (a connected
/// AF_UNIX socket), 0 for level-only sockets (listeners, inet) so the epoll
/// layer treats them level-triggered. Mirrors vfs::poll_reply plus the
/// has-seq flag.
fn net_poll_reply(revents: u64, seq: Option<u64>) -> Message {
    let mut m = make_reply(revents as i64);
    if let Some(s) = seq {
        m.data[8..16].copy_from_slice(&s.to_le_bytes());
        m.data[16] = 1;
    }
    m
}

// ── Unix connection ring buffers ──────────────────────────────────────────────

struct UnixRing {
    buf:   [u8; RING_SIZE],
    rpos:  usize,
    wpos:  usize,
    count: usize,
    /// Monotonic total bytes ever written / read on this direction. Unlike
    /// rpos/wpos these never wrap, so an SCM_RIGHTS fd batch can be pinned to
    /// the absolute stream offset of the byte it rides with (see
    /// `PendingFdBatch`) and delivered on the recv that consumes that byte.
    wtotal: u64,
    rtotal: u64,
}

impl UnixRing {
    const fn new() -> Self {
        Self { buf: [0u8; RING_SIZE], rpos: 0, wpos: 0, count: 0, wtotal: 0, rtotal: 0 }
    }

    fn write(&mut self, data: *const u8, len: usize) -> usize {
        let free = RING_SIZE - self.count;
        let n = len.min(free);
        for i in 0..n {
            self.buf[self.wpos] = unsafe { *data.add(i) };
            self.wpos = (self.wpos + 1) % RING_SIZE;
        }
        self.count += n;
        self.wtotal += n as u64;
        n
    }

    fn read(&mut self, data: *mut u8, len: usize) -> usize {
        let n = len.min(self.count);
        for i in 0..n {
            unsafe { *data.add(i) = self.buf[self.rpos]; }
            self.rpos = (self.rpos + 1) % RING_SIZE;
        }
        self.count -= n;
        self.rtotal += n as u64;
        n
    }

    fn write_dgram(&mut self, data: *const u8, len: usize) -> Option<usize> {
        let free = RING_SIZE - self.count;
        if free < 4 + len {
            return None;
        }
        let len_bytes = (len as u32).to_le_bytes();
        self.write(len_bytes.as_ptr(), 4);
        self.write(data, len);
        Some(len)
    }

    fn read_dgram(&mut self, data: *mut u8, len: usize) -> Option<usize> {
        if self.count < 4 {
            return None;
        }
        let mut len_bytes = [0u8; 4];
        self.read(len_bytes.as_mut_ptr(), 4);
        let dgram_len = u32::from_le_bytes(len_bytes) as usize;
        
        let to_read = dgram_len.min(len);
        self.read(data, to_read);
        if dgram_len > to_read {
            let discard = dgram_len - to_read;
            let mut discard_buf = [0u8; 128];
            let mut remaining = discard;
            while remaining > 0 {
                let chunk = remaining.min(128);
                self.read(discard_buf.as_mut_ptr(), chunk);
                remaining -= chunk;
            }
        }
        Some(to_read)
    }
}

// ── Unix connection pair ──────────────────────────────────────────────────────

/// Credentials of a socket end, captured at socketpair/connect/accept time and
/// reported to the peer via getsockopt(SO_PEERCRED) (D-Bus EXTERNAL auth).
#[derive(Clone, Copy)]
struct Ucred { pid: u32, uid: u32, gid: u32 }
impl Ucred {
    const fn zero() -> Self { Self { pid: 0, uid: 0, gid: 0 } }
}

/// One SCM_RIGHTS fd batch queued on a stream direction. `seq_byte` is the
/// absolute stream offset (UnixRing::wtotal) of the first data byte these fds
/// accompany; the recv that consumes that byte delivers them (Linux: fds ride
/// with the first byte of their segment). Ordered ascending by `seq_byte`
/// within a direction, since sends append in order.
struct PendingFdBatch {
    seq_byte: u64,
    fds:      alloc::vec::Vec<vfs::TransferFd>,
}

struct UnixConn {
    in_use: bool,
    ring_ab: UnixRing,
    ring_ba: UnixRing,
    closed_a: bool,
    closed_b: bool,
    /// Per-end fd alias counts. dup (fcntl F_DUPFD*) creates a second fd for
    /// the same end; the end only really closes when its last alias does.
    refs_a: u32,
    refs_b: u32,
    /// In-flight SCM_RIGHTS fds. `fdq_ab` rides the a→b stream (written by the
    /// a end, delivered to the b end), `fdq_ba` the reverse.
    fdq_ab: alloc::vec::Vec<PendingFdBatch>,
    fdq_ba: alloc::vec::Vec<PendingFdBatch>,
    /// Credentials of each end for the peer's SO_PEERCRED.
    cred_a: Ucred,
    cred_b: Ucred,
    /// Combined per-connection edge-trigger sequence, bumped on every state
    /// change that can newly assert readiness on either end (data-in,
    /// space-freed, peer-close, connect/accept). handle_poll returns it so the
    /// epoll layer emulates EPOLLET on AF_UNIX sockets without dropping edges
    /// (before this, sockets reported no seq → an EPOLLET tokio socket that
    /// was level-writable re-fired on every epoll_wait return, a POLLOUT
    /// storm under real blocking). One combined counter is enough for
    /// tokio/mio: they register both directions on one interest and re-derive
    /// per-direction readiness after each wake; a cross-direction spurious
    /// wake just finds nothing new and re-blocks.
    seq: u64,
}

impl UnixConn {
    const fn new() -> Self {
        Self {
            in_use: false,
            ring_ab: UnixRing::new(),
            ring_ba: UnixRing::new(),
            closed_a: false,
            closed_b: false,
            refs_a: 1,
            refs_b: 1,
            fdq_ab: alloc::vec::Vec::new(),
            fdq_ba: alloc::vec::Vec::new(),
            cred_a: Ucred::zero(),
            cred_b: Ucred::zero(),
            seq: 0,
        }
    }

    /// Release every queued-but-undelivered fd on both directions (socket torn
    /// down). Linux closes in-flight SCM_RIGHTS fds when the socket dies.
    fn drain_fds(&mut self) {
        for b in self.fdq_ab.drain(..) { for tf in b.fds { vfs::drop_transfer(tf); } }
        for b in self.fdq_ba.drain(..) { for tf in b.fds { vfs::drop_transfer(tf); } }
    }
}

static UNIX_CONNS: Mutex<[UnixConn; MAX_CONNS]> =
    Mutex::new([const { UnixConn::new() }; MAX_CONNS]);

// ── Bound AF_UNIX paths ───────────────────────────────────────────────────────

struct BoundPath {
    in_use:     bool,
    path:       [u8; PATH_MAX],
    path_len:   usize,
    /// Monotonic identity of this bound address, handed to the VFS socket node
    /// (pathname sockets) so `connect` resolving the node maps back to the
    /// right listener even after the slot index is reused (ABA-proof). Abstract
    /// sockets carry one too, so accept-matching is uniform.
    sock_id:    u64,
    /// True for a Linux abstract-namespace address (sun_path[0] == 0): matched
    /// by bytes here, never backed by a VFS node.
    is_abstract: bool,
    _owner_pid:  u32,
    _owner_sock: usize,
}

impl BoundPath {
    const fn new() -> Self {
        Self { in_use: false, path: [0u8; PATH_MAX], path_len: 0,
               sock_id: 0, is_abstract: false,
               _owner_pid: 0, _owner_sock: 0 }
    }
}

static BOUND_PATHS: Mutex<[BoundPath; MAX_BOUND]> =
    Mutex::new([const { BoundPath::new() }; MAX_BOUND]);

/// Source of unique, never-reused `sock_id`s (see `BoundPath::sock_id`).
static NEXT_SOCK_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

fn alloc_sock_id() -> u64 {
    NEXT_SOCK_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed)
}

// ── Socket kind ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum SockState {
    None,
    Unbound { domain: u8, sock_type: u8 },
    UnixListening { bound_idx: usize },
    UnixConnected { conn_idx: usize, is_a: bool },
    /// A connect() waiting to be paired by the listener bound to `sock_id`.
    UnixPendingAccept { conn_idx: usize, sock_id: u64 },
    InetBound { domain: u8, sock_type: u8, local_endpoint: IpEndpoint },
    InetListening { socket_handle: SocketHandle },
    InetConnected { socket_handle: SocketHandle, remote_endpoint: Option<IpEndpoint> },
    IcmpUnbound,
    IcmpBound { socket_handle: SocketHandle },
}

#[derive(Clone, Copy)]
struct SockEntry {
    state:      SockState,
    in_use:     bool,
    bound_port: u16,
    domain:     u8,
    sock_type:  u8,
    /// SOCK_CLOEXEC / F_DUPFD_CLOEXEC: closed by NET_EXEC_CLOEXEC at execve.
    cloexec:    bool,
    /// SOCK_NONBLOCK / fcntl(F_SETFL, O_NONBLOCK): empty/full rings return
    /// EAGAIN to the caller instead of the kernel read/write loop blocking.
    nonblock:   bool,
}

impl SockEntry {
    const fn empty() -> Self {
        Self { state: SockState::None, in_use: false, bound_port: 0, domain: 0,
               sock_type: 0, cloexec: false, nonblock: false }
    }
}

#[derive(Clone, Copy)]
struct ProcSockTable {
    pid:    u32,
    socks:  [SockEntry; MAX_SOCKS],
    in_use: bool,
}

impl ProcSockTable {
    const fn empty() -> Self {
        Self { pid: 0, socks: [const { SockEntry::empty() }; MAX_SOCKS], in_use: false }
    }

    /// Clear in place. At MAX_SOCKS=512 a `*self = ProcSockTable::empty()` would
    /// materialise a ~24 KB temporary on the kernel stack; resetting field by
    /// field keeps every clear cheap.
    fn reset(&mut self) {
        self.pid = 0;
        self.in_use = false;
        for s in self.socks.iter_mut() { *s = SockEntry::empty(); }
    }

    fn alloc(&mut self) -> Option<usize> {
        self.socks.iter().position(|s| !s.in_use)
    }
}

static SOCK_TABLES: Mutex<[ProcSockTable; MAX_PROCS]> =
    Mutex::new([const { ProcSockTable::empty() }; MAX_PROCS]);

// ── Table helpers ─────────────────────────────────────────────────────────────

fn find_tbl<'a>(pid: u32, tbls: &'a mut [ProcSockTable]) -> Option<&'a mut ProcSockTable> {
    tbls.iter_mut().find(|t| t.in_use && t.pid == pid)
}

fn get_or_create<'a>(pid: u32, tbls: &'a mut [ProcSockTable]) -> Option<&'a mut ProcSockTable> {
    if let Some(pos) = tbls.iter().position(|t| t.in_use && t.pid == pid) {
        return Some(&mut tbls[pos]);
    }
    if let Some(pos) = tbls.iter().position(|t| !t.in_use) {
        tbls[pos].reset();
        tbls[pos].in_use = true;
        tbls[pos].pid    = pid;
        return Some(&mut tbls[pos]);
    }
    None
}

fn fd_to_slot(fd: usize) -> Option<usize> {
    if fd >= SOCK_FD_BASE && fd < SOCK_FD_END { Some(fd - SOCK_FD_BASE) } else { None }
}

/// Free the BOUND_PATHS slot a `UnixListening` socket owned. Called when the
/// listener closes: the address stops resolving to a live listener (a pathname
/// socket's VFS node lingers per Linux, but connecting to it now yields
/// ECONNREFUSED), and the slot is reclaimable. Caller must not hold BOUND_PATHS.
fn free_bound_idx(bound_idx: usize) {
    let mut bound = BOUND_PATHS.lock();
    if bound_idx < MAX_BOUND && bound[bound_idx].in_use {
        bound[bound_idx] = BoundPath::new();
    }
}

// ── Smoltcp Integration ───────────────────────────────────────────────────────

pub struct NetStack {
    pub interface: Interface,
    pub socket_set: SocketSet<'static>,
    pub dhcp_handle: SocketHandle,
}

pub static NET_STACK: Mutex<Option<NetStack>> = Mutex::new(None);
pub static NFTABLES: Mutex<nftables::NftablesEngine> = Mutex::new(nftables::NftablesEngine::new());

pub struct VirtioNetDeviceWrapper {
    pub dev_idx: usize,
}

impl<'a> smoltcp::phy::Device for VirtioNetDeviceWrapper {
    type RxToken<'b> = RxToken where Self: 'b;
    type TxToken<'b> = TxToken where Self: 'b;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let mut buf = [0u8; 2048];
        if let Some(len) = drivers::virtio_net::poll_receive(self.dev_idx, &mut buf) {
            if len >= 14 {
                let eth_proto = u16::from_be_bytes([buf[12], buf[13]]);
                if eth_proto == 0x0800 {
                    if len >= 34 {
                        let proto = buf[23];
                        let src_ip = IpAddress::Ipv4(smoltcp::wire::Ipv4Address::from_bytes(&buf[26..30]));
                        let dst_ip = IpAddress::Ipv4(smoltcp::wire::Ipv4Address::from_bytes(&buf[30..34]));
                        let mut src_port = None;
                        let mut dst_port = None;
                        if proto == 6 || proto == 17 {
                            if len >= 38 {
                                src_port = Some(u16::from_be_bytes([buf[34], buf[35]]));
                                dst_port = Some(u16::from_be_bytes([buf[36], buf[37]]));
                            }
                        }

                        let mut engine = NFTABLES.lock();
                        let verdict = engine.evaluate(
                            nftables::Hook::Prerouting,
                            proto,
                            src_ip,
                            dst_ip,
                            src_port,
                            dst_port,
                        );

                        if verdict == nftables::Verdict::Drop {
                            return None;
                        }
                    }
                }
            }

            let mut data = alloc::vec![0u8; len];
            data.copy_from_slice(&buf[..len]);
            Some((RxToken { buf: data }, TxToken { dev_idx: self.dev_idx }))
        } else {
            None
        }
    }

    fn transmit(&mut self, _timestamp: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken { dev_idx: self.dev_idx })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.max_transmission_unit = 1500;
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps
    }
}

pub struct RxToken {
    buf: alloc::vec::Vec<u8>,
}

impl smoltcp::phy::RxToken for RxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.buf)
    }
}

pub struct TxToken {
    dev_idx: usize,
}

impl smoltcp::phy::TxToken for TxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = alloc::vec![0u8; len];
        let result = f(&mut buf);
        
        let mut transmit_allowed = true;
        if len >= 34 {
            let eth_proto = u16::from_be_bytes([buf[12], buf[13]]);
            if eth_proto == 0x0800 {
                let proto = buf[23];
                let src_ip = IpAddress::Ipv4(smoltcp::wire::Ipv4Address::from_bytes(&buf[26..30]));
                let dst_ip = IpAddress::Ipv4(smoltcp::wire::Ipv4Address::from_bytes(&buf[30..34]));
                let mut src_port = None;
                let mut dst_port = None;
                if proto == 6 || proto == 17 {
                    src_port = Some(u16::from_be_bytes([buf[34], buf[35]]));
                    dst_port = Some(u16::from_be_bytes([buf[36], buf[37]]));
                }

                let mut engine = NFTABLES.lock();
                let verdict = engine.evaluate(
                    nftables::Hook::Output,
                    proto,
                    src_ip,
                    dst_ip,
                    src_port,
                    dst_port,
                );
                if verdict == nftables::Verdict::Drop {
                    transmit_allowed = false;
                }
            }
        }

        if transmit_allowed {
            drivers::virtio_net::send_packet(self.dev_idx, &buf);
        }
        result
    }
}

pub fn init() {
    if drivers::virtio_net::device_count() > 0 {
        if let Some(mac) = drivers::virtio_net::get_mac_address(0) {
            let mut device = VirtioNetDeviceWrapper { dev_idx: 0 };
            let hardware_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
            let mut config = Config::new(hardware_addr);
            config.random_seed = sched::ticks();

            let mut interface = Interface::new(config, &mut device, Instant::from_millis((sched::ticks() * 10) as i64));
            
            interface.update_ip_addrs(|addrs| {
                addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
            });
            interface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2)).unwrap();

            let mut socket_set = SocketSet::new(alloc::vec![]);
            let dhcp_socket = smoltcp::socket::dhcpv4::Socket::new();
            let dhcp_handle = socket_set.add(dhcp_socket);

            let stack = NetStack {
                interface,
                socket_set,
                dhcp_handle,
            };
            *NET_STACK.lock() = Some(stack);

            extern "C" { fn arch_serial_putc(b: u8); }
            let msg = b"[NET] Interface configured, net server initialized successfully\r\n";
            for &b in msg { unsafe { arch_serial_putc(b); } }
        }
    }
}

pub fn net_daemon() -> ! {
    loop {
        let timestamp = Instant::from_millis((sched::ticks() * 10) as i64);
        let mut device = VirtioNetDeviceWrapper { dev_idx: 0 };
        
        let dhcp_status = {
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                s.interface.poll(timestamp, &mut device, &mut s.socket_set);

                let dhcp_socket = s.socket_set.get_mut::<smoltcp::socket::dhcpv4::Socket>(s.dhcp_handle);
                match dhcp_socket.poll() {
                    Some(smoltcp::socket::dhcpv4::Event::Configured(config)) => {
                        Some((config.address, config.router, config.dns_servers))
                    }
                    _ => None,
                }
            } else {
                None
            }
        };

        if let Some((addr, router, _dns)) = dhcp_status {
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                s.interface.update_ip_addrs(|addrs| {
                    addrs.clear();
                    let cidr = IpCidr::new(IpAddress::Ipv4(addr.address()), addr.prefix_len());
                    addrs.push(cidr).unwrap();
                });
                if let Some(gateway) = router {
                    s.interface.routes_mut().add_default_ipv4_route(gateway).unwrap();
                }
            }

            extern "C" { fn arch_serial_putc(b: u8); }
            let msg = b"[NET] DHCP configured, address: ";
            for &b in msg { unsafe { arch_serial_putc(b); } }
            let octets = addr.address().0;
            for (i, &o) in octets.iter().enumerate() {
                if i > 0 { unsafe { arch_serial_putc(b'.'); } }
                if o >= 100 { unsafe { arch_serial_putc(b'0' + o / 100); } }
                if o >= 10  { unsafe { arch_serial_putc(b'0' + (o / 10) % 10); } }
                unsafe { arch_serial_putc(b'0' + o % 10); }
            }
            for &b in b"\r\n" { unsafe { arch_serial_putc(b); } }
        }

        // Block until the next tick (~100 Hz poll cadence) instead of a tight
        // yield_now busy-poll. The old spin pinned this kernel task's CPU at
        // 100 % from boot — the dominant component of the "compositor is
        // compute-bound" misread. 100 Hz smoltcp polling is ample here, and an
        // earlier wake (any wake_poll from socket traffic) re-polls immediately.
        sched::block_on_poll_prepare_until(sched::ticks() + 1);
        sched::block_on_poll_commit();
    }
}

// ── Public dispatch ───────────────────────────────────────────────────────────

pub fn force_bind_unix(path_str: &str, _port: u32) {
    let mut bound = BOUND_PATHS.lock();
    let path_bytes = path_str.as_bytes();
    let path_len = path_bytes.len().min(PATH_MAX);

    if let Some(idx) = bound.iter().position(|b| !b.in_use) {
        let mut path = [0u8; PATH_MAX];
        path[..path_len].copy_from_slice(&path_bytes[..path_len]);
        bound[idx] = BoundPath {
            in_use: true,
            path,
            path_len,
            sock_id: alloc_sock_id(),
            is_abstract: false,
            _owner_pid: 0,
            _owner_sock: 0
        };
    }
}

pub fn handle(msg: &Message, caller_pid: u32) -> Message {
    // Socket tables are per-*process*: canonicalize the caller to its
    // thread-group id so CLONE_THREAD siblings share one fd table (a
    // socketpair created on brush's main thread is read by its tokio
    // worker threads — with raw task pids the worker found an empty table
    // and every operation failed with EBADF).
    let caller_pid = sched::tgid_of(caller_pid);
    match msg.tag {
        NET_SOCKET      => handle_socket(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize, arg(msg,2) as usize),
        NET_BIND        => handle_bind(caller_pid, arg(msg,0) as usize,
                                       arg(msg,1) as usize, arg(msg,2) as usize),
        NET_LISTEN      => handle_listen(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        NET_ACCEPT      => handle_accept(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize, arg(msg,2) as usize,
                                         arg(msg,3) as usize),
        NET_CONNECT     => handle_connect(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize, arg(msg,2) as usize),
        NET_SEND        => handle_send(caller_pid, arg(msg,0) as usize,
                                        arg(msg,1) as usize, arg(msg,2) as usize,
                                        arg(msg,4) as usize, arg(msg,5) as usize),
        NET_RECV        => handle_recv(caller_pid, arg(msg,0) as usize,
                                        arg(msg,1) as usize, arg(msg,2) as usize,
                                        arg(msg,4) as usize, arg(msg,5) as usize),
        NET_SENDMSG     => handle_sendmsg(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize, arg(msg,2) as usize),
        NET_RECVMSG     => handle_recvmsg(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize, arg(msg,2) as usize),
        NET_SHUTDOWN    => handle_shutdown(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        NET_GETSOCKNAME  => handle_getsockname(caller_pid, arg(msg,0) as usize,
                                                arg(msg,1) as usize, arg(msg,2) as usize),
        NET_GETPEERNAME  => handle_getpeername(caller_pid, arg(msg,0) as usize,
                                                arg(msg,1) as usize, arg(msg,2) as usize),
        NET_SOCKETPAIR  => handle_socketpair(caller_pid, arg(msg,0) as usize,
                                             arg(msg,1) as usize, arg(msg,2) as usize,
                                             arg(msg,3) as usize),
        NET_SETSOCKOPT  => ok_reply(),
        NET_GETSOCKOPT  => handle_getsockopt(caller_pid, arg(msg,0) as usize,
                                             arg(msg,1) as usize, arg(msg,2) as usize,
                                             arg(msg,3) as usize, arg(msg,4) as usize),
        NET_CLOSE_ALL   => { handle_close_all(caller_pid); ok_reply() }
        NET_CLOSE       => handle_close(caller_pid, arg(msg,0) as usize),
        NET_POLL        => handle_poll(caller_pid, arg(msg,0) as usize),
        NET_DUP         => handle_dup(caller_pid, arg(msg,0) as usize, arg(msg,1) != 0),
        NET_FORK_DUP    => handle_fork_dup(arg(msg,0) as u32, arg(msg,1) as u32),
        NET_EXEC_CLOEXEC => handle_exec_cloexec(arg(msg,0) as u32),
        NET_SETFL       => handle_setfl(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        NET_GETFL       => handle_getfl(caller_pid, arg(msg,0) as usize),
        NET_SETFD       => handle_setfd(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        NET_GETFD       => handle_getfd(caller_pid, arg(msg,0) as usize),
        _               => err_reply(-38),
    }
}

// ── Handlers ─────────────────────────────────────────────────────────────────

fn handle_socket(pid: u32, domain: usize, sock_type: usize, protocol: usize) -> Message {
    match domain {
        AF_UNIX | AF_INET => {}
        _                 => return err_reply(-97),
    }
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) {
        Some(t) => t, None => return err_reply(-12),
    };
    let slot = match tbl.alloc() { Some(s) => s, None => return err_reply(-24) };
    let state = if domain == AF_INET && protocol == IPPROTO_ICMP {
        SockState::IcmpUnbound
    } else {
        SockState::Unbound { domain: domain as u8, sock_type: sock_type as u8 }
    };
    tbl.socks[slot] = SockEntry {
        state,
        in_use:     true,
        bound_port: 0,
        domain:     domain as u8,
        sock_type:  sock_type as u8,
        cloexec:    sock_type & 0x80000 != 0, // SOCK_CLOEXEC
        nonblock:   sock_type & 0x800 != 0,    // SOCK_NONBLOCK
    };
    val_reply((slot + SOCK_FD_BASE) as u64)
}

fn handle_bind(pid: u32, fd: usize, addr_ptr: usize, addrlen: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    if addrlen < 2 { return err_reply(-22); }

    let sa_family = unsafe { (addr_ptr as *const u16).read_unaligned() } as usize;

    match sa_family {
        AF_INET => {
            if addrlen < 8 { return err_reply(-22); }
            let port_be = unsafe { ((addr_ptr + 2) as *const u16).read_unaligned() };
            let port = u16::from_be(port_be);
            let sin_addr = unsafe { ((addr_ptr + 4) as *const u32).read_unaligned() };
            
            let mut tbls = SOCK_TABLES.lock();
            let tbl = match find_tbl(pid, &mut *tbls) {
                Some(t) => t, None => return err_reply(-9),
            };
            if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
            
            let ip = IpAddress::from(smoltcp::wire::Ipv4Address::from_bytes(&sin_addr.to_ne_bytes()));
            let local_endpoint = IpEndpoint::new(ip, port);
            
            tbl.socks[slot].state = SockState::InetBound {
                domain: AF_INET as u8,
                sock_type: tbl.socks[slot].sock_type,
                local_endpoint,
            };
            tbl.socks[slot].bound_port = port;
            ok_reply()
        }
        AF_UNIX | _ => {
            if addrlen < 3 || addrlen > 2 + PATH_MAX { return err_reply(-22); }
            let path_len = addrlen - 2;
            let path_ptr = (addr_ptr + 2) as *const u8;

            // Copy sun_path out of user memory into a kernel buffer up front —
            // no user dereference happens under any lock or inside the VFS.
            let mut pbytes = [0u8; PATH_MAX];
            unsafe { core::ptr::copy_nonoverlapping(path_ptr, pbytes.as_mut_ptr(), path_len); }
            let is_abstract = pbytes[0] == 0;
            let sock_id = alloc_sock_id();

            // ── Pathname socket: the VFS owns uniqueness via a real S_IFSOCK
            // node. Bind does not follow a symlink for the final component.
            if !is_abstract {
                // sun_path is a C string for pathname sockets; the address ends
                // at the first NUL (addrlen may or may not include it).
                let name_end = pbytes[..path_len].iter().position(|&b| b == 0).unwrap_or(path_len);
                let name = &pbytes[..name_end];
                if name.is_empty() { return err_reply(-22); }
                match vfs::unix_bind_node(pid, name, sock_id) {
                    0   => {}
                    -17 => return err_reply(-98), // node exists → EADDRINUSE
                    e   => return make_reply(e as i64), // ENOENT/EACCES/EOPNOTSUPP/ENOSPC
                }
            } else {
                // Abstract namespace: byte-matched here, never a VFS node.
                let bound = BOUND_PATHS.lock();
                for bp in bound.iter() {
                    if bp.in_use && bp.is_abstract && bp.path_len == path_len
                       && bp.path[..path_len] == pbytes[..path_len] {
                        return err_reply(-98); // EADDRINUSE
                    }
                }
            }

            let mut bound = BOUND_PATHS.lock();
            let idx = match bound.iter().position(|b| !b.in_use) {
                Some(i) => i,
                // Pathname node already created; leave it as a stale socket file
                // (Linux keeps them too) rather than unwinding the VFS op.
                None => return err_reply(-12),
            };
            bound[idx] = BoundPath { in_use: true, path: pbytes, path_len,
                                     sock_id, is_abstract,
                                     _owner_pid: pid, _owner_sock: slot };
            drop(bound);

            // Scope the SOCK_TABLES lock so it is released before any
            // free_bound_idx (which takes BOUND_PATHS) — the two are never held
            // nested, keeping BOUND_PATHS strictly a leaf.
            let ok = {
                let mut tbls = SOCK_TABLES.lock();
                match find_tbl(pid, &mut *tbls) {
                    Some(t) if slot < MAX_SOCKS && t.socks[slot].in_use => {
                        t.socks[slot].state = SockState::UnixListening { bound_idx: idx };
                        true
                    }
                    _ => false,
                }
            };
            if !ok { free_bound_idx(idx); return err_reply(-9); }
            ok_reply()
        }
    }
}

fn handle_listen(pid: u32, fd: usize, _backlog: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }

    if tbl.socks[slot].domain == AF_INET as u8 {
        let bound_port = tbl.socks[slot].bound_port;
        if bound_port == 0 { return err_reply(-22); }
        
        let mut stack = NET_STACK.lock();
        if let Some(ref mut s) = *stack {
            let rx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
            let tx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
            let mut socket = tcp::Socket::new(rx_buffer, tx_buffer);
            socket.listen(bound_port).unwrap();
            let handle = s.socket_set.add(socket);
            tbl.socks[slot].state = SockState::InetListening { socket_handle: handle };
            ok_reply()
        } else {
            err_reply(-100)
        }
    } else {
        ok_reply()
    }
}

fn handle_accept(pid: u32, fd: usize, addr_ptr: usize, addrlen_ptr: usize, flags: usize) -> Message {
    // accept4() flags: SOCK_NONBLOCK / SOCK_CLOEXEC apply to the *accepted*
    // socket, not the listener. Dropping them (the old ACCEPT|ACCEPT4 alias
    // that forwarded no a3) left every accepted fd blocking — a recv on an
    // empty ring then hangs forever instead of returning EAGAIN, which is the
    // W1 tokio/zbus wedge.
    const SOCK_CLOEXEC:  usize = 0x80000;
    const SOCK_NONBLOCK: usize = 0x800;
    let acc_cloexec  = flags & SOCK_CLOEXEC  != 0;
    let acc_nonblock = flags & SOCK_NONBLOCK != 0;
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };

    let (state, bound_port, sock_type) = {
        let tbls = SOCK_TABLES.lock();
        let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
        (tbl.socks[slot].state, tbl.socks[slot].bound_port, tbl.socks[slot].sock_type)
    };

    match state {
        SockState::InetListening { socket_handle } => {
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                let is_connected = {
                    let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    socket.is_active() && socket.state() == tcp::State::Established
                };
                
                if !is_connected {
                    return err_reply(-11);
                }

                let rx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
                let tx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
                let mut new_listening_socket = tcp::Socket::new(rx_buffer, tx_buffer);
                new_listening_socket.listen(bound_port).unwrap();
                let new_handle = s.socket_set.add(new_listening_socket);

                let mut tbls = SOCK_TABLES.lock();
                let tbl = find_tbl(pid, &mut *tbls).unwrap();
                tbl.socks[slot].state = SockState::InetListening { socket_handle: new_handle };

                let new_slot = match tbl.alloc() { Some(sn) => sn, None => return err_reply(-24) };
                tbl.socks[new_slot] = SockEntry {
                    state: SockState::InetConnected { socket_handle, remote_endpoint: None },
                    in_use: true,
                    bound_port: 0,
                    domain: AF_INET as u8,
                    sock_type: SOCK_STREAM as u8,
                    cloexec: acc_cloexec,
                    nonblock: acc_nonblock,
                };

                if addr_ptr != 0 && addrlen_ptr != 0 {
                    let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    if let Some(endpoint) = socket.remote_endpoint() {
                        unsafe {
                            core::ptr::write_bytes(addr_ptr as *mut u8, 0, 16);
                            core::ptr::write(addr_ptr as *mut u16, AF_INET as u16);
                            core::ptr::write((addr_ptr + 2) as *mut u16, endpoint.port.to_be());
                            if let IpAddress::Ipv4(ipv4) = endpoint.addr {
                                core::ptr::write((addr_ptr + 4) as *mut u32, u32::from_ne_bytes(ipv4.0));
                            }
                            *(addrlen_ptr as *mut u32) = 16;
                        }
                    }
                }

                val_reply((new_slot + SOCK_FD_BASE) as u64)
            } else {
                err_reply(-100)
            }
        }
        SockState::UnixListening { bound_idx } => {
            // Only pair a pending connect that targeted *this* listener's
            // address (matched by its unique sock_id), so multiple concurrent
            // listeners each accept their own connectors.
            let listen_sock_id = {
                let bound = BOUND_PATHS.lock();
                if bound_idx >= MAX_BOUND || !bound[bound_idx].in_use {
                    return err_reply(-11); // listener's address gone → nothing to accept
                }
                bound[bound_idx].sock_id
            };
            let mut tbls = SOCK_TABLES.lock();
            let mut found = None;
            'outer: for t in tbls.iter() {
                if !t.in_use { continue; }
                for s in t.socks.iter() {
                    if let SockState::UnixPendingAccept { conn_idx, sock_id } = s.state {
                        if sock_id == listen_sock_id {
                            found = Some(conn_idx);
                            break 'outer;
                        }
                    }
                }
            }
            match found {
                Some(conn_idx) => {
                    // The accepting socket is end B; record its creds for the
                    // connector's SO_PEERCRED.
                    let cred_b = Ucred { pid, uid: sched::euid_of(pid), gid: sched::egid_of(pid) };
                    {
                        let mut c = UNIX_CONNS.lock();
                        c[conn_idx].cred_b = cred_b;
                        // Connection established: the connector's poll becomes
                        // writable/connected (an edge for it).
                        c[conn_idx].seq = c[conn_idx].seq.wrapping_add(1);
                    }
                    let tbl = find_tbl(pid, &mut *tbls).unwrap();
                    let new_slot = match tbl.alloc() { Some(s) => s, None => return err_reply(-24) };
                    tbl.socks[new_slot] = SockEntry {
                        state:      SockState::UnixConnected { conn_idx, is_a: false },
                        in_use:     true,
                        bound_port: 0,
                        domain:     AF_UNIX as u8,
                        sock_type,
                        cloexec:    acc_cloexec,
                        nonblock:   acc_nonblock,
                    };

                    for t in tbls.iter_mut() {
                        if !t.in_use { continue; }
                        for s in t.socks.iter_mut() {
                            if let SockState::UnixPendingAccept { conn_idx: pending_conn, .. } = s.state {
                                if pending_conn == conn_idx {
                                    s.state = SockState::UnixConnected { conn_idx, is_a: true };
                                    break;
                                }
                            }
                        }
                    }

                    drop(tbls);
                    // Report the peer address AFTER releasing SOCK_TABLES: writing
                    // user memory can demand-page, and faulting under a spinlock is
                    // the freeze hazard documented in the memory notes. The connector
                    // has no bound path, so the peer is an unnamed AF_UNIX address:
                    // sun_family = AF_UNIX, addrlen = sizeof(sa_family_t). std's
                    // SocketAddr parser rejects a nonzero addrlen whose family is not
                    // AF_UNIX ("file descriptor did not correspond to a Unix socket"),
                    // so both fields must be set — previously sun_family was zeroed to
                    // AF_UNSPEC and addrlen was left untouched.
                    if addr_ptr != 0 {
                        unsafe {
                            core::ptr::write_bytes(addr_ptr as *mut u8, 0, 2);
                            core::ptr::write(addr_ptr as *mut u16, AF_UNIX as u16);
                        }
                    }
                    if addrlen_ptr != 0 {
                        unsafe { *(addrlen_ptr as *mut u32) = 2; }
                    }
                    // Wake the connector parked in poll/connect (K2).
                    sched::wake_poll();
                    val_reply((new_slot + SOCK_FD_BASE) as u64)
                }
                None => err_reply(-11),
            }
        }
        _ => err_reply(-22),
    }
}

fn handle_connect(pid: u32, fd: usize, addr_ptr: usize, addrlen: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    if addrlen < 2 { return err_reply(-22); }

    let sa_family = unsafe { (addr_ptr as *const u16).read_unaligned() } as usize;

    if sa_family == AF_INET {
        if addrlen < 8 { return err_reply(-22); }
        let port_be  = unsafe { ((addr_ptr + 2) as *const u16).read_unaligned() };
        let port     = u16::from_be(port_be);
        let sin_addr = unsafe { ((addr_ptr + 4) as *const u32).read_unaligned() };
        
        let mut tbls = SOCK_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }

        let sock_type = tbl.socks[slot].sock_type;
        let mut stack = NET_STACK.lock();
        if let Some(ref mut s) = *stack {
            let remote_ip = IpAddress::from(smoltcp::wire::Ipv4Address::from_bytes(&sin_addr.to_ne_bytes()));
            let remote_endpoint = IpEndpoint::new(remote_ip, port);
            
            if sock_type == SOCK_STREAM as u8 {
                let rx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
                let tx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
                let mut socket = tcp::Socket::new(rx_buffer, tx_buffer);
                
                let local_port = if tbl.socks[slot].bound_port != 0 {
                    tbl.socks[slot].bound_port
                } else {
                    let p = 49152 + (sched::ticks() % 16384) as u16;
                    tbl.socks[slot].bound_port = p;
                    p
                };
                
                socket.connect(s.interface.context(), remote_endpoint, local_port).unwrap();
                let handle = s.socket_set.add(socket);
                tbl.socks[slot].state = SockState::InetConnected { socket_handle: handle, remote_endpoint: Some(remote_endpoint) };
                ok_reply()
            } else {
                // UDP Connect: just store remote endpoint for send/recv filtering
                let rx_buffer = udp::PacketBuffer::new(
                    alloc::vec![udp::PacketMetadata::EMPTY; 8],
                    alloc::vec![0; 65536],
                );
                let tx_buffer = udp::PacketBuffer::new(
                    alloc::vec![udp::PacketMetadata::EMPTY; 8],
                    alloc::vec![0; 65536],
                );
                let mut socket = udp::Socket::new(rx_buffer, tx_buffer);
                let local_port = if tbl.socks[slot].bound_port != 0 {
                    tbl.socks[slot].bound_port
                } else {
                    let p = 49152 + (sched::ticks() % 16384) as u16;
                    tbl.socks[slot].bound_port = p;
                    p
                };
                socket.bind(local_port).unwrap();
                let handle = s.socket_set.add(socket);
                tbl.socks[slot].state = SockState::InetConnected { socket_handle: handle, remote_endpoint: Some(remote_endpoint) };
                ok_reply()
            }
        } else {
            err_reply(-100)
        }
    } else {
        // AF_UNIX Connect
        if addrlen < 3 || addrlen > 2 + PATH_MAX { return err_reply(-22); }
        let path_len = addrlen - 2;
        let path_ptr = (addr_ptr + 2) as *const u8;
        let mut pbytes = [0u8; PATH_MAX];
        unsafe { core::ptr::copy_nonoverlapping(path_ptr, pbytes.as_mut_ptr(), path_len); }
        let is_abstract = pbytes[0] == 0;

        // Resolve the address to the sock_id of a live listener. Abstract
        // names byte-match here; pathnames resolve through the VFS (symlinks,
        // the /dev/shm and /run/user/0 tmpfs mounts, S_IFSOCK check).
        let sock_id = if is_abstract {
            let bound = BOUND_PATHS.lock();
            let mut found = None;
            for bp in bound.iter() {
                if bp.in_use && bp.is_abstract && bp.path_len == path_len
                   && bp.path[..path_len] == pbytes[..path_len] {
                    found = Some(bp.sock_id);
                    break;
                }
            }
            match found { Some(id) => id, None => return err_reply(-111) } // ECONNREFUSED
        } else {
            let name_end = pbytes[..path_len].iter().position(|&b| b == 0).unwrap_or(path_len);
            let name = &pbytes[..name_end];
            if name.is_empty() { return err_reply(-22); }
            let r = vfs::unix_resolve_node(pid, name); // sock_id (>=0) or -errno
            if r < 0 { return make_reply(r); }
            let sock_id = r as u64;
            // The node resolved, but its listener may have since closed (Linux
            // leaves the socket file behind) — connecting then is ECONNREFUSED.
            let live = BOUND_PATHS.lock().iter()
                .any(|bp| bp.in_use && !bp.is_abstract && bp.sock_id == sock_id);
            if !live { return err_reply(-111); }
            sock_id
        };

        // The connecting socket becomes end A at accept time; capture its creds.
        let cred = Ucred { pid, uid: sched::euid_of(pid), gid: sched::egid_of(pid) };
        let conn_idx = {
            let mut conns = UNIX_CONNS.lock();
            let idx = match conns.iter().position(|c| !c.in_use) {
                Some(i) => i, None => return err_reply(-12),
            };
            conns[idx].drain_fds();
            conns[idx] = UnixConn::new();
            conns[idx].in_use = true;
            conns[idx].cred_a = cred;
            idx
        };

        let mut tbls = SOCK_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
        tbl.socks[slot].state = SockState::UnixPendingAccept { conn_idx, sock_id };
        drop(tbls);
        // A listener parked in accept/poll now has a pending connect on its
        // address (K2 real blocking).
        sched::wake_poll();
        ok_reply()
    }
}

fn handle_socketpair(pid: u32, domain: usize, sock_type: usize,
                     _protocol: usize, sv_ptr: usize) -> Message {
    if domain != AF_UNIX { return err_reply(-97); }

    // Both ends belong to the creating process — SO_PEERCRED on either reports it.
    let cred = Ucred { pid, uid: sched::euid_of(pid), gid: sched::egid_of(pid) };
    let conn_idx = {
        let mut conns = UNIX_CONNS.lock();
        let idx = match conns.iter().position(|c| !c.in_use) {
            Some(i) => i, None => return err_reply(-12),
        };
        conns[idx].drain_fds();
        conns[idx] = UnixConn::new();
        conns[idx].in_use = true;
        conns[idx].cred_a = cred;
        conns[idx].cred_b = cred;
        idx
    };

    let mut tbls = SOCK_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) {
        Some(t) => t, None => return err_reply(-12),
    };
    const SOCK_CLOEXEC: usize = 0x80000;
    const SOCK_NONBLOCK: usize = 0x800;
    let cloexec  = sock_type & SOCK_CLOEXEC != 0;
    let nonblock = sock_type & SOCK_NONBLOCK != 0;
    let slot_a = match tbl.alloc() { Some(s) => s, None => return err_reply(-24) };
    tbl.socks[slot_a] = SockEntry {
        state: SockState::UnixConnected { conn_idx, is_a: true },
        in_use: true, bound_port: 0, domain: AF_UNIX as u8, sock_type: sock_type as u8,
        cloexec, nonblock,
    };
    let slot_b = match tbl.alloc() { Some(s) => s, None => {
        tbl.socks[slot_a] = SockEntry::empty(); return err_reply(-24);
    }};
    tbl.socks[slot_b] = SockEntry {
        state: SockState::UnixConnected { conn_idx, is_a: false },
        in_use: true, bound_port: 0, domain: AF_UNIX as u8, sock_type: sock_type as u8,
        cloexec, nonblock,
    };
    unsafe {
        core::ptr::write(sv_ptr as *mut u32, (slot_a + SOCK_FD_BASE) as u32);
        core::ptr::write((sv_ptr + 4) as *mut u32, (slot_b + SOCK_FD_BASE) as u32);
    }
    ok_reply()
}

fn handle_send(pid: u32, fd: usize, buf_ptr: usize, len: usize, addr_ptr: usize, addrlen: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }

    let state = tbl.socks[slot].state;
    let sock_type = tbl.socks[slot].sock_type;

    match state {
        SockState::UnixConnected { conn_idx, is_a } => {
            drop(tbls);
            let mut conns = UNIX_CONNS.lock();
            let conn = &mut conns[conn_idx];
            if !conn.in_use { return err_reply(-32); }
            // Peer end closed (all its aliases gone): writes raise EPIPE.
            let peer_closed = if is_a { conn.closed_b } else { conn.closed_a };
            if peer_closed { return err_reply(-32); } // EPIPE
            let n = if sock_type == SOCK_STREAM as u8 {
                if is_a {
                    conn.ring_ab.write(buf_ptr as *const u8, len)
                } else {
                    conn.ring_ba.write(buf_ptr as *const u8, len)
                }
            } else {
                if is_a {
                    conn.ring_ab.write_dgram(buf_ptr as *const u8, len).unwrap_or(0)
                } else {
                    conn.ring_ba.write_dgram(buf_ptr as *const u8, len).unwrap_or(0)
                }
            };
            // New readable edge for the peer end.
            if n > 0 { conn.seq = conn.seq.wrapping_add(1); }
            drop(conns); // release UNIX_CONNS before waking pollers (K2 lock order)
            if n > 0 { sched::wake_poll(); }
            // Full ring on a non-empty send (couldn't place even one byte) while
            // the peer is still open → EAGAIN, not a bogus "sent 0 bytes".
            // net_blocking_op only retries on -11; a 0 return reaches libwayland,
            // which treats it as "flushed 0, tail unadvanced" and busy-loops in
            // wl_connection_flush. Mirrors the fd-path (handle_sendmsg total==0
            // → EAGAIN) and handle_recv's EOF-vs-EAGAIN split. (peer_closed is
            // already false here — checked above — kept explicit per the invariant.)
            if n == 0 && len > 0 && !peer_closed { return err_reply(-11); }
            val_reply(n as u64)
        }
        SockState::UnixPendingAccept { conn_idx, .. } => {
            // Connector (end A); connection established at connect() time. Linux
            // buffers writes pre-accept — an early client write (e.g. Wayland
            // get_registry the instant after connect, before the compositor's
            // event loop accept()s) must NOT EPIPE. Peer reads it after accept
            // (which preserves conn_idx + ring_ab). Stream only — connect()/accept
            // are stream, so no dgram path here.
            drop(tbls);
            let mut conns = UNIX_CONNS.lock();
            let conn = &mut conns[conn_idx];
            if !conn.in_use { return err_reply(-32); } // EPIPE — conn torn down
            if conn.closed_b { return err_reply(-32); } // peer (post-accept) gone
            let n = conn.ring_ab.write(buf_ptr as *const u8, len);
            if n > 0 { conn.seq = conn.seq.wrapping_add(1); }
            drop(conns);
            if n > 0 { sched::wake_poll(); }
            // Full pre-accept buffer on a non-empty send → EAGAIN, not a bogus 0
            // (same livelock as the UnixConnected branch above; peer_closed/
            // closed_b already handled above).
            if n == 0 && len > 0 { return err_reply(-11); }
            val_reply(n as u64)
        }
        SockState::InetConnected { socket_handle, remote_endpoint } => {
            drop(tbls);
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                if sock_type == SOCK_STREAM as u8 {
                    let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    if !socket.can_send() {
                        return err_reply(-11);
                    }
                    let mut data = alloc::vec![0u8; len];
                    unsafe {
                        core::ptr::copy_nonoverlapping(buf_ptr as *const u8, data.as_mut_ptr(), len);
                    }
                    match socket.send_slice(&data) {
                        Ok(n) => val_reply(n as u64),
                        Err(_) => err_reply(-104),
                    }
                } else {
                    let socket = s.socket_set.get_mut::<udp::Socket>(socket_handle);
                    let endpoint = if addr_ptr != 0 && addrlen >= 8 {
                        let port_be = unsafe { ((addr_ptr + 2) as *const u16).read_unaligned() };
                        let port = u16::from_be(port_be);
                        let sin_addr = unsafe { ((addr_ptr + 4) as *const u32).read_unaligned() };
                        let ip = IpAddress::from(smoltcp::wire::Ipv4Address::from_bytes(&sin_addr.to_ne_bytes()));
                        IpEndpoint::new(ip, port)
                    } else if let Some(ep) = remote_endpoint {
                        ep
                    } else {
                        return err_reply(-89);
                    };

                    let mut data = alloc::vec![0u8; len];
                    unsafe {
                        core::ptr::copy_nonoverlapping(buf_ptr as *const u8, data.as_mut_ptr(), len);
                    }
                    match socket.send_slice(&data, endpoint) {
                        Ok(()) => val_reply(len as u64),
                        Err(_) => err_reply(-12),
                    }
                }
            } else {
                err_reply(-100)
            }
        }
        SockState::IcmpUnbound => {
            if addr_ptr == 0 || addrlen < 8 { return err_reply(-89); }
            if len < 8 { return err_reply(-22); }
            let sin_addr = unsafe { ((addr_ptr + 4) as *const u32).read_unaligned() };
            let dest_ip = IpAddress::from(smoltcp::wire::Ipv4Address::from_bytes(&sin_addr.to_ne_bytes()));
            // Bind smoltcp's filter to whatever ident the caller already put in its
            // own ICMP header (bytes 4..6), rather than picking one ourselves and
            // having no way to tell the caller — see plan for why.
            let ident = u16::from_be_bytes(unsafe {
                [*(buf_ptr as *const u8).add(4), *(buf_ptr as *const u8).add(5)]
            });
            let mut data = alloc::vec![0u8; len];
            unsafe { core::ptr::copy_nonoverlapping(buf_ptr as *const u8, data.as_mut_ptr(), len); }
            drop(tbls);

            let mut stack = NET_STACK.lock();
            if stack.is_none() { return err_reply(-100); }
            let s = stack.as_mut().unwrap();

            let rx_buffer = icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; 4], alloc::vec![0; 2048]);
            let tx_buffer = icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; 4], alloc::vec![0; 2048]);
            let mut socket = icmp::Socket::new(rx_buffer, tx_buffer);
            if socket.bind(icmp::Endpoint::Ident(ident)).is_err() { return err_reply(-22); }
            let socket_handle = s.socket_set.add(socket);
            let result = s.socket_set.get_mut::<icmp::Socket>(socket_handle).send_slice(&data, dest_ip);
            drop(stack);

            let mut tbls2 = SOCK_TABLES.lock();
            if let Some(tbl2) = find_tbl(pid, &mut *tbls2) {
                tbl2.socks[slot].state = SockState::IcmpBound { socket_handle };
            }

            match result {
                Ok(()) => val_reply(len as u64),
                Err(icmp::SendError::BufferFull)    => err_reply(-11),
                Err(icmp::SendError::Unaddressable) => err_reply(-89),
            }
        }
        SockState::IcmpBound { socket_handle } => {
            if addr_ptr == 0 || addrlen < 8 { return err_reply(-89); }
            let sin_addr = unsafe { ((addr_ptr + 4) as *const u32).read_unaligned() };
            let dest_ip = IpAddress::from(smoltcp::wire::Ipv4Address::from_bytes(&sin_addr.to_ne_bytes()));
            let mut data = alloc::vec![0u8; len];
            unsafe { core::ptr::copy_nonoverlapping(buf_ptr as *const u8, data.as_mut_ptr(), len); }
            drop(tbls);

            let mut stack = NET_STACK.lock();
            if stack.is_none() { return err_reply(-100); }
            let s = stack.as_mut().unwrap();
            let socket = s.socket_set.get_mut::<icmp::Socket>(socket_handle);
            match socket.send_slice(&data, dest_ip) {
                Ok(()) => val_reply(len as u64),
                Err(icmp::SendError::BufferFull)    => err_reply(-11),
                Err(icmp::SendError::Unaddressable) => err_reply(-89),
            }
        }
        _ => err_reply(-32),
    }
}

fn handle_recv(pid: u32, fd: usize, buf_ptr: usize, len: usize, addr_ptr: usize, addrlen_ptr: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    let tbls = SOCK_TABLES.lock();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }

    let state = tbl.socks[slot].state;
    let sock_type = tbl.socks[slot].sock_type;
    drop(tbls);

    match state {
        SockState::UnixConnected { conn_idx, is_a } => {
            let mut conns = UNIX_CONNS.lock();
            let conn = &mut conns[conn_idx];
            if !conn.in_use { return val_reply(0); }
            let n = if sock_type == SOCK_STREAM as u8 {
                if is_a {
                    conn.ring_ba.read(buf_ptr as *mut u8, len)
                } else {
                    conn.ring_ab.read(buf_ptr as *mut u8, len)
                }
            } else {
                if is_a {
                    conn.ring_ba.read_dgram(buf_ptr as *mut u8, len).unwrap_or(0)
                } else {
                    conn.ring_ab.read_dgram(buf_ptr as *mut u8, len).unwrap_or(0)
                }
            };
            // POSIX stream semantics: 0 bytes means EOF, and EOF only exists
            // once the peer end has closed. An empty ring with a live peer is
            // EAGAIN — returning 0 here made tokio's signal driver see "EOF
            // on self-pipe" on its very first empty poll and panic.
            if n == 0 && len > 0 {
                let peer_closed = if is_a { conn.closed_b } else { conn.closed_a };
                if !peer_closed { return err_reply(-11); } // EAGAIN
            }
            // Draining bytes frees ring space → a POLLOUT edge for the peer.
            if n > 0 { conn.seq = conn.seq.wrapping_add(1); }
            drop(conns); // release UNIX_CONNS before waking pollers (K2 lock order)
            if n > 0 { sched::wake_poll(); }
            val_reply(n as u64)
        }
        SockState::UnixPendingAccept { conn_idx, .. } => {
            // Connector (end A) before the peer accept()s: reads its inbound
            // direction (ring_ba), which stays empty until the peer accepts and
            // replies. Empty + peer-not-closed is EAGAIN, never a spurious EOF.
            let mut conns = UNIX_CONNS.lock();
            let conn = &mut conns[conn_idx];
            if !conn.in_use { return val_reply(0); }
            let n = conn.ring_ba.read(buf_ptr as *mut u8, len);
            if n == 0 && len > 0 && !conn.closed_b { return err_reply(-11); } // EAGAIN
            if n > 0 { conn.seq = conn.seq.wrapping_add(1); }
            drop(conns);
            if n > 0 { sched::wake_poll(); }
            val_reply(n as u64)
        }
        SockState::InetConnected { socket_handle, .. } => {
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                if sock_type == SOCK_STREAM as u8 {
                    let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    if !socket.can_recv() {
                        if !socket.is_active() {
                            return val_reply(0);
                        }
                        return err_reply(-11);
                    }
                    let mut data = alloc::vec![0u8; len];
                    match socket.recv_slice(&mut data) {
                        Ok(n) => {
                            unsafe {
                                core::ptr::copy_nonoverlapping(data.as_ptr(), buf_ptr as *mut u8, n);
                            }
                            val_reply(n as u64)
                        }
                        Err(_) => err_reply(-104),
                    }
                } else {
                    let socket = s.socket_set.get_mut::<udp::Socket>(socket_handle);
                    if !socket.can_recv() {
                        return err_reply(-11);
                    }
                    let mut data = alloc::vec![0u8; len];
                    match socket.recv_slice(&mut data) {
                        Ok((n, endpoint)) => {
                            unsafe {
                                core::ptr::copy_nonoverlapping(data.as_ptr(), buf_ptr as *mut u8, n);
                            }
                            if addr_ptr != 0 && addrlen_ptr != 0 {
                                let max_len = unsafe { *(addrlen_ptr as *mut u32) } as usize;
                                if max_len >= 8 {
                                    unsafe {
                                        core::ptr::write_bytes(addr_ptr as *mut u8, 0, max_len);
                                        core::ptr::write(addr_ptr as *mut u16, AF_INET as u16);
                                        let port_be = endpoint.endpoint.port.to_be();
                                        core::ptr::write((addr_ptr + 2) as *mut u16, port_be);
                                        if let IpAddress::Ipv4(ipv4) = endpoint.endpoint.addr {
                                            core::ptr::write((addr_ptr + 4) as *mut u32, u32::from_ne_bytes(ipv4.0));
                                        }
                                        *(addrlen_ptr as *mut u32) = 16;
                                    }
                                }
                            }
                            val_reply(n as u64)
                        }
                        Err(_) => err_reply(-11),
                    }
                }
            } else {
                err_reply(-100)
            }
        }
        SockState::IcmpBound { socket_handle } => {
            let mut stack = NET_STACK.lock();
            if stack.is_none() { return err_reply(-100); }
            let s = stack.as_mut().unwrap();
            let socket = s.socket_set.get_mut::<icmp::Socket>(socket_handle);
            if !socket.can_recv() { return err_reply(-11); }
            match socket.recv() {
                Ok((payload, from_addr)) => {
                    let n = len.min(payload.len());
                    unsafe {
                        core::ptr::copy_nonoverlapping(payload.as_ptr(), buf_ptr as *mut u8, n);
                    }
                    if addr_ptr != 0 && addrlen_ptr != 0 {
                        unsafe {
                            core::ptr::write_bytes(addr_ptr as *mut u8, 0, 16);
                            core::ptr::write(addr_ptr as *mut u16, AF_INET as u16);
                            core::ptr::write((addr_ptr + 2) as *mut u16, 0u16); // ICMP has no port
                            if let IpAddress::Ipv4(ipv4) = from_addr {
                                core::ptr::write((addr_ptr + 4) as *mut u32, u32::from_ne_bytes(ipv4.0));
                            }
                            *(addrlen_ptr as *mut u32) = 16;
                        }
                    }
                    val_reply(n as u64)
                }
                Err(_) => err_reply(-11),
            }
        }
        _ => err_reply(-9),
    }
}

/// Resolve `fd` to a connected AF_UNIX **stream** end. SCM_RIGHTS fd passing is
/// scoped to stream sockets (Wayland/D-Bus): dgram framing and per-message fd
/// attachment don't compose cleanly, so a dgram/inet fd returns None and the
/// caller takes the plain-data path.
fn unix_stream_end(pid: u32, fd: usize) -> Option<(usize, bool)> {
    let slot = fd_to_slot(fd)?;
    let tbls = SOCK_TABLES.lock();
    let tbl = tbls.iter().find(|t| t.in_use && t.pid == pid)?;
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return None; }
    if tbl.socks[slot].sock_type != SOCK_STREAM as u8 { return None; }
    match tbl.socks[slot].state {
        SockState::UnixConnected { conn_idx, is_a } => Some((conn_idx, is_a)),
        // A connected-but-not-yet-accepted stream socket is end A (the connector).
        // Linux: post-connect it is ESTABLISHED and writable; data written before
        // the peer accept()s buffers into the ring and is delivered on the peer's
        // first recv after accept (accept preserves this conn_idx + ring). Covers
        // sendmsg/recvmsg with fds (Wayland get_registry right after connect).
        SockState::UnixPendingAccept { conn_idx, .. } => Some((conn_idx, true)),
        _ => None,
    }
}

#[inline]
unsafe fn rd_usize(p: usize) -> usize { core::ptr::read_unaligned(p as *const usize) }

/// Parse SCM_RIGHTS control messages out of a user control buffer into
/// `out` (int fds). Returns the fd count, or Err(errno) on a malformed cmsg
/// (EINVAL). Non-SCM_RIGHTS cmsgs are skipped. Reads user memory directly
/// (caller's address space is active) — no lock is held here.
unsafe fn parse_scm_rights(ctrl_ptr: usize, ctrl_len: usize, out: &mut [i32; SCM_MAX_FD]) -> Result<usize, i32> {
    let mut pos = 0usize;
    let mut nfd = 0usize;
    while pos + CMSG_HDR_LEN <= ctrl_len {
        let cmsg_len   = rd_usize(ctrl_ptr + pos);
        let cmsg_level = core::ptr::read_unaligned((ctrl_ptr + pos + 8) as *const i32);
        let cmsg_type  = core::ptr::read_unaligned((ctrl_ptr + pos + 12) as *const i32);
        if cmsg_len < CMSG_HDR_LEN || pos + cmsg_len > ctrl_len { return Err(-22); } // EINVAL
        if cmsg_level == SOL_SOCKET && cmsg_type == SCM_RIGHTS {
            let data_len = cmsg_len - CMSG_HDR_LEN;
            let count = data_len / 4;
            for i in 0..count {
                if nfd >= SCM_MAX_FD { return Err(-22); } // > SCM_MAX_FD → EINVAL
                out[nfd] = core::ptr::read_unaligned((ctrl_ptr + pos + CMSG_HDR_LEN + i * 4) as *const i32);
                nfd += 1;
            }
        }
        let step = cmsg_align(cmsg_len);
        if step == 0 { break; }
        pos += step;
    }
    Ok(nfd)
}

fn handle_sendmsg(pid: u32, fd: usize, msghdr_ptr: usize, _flags: usize) -> Message {
    if msghdr_ptr == 0 { return err_reply(-14); }
    let iov_ptr  = unsafe { rd_usize(msghdr_ptr + 16) };
    let iovcnt   = unsafe { rd_usize(msghdr_ptr + 24) };
    let ctrl_ptr = unsafe { rd_usize(msghdr_ptr + 32) };
    let ctrl_len = unsafe { rd_usize(msghdr_ptr + 40) };

    let unix_end = unix_stream_end(pid, fd);

    // Parse SCM_RIGHTS fds only for a unix stream socket that actually carries
    // a control buffer. Inet/dgram control is ignored (can't carry fds).
    let mut fd_nums = [0i32; SCM_MAX_FD];
    let mut nfd = 0usize;
    if unix_end.is_some() && ctrl_ptr != 0 && ctrl_len >= CMSG_HDR_LEN {
        match unsafe { parse_scm_rights(ctrl_ptr, ctrl_len.min(MAX_CONTROL), &mut fd_nums) } {
            Ok(n)  => nfd = n,
            Err(e) => return err_reply(e),
        }
    }

    // No ancillary fds → original plain-data fast path (also covers inet).
    if nfd == 0 {
        let mut total = 0isize;
        for i in 0..iovcnt.min(16) {
            let iov  = iov_ptr + i * 16;
            let base = unsafe { rd_usize(iov) };
            let len  = unsafe { rd_usize(iov + 8) };
            let n = net_val(&handle_send(pid, fd, base, len, 0, 0));
            if n < 0 { return if total > 0 { val_reply(total as u64) } else { make_reply(n as i64) }; }
            total += n;
        }
        return val_reply(total as u64);
    }

    // Have fds. Export them out of the sender's fd table (locks FD_TABLES —
    // done BEFORE taking UNIX_CONNS so the two locks are never nested here).
    let (conn_idx, is_a) = unix_end.unwrap();
    let mut batch: alloc::vec::Vec<vfs::TransferFd> = alloc::vec::Vec::new();
    for i in 0..nfd {
        match vfs::export_fd(pid, fd_nums[i] as usize) {
            Some(tf) => batch.push(tf),
            None => { for tf in batch { vfs::drop_transfer(tf); } return err_reply(-9); } // EBADF
        }
    }

    // Pre-read the iov descriptors before taking the lock.
    let mut iovs = [(0usize, 0usize); 16];
    let n_iov = iovcnt.min(16);
    let mut requested = 0usize;
    for i in 0..n_iov {
        let iov = iov_ptr + i * 16;
        iovs[i] = (unsafe { rd_usize(iov) }, unsafe { rd_usize(iov + 8) });
        requested += iovs[i].1;
    }
    // A stream sendmsg needs at least one data byte to carry the fds; with none
    // requested there is no byte to ride and no EAGAIN retry could ever place
    // one — drop the batch (Linux does not queue fds for a zero-length send).
    if requested == 0 {
        for tf in batch { vfs::drop_transfer(tf); }
        return val_reply(0);
    }

    // Write the data and attach the fd batch to the first byte written, under a
    // single UNIX_CONNS critical section so the stream offset can't race.
    let mut conns = UNIX_CONNS.lock();
    let conn = &mut conns[conn_idx];
    if !conn.in_use {
        drop(conns);
        for tf in batch { vfs::drop_transfer(tf); }
        return err_reply(-32);
    }
    let peer_closed = if is_a { conn.closed_b } else { conn.closed_a };
    if peer_closed {
        drop(conns);
        for tf in batch { vfs::drop_transfer(tf); }
        return err_reply(-32); // EPIPE
    }
    // Bound the in-flight SCM_RIGHTS fds on this direction. Past the cap the
    // send fails rather than letting the PendingFdBatch queue grow unbounded
    // (Linux: ETOOMANYREFS). Closes the K1-A handoff note.
    let queued: usize = if is_a {
        conn.fdq_ab.iter().map(|b| b.fds.len()).sum()
    } else {
        conn.fdq_ba.iter().map(|b| b.fds.len()).sum()
    };
    if queued + nfd > QUEUED_FD_CAP {
        drop(conns);
        for tf in batch { vfs::drop_transfer(tf); }
        return err_reply(ETOOMANYREFS);
    }
    let seq = if is_a { conn.ring_ab.wtotal } else { conn.ring_ba.wtotal };
    let mut total = 0usize;
    for i in 0..n_iov {
        let (base, len) = iovs[i];
        let n = if is_a {
            conn.ring_ab.write(base as *const u8, len)
        } else {
            conn.ring_ba.write(base as *const u8, len)
        };
        total += n;
        if n < len { break; } // ring full → partial write
    }
    if total == 0 {
        // Couldn't place even the first byte; the blocking wrapper will retry.
        // Release the batch so the retry re-exports a fresh copy.
        drop(conns);
        for tf in batch { vfs::drop_transfer(tf); }
        return err_reply(-11); // EAGAIN
    }
    if is_a {
        conn.fdq_ab.push(PendingFdBatch { seq_byte: seq, fds: batch });
    } else {
        conn.fdq_ba.push(PendingFdBatch { seq_byte: seq, fds: batch });
    }
    // New readable edge for the peer (total > 0 guaranteed above).
    conn.seq = conn.seq.wrapping_add(1);
    drop(conns); // release UNIX_CONNS before waking pollers (K2 lock order)
    sched::wake_poll();
    val_reply(total as u64)
}

/// Write msg_controllen (offset 40) and msg_flags (offset 48) into a user
/// msghdr. Always called on the recvmsg success path — msg_flags must be set
/// even when it is 0.
unsafe fn write_msg_tail(msghdr_ptr: usize, controllen: usize, msg_flags: i32) {
    core::ptr::write_unaligned((msghdr_ptr + 40) as *mut usize, controllen);
    core::ptr::write_unaligned((msghdr_ptr + 48) as *mut i32, msg_flags);
}

fn handle_recvmsg(pid: u32, fd: usize, msghdr_ptr: usize, flags: usize) -> Message {
    if msghdr_ptr == 0 { return err_reply(-14); }
    let iov_ptr  = unsafe { rd_usize(msghdr_ptr + 16) };
    let iovcnt   = unsafe { rd_usize(msghdr_ptr + 24) };
    let ctrl_ptr = unsafe { rd_usize(msghdr_ptr + 32) };
    let ctrl_cap = unsafe { rd_usize(msghdr_ptr + 40) };

    let unix_end = unix_stream_end(pid, fd);

    // Non-unix (inet/dgram): plain-data path; still writes msg_flags/controllen.
    let (conn_idx, is_a) = match unix_end {
        Some(e) => e,
        None => {
            let mut total = 0isize;
            for i in 0..iovcnt.min(16) {
                let iov  = iov_ptr + i * 16;
                let base = unsafe { rd_usize(iov) };
                let len  = unsafe { rd_usize(iov + 8) };
                let n = net_val(&handle_recv(pid, fd, base, len, 0, 0));
                if n < 0 { return if total > 0 { val_reply(total as u64) } else { make_reply(n as i64) }; }
                total += n;
            }
            unsafe { write_msg_tail(msghdr_ptr, 0, 0); }
            return val_reply(total as u64);
        }
    };

    // Pre-read iov descriptors.
    let mut iovs = [(0usize, 0usize); 16];
    let n_iov = iovcnt.min(16);
    let mut requested = 0usize;
    for i in 0..n_iov {
        let iov = iov_ptr + i * 16;
        iovs[i] = (unsafe { rd_usize(iov) }, unsafe { rd_usize(iov + 8) });
        requested += iovs[i].1;
    }
    // A zero-length recv can't consume the fd-carrying byte and would spin the
    // blocking wrapper on EAGAIN forever — return 0 immediately (msg_flags set).
    if requested == 0 {
        unsafe { write_msg_tail(msghdr_ptr, 0, 0); }
        return val_reply(0);
    }

    // Read data and pop at most one deliverable fd batch, under one lock.
    let mut conns = UNIX_CONNS.lock();
    let conn = &mut conns[conn_idx];
    if !conn.in_use {
        drop(conns);
        unsafe { write_msg_tail(msghdr_ptr, 0, 0); }
        return val_reply(0);
    }
    let rstart = if is_a { conn.ring_ba.rtotal } else { conn.ring_ab.rtotal };
    // Don't read across a second ancillary boundary: a recv delivers at most
    // one fd batch, so cap the byte count so it can't consume the byte the
    // *next* batch rides with (Linux stops coalescing at an ancillary skb).
    let q_len = if is_a { conn.fdq_ba.len() } else { conn.fdq_ab.len() };
    let max_read = if q_len >= 2 {
        let second = if is_a { conn.fdq_ba[1].seq_byte } else { conn.fdq_ab[1].seq_byte };
        (second - rstart) as usize
    } else {
        usize::MAX
    };

    let mut nread = 0usize;
    for i in 0..n_iov {
        if nread >= max_read { break; }
        let (base, len) = iovs[i];
        let want = len.min(max_read - nread);
        let n = if is_a {
            conn.ring_ba.read(base as *mut u8, want)
        } else {
            conn.ring_ab.read(base as *mut u8, want)
        };
        nread += n;
        if n < want { break; } // ring drained
    }

    if nread == 0 {
        // Empty ring: EOF only if the peer end has closed, else EAGAIN. Mirrors
        // handle_recv — tokio's self-pipe must not see a spurious EOF.
        let peer_closed = if is_a { conn.closed_b } else { conn.closed_a };
        if !peer_closed && conn.in_use {
            drop(conns);
            return err_reply(-11); // EAGAIN — blocking wrapper retries
        }
    }

    let rtotal = if is_a { conn.ring_ba.rtotal } else { conn.ring_ab.rtotal };
    let deliver: Option<alloc::vec::Vec<vfs::TransferFd>> = {
        let q = if is_a { &mut conn.fdq_ba } else { &mut conn.fdq_ab };
        if !q.is_empty() && q[0].seq_byte < rtotal {
            Some(q.remove(0).fds)
        } else {
            None
        }
    };
    // Draining bytes frees ring space → a POLLOUT edge for the peer.
    let freed = nread > 0;
    if freed { conn.seq = conn.seq.wrapping_add(1); }
    drop(conns); // release before importing (locks FD_TABLES)
    if freed { sched::wake_poll(); }

    // Install the delivered fds into the receiver and serialize the cmsg.
    let cloexec = flags & MSG_CMSG_CLOEXEC != 0;
    let mut ctrunc = false;
    let mut ctrl_written = 0usize;
    if let Some(fds) = deliver {
        let nfds = fds.len();
        // How many fds fit: need the cmsg header plus 4 bytes each.
        let mut fit = if ctrl_ptr == 0 || ctrl_cap < CMSG_HDR_LEN + 4 {
            0
        } else {
            ((ctrl_cap - CMSG_HDR_LEN) / 4).min(nfds)
        };
        if fit < nfds { ctrunc = true; }
        let mut installed = [0i32; SCM_MAX_FD];
        let mut i = 0usize;
        while i < fit {
            let newfd = vfs::import_fd(pid, fds[i], cloexec);
            if newfd < 0 {
                // Receiver's fd table is full: everything from here truncates.
                ctrunc = true;
                fit = i;
                break;
            }
            installed[i] = newfd as i32;
            i += 1;
        }
        // Close every fd that didn't fit (Linux drops the overflow).
        for j in fit..nfds { vfs::drop_transfer(fds[j]); }
        if fit > 0 {
            unsafe {
                let clen = CMSG_HDR_LEN + fit * 4;
                core::ptr::write_unaligned(ctrl_ptr as *mut usize, clen);
                core::ptr::write_unaligned((ctrl_ptr + 8) as *mut i32, SOL_SOCKET);
                core::ptr::write_unaligned((ctrl_ptr + 12) as *mut i32, SCM_RIGHTS);
                for k in 0..fit {
                    core::ptr::write_unaligned((ctrl_ptr + CMSG_HDR_LEN + k * 4) as *mut i32, installed[k]);
                }
                ctrl_written = clen;
            }
        }
    }

    unsafe { write_msg_tail(msghdr_ptr, ctrl_written, if ctrunc { MSG_CTRUNC } else { 0 }); }
    val_reply(nread as u64)
}

fn handle_shutdown(pid: u32, fd: usize, _how: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    let mut tbls = SOCK_TABLES.lock();
    let mut do_wake = false;
    if let Some(tbl) = find_tbl(pid, &mut *tbls) {
        if slot < MAX_SOCKS && tbl.socks[slot].in_use {
            if let SockState::UnixConnected { conn_idx, is_a } = tbl.socks[slot].state {
                let mut conns = UNIX_CONNS.lock();
                if is_a { conns[conn_idx].closed_a = true; }
                else    { conns[conn_idx].closed_b = true; }
                conns[conn_idx].seq = conns[conn_idx].seq.wrapping_add(1);
                if conns[conn_idx].closed_a && conns[conn_idx].closed_b {
                    conns[conn_idx].drain_fds();
                    conns[conn_idx].in_use = false;
                }
                drop(conns);
                do_wake = true;
            }
            tbl.socks[slot] = SockEntry::empty();
        }
    }
    drop(tbls);
    // Peer sees POLLHUP/POLLIN (EOF) — wake after releasing all server locks.
    if do_wake { sched::wake_poll(); }
    ok_reply()
}

fn handle_getsockname(_pid: u32, _fd: usize, addr_ptr: usize, addrlen_ptr: usize) -> Message {
    if addr_ptr == 0 || addrlen_ptr == 0 { return err_reply(-14); }
    unsafe {
        core::ptr::write_bytes(addr_ptr as *mut u8, 0, 2);
        core::ptr::write(addrlen_ptr as *mut u32, 2);
    }
    ok_reply()
}

fn handle_getpeername(pid: u32, fd: usize, addr_ptr: usize, addrlen_ptr: usize) -> Message {
    handle_getsockname(pid, fd, addr_ptr, addrlen_ptr)
}

fn handle_getsockopt(pid: u32, fd: usize, level: usize, optname: usize,
                     optval_ptr: usize, optlen_ptr: usize) -> Message {
    // SO_PEERCRED: report the peer end's captured {pid,uid,gid} (struct ucred).
    // D-Bus EXTERNAL auth reads the uid from here.
    if level == SOL_SOCKET as usize && optname == SO_PEERCRED {
        if optval_ptr == 0 { return err_reply(-14); }
        let cred = unix_peer_cred(pid, fd);
        unsafe {
            core::ptr::write_unaligned(optval_ptr as *mut u32, cred.pid);
            core::ptr::write_unaligned((optval_ptr + 4) as *mut u32, cred.uid);
            core::ptr::write_unaligned((optval_ptr + 8) as *mut u32, cred.gid);
        }
        if optlen_ptr != 0 {
            unsafe { core::ptr::write_unaligned(optlen_ptr as *mut u32, 12); }
        }
        return ok_reply();
    }
    // SO_ERROR: report "no pending error" (0). mio/tokio read this after a
    // non-blocking connect to detect completion; keep it a success.
    if level == SOL_SOCKET as usize && optname == SO_ERROR {
        if optval_ptr != 0 {
            unsafe { core::ptr::write(optval_ptr as *mut u32, 0); }
        }
        if optlen_ptr != 0 {
            unsafe { core::ptr::write(optlen_ptr as *mut u32, 4); }
        }
        return ok_reply();
    }
    // Any other option is unsupported. Linux returns ENOPROTOOPT; returning a
    // bogus success (with optval left unwritten) makes callers read garbage —
    // e.g. zbus's SO_PEERPIDFD probe (connection/socket/unix.rs) treats ret==0
    // as "kernel supports pidfd" and wraps the zero-initialised buffer as
    // OwnedFd::from_raw_fd(0), taking ownership of fd 0 and closing it out from
    // under the process when the credentials drop. Match Linux and fail it so
    // zbus takes its graceful ENOPROTOOPT fallback.
    err_reply(-ENOPROTOOPT)
}

/// The credentials of the peer of `fd` (the other end of the connection), for
/// SO_PEERCRED. Falls back to zeroes for a non-connected/unknown fd.
fn unix_peer_cred(pid: u32, fd: usize) -> Ucred {
    let (conn_idx, is_a) = match unix_stream_end(pid, fd) {
        Some(e) => e,
        None => return Ucred::zero(),
    };
    let conns = UNIX_CONNS.lock();
    // Peer of end A is end B and vice-versa.
    if is_a { conns[conn_idx].cred_b } else { conns[conn_idx].cred_a }
}

/// fork(): give `child` its own copies of `parent`'s socket fds, per POSIX
/// fd-inheritance. Each copied fd is an alias of the same connection end
/// (per-end refcount bumped), so the child closing its inherited exec-error
/// socketpair — CLOEXEC at exec, or exit — participates in the same
/// EOF/EPIPE accounting the parent observes. Inet/listening sockets are NOT
/// copied: their smoltcp handles have no refcounting and a dup'd close
/// would double-free them (fork children exec immediately in practice).
fn handle_fork_dup(parent: u32, child: u32) -> Message {
    let mut tbls = SOCK_TABLES.lock();
    // Work by table index, copying one SockEntry at a time. Snapshotting the
    // whole `socks` array (as `let x = t.socks`) would put a ~24 KB copy
    // (MAX_SOCKS * SockEntry) on the kernel stack at every fork.
    if tbls.iter().position(|t| t.in_use && t.pid == parent).is_none() {
        return ok_reply(); // parent has no sockets — nothing to do
    }
    if get_or_create(child, &mut *tbls).is_none() {
        return err_reply(-12);
    }
    let parent_pos = tbls.iter().position(|t| t.in_use && t.pid == parent).unwrap();
    let child_pos  = tbls.iter().position(|t| t.in_use && t.pid == child).unwrap();

    let mut ends: alloc::vec::Vec<(usize, bool)> = alloc::vec::Vec::new();
    for i in 0..MAX_SOCKS {
        let e = tbls[parent_pos].socks[i];
        if !e.in_use { continue; }
        match e.state {
            SockState::UnixConnected { conn_idx, is_a } => {
                tbls[child_pos].socks[i] = e;
                ends.push((conn_idx, is_a));
            }
            SockState::Unbound { .. } => { tbls[child_pos].socks[i] = e; }
            _ => {} // inet/listening: skipped (see doc comment)
        }
    }
    drop(tbls);
    let mut conns = UNIX_CONNS.lock();
    for (conn_idx, is_a) in ends {
        if !conns[conn_idx].in_use { continue; }
        if is_a { conns[conn_idx].refs_a += 1; } else { conns[conn_idx].refs_b += 1; }
    }
    ok_reply()
}

/// execve(): close every close-on-exec socket fd of `pid`'s process.
fn handle_exec_cloexec(pid: u32) -> Message {
    for slot in 0..MAX_SOCKS {
        let is_cloexec = {
            let tbls = SOCK_TABLES.lock();
            match tbls.iter().find(|t| t.in_use && t.pid == pid) {
                Some(t) => t.socks[slot].in_use && t.socks[slot].cloexec,
                None    => return ok_reply(),
            }
        };
        if is_cloexec {
            let _ = handle_close(pid, SOCK_FD_BASE + slot);
        }
    }
    ok_reply()
}

/// fcntl(F_SETFL): only O_NONBLOCK (0x800) is meaningful for socket rings.
fn handle_setfl(pid: u32, sockfd: usize, flags: u32) -> Message {
    let slot = match fd_to_slot(sockfd) { Some(s) => s, None => return err_reply(-9) };
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
    tbl.socks[slot].nonblock = flags & 0x800 != 0;
    ok_reply()
}

/// fcntl(F_GETFL) for sockets: report O_NONBLOCK.
fn handle_getfl(pid: u32, sockfd: usize) -> Message {
    let slot = match fd_to_slot(sockfd) { Some(s) => s, None => return err_reply(-9) };
    let tbls = SOCK_TABLES.lock();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) { Some(t) => t, None => return err_reply(-9) };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
    val_reply(if tbl.socks[slot].nonblock { 0x800 } else { 0 })
}

/// fcntl(F_SETFD) for sockets: set/clear the close-on-exec flag. Critical for
/// programs that inherit a SOCK_CLOEXEC socket and clear FD_CLOEXEC before
/// execve so the fd survives — e.g. launch_pad's `with_fds`, which hands
/// cosmic-panel / cosmic-notifications their notification socket by clearing
/// cloexec in the child's pre_exec. Without this the flag never changed, the
/// socket stayed cloexec, and the execve cloexec-sweep closed it → the child
/// saw EBADF ("Bad file descriptor"). Only FD_CLOEXEC (bit 0) is defined.
fn handle_setfd(pid: u32, sockfd: usize, arg: u32) -> Message {
    let slot = match fd_to_slot(sockfd) { Some(s) => s, None => return err_reply(-9) };
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) { Some(t) => t, None => return err_reply(-9) };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
    tbl.socks[slot].cloexec = arg & 1 != 0; // FD_CLOEXEC
    ok_reply()
}

/// fcntl(F_GETFD) for sockets: report FD_CLOEXEC.
fn handle_getfd(pid: u32, sockfd: usize) -> Message {
    let slot = match fd_to_slot(sockfd) { Some(s) => s, None => return err_reply(-9) };
    let tbls = SOCK_TABLES.lock();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) { Some(t) => t, None => return err_reply(-9) };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
    val_reply(if tbl.socks[slot].cloexec { 1 } else { 0 })
}

/// fcntl(F_DUPFD/F_DUPFD_CLOEXEC) on a socket fd: allocate a second slot
/// aliasing the same connection end (tokio/mio clone the fd of one
/// socketpair end this way for their signal driver).
fn handle_dup(pid: u32, sockfd: usize, cloexec: bool) -> Message {
    let slot = match fd_to_slot(sockfd) { Some(s) => s, None => return err_reply(-9) };
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match find_tbl(pid, &mut *tbls) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
    let entry = tbl.socks[slot];
    let new_slot = match tbl.alloc() { Some(s) => s, None => return err_reply(-24) };
    if let SockState::UnixConnected { conn_idx, is_a } = entry.state {
        let mut conns = UNIX_CONNS.lock();
        if !conns[conn_idx].in_use { return err_reply(-9); }
        if is_a { conns[conn_idx].refs_a += 1; } else { conns[conn_idx].refs_b += 1; }
    }
    // Inet/listening sockets alias fine at the table level: their smoltcp
    // handle is only released by handle_close, which the refcountless copy
    // would double-free — so restrict dup to states that carry no handle.
    else if !matches!(entry.state, SockState::Unbound { .. }) {
        return err_reply(-22); // EINVAL — dup of this socket kind unsupported
    }
    let mut new_entry = entry;
    new_entry.cloexec = cloexec;
    tbl.socks[new_slot] = new_entry;
    val_reply((new_slot + SOCK_FD_BASE) as u64)
}

fn handle_close(pid: u32, sockfd: usize) -> Message {
    if sockfd < SOCK_FD_BASE { return err_reply(-9); }
    let slot = sockfd - SOCK_FD_BASE;
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        Some(t) => t,
        None    => return err_reply(-9),
    };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
    let state  = tbl.socks[slot].state;
    let port   = tbl.socks[slot].bound_port;
    match state {
        SockState::UnixConnected { conn_idx, is_a } => {
            drop(tbls);
            let mut conns = UNIX_CONNS.lock();
            // Only the last alias of this end actually closes the end (dup'd
            // fds share it — see refs_a/refs_b). Closing one end marks it
            // closed so the peer observes EOF/EPIPE; the connection object
            // itself lives until both ends are gone.
            let c = &mut conns[conn_idx];
            let refs = if is_a { &mut c.refs_a } else { &mut c.refs_b };
            *refs = refs.saturating_sub(1);
            let mut end_closed = false;
            if *refs == 0 {
                if is_a { c.closed_a = true; } else { c.closed_b = true; }
                c.seq = c.seq.wrapping_add(1);
                end_closed = true;
                if c.closed_a && c.closed_b { c.drain_fds(); c.in_use = false; }
            }
            drop(conns);
            let mut tbls2 = SOCK_TABLES.lock();
            if let Some(t2) = tbls2.iter_mut().find(|t| t.in_use && t.pid == pid) {
                t2.socks[slot] = SockEntry::empty();
            }
            drop(tbls2);
            // Peer sees POLLHUP/POLLIN (EOF) once this end really closed.
            if end_closed { sched::wake_poll(); }
        }
        SockState::InetConnected { socket_handle, .. } => {
            tbl.socks[slot] = SockEntry::empty();
            drop(tbls);
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                s.socket_set.remove(socket_handle);
            }
        }
        SockState::InetListening { socket_handle } => {
            tbl.socks[slot] = SockEntry::empty();
            drop(tbls);
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                s.socket_set.remove(socket_handle);
            }
        }
        SockState::IcmpBound { socket_handle } => {
            tbl.socks[slot] = SockEntry::empty();
            drop(tbls);
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                s.socket_set.remove(socket_handle);
            }
        }
        SockState::UnixListening { bound_idx } => {
            tbl.socks[slot] = SockEntry::empty();
            drop(tbls);
            // Reclaim the address (its VFS node, if any, lingers per Linux).
            free_bound_idx(bound_idx);
        }
        SockState::UnixPendingAccept { conn_idx, .. } => {
            tbl.socks[slot] = SockEntry::empty();
            drop(tbls);
            // A connect that never got accepted: drop its half-open connection.
            let mut conns = UNIX_CONNS.lock();
            if conn_idx < MAX_CONNS && conns[conn_idx].in_use {
                conns[conn_idx].drain_fds();
                conns[conn_idx].in_use = false;
            }
        }
        _ => { tbl.socks[slot] = SockEntry::empty(); }
    }
    ok_reply()
}

fn handle_poll(pid: u32, fd: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    let tbls = SOCK_TABLES.lock();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
    let state = tbl.socks[slot].state;
    let sock_type = tbl.socks[slot].sock_type;

    let (revents, seq): (u64, Option<u64>) = match state {
        SockState::UnixConnected { conn_idx, is_a } => {
            drop(tbls);
            let conns = UNIX_CONNS.lock();
            let conn = &conns[conn_idx];
            let readable   = if is_a { conn.ring_ba.count } else { conn.ring_ab.count };
            let write_free = RING_SIZE - if is_a { conn.ring_ab.count } else { conn.ring_ba.count };
            let peer_closed = if is_a { conn.closed_b } else { conn.closed_a };
            let mut ev = 0;
            // Peer-closed asserts readable: the pending EOF must wake
            // poll/epoll waiters (level-triggered "read will not block").
            if readable > 0 || peer_closed || !conn.in_use { ev |= POLLIN; }
            if conn.in_use && !peer_closed && write_free > 0 { ev |= POLLOUT; }
            if !conn.in_use || peer_closed { ev |= POLLHUP; }
            // Connected sockets carry the edge-seq so EPOLLET works.
            (ev, Some(conn.seq))
        }
        SockState::UnixPendingAccept { conn_idx, .. } => {
            // Connector (end A) awaiting accept: established + writable now (Linux),
            // so a client that waits for POLLOUT before its first write proceeds.
            // Readable once the peer accepts and replies (ring_ba) or the conn dies.
            drop(tbls);
            let conns = UNIX_CONNS.lock();
            let conn = &conns[conn_idx];
            let readable   = conn.ring_ba.count;
            let write_free = RING_SIZE - conn.ring_ab.count;
            let mut ev = 0;
            if readable > 0 || !conn.in_use { ev |= POLLIN; }
            if conn.in_use && !conn.closed_b && write_free > 0 { ev |= POLLOUT; }
            if !conn.in_use || conn.closed_b { ev |= POLLHUP; }
            (ev, Some(conn.seq))
        }
        SockState::UnixListening { bound_idx } => {
            drop(tbls);
            // Readable only when a connect is pending against *this* listener's
            // address (matched by its unique sock_id — see handle_accept), not
            // against any listener in the system. Without this filter a connect
            // to one listener made every listener report POLLIN — spurious
            // wakeups / thundering herd under real blocking (K1-C handoff #1).
            let listen_sock_id = {
                let bound = BOUND_PATHS.lock();
                if !bound[bound_idx].in_use { 0 } else { bound[bound_idx].sock_id }
            };
            let tbls2 = SOCK_TABLES.lock();
            let pending = listen_sock_id != 0 && tbls2.iter().any(|t| t.in_use && t.socks.iter()
                .any(|s| matches!(s.state,
                    SockState::UnixPendingAccept { sock_id, .. } if sock_id == listen_sock_id)));
            (if pending { POLLIN } else { 0 }, None)
        }
        SockState::InetConnected { socket_handle, .. } => {
            drop(tbls);
            let mut stack = NET_STACK.lock();
            let ev = if let Some(ref mut s) = *stack {
                if sock_type == SOCK_STREAM as u8 {
                    let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    let mut ev = 0;
                    if socket.can_recv() || !socket.is_active() { ev |= POLLIN; }
                    if socket.can_send() && socket.is_active() { ev |= POLLOUT; }
                    if !socket.is_active() { ev |= POLLHUP; }
                    ev
                } else {
                    let socket = s.socket_set.get_mut::<udp::Socket>(socket_handle);
                    let mut ev = 0;
                    if socket.can_recv() { ev |= POLLIN; }
                    ev |= POLLOUT;
                    ev
                }
            } else {
                0
            };
            (ev, None)
        }
        SockState::InetListening { socket_handle } => {
            drop(tbls);
            let mut stack = NET_STACK.lock();
            let ev = if let Some(ref mut s) = *stack {
                let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                if socket.is_active() && socket.state() == tcp::State::Established {
                    POLLIN
                } else {
                    0
                }
            } else {
                0
            };
            (ev, None)
        }
        SockState::IcmpBound { socket_handle } => {
            drop(tbls);
            let mut stack = NET_STACK.lock();
            let ev = if let Some(ref mut s) = *stack {
                let socket = s.socket_set.get_mut::<icmp::Socket>(socket_handle);
                let mut ev = 0;
                if socket.can_recv() { ev |= POLLIN; }
                ev |= POLLOUT;
                ev
            } else {
                0
            };
            (ev, None)
        }
        _ => { drop(tbls); (0, None) }
    };
    net_poll_reply(revents, seq)
}

fn handle_close_all(pid: u32) {
    let mut tbls = SOCK_TABLES.lock();
    if let Some(tbl) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        // Heap-collected, not MAX_SOCKS-sized stack arrays — at 512 those would
        // be ~12 KB of stack in a process-teardown path.
        // Connected ends carry a per-end refcount (refs_a/refs_b) because a dup
        // or a fork copies the fd without a second connection object. Track the
        // end (is_a) so teardown can decrement that refcount instead of force-
        // freeing a still-referenced connection.
        let mut unix_conn_close: alloc::vec::Vec<(usize, bool)> = alloc::vec::Vec::new();
        // Pending-accept half-open connections are never dup'd/forked (see
        // handle_fork_dup, which copies only UnixConnected/Unbound), so they are
        // dropped outright, matching handle_close's PendingAccept arm.
        let mut unix_pending_close: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        let mut inet_to_close: alloc::vec::Vec<SocketHandle> = alloc::vec::Vec::new();
        let mut bound_to_free: alloc::vec::Vec<usize> = alloc::vec::Vec::new();

        for s in tbl.socks.iter() {
            match s.state {
                SockState::UnixConnected { conn_idx, is_a } => {
                    unix_conn_close.push((conn_idx, is_a));
                }
                SockState::UnixPendingAccept { conn_idx, .. } => {
                    unix_pending_close.push(conn_idx);
                }
                SockState::UnixListening { bound_idx } => {
                    bound_to_free.push(bound_idx);
                }
                SockState::InetConnected { socket_handle, .. }
                | SockState::InetListening { socket_handle }
                | SockState::IcmpBound { socket_handle } => {
                    inet_to_close.push(socket_handle);
                }
                _ => {}
            }
        }
        drop(tbls);

        let mut conns = UNIX_CONNS.lock();
        let mut peer_hup = false;
        // Connected ends: decrement the per-end refcount and only mark the end
        // closed (peer observes EOF/EPIPE) once the LAST alias of this end is
        // gone — identical to handle_close. Force-freeing the whole connection
        // here (the old behaviour) tore down a still-referenced connection when
        // a forked child that inherited the fd exited: cosmic-comp's failed
        // kiosk-child fork copied comp's live session-bus socket, and the
        // child's exec-error _exit force-freed the connection, so comp's next
        // recvmsg saw a spurious EOF and its zbus socket-reader errored out.
        for (ci, is_a) in unix_conn_close {
            if ci < MAX_CONNS && conns[ci].in_use {
                let c = &mut conns[ci];
                let refs = if is_a { &mut c.refs_a } else { &mut c.refs_b };
                *refs = refs.saturating_sub(1);
                if *refs == 0 {
                    if is_a { c.closed_a = true; } else { c.closed_b = true; }
                    c.seq = c.seq.wrapping_add(1);
                    peer_hup = true;
                    if c.closed_a && c.closed_b { c.drain_fds(); c.in_use = false; }
                }
            }
        }
        // Pending-accept half-open connections: drop outright.
        for ci in unix_pending_close {
            if ci < MAX_CONNS && conns[ci].in_use {
                conns[ci].drain_fds();
                conns[ci].in_use = false;
                conns[ci].seq = conns[ci].seq.wrapping_add(1);
                peer_hup = true;
            }
        }
        drop(conns);

        for bi in bound_to_free { free_bound_idx(bi); }

        let mut stack = NET_STACK.lock();
        if let Some(ref mut s) = *stack {
            for handle in inet_to_close {
                s.socket_set.remove(handle);
            }
        }
        drop(stack);

        let mut tbls = SOCK_TABLES.lock();
        if let Some(tbl) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
            tbl.reset();
        }
        drop(tbls);
        // Peers of the torn-down connections see POLLHUP/EOF (K2).
        if peer_hup { sched::wake_poll(); }
    }
}

fn net_val(m: &Message) -> isize {
    let bytes: [u8; 8] = m.data[0..8].try_into().unwrap_or([0u8; 8]);
    i64::from_le_bytes(bytes) as isize
}
