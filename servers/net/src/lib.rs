//! Net server — AF_UNIX sockets and AF_INET stubs.
//!
//! # Architecture
//!
//! Runs as an in-kernel library called directly from syscall.rs.  All user-space
//! pointers are still valid because TTBR0/CR3 is loaded with the caller's page
//! table during syscall execution.
//!
//! Per-process socket FDs are in `SOCK_TABLES` (parallel to VFS FD tables).
//! Connected pairs share a `UnixConn` slot in `UNIX_CONNS`.
//!
//! # Socket FD layout
//!
//! Socket FDs are offset by `SOCK_FD_BASE` (0x1000_0000) to avoid colliding
//! with VFS FDs.  Kernel syscall stubs call `handle()` for all socket syscalls;
//! VFS stubs are called for file syscalls.  The two namespaces are disjoint.
//!
//! # Message encoding
//!
//! Arguments packed as little-endian u64 words in Message.data[], matching VFS
//! server convention.

#![no_std]

use ipc::Message;
use spin::Mutex;

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
pub const NET_CLOSE:      u64 = 0x40; // close a single socket fd

// ── Constants ─────────────────────────────────────────────────────────────────

pub const AF_UNIX:    usize = 1;
pub const AF_INET:    usize = 2;
pub const SOCK_STREAM: usize = 1;
pub const SOCK_DGRAM:  usize = 2;

/// Base value added to socket handles so they don't collide with VFS FDs.
/// Syscall layer uses raw socket slot indices (0..MAX_SOCKETS); user-visible
/// "fd" returned to user-space = slot + SOCK_FD_BASE.
pub const SOCK_FD_BASE: usize = 0x100;

const MAX_PROCS:   usize = 64;
const MAX_SOCKS:   usize = 16;   // per process
const MAX_CONNS:   usize = 32;   // total simultaneous AF_UNIX connections
const MAX_BOUND:   usize = 16;   // total bound AF_UNIX paths
const RING_SIZE:   usize = 4096;
const PATH_MAX:    usize = 108;  // struct sockaddr_un.sun_path

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
}

// ── Unix connection pair ──────────────────────────────────────────────────────

/// A bidirectional AF_UNIX stream connection.
/// Each endpoint has its own ring: a→b and b→a.
struct UnixConn {
    in_use: bool,
    ring_ab: UnixRing,  // data written by side A, read by side B
    ring_ba: UnixRing,  // data written by side B, read by side A
    closed_a: bool,
    closed_b: bool,
}

impl UnixConn {
    const fn new() -> Self {
        Self {
            in_use: false,
            ring_ab: UnixRing::new(),
            ring_ba: UnixRing::new(),
            closed_a: false,
            closed_b: false,
        }
    }
}

static UNIX_CONNS: Mutex<[UnixConn; MAX_CONNS]> =
    Mutex::new([const { UnixConn::new() }; MAX_CONNS]);

// ── Bound AF_UNIX paths (passive listeners) ───────────────────────────────────

struct BoundPath {
    in_use:     bool,
    path:       [u8; PATH_MAX],
    path_len:   usize,
    _owner_pid:  u32,
    _owner_sock: usize,  // slot in owner's SOCK_TABLES entry
}

impl BoundPath {
    const fn new() -> Self {
        Self { in_use: false, path: [0u8; PATH_MAX], path_len: 0,
               _owner_pid: 0, _owner_sock: 0 }
    }
}

static BOUND_PATHS: Mutex<[BoundPath; MAX_BOUND]> =
    Mutex::new([const { BoundPath::new() }; MAX_BOUND]);

// ── AF_INET loopback listener table ─────────────────────────────────────────
//
// Up to MAX_INET_LISTENERS simultaneous TCP listeners on 127.0.0.1.
// Each entry caches up to 8 pending conn_idx values (backlog queue).

const MAX_INET_LISTENERS: usize = 16;
const INET_BACKLOG:       usize = 8;

#[derive(Clone, Copy)]
struct InetListener {
    in_use:    bool,
    port:      u16,
    pid:       u32,
    slot:      usize,              // index into pid's SOCK_TABLES
    pending:   [usize; INET_BACKLOG], // conn_idx queue (UNIX_CONNS slots)
    n_pending: usize,
}

