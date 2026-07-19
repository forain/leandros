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

const POLLIN:  u64 = 0x0001;
const POLLOUT: u64 = 0x0004;
const POLLHUP: u64 = 0x0010;

// ── Constants ─────────────────────────────────────────────────────────────────

pub const AF_UNIX:    usize = 1;
pub const AF_INET:    usize = 2;
pub const SOCK_STREAM: usize = 1;
pub const SOCK_DGRAM:  usize = 2;
pub const SOCK_RAW:    usize = 3;

pub const IPPROTO_ICMP: usize = 1;

pub const SOCK_FD_BASE: usize = 0x100;

const MAX_PROCS:   usize = 64;
const MAX_SOCKS:   usize = 16;
const MAX_CONNS:   usize = 32;
const MAX_BOUND:   usize = 16;
const RING_SIZE:   usize = 4096;
const PATH_MAX:    usize = 108;

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

// ── Unix connection ring buffers ──────────────────────────────────────────────

struct UnixRing {
    buf:   [u8; RING_SIZE],
    rpos:  usize,
    wpos:  usize,
    count: usize,
}

impl UnixRing {
    const fn new() -> Self {
        Self { buf: [0u8; RING_SIZE], rpos: 0, wpos: 0, count: 0 }
    }

    fn write(&mut self, data: *const u8, len: usize) -> usize {
        let free = RING_SIZE - self.count;
        let n = len.min(free);
        for i in 0..n {
            self.buf[self.wpos] = unsafe { *data.add(i) };
            self.wpos = (self.wpos + 1) % RING_SIZE;
        }
        self.count += n;
        n
    }

    fn read(&mut self, data: *mut u8, len: usize) -> usize {
        let n = len.min(self.count);
        for i in 0..n {
            unsafe { *data.add(i) = self.buf[self.rpos]; }
            self.rpos = (self.rpos + 1) % RING_SIZE;
        }
        self.count -= n;
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
        }
    }
}

static UNIX_CONNS: Mutex<[UnixConn; MAX_CONNS]> =
    Mutex::new([const { UnixConn::new() }; MAX_CONNS]);

// ── Bound AF_UNIX paths ───────────────────────────────────────────────────────

struct BoundPath {
    in_use:     bool,
    path:       [u8; PATH_MAX],
    path_len:   usize,
    _owner_pid:  u32,
    _owner_sock: usize,
}

impl BoundPath {
    const fn new() -> Self {
        Self { in_use: false, path: [0u8; PATH_MAX], path_len: 0,
               _owner_pid: 0, _owner_sock: 0 }
    }
}

static BOUND_PATHS: Mutex<[BoundPath; MAX_BOUND]> =
    Mutex::new([const { BoundPath::new() }; MAX_BOUND]);

// ── Socket kind ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum SockState {
    None,
    Unbound { domain: u8, sock_type: u8 },
    UnixListening { bound_idx: usize },
    UnixConnected { conn_idx: usize, is_a: bool },
    UnixPendingAccept { conn_idx: usize },
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
        tbls[pos] = ProcSockTable::empty();
        tbls[pos].in_use = true;
        tbls[pos].pid    = pid;
        return Some(&mut tbls[pos]);
    }
    None
}