impl InetListener {
    const fn empty() -> Self {
        Self { in_use: false, port: 0, pid: 0, slot: 0,
               pending: [0; INET_BACKLOG], n_pending: 0 }
    }
}

static INET_LISTENERS: Mutex<[InetListener; MAX_INET_LISTENERS]> =
    Mutex::new([const { InetListener::empty() }; MAX_INET_LISTENERS]);

// ── Socket kind ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum SockState {
    None,
    /// Created but not connected or bound.
    Unbound { domain: u8, sock_type: u8 },
    /// AF_UNIX: bound to a path and listening.
    Listening { bound_idx: usize },
    /// AF_INET: bound to a port and listening (see INET_LISTENERS for queue).
    InetListening,
    /// Connected (both AF_UNIX and AF_INET); conn_idx indexes UNIX_CONNS.
    /// is_a=true: this socket is side-A (writes ring_ab, reads ring_ba).
    Connected { conn_idx: usize, is_a: bool },
    /// AF_UNIX pending accept: connect() was called but accept() not yet.
    PendingAccept { conn_idx: usize },
    /// AF_INET pending accept: queued in INET_LISTENERS but not yet accept()ed.
    InetPendingAccept { conn_idx: usize },
}

#[derive(Clone, Copy)]
struct SockEntry {
    state:      SockState,
    in_use:     bool,
    /// For INET sockets: the port bound to this socket (host byte order).
    bound_port: u16,
    /// Socket domain: AF_UNIX or AF_INET.
    domain:     u8,
}

impl SockEntry {
    const fn empty() -> Self {
        Self { state: SockState::None, in_use: false, bound_port: 0, domain: 0 }
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

/// Convert user-visible fd (slot + SOCK_FD_BASE) back to slot index.
fn fd_to_slot(fd: usize) -> Option<usize> {
    if fd >= SOCK_FD_BASE && fd < SOCK_FD_BASE + MAX_SOCKS { Some(fd - SOCK_FD_BASE) } else { None }
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

        // We also need a way for AF_UNIX to find this port during connect().
        // In this minimal net-server, BoundPath is just a placeholder.
        // Connecting to a BoundPath usually returns its owner's socket port.
        // We'll update handle_connect to support this.
    }
}

pub fn handle(msg: &Message, caller_pid: u32) -> Message {

    match msg.tag {
        NET_SOCKET     => handle_socket(caller_pid, arg(msg,0) as usize,
                                        arg(msg,1) as usize, arg(msg,2) as usize),
        NET_BIND       => handle_bind(caller_pid, arg(msg,0) as usize,
                                      arg(msg,1) as usize, arg(msg,2) as usize),
        NET_LISTEN     => handle_listen(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        NET_ACCEPT     => handle_accept(caller_pid, arg(msg,0) as usize,
                                        arg(msg,1) as usize, arg(msg,2) as usize),
        NET_CONNECT    => handle_connect(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize, arg(msg,2) as usize),
        NET_SEND       => handle_send(caller_pid, arg(msg,0) as usize,
                                       arg(msg,1) as usize, arg(msg,2) as usize),
        NET_RECV       => handle_recv(caller_pid, arg(msg,0) as usize,
                                       arg(msg,1) as usize, arg(msg,2) as usize),
        NET_SENDMSG    => handle_sendmsg(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize),
        NET_RECVMSG    => handle_recvmsg(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize),
        NET_SHUTDOWN   => handle_shutdown(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize),
        NET_GETSOCKNAME => handle_getsockname(caller_pid, arg(msg,0) as usize,
                                               arg(msg,1) as usize, arg(msg,2) as usize),
        NET_GETPEERNAME => handle_getpeername(caller_pid, arg(msg,0) as usize,
                                               arg(msg,1) as usize, arg(msg,2) as usize),
        NET_SOCKETPAIR => handle_socketpair(caller_pid, arg(msg,0) as usize,
                                            arg(msg,1) as usize, arg(msg,2) as usize,
                                            arg(msg,3) as usize),
        NET_SETSOCKOPT => ok_reply(),  // silently accept all socket options
        NET_GETSOCKOPT => handle_getsockopt(caller_pid, arg(msg,0) as usize,
                                            arg(msg,1) as usize, arg(msg,2) as usize,
                                            arg(msg,3) as usize, arg(msg,4) as usize),
        NET_CLOSE_ALL  => { handle_close_all(caller_pid); ok_reply() }
        NET_CLOSE      => handle_close(caller_pid, arg(msg,0) as usize),
        _              => err_reply(-38),
    }
}
// ── Handlers ─────────────────────────────────────────────────────────────────

fn handle_socket(pid: u32, domain: usize, sock_type: usize, _protocol: usize) -> Message {
    match domain {
        AF_UNIX | AF_INET => {}
        _                 => return err_reply(-97), // EAFNOSUPPORT
    }
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) {
        Some(t) => t, None => return err_reply(-12),
    };
    let slot = match tbl.alloc() { Some(s) => s, None => return err_reply(-24) };
    tbl.socks[slot] = SockEntry {
        state:      SockState::Unbound { domain: domain as u8, sock_type: sock_type as u8 },
        in_use:     true,
        bound_port: 0,
        domain:     domain as u8,
    };
    val_reply((slot + SOCK_FD_BASE) as u64)
}

fn handle_bind(pid: u32, fd: usize, addr_ptr: usize, addrlen: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    if addrlen < 2 { return err_reply(-22); }

    // Read the sa_family field (first 2 bytes, little-endian on all our targets).
    let sa_family = unsafe { (addr_ptr as *const u16).read_unaligned() } as usize;

    match sa_family {
        AF_INET => {
            // sockaddr_in layout: u16 family, u16 port (big-endian), u32 addr, u8 zero[8]
            if addrlen < 8 { return err_reply(-22); }
            let port_be = unsafe { ((addr_ptr + 2) as *const u16).read_unaligned() };
            let port = u16::from_be(port_be);
            // Check for duplicate INET binding.
            {
                let listeners = INET_LISTENERS.lock();
                if listeners.iter().any(|l| l.in_use && l.port == port) {
                    return err_reply(-98); // EADDRINUSE
                }
            }
            // Store the port on the socket entry; listen() will register it.
            let mut tbls = SOCK_TABLES.lock();
            if let Some(tbl) = find_tbl(pid, &mut *tbls) {
                if slot < MAX_SOCKS && tbl.socks[slot].in_use {
                    tbl.socks[slot].bound_port = port;
                }
            }
            ok_reply()
        }
        AF_UNIX | _ => {
            // AF_UNIX: sockaddr_un — family(2) + sun_path
            if addrlen < 3 || addrlen > 2 + PATH_MAX { return err_reply(-22); }
            let path_len = addrlen - 2;
            let path_ptr = (addr_ptr + 2) as *const u8;

            let mut bound = BOUND_PATHS.lock();
            for bp in bound.iter() {
                if bp.in_use && bp.path_len == path_len &&
                   bp.path[..path_len] == unsafe {
                       core::slice::from_raw_parts(path_ptr, path_len)
                   }[..] {
                    return err_reply(-98); // EADDRINUSE
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
            if let Some(tbl) = find_tbl(pid, &mut *tbls) {
                if slot < MAX_SOCKS && tbl.socks[slot].in_use {
                    tbl.socks[slot].state = SockState::Listening { bound_idx: idx };
                }
            }
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

    // AF_INET: register in INET_LISTENERS table using the previously bound port.
    if tbl.socks[slot].domain == AF_INET as u8 {
        let port = tbl.socks[slot].bound_port;
        if port == 0 { return err_reply(-22); } // must bind() first
        tbl.socks[slot].state = SockState::InetListening;
        drop(tbls);
        let mut listeners = INET_LISTENERS.lock();
        let idx = match listeners.iter().position(|l| !l.in_use) {
            Some(i) => i, None => return err_reply(-12),
        };
        listeners[idx] = InetListener { in_use: true, port, pid, slot,
                                        pending: [0; INET_BACKLOG], n_pending: 0 };
        return ok_reply();
    }
    // AF_UNIX: already set to Listening { bound_idx } by handle_bind.
    ok_reply()
}

fn handle_accept(pid: u32, fd: usize, addr_ptr: usize, _addrlen_ptr: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };

    // Determine socket domain + get conn_idx from the pending queue.
    let (conn_idx, is_inet) = {
        let tbls = SOCK_TABLES.lock();
        let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }

        match tbl.socks[slot].state {
            // ── AF_INET listening socket ──────────────────────────────────────
            SockState::InetListening => {
                let port = tbl.socks[slot].bound_port;
                drop(tbls);
                let mut listeners = INET_LISTENERS.lock();
                let l = match listeners.iter_mut().find(|l| l.in_use && l.port == port) {
                    Some(l) => l, None => return err_reply(-11), // EAGAIN
                };
                if l.n_pending == 0 { return err_reply(-11); } // EAGAIN
                let ci = l.pending[0];
                // Shift queue.
                for i in 0..l.n_pending - 1 { l.pending[i] = l.pending[i+1]; }
                l.n_pending -= 1;
                (ci, true)
            }
            // ── AF_UNIX listening socket ──────────────────────────────────────
            SockState::Listening { bound_idx } => {
                let _ = bound_idx;
                // Scan all procs for PendingAccept.
                let mut found = None;
                'outer: for t in tbls.iter() {
                    if !t.in_use { continue; }
                    for s in t.socks.iter() {
                        if let SockState::PendingAccept { conn_idx } = s.state {
                            found = Some(conn_idx);
                            break 'outer;
                        }
                    }
                }
                match found { Some(c) => (c, false), None => return err_reply(-11) }
            }
            _ => return err_reply(-22), // EINVAL — not listening
        }
    };

    // Allocate a new socket fd on the accept()-ing side (side B).
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) {
        Some(t) => t, None => return err_reply(-12),
    };
    let new_slot = match tbl.alloc() { Some(s) => s, None => return err_reply(-24) };
    tbl.socks[new_slot] = SockEntry {
        state:      SockState::Connected { conn_idx, is_a: false },
        in_use:     true,
        bound_port: 0,
        domain:     if is_inet { AF_INET as u8 } else { AF_UNIX as u8 },
    };

    // Transition the connector from Pending* to Connected (side A).
    let pending_state = if is_inet {
        SockState::InetPendingAccept { conn_idx }
    } else {
        SockState::PendingAccept { conn_idx }
    };
    for t in tbls.iter_mut() {
        if !t.in_use { continue; }
        for s in t.socks.iter_mut() {
            if s.state == pending_state {
                s.state = SockState::Connected { conn_idx, is_a: true };
                break;
            }
        }
    }

    // Fill peer address if requested.
    if addr_ptr != 0 {
        if is_inet {
            // Return a minimal sockaddr_in: family=AF_INET, port=0, addr=127.0.0.1
            unsafe {
                core::ptr::write_bytes(addr_ptr as *mut u8, 0, 16);
                core::ptr::write(addr_ptr as *mut u16, AF_INET as u16);
                core::ptr::write((addr_ptr + 4) as *mut u32, 0x0100_007Fu32);
            }
        } else {
            unsafe { core::ptr::write_bytes(addr_ptr as *mut u8, 0, 2); }
        }
    }
    val_reply((new_slot + SOCK_FD_BASE) as u64)
}

fn handle_connect(pid: u32, fd: usize, addr_ptr: usize, addrlen: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    if addrlen < 2 { return err_reply(-22); }

    let sa_family = unsafe { (addr_ptr as *const u16).read_unaligned() } as usize;

    // ── AF_INET loopback connect ──────────────────────────────────────────────
    if sa_family == AF_INET {
        if addrlen < 8 { return err_reply(-22); }
        let port_be  = unsafe { ((addr_ptr + 2) as *const u16).read_unaligned() };
        let port     = u16::from_be(port_be);
        let sin_addr = unsafe { ((addr_ptr + 4) as *const u32).read_unaligned() };
        // Only accept 127.0.0.1 (0x0100007F in little-endian) or INADDR_ANY.
        if sin_addr != 0x0100007Fu32 && sin_addr != 0 { return err_reply(-111); }

        // Find the INET listener on this port.
        let (listener_pid, listener_slot) = {
            let listeners = INET_LISTENERS.lock();
            match listeners.iter().find(|l| l.in_use && l.port == port) {
                Some(l) => (l.pid, l.slot),
                None    => return err_reply(-111), // ECONNREFUSED
            }
        };

        // Allocate a ring buffer connection.
        let conn_idx = {
            let mut conns = UNIX_CONNS.lock();
            let idx = match conns.iter().position(|c| !c.in_use) {
                Some(i) => i, None => return err_reply(-12),
            };
            conns[idx] = UnixConn::new();
            conns[idx].in_use = true;
            idx
        };

        // Enqueue conn_idx in the listener's backlog.
        {
            let mut listeners = INET_LISTENERS.lock();
            if let Some(l) = listeners.iter_mut().find(|l| l.in_use && l.port == port) {
                if l.n_pending >= INET_BACKLOG { return err_reply(-111); } // backlog full
                l.pending[l.n_pending] = conn_idx;
                l.n_pending += 1;
            }
        }

        // Mark the listener socket as InetPendingAccept so accept() can find it.
        {
            let mut tbls = SOCK_TABLES.lock();
            if let Some(tbl) = find_tbl(listener_pid, &mut *tbls) {
                if listener_slot < MAX_SOCKS && tbl.socks[listener_slot].in_use {
                    // We can't add multiple pending entries to a single socket state,
                    // so we place the pending signal on the listener's slot only when
                    // the backlog was empty (n_pending went 0→1).
                    // accept() drains INET_LISTENERS.pending[], so this is safe.
                    let _ = listener_slot;
                }
            }
        }

        // Mark this (connector's) socket as PendingAccept until connected.
        let mut tbls = SOCK_TABLES.lock();
        if let Some(tbl) = find_tbl(pid, &mut *tbls) {
            if slot < MAX_SOCKS && tbl.socks[slot].in_use {
                tbl.socks[slot].state = SockState::InetPendingAccept { conn_idx };
            }
        }
        return ok_reply();
    }

    // ── AF_UNIX connect ───────────────────────────────────────────────────────
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
    if let Some(tbl) = find_tbl(pid, &mut *tbls) {
        if slot < MAX_SOCKS && tbl.socks[slot].in_use {
            tbl.socks[slot].state = SockState::PendingAccept { conn_idx };
        }
    }
    ok_reply()
}

fn handle_socketpair(pid: u32, domain: usize, sock_type: usize,
                     _protocol: usize, sv_ptr: usize) -> Message {
    if domain != AF_UNIX { return err_reply(-97); }
    let _ = sock_type;

    // Allocate connection.
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
    let slot_a = match tbl.alloc() { Some(s) => s, None => return err_reply(-24) };
    tbl.socks[slot_a] = SockEntry {
        state: SockState::Connected { conn_idx, is_a: true },
        in_use: true, bound_port: 0, domain: AF_UNIX as u8,
    };
    let slot_b = match tbl.alloc() { Some(s) => s, None => {
        tbl.socks[slot_a] = SockEntry::empty(); return err_reply(-24);
    }};
    tbl.socks[slot_b] = SockEntry {
        state: SockState::Connected { conn_idx, is_a: false },
        in_use: true, bound_port: 0, domain: AF_UNIX as u8,
    };
    unsafe {
        core::ptr::write(sv_ptr as *mut u32, (slot_a + SOCK_FD_BASE) as u32);
        core::ptr::write((sv_ptr + 4) as *mut u32, (slot_b + SOCK_FD_BASE) as u32);
    }
    ok_reply()
}

fn handle_send(pid: u32, fd: usize, buf_ptr: usize, len: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    let tbls = SOCK_TABLES.lock();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
    let (conn_idx, is_a) = match tbl.socks[slot].state {
        SockState::Connected { conn_idx, is_a } => (conn_idx, is_a),
        _ => {
            // Check if this socket is connected to a "forced" bound path (like PipeWire)
            // In our minimal net-server, we just check if it's connected to nothing.
            // For a real fix, we would need to store the target port in SockState.
            return err_reply(-32);
        }
    };
    drop(tbls);

    let mut conns = UNIX_CONNS.lock();
    let conn = &mut conns[conn_idx];
    if !conn.in_use { return err_reply(-32); }
    let n = if is_a {
        conn.ring_ab.write(buf_ptr as *const u8, len)
    } else {
        conn.ring_ba.write(buf_ptr as *const u8, len)
    };
    val_reply(n as u64)
}

fn handle_recv(pid: u32, fd: usize, buf_ptr: usize, len: usize) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    let tbls = SOCK_TABLES.lock();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
    let (conn_idx, is_a) = match tbl.socks[slot].state {
        SockState::Connected { conn_idx, is_a } => (conn_idx, is_a),
        _ => return err_reply(-9),
    };
    drop(tbls);