fn fd_to_slot(fd: usize) -> Option<usize> {
    if fd >= SOCK_FD_BASE && fd < SOCK_FD_BASE + MAX_SOCKS { Some(fd - SOCK_FD_BASE) } else { None }
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

        sched::yield_now("net_daemon");
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
                                         arg(msg,1) as usize, arg(msg,2) as usize),
        NET_CONNECT     => handle_connect(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize, arg(msg,2) as usize),
        NET_SEND        => handle_send(caller_pid, arg(msg,0) as usize,
                                        arg(msg,1) as usize, arg(msg,2) as usize,
                                        arg(msg,4) as usize, arg(msg,5) as usize),
        NET_RECV        => handle_recv(caller_pid, arg(msg,0) as usize,
                                        arg(msg,1) as usize, arg(msg,2) as usize,
                                        arg(msg,4) as usize, arg(msg,5) as usize),
        NET_SENDMSG     => handle_sendmsg(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize),
        NET_RECVMSG     => handle_recvmsg(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize),
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

            let mut bound = BOUND_PATHS.lock();
            for bp in bound.iter() {
                if bp.in_use && bp.path_len == path_len &&
                   bp.path[..path_len] == unsafe {
                       core::slice::from_raw_parts(path_ptr, path_len)
                   }[..] {
                    return err_reply(-98);
                }
            }
            let idx = match bound.iter().position(|b| !b.in_use) {
                Some(i) => i, None => return err_reply(-12),
            };
            let mut path = [0u8; PATH_MAX];
            unsafe { core::ptr::copy_nonoverlapping(path_ptr, path.as_mut_ptr(), path_len); }
            bound[idx] = BoundPath { in_use: true, path, path_len,
                                     _owner_pid: pid, _owner_sock: slot };
            let mut tbls = SOCK_TABLES.lock();
            let tbl = match find_tbl(pid, &mut *tbls) {
                Some(t) => t, None => return err_reply(-9),
            };
            if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
            tbl.socks[slot].state = SockState::UnixListening { bound_idx: idx };
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

fn handle_accept(pid: u32, fd: usize, addr_ptr: usize, addrlen_ptr: usize) -> Message {
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
                    cloexec: false,
                    nonblock: false,
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
            let _ = bound_idx;
            let mut tbls = SOCK_TABLES.lock();
            let mut found = None;
            'outer: for t in tbls.iter() {
                if !t.in_use { continue; }
                for s in t.socks.iter() {
                    if let SockState::UnixPendingAccept { conn_idx } = s.state {
                        found = Some(conn_idx);
                        break 'outer;
                    }
                }
            }
            match found {
                Some(conn_idx) => {
                    let tbl = find_tbl(pid, &mut *tbls).unwrap();
                    let new_slot = match tbl.alloc() { Some(s) => s, None => return err_reply(-24) };
                    tbl.socks[new_slot] = SockEntry {
                        state:      SockState::UnixConnected { conn_idx, is_a: false },
                        in_use:     true,
                        bound_port: 0,
                        domain:     AF_UNIX as u8,
                        sock_type,
                        cloexec:    false,
                        nonblock:   false,
                    };

                    for t in tbls.iter_mut() {
                        if !t.in_use { continue; }
                        for s in t.socks.iter_mut() {
                            if let SockState::UnixPendingAccept { conn_idx: pending_conn } = s.state {
                                if pending_conn == conn_idx {
                                    s.state = SockState::UnixConnected { conn_idx, is_a: true };
                                    break;
                                }
                            }
                        }
                    }

                    if addr_ptr != 0 {
                        unsafe { core::ptr::write_bytes(addr_ptr as *mut u8, 0, 2); }
                    }
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

        let bound_idx = {
            let bound = BOUND_PATHS.lock();
            let mut found = None;
            for (i, bp) in bound.iter().enumerate() {
                if bp.in_use && bp.path_len == path_len &&
                   bp.path[..path_len] == unsafe {
                       core::slice::from_raw_parts(path_ptr, path_len)
                   }[..] {
                    found = Some(i);
                    break;
                }
            }
            match found { Some(i) => i, None => return err_reply(-111) }
        };
        let _ = bound_idx;

        let conn_idx = {
            let mut conns = UNIX_CONNS.lock();
            let idx = match conns.iter().position(|c| !c.in_use) {
                Some(i) => i, None => return err_reply(-12),
            };
            conns[idx] = UnixConn::new();
            conns[idx].in_use = true;
            idx
        };

        let mut tbls = SOCK_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
        tbl.socks[slot].state = SockState::UnixPendingAccept { conn_idx };
        ok_reply()
    }
}

fn handle_socketpair(pid: u32, domain: usize, sock_type: usize,
                     _protocol: usize, sv_ptr: usize) -> Message {
    if domain != AF_UNIX { return err_reply(-97); }

    let conn_idx = {
        let mut conns = UNIX_CONNS.lock();
        let idx = match conns.iter().position(|c| !c.in_use) {
            Some(i) => i, None => return err_reply(-12),
        };
        conns[idx] = UnixConn::new();
        conns[idx].in_use = true;
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

fn handle_sendmsg(pid: u32, fd: usize, msghdr_ptr: usize) -> Message {
    if msghdr_ptr == 0 { return err_reply(-14); }
    let iov_ptr    = unsafe { core::ptr::read((msghdr_ptr + 16) as *const usize) };
    let iovcnt     = unsafe { core::ptr::read((msghdr_ptr + 24) as *const usize) };
    let mut total = 0isize;
    for i in 0..iovcnt.min(16) {
        let iov = iov_ptr + i * 16;
        let base = unsafe { core::ptr::read(iov as *const usize) };
        let len  = unsafe { core::ptr::read((iov + 8) as *const usize) };
        let n = net_val(&handle_send(pid, fd, base, len, 0, 0));
        if n < 0 { return if total > 0 { val_reply(total as u64) } else { make_reply(n as i64) }; }
        total += n;
    }
    val_reply(total as u64)
}

fn handle_recvmsg(pid: u32, fd: usize, msghdr_ptr: usize) -> Message {
    if msghdr_ptr == 0 { return err_reply(-14); }
    let iov_ptr = unsafe { core::ptr::read((msghdr_ptr + 16) as *const usize) };
    let iovcnt  = unsafe { core::ptr::read((msghdr_ptr + 24) as *const usize) };
    let mut total = 0isize;
    for i in 0..iovcnt.min(16) {
        let iov = iov_ptr + i * 16;
        let base = unsafe { core::ptr::read(iov as *const usize) };
        let len  = unsafe { core::ptr::read((iov + 8) as *const usize) };
        let n = net_val(&handle_recv(pid, fd, base, len, 0, 0));
        if n < 0 { return if total > 0 { val_reply(total as u64) } else { make_reply(n as i64) }; }
        total += n;
    }
    val_reply(total as u64)
}

fn handle_shutdown(pid: u32, fd: usize, _how: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    let mut tbls = SOCK_TABLES.lock();
    if let Some(tbl) = find_tbl(pid, &mut *tbls) {
        if slot < MAX_SOCKS && tbl.socks[slot].in_use {
            if let SockState::UnixConnected { conn_idx, is_a } = tbl.socks[slot].state {
                let mut conns = UNIX_CONNS.lock();
                if is_a { conns[conn_idx].closed_a = true; }
                else    { conns[conn_idx].closed_b = true; }
                if conns[conn_idx].closed_a && conns[conn_idx].closed_b {
                    conns[conn_idx].in_use = false;
                }
            }
            tbl.socks[slot] = SockEntry::empty();
        }
    }
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

fn handle_getsockopt(_pid: u32, _fd: usize, level: usize, optname: usize,
                     optval_ptr: usize, optlen_ptr: usize) -> Message {
    if level == 1 && optname == 4 {
        if optval_ptr != 0 {
            unsafe { core::ptr::write(optval_ptr as *mut u32, 0); }
        }
        if optlen_ptr != 0 {
            unsafe { core::ptr::write(optlen_ptr as *mut u32, 4); }
        }
    }
    ok_reply()
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
    let parent_socks = match tbls.iter().find(|t| t.in_use && t.pid == parent) {
        Some(t) => t.socks,
        None    => return ok_reply(), // parent has no sockets — nothing to do
    };
    let child_tbl = match get_or_create(child, &mut *tbls) {
        Some(t) => t, None => return err_reply(-12),
    };
    let mut ends: [Option<(usize, bool)>; MAX_SOCKS] = [None; MAX_SOCKS];
    for (i, e) in parent_socks.iter().enumerate() {
        if !e.in_use { continue; }
        match e.state {
            SockState::UnixConnected { conn_idx, is_a } => {
                child_tbl.socks[i] = *e;
                ends[i] = Some((conn_idx, is_a));
            }
            SockState::Unbound { .. } => { child_tbl.socks[i] = *e; }
            _ => {} // inet/listening: skipped (see doc comment)
        }
    }
    drop(tbls);
    let mut conns = UNIX_CONNS.lock();
    for end in ends.iter().flatten() {
        let (conn_idx, is_a) = *end;
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
            if *refs == 0 {
                if is_a { c.closed_a = true; } else { c.closed_b = true; }
                if c.closed_a && c.closed_b { c.in_use = false; }
            }
            drop(conns);
            let mut tbls2 = SOCK_TABLES.lock();
            if let Some(t2) = tbls2.iter_mut().find(|t| t.in_use && t.pid == pid) {
                t2.socks[slot] = SockEntry::empty();
            }
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

    let revents: u64 = match state {
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
            ev
        }
        SockState::UnixListening { .. } => {
            drop(tbls);
            let tbls2 = SOCK_TABLES.lock();
            let pending = tbls2.iter().any(|t| t.in_use && t.socks.iter()
                .any(|s| matches!(s.state, SockState::UnixPendingAccept { .. })));
            if pending { POLLIN } else { 0 }
        }
        SockState::InetConnected { socket_handle, .. } => {
            drop(tbls);
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
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
            }
        }
        SockState::InetListening { socket_handle } => {
            drop(tbls);
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                if socket.is_active() && socket.state() == tcp::State::Established {
                    POLLIN
                } else {
                    0
                }
            } else {
                0
            }
        }
        SockState::IcmpBound { socket_handle } => {
            drop(tbls);
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                let socket = s.socket_set.get_mut::<icmp::Socket>(socket_handle);
                let mut ev = 0;
                if socket.can_recv() { ev |= POLLIN; }
                ev |= POLLOUT;
                ev
            } else {
                0
            }
        }
        _ => { drop(tbls); 0 }
    };
    val_reply(revents)
}