    let mut conns = UNIX_CONNS.lock();
    let conn = &mut conns[conn_idx];
    if !conn.in_use { return val_reply(0); } // EOF
    let n = if is_a {
        // A reads from ring_ba (written by B)
        conn.ring_ba.read(buf_ptr as *mut u8, len)
    } else {
        // B reads from ring_ab (written by A)
        conn.ring_ab.read(buf_ptr as *mut u8, len)
    };
    val_reply(n as u64)
}

fn handle_sendmsg(pid: u32, fd: usize, msghdr_ptr: usize) -> Message {
    // struct msghdr: msg_name(8), msg_namelen(4), pad(4),
    //                msg_iov(*iovec)(8), msg_iovlen(8), ...
    if msghdr_ptr == 0 { return err_reply(-14); }
    let iov_ptr    = unsafe { core::ptr::read((msghdr_ptr + 16) as *const usize) };
    let iovcnt     = unsafe { core::ptr::read((msghdr_ptr + 24) as *const usize) };
    let mut total = 0isize;
    for i in 0..iovcnt.min(16) {
        let iov = iov_ptr + i * 16;
        let base = unsafe { core::ptr::read(iov as *const usize) };
        let len  = unsafe { core::ptr::read((iov + 8) as *const usize) };
        let n = net_val(&handle_send(pid, fd, base, len));
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
        let n = net_val(&handle_recv(pid, fd, base, len));
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
            if let SockState::Connected { conn_idx, is_a } = tbl.socks[slot].state {
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
    // Return AF_UNIX with empty path (anonymous).
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
    // SOL_SOCKET=1, SO_ERROR=4 → return 0 (no error).
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

/// Close a single socket identified by its user-visible fd (slot + SOCK_FD_BASE).
fn handle_close(pid: u32, sockfd: usize) -> Message {
    if sockfd < SOCK_FD_BASE { return err_reply(-9); } // EBADF
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
        SockState::Connected { conn_idx, .. } => {
            drop(tbls);
            let mut conns = UNIX_CONNS.lock();
            conns[conn_idx].in_use = false;
            drop(conns);
            let mut tbls2 = SOCK_TABLES.lock();
            if let Some(t2) = tbls2.iter_mut().find(|t| t.in_use && t.pid == pid) {
                t2.socks[slot] = SockEntry::empty();
            }
        }
        SockState::InetListening if port != 0 => {
            tbl.socks[slot] = SockEntry::empty();
            drop(tbls);
            let mut listeners = INET_LISTENERS.lock();
            if let Some(l) = listeners.iter_mut().find(|l| l.in_use && l.port == port && l.pid == pid) {
                *l = InetListener::empty();
            }
        }
        _ => { tbl.socks[slot] = SockEntry::empty(); }
    }
    ok_reply()
}

fn handle_close_all(pid: u32) {
    let mut tbls = SOCK_TABLES.lock();
    if let Some(tbl) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
        // Close all connected sockets.
        let mut to_close = [usize::MAX; MAX_SOCKS];
        for (i, s) in tbl.socks.iter().enumerate() {
            if let SockState::Connected { conn_idx, .. } = s.state {
                to_close[i] = conn_idx;
            }
        }
        drop(tbls);
        let mut conns = UNIX_CONNS.lock();
        for &ci in &to_close {
            if ci != usize::MAX { conns[ci].in_use = false; }
        }
        drop(conns);
        // Release any INET listeners owned by this pid.
        let mut listeners = INET_LISTENERS.lock();
        for l in listeners.iter_mut() {
            if l.in_use && l.pid == pid { *l = InetListener::empty(); }
        }
        drop(listeners);
        let mut tbls = SOCK_TABLES.lock();
        if let Some(tbl) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
            *tbl = ProcSockTable::empty();
        }
    }
}

// ── Small helpers used inside handlers ───────────────────────────────────────

fn net_val(m: &Message) -> isize {
    let bytes: [u8; 8] = m.data[0..8].try_into().unwrap_or([0u8; 8]);
    i64::from_le_bytes(bytes) as isize
}