fn handle_close_all(pid: u32) {
    let mut tbls = SOCK_TABLES.lock();
    if let Some(tbl) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        let mut unix_to_close = [usize::MAX; MAX_SOCKS];
        let mut inet_to_close = [None; MAX_SOCKS];
        
        for (i, s) in tbl.socks.iter().enumerate() {
            match s.state {
                SockState::UnixConnected { conn_idx, .. } => {
                    unix_to_close[i] = conn_idx;
                }
                SockState::InetConnected { socket_handle, .. }
                | SockState::InetListening { socket_handle }
                | SockState::IcmpBound { socket_handle } => {
                    inet_to_close[i] = Some(socket_handle);
                }
                _ => {}
            }
        }
        drop(tbls);
        
        let mut conns = UNIX_CONNS.lock();
        for &ci in &unix_to_close {
            if ci != usize::MAX { conns[ci].in_use = false; }
        }
        drop(conns);

        let mut stack = NET_STACK.lock();
        if let Some(ref mut s) = *stack {
            for &handle_opt in &inet_to_close {
                if let Some(handle) = handle_opt {
                    s.socket_set.remove(handle);
                }
            }
        }
        drop(stack);

        let mut tbls = SOCK_TABLES.lock();
        if let Some(tbl) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
            *tbl = ProcSockTable::empty();
        }
    }
}

fn net_val(m: &Message) -> isize {
    let bytes: [u8; 8] = m.data[0..8].try_into().unwrap_or([0u8; 8]);
    i64::from_le_bytes(bytes) as isize
}
