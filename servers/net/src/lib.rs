//! Net server — smoltcp and AF_UNIX sockets.

#![no_std]

extern crate alloc;

pub mod nftables;

use core::sync::atomic::{AtomicU32, Ordering};
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

// ── AF_UNIX pending-accept lifecycle trace (off) ──────────────────────────────
//
// Traces the five points a privileged socket passes through between
// cosmic-panel's `connect()` and cosmic-comp's `accept()`: connect,
// fork-inheritance, an EBADF from the child's pre_exec `fcntl(F_GETFD)`,
// accept, and close. Written to prove the `UnixPendingAccept` refcount fix
// below; kept because that path has no other visibility and re-deriving the
// prints costs more than the dead code does.
//
// Flip to `true` to re-enable — every print early-returns on this `const`, so
// off it compiles out entirely. Idiom copied from `servers/drm/src/lib.rs`.
// What it measured is recorded in `artifacts/notes/item9b-applet-spawn.md`.
const NET_DEBUG: bool = false;

/// The `accept → EAGAIN` arm is the hot one: every nonblocking accept loop
/// turn on every AF_UNIX listener in the session reaches it. Bound the
/// evidence to a handful of lines instead of one per poll, as
/// `servers/drm/src/lib.rs` bounds its mmap trace.
static ACCEPT_EAGAIN_SEEN: AtomicU32 = AtomicU32::new(0);
const ACCEPT_EAGAIN_LIMIT: u32 = 64;

fn dbg(msg: &str) {
    if !NET_DEBUG { return; }
    extern "C" { fn arch_serial_putc(c: u8); }
    for &b in msg.as_bytes() { unsafe { arch_serial_putc(b); } }
}

/// Unsigned decimal, for pids / fds / table indices. Kept decimal rather than
/// hex so a trace line can be diffed against the `Starting: <applet>` ordering
/// in a cosmic-session capture without conversion.
fn dbg_u(mut v: u64) {
    if !NET_DEBUG { return; }
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 { break; }
    }
    dbg(unsafe { core::str::from_utf8_unchecked(&buf[i..]) });
}

// ── shutdown(2) `how` ─────────────────────────────────────────────────────────
/// Retire the read direction: later recvs report EOF, and the peer's sends
/// raise EPIPE.
const SHUT_RD:   usize = 0;
/// Retire the write direction: later sends raise EPIPE, and the peer's recvs
/// report EOF once the ring drains. This is the one tokio issues from
/// `OwnedWriteHalf::drop`.
const SHUT_WR:   usize = 1;
/// Both of the above. Still not a close: the fd stays valid.
const SHUT_RDWR: usize = 2;

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
pub const SOCK_SEQPACKET: usize = 5;
/// EOPNOTSUPP. `unix_listen` answers this — not EINVAL — for a socket type that
/// cannot accept, and it checks the type *before* the address, so even a bound
/// AF_UNIX SOCK_DGRAM socket gets EOPNOTSUPP.
const EOPNOTSUPP: i32 = 95;

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

// ── TIME_WAIT ────────────────────────────────────────────────────────────────
//
// `handle_close` used to hand a TCP port straight back, so a server could be
// restarted onto the same port instantly where Linux answers EADDRINUSE. What
// is modelled here is the *port reservation* only, not the protocol state: the
// smoltcp socket is still torn down at close (see the note in `handle_close`),
// so nothing lingers to absorb a late segment or to re-ACK a retransmitted FIN.
//
/// Linux's `TCP_TIMEWAIT_LEN` is a fixed 60 s (2*MSL with MSL = 30 s) and is not
/// tunable there either. `sched::ticks()` runs at 100 Hz (sched/src/lib.rs).
const TIME_WAIT_TICKS: u64 = 60 * 100;
/// Reservation slots. A full table is handled by *not* recording the newest
/// reservation, so the failure mode under pressure is the old behaviour (a port
/// that is instantly reusable) rather than a bind that cannot be satisfied.
const MAX_TIME_WAIT: usize = 64;

#[derive(Clone, Copy)]
struct TimeWait { in_use: bool, port: u16, expires: u64 }

/// Leaf lock: never taken while SOCK_TABLES, UNIX_CONNS, BOUND_PATHS or either
/// stack lock is held. Every caller snapshots it first and then locks the rest.
static TIME_WAIT: Mutex<[TimeWait; MAX_TIME_WAIT]> =
    Mutex::new([TimeWait { in_use: false, port: 0, expires: 0 }; MAX_TIME_WAIT]);

/// Park `port` for 2*MSL. Re-parking a port that is already parked refreshes it
/// rather than consuming a second slot.
fn time_wait_add(port: u16) {
    if port == 0 { return; }
    let now = sched::ticks();
    let deadline = now.saturating_add(TIME_WAIT_TICKS);
    let mut tw = TIME_WAIT.lock();
    for e in tw.iter_mut() {
        if e.in_use && e.expires > now && e.port == port { e.expires = deadline; return; }
    }
    for e in tw.iter_mut() {
        if !e.in_use || e.expires <= now {
            *e = TimeWait { in_use: true, port, expires: deadline };
            return;
        }
    }
    // Table full of live reservations — fail open, as documented above.
}

/// Copy the still-live reservations into `out`, returning how many. Taken
/// before any other server lock so TIME_WAIT stays a leaf; the answer is a
/// snapshot, which is all a bind decision needs.
fn time_wait_snapshot(out: &mut [u16; MAX_TIME_WAIT]) -> usize {
    let now = sched::ticks();
    let mut tw = TIME_WAIT.lock();
    let mut n = 0;
    for e in tw.iter_mut() {
        if !e.in_use { continue; }
        if e.expires <= now { e.in_use = false; continue; } // lazily reaped
        out[n] = e.port;
        n += 1;
    }
    n
}

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

/// One descriptor in flight over SCM_RIGHTS.
///
/// A descriptor number tells you which table owns it: the VFS's per-process
/// table below `SOCK_FD_BASE`, this server's `SOCK_TABLES` above it. Only the
/// first kind used to be transferable, because `vfs::export_fd` rejects
/// anything `>= MAX_FDS` — so passing a SOCKET over SCM_RIGHTS returned EBADF.
///
/// That is not a corner case: `wp_security_context_v1.create_listener` passes a
/// *listening* AF_UNIX socket, and it is how COSMIC hands the compositor a
/// private per-applet listener. With the socket arm missing, every
/// `X-HostWaylandDisplay=true` applet took cosmic-panel's Wayland connection
/// down with it (see `handle_dup`, which is the other half of the same bug).
///
/// `Copy` like the `vfs::TransferFd` it wraps, and for the same reason: the
/// reference it carries is released by an explicit `xfer_drop`, never by going
/// out of scope, so the queued batch can be indexed rather than moved out of.
#[derive(Clone, Copy)]
enum XferFd {
    Vfs(vfs::TransferFd),
    /// A lifted `SockEntry`. The in-flight reference it holds on the underlying
    /// object is taken by `xfer_export` and released by exactly one of
    /// `xfer_import` (handing it to the receiver) or `xfer_drop`.
    Sock(SockEntry),
}

/// One SCM_RIGHTS fd batch queued on a stream direction. `seq_byte` is the
/// absolute stream offset (UnixRing::wtotal) of the first data byte these fds
/// accompany; the recv that consumes that byte delivers them (Linux: fds ride
/// with the first byte of their segment). Ordered ascending by `seq_byte`
/// within a direction, since sends append in order.
struct PendingFdBatch {
    seq_byte: u64,
    fds:      alloc::vec::Vec<XferFd>,
}

struct UnixConn {
    in_use: bool,
    ring_ab: UnixRing,
    ring_ba: UnixRing,
    closed_a: bool,
    closed_b: bool,
    /// Per-end, per-direction half-close (shutdown(2)), deliberately kept
    /// distinct from `closed_a`/`closed_b`. Those mean "this end is gone,
    /// every alias included" and are only ever set once `refs_*` hits 0. A
    /// shutdown retires one *direction* of a still-open end: the fd stays a
    /// valid socket, the other aliases stay usable, and the peer sees EOF on
    /// reads rather than an error. Conflating the two made
    /// `shutdown(fd, SHUT_WR)` strictly more destructive than `close(fd)` —
    /// it ignored the dup refcount and destroyed the caller's fd — which is
    /// what wedged cosmic-session's readiness handshake, since tokio's
    /// `OwnedWriteHalf::drop` issues exactly that call on the half it is not
    /// keeping.
    shut_rd_a: bool,
    shut_wr_a: bool,
    shut_rd_b: bool,
    shut_wr_b: bool,
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
            shut_rd_a: false,
            shut_wr_a: false,
            shut_rd_b: false,
            shut_wr_b: false,
            refs_a: 1,
            refs_b: 1,
            fdq_ab: alloc::vec::Vec::new(),
            fdq_ba: alloc::vec::Vec::new(),
            cred_a: Ucred::zero(),
            cred_b: Ucred::zero(),
            seq: 0,
        }
    }

    /// True when the `is_a` end's write direction has been retired by a
    /// shutdown. Pass `!is_a` to ask the same about the peer.
    fn wr_shut(&self, is_a: bool) -> bool {
        if is_a { self.shut_wr_a } else { self.shut_wr_b }
    }

    /// True when the `is_a` end's read direction has been retired by a
    /// shutdown. Pass `!is_a` to ask the same about the peer.
    fn rd_shut(&self, is_a: bool) -> bool {
        if is_a { self.shut_rd_a } else { self.shut_rd_b }
    }

    /// Apply one shutdown(2) `how` to one end. One-way and idempotent: a
    /// direction never comes back, and neither `closed_*` nor `refs_*` is
    /// touched — shutdown is not close.
    fn shutdown_end(&mut self, is_a: bool, how: usize) {
        let rd = how == SHUT_RD || how == SHUT_RDWR;
        let wr = how == SHUT_WR || how == SHUT_RDWR;
        if is_a {
            self.shut_rd_a |= rd;
            self.shut_wr_a |= wr;
        } else {
            self.shut_rd_b |= rd;
            self.shut_wr_b |= wr;
        }
    }

    /// Lift every queued-but-undelivered fd off both directions (socket torn
    /// down). Linux closes in-flight SCM_RIGHTS fds when the socket dies, and
    /// the caller does exactly that by passing the result to `xfer_drop`.
    ///
    /// The releasing is deliberately NOT done here: an in-flight fd may itself
    /// be a socket referencing a `UnixConn` (see `XferFd::Sock`), so dropping
    /// one takes UNIX_CONNS — which every caller of this already holds. Handing
    /// the orphans back keeps that release outside the lock.
    #[must_use = "the lifted descriptors must be passed to xfer_drop"]
    fn take_fds(&mut self) -> alloc::vec::Vec<XferFd> {
        let mut out = alloc::vec::Vec::new();
        for b in self.fdq_ab.drain(..) { out.extend(b.fds); }
        for b in self.fdq_ba.drain(..) { out.extend(b.fds); }
        out
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
    /// How many things still hold this bound address: one per `UnixListening`
    /// fd naming it, plus one per in-flight SCM_RIGHTS copy. The address stops
    /// resolving only when the LAST of them goes away.
    ///
    /// It was implicitly 1 — the slot was freed by whichever fd closed first —
    /// which is why `dup()` of a listener had to be refused outright, and that
    /// refusal is what killed cosmic-panel: libwayland dups every fd it
    /// marshals, so `wp_security_context_v1.create_listener` (the request
    /// behind every `X-HostWaylandDisplay=true` applet) failed to marshal and
    /// poisoned the panel's whole Wayland connection. See `handle_dup`.
    refs:       u32,
    _owner_pid:  u32,
    _owner_sock: usize,
}

impl BoundPath {
    const fn new() -> Self {
        Self { in_use: false, path: [0u8; PATH_MAX], path_len: 0,
               sock_id: 0, is_abstract: false, refs: 0,
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
    /// A listening TCP socket, one smoltcp handle per stack it listens on:
    /// `main` on the NIC's interface, `lo` on the loopback one. A bind to
    /// INADDR_ANY listens on both (as Linux does), a bind to an explicit
    /// address on exactly one, so at least one of the two is always `Some`.
    /// `local` is the bound endpoint, kept for getsockname and for re-arming a
    /// listener after accept.
    InetListening { main: Option<SocketHandle>, lo: Option<SocketHandle>, local: IpEndpoint },
    /// `lo` says which stack owns `socket_handle` — see `stack_for`.
    InetConnected { socket_handle: SocketHandle, remote_endpoint: Option<IpEndpoint>, lo: bool },
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
    /// SO_REUSEADDR. The one thing it is consulted for here is whether bind()
    /// may take a port still parked in TIME_WAIT — which is the reason a real
    /// server sets it at all. It is recorded per socket and must be set before
    /// bind(), as on Linux.
    reuseaddr:  bool,
}

impl SockEntry {
    const fn empty() -> Self {
        Self { state: SockState::None, in_use: false, bound_port: 0, domain: 0,
               sock_type: 0, cloexec: false, nonblock: false, reuseaddr: false }
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

/// The stack that owns a socket's smoltcp handle. Both statics have the same
/// type, so a site that used to say `NET_STACK.lock()` now says `stack_for(lo)`.
/// The two are never locked at the same time — every caller takes one, finishes
/// with it, and only then takes the other.
#[inline]
fn stack_for(lo: bool) -> spin::MutexGuard<'static, Option<NetStack>> {
    if lo { LO_STACK.lock() } else { NET_STACK.lock() }
}

/// True for an address the loopback stack owns (127.0.0.0/8).
#[inline]
fn is_loopback_addr(addr: IpAddress) -> bool {
    matches!(addr, IpAddress::Ipv4(v4) if v4.is_loopback())
}

/// Pick a free local port from Linux's ephemeral range for a `bind()` to port 0
/// or a `connect()` from an unbound socket. "Free" means no socket in any
/// process table claims it. Caller holds SOCK_TABLES; this only reads it, so it
/// must run before the caller borrows its own table mutably.
/// `reserved` is a `time_wait_snapshot` taken by the caller before it locked
/// SOCK_TABLES: an automatic port must skip a port still in TIME_WAIT, and the
/// snapshot is what keeps TIME_WAIT from being locked underneath SOCK_TABLES.
fn alloc_ephemeral_port(tbls: &[ProcSockTable], reserved: &[u16]) -> Option<u16> {
    const EPHEMERAL_LO: u32 = 32768;
    const EPHEMERAL_HI: u32 = 60999;
    let span  = EPHEMERAL_HI - EPHEMERAL_LO + 1;
    let start = (sched::ticks() as u32) % span;
    for i in 0..span {
        let p = (EPHEMERAL_LO + (start + i) % span) as u16;
        if reserved.contains(&p) { continue; }
        let taken = tbls.iter().any(|t| t.in_use
            && t.socks.iter().any(|s| s.in_use && s.bound_port == p));
        if !taken { return Some(p); }
    }
    None
}

/// Write a `sockaddr_in` (Linux ABI: sa_family(2) + sin_port BE(2) + sin_addr
/// BE(4) + 8 pad) and its length. `addrlen_ptr` is in/out, as on Linux: its
/// incoming value caps how much of the caller's buffer may be written, and it
/// always reports the full address length back. The caller must hold NO server
/// lock — this touches user memory, which can demand-page and re-enter the
/// scheduler.
unsafe fn write_sockaddr_in(addr_ptr: usize, addrlen_ptr: usize, endpoint: IpEndpoint) {
    let mut sa = [0u8; 16];
    sa[0..2].copy_from_slice(&(AF_INET as u16).to_ne_bytes());
    sa[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
    if let IpAddress::Ipv4(ipv4) = endpoint.addr {
        sa[4..8].copy_from_slice(&ipv4.0); // already network order
    }
    let cap = ((addrlen_ptr as *const u32).read_unaligned() as usize).min(sa.len());
    core::ptr::copy_nonoverlapping(sa.as_ptr(), addr_ptr as *mut u8, cap);
    (addrlen_ptr as *mut u32).write_unaligned(sa.len() as u32);
}

/// Add a fresh listening TCP socket for `port` to one stack. `None` when that
/// stack does not exist (no virtio-net device) or the port is unusable. Takes
/// exactly one stack lock.
fn listen_on(lo: bool, port: u16) -> Option<SocketHandle> {
    let mut stack = stack_for(lo);
    let s = stack.as_mut()?;
    let rx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
    let tx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
    let mut socket = tcp::Socket::new(rx_buffer, tx_buffer);
    socket.listen(port).ok()?;
    Some(s.socket_set.add(socket))
}

/// If this stack's listening socket has completed a handshake, hand its handle
/// back together with a replacement listener on the same port. smoltcp has no
/// accept queue: the listening socket *becomes* the connection, so accepting it
/// means taking it over and arming a new one in its place. Takes exactly one
/// stack lock.
fn accept_on(lo: bool, handle: Option<SocketHandle>, port: u16)
    -> Option<(SocketHandle, SocketHandle)>
{
    let handle = handle?;
    let mut stack = stack_for(lo);
    let s = stack.as_mut()?;
    {
        let socket = s.socket_set.get_mut::<tcp::Socket>(handle);
        if !(socket.is_active() && socket.state() == tcp::State::Established) {
            return None;
        }
    }
    let rx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
    let tx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
    let mut replacement = tcp::Socket::new(rx_buffer, tx_buffer);
    replacement.listen(port).ok()?;
    Some((handle, s.socket_set.add(replacement)))
}

/// The local endpoint of an AF_INET socket, for getsockname. `None` for
/// anything that is not an inet socket (the AF_UNIX answer is written by the
/// caller). Takes SOCK_TABLES and then at most one stack lock, never nested
/// the other way round.
fn inet_local_endpoint(pid: u32, fd: usize) -> Option<IpEndpoint> {
    let (state, sock_type, bound_port) = inet_sock_info(pid, fd)?;
    match state {
        SockState::InetBound { local_endpoint, .. }  => Some(local_endpoint),
        SockState::InetListening { local, .. }       => Some(local),
        // Only a TCP socket may be fetched as a tcp::Socket — `SocketSet::get_mut`
        // panics on a type mismatch, so a connected UDP socket answers from the
        // fd table instead.
        SockState::InetConnected { socket_handle, lo, .. }
            if sock_type == SOCK_STREAM as u8 =>
        {
            let mut stack = stack_for(lo);
            let s = stack.as_mut()?;
            s.socket_set.get_mut::<tcp::Socket>(socket_handle).local_endpoint()
        }
        SockState::InetConnected { .. } =>
            Some(IpEndpoint::new(IpAddress::v4(0, 0, 0, 0), bound_port)),
        _ => None,
    }
}

/// The remote endpoint of a connected AF_INET socket, for getpeername.
fn inet_remote_endpoint(pid: u32, fd: usize) -> Option<IpEndpoint> {
    let (state, sock_type, _) = inet_sock_info(pid, fd)?;
    match state {
        SockState::InetConnected { socket_handle, remote_endpoint, lo }
            if sock_type == SOCK_STREAM as u8 =>
        {
            let mut stack = stack_for(lo);
            match stack.as_mut() {
                Some(s) => s.socket_set.get_mut::<tcp::Socket>(socket_handle)
                            .remote_endpoint().or(remote_endpoint),
                None => remote_endpoint,
            }
        }
        // A connected UDP socket has no smoltcp-side peer: report the endpoint
        // connect() recorded.
        SockState::InetConnected { remote_endpoint, .. } => remote_endpoint,
        _ => None,
    }
}

/// `(state, sock_type, bound_port)` of one fd, read under SOCK_TABLES and with
/// the lock released before the caller touches any stack.
fn inet_sock_info(pid: u32, fd: usize) -> Option<(SockState, u8, u16)> {
    let slot = fd_to_slot(fd)?;
    let tbls = SOCK_TABLES.lock();
    let tbl = tbls.iter().find(|t| t.in_use && t.pid == pid)?;
    if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return None; }
    Some((tbl.socks[slot].state, tbl.socks[slot].sock_type, tbl.socks[slot].bound_port))
}

/// Release ONE reference to the BOUND_PATHS slot a `UnixListening` socket
/// names. Called once per owner going away — a listener fd closing, a process
/// exiting, an in-flight SCM_RIGHTS copy being dropped undelivered.
///
/// The slot is reclaimed on the last release: only then does the address stop
/// resolving to a live listener (a pathname socket's VFS node lingers per
/// Linux, but connecting to it now yields ECONNREFUSED). Every caller
/// corresponds to exactly one reference taken by `bind`, `handle_dup`, or
/// `xfer_export`, so callers need no refcount awareness of their own.
///
/// Caller must not hold BOUND_PATHS.
fn free_bound_idx(bound_idx: usize) {
    let mut bound = BOUND_PATHS.lock();
    if bound_idx < MAX_BOUND && bound[bound_idx].in_use {
        bound[bound_idx].refs = bound[bound_idx].refs.saturating_sub(1);
        if bound[bound_idx].refs == 0 {
            bound[bound_idx] = BoundPath::new();
        }
    }
}

/// Take one extra reference to a bound address (`handle_dup`, `xfer_export`).
/// Caller must not hold BOUND_PATHS.
fn bound_ref_inc(bound_idx: usize) {
    let mut bound = BOUND_PATHS.lock();
    if bound_idx < MAX_BOUND && bound[bound_idx].in_use {
        bound[bound_idx].refs = bound[bound_idx].refs.saturating_add(1);
    }
}

// ── SCM_RIGHTS descriptor transfer ────────────────────────────────────────────
//
// `xfer_export` / `xfer_import` / `xfer_drop` are the three sides of one
// ownership rule, the same one `vfs::export_fd` documents: an exported
// descriptor carries a reference, and that reference is consumed by exactly
// one successful `xfer_import` or one `xfer_drop`. Never both, never neither.

/// Lift `fd` out of `pid`'s table into an in-flight descriptor, taking a
/// reference on whatever it names. `None` (→ EBADF) for a descriptor that
/// cannot be transferred.
fn xfer_export(pid: u32, fd: usize) -> Option<XferFd> {
    let Some(slot) = fd_to_slot(fd) else {
        // Ordinary VFS descriptor (memfd, pipe, file).
        return vfs::export_fd(pid, fd).map(XferFd::Vfs);
    };
    let entry = {
        let tbls = SOCK_TABLES.lock();
        let tbl = tbls.iter().find(|t| t.in_use && t.pid == pid)?;
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return None; }
        tbl.socks[slot]
    };
    match entry.state {
        // The case this exists for: handing a peer a listening socket.
        SockState::UnixListening { bound_idx } => bound_ref_inc(bound_idx),
        // Nothing underneath yet — the entry itself is the whole state.
        SockState::Unbound { .. } => {}
        // A connected end could be refcounted the same way `handle_dup` does,
        // but nothing in the session passes one and doing it here would make a
        // queued fd able to reference the very connection it is queued on.
        // Refused explicitly rather than silently mis-refcounted.
        _ => return None,
    }
    Some(XferFd::Sock(entry))
}

/// Install an in-flight descriptor into `pid`'s table, consuming its reference.
/// Returns the new fd, or a negative errno **without consuming** it — the
/// caller still owns it and must `xfer_drop` it (see the recvmsg overflow path).
fn xfer_import(pid: u32, x: XferFd, cloexec: bool) -> isize {
    match x {
        XferFd::Vfs(tf) => vfs::import_fd(pid, tf, cloexec),
        XferFd::Sock(entry) => {
            let mut tbls = SOCK_TABLES.lock();
            let Some(tbl) = get_or_create(pid, &mut *tbls) else { return -12 }; // ENOMEM
            let Some(slot) = tbl.alloc() else { return -24 };                   // EMFILE
            let mut e = entry;
            e.cloexec = cloexec;
            tbl.socks[slot] = e;
            (slot + SOCK_FD_BASE) as isize
        }
    }
}

/// Release an in-flight descriptor that never reached a receiver.
/// Caller must hold neither UNIX_CONNS nor BOUND_PATHS.
fn xfer_drop(x: XferFd) {
    match x {
        XferFd::Vfs(tf) => vfs::drop_transfer(tf),
        XferFd::Sock(entry) => match entry.state {
            SockState::UnixListening { bound_idx } => free_bound_idx(bound_idx),
            // `xfer_export` admits no other reference-holding state.
            _ => {}
        },
    }
}

// ── Smoltcp Integration ───────────────────────────────────────────────────────

pub struct NetStack {
    pub interface: Interface,
    pub socket_set: SocketSet<'static>,
    /// `Some` only on the NIC's stack. The loopback stack runs no DHCP client:
    /// a dhcpv4 socket there would broadcast DISCOVERs into its own queue and
    /// then receive them back forever.
    pub dhcp_handle: Option<SocketHandle>,
    /// `Some` only on the loopback stack. `phy::Loopback` owns the queue that
    /// carries a frame from transmit back to receive, so it has to persist
    /// across polls; the virtio wrapper is a stateless `dev_idx` and is rebuilt
    /// at every poll instead.
    pub loopback_dev: Option<smoltcp::phy::Loopback>,
}

pub static NET_STACK: Mutex<Option<NetStack>> = Mutex::new(None);
/// The loopback stack (127.0.0.0/8). Deliberately a *second* `Interface` with
/// its own `SocketSet`, not a second address on the NIC's interface: two
/// interfaces sharing one socket set would let whichever polls first claim a
/// socket's pending segment, so a loopback SYN could be emitted onto the wire
/// and never delivered. Separate sets make the choice explicit and make it
/// once, at bind/connect time.
pub static LO_STACK: Mutex<Option<NetStack>> = Mutex::new(None);
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
    init_loopback();
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
                dhcp_handle: Some(dhcp_handle),
                loopback_dev: None,
            };
            *NET_STACK.lock() = Some(stack);

            extern "C" { fn arch_serial_putc(b: u8); }
            let msg = b"[NET] Interface configured, net server initialized successfully\r\n";
            for &b in msg { unsafe { arch_serial_putc(b); } }
        }
    }
}

/// Stand up the loopback stack. Unconditional, unlike the NIC's: 127.0.0.1 has
/// to work on a guest with no virtio-net device at all, which is exactly the
/// configuration `init` skips above.
fn init_loopback() {
    let mut device = smoltcp::phy::Loopback::new(smoltcp::phy::Medium::Ethernet);
    // A locally-administered MAC, distinct from the NIC's. It is never seen off
    // the box; smoltcp needs one only so the interface can answer its own ARP
    // request for 127.0.0.1. Ethernet medium (rather than Medium::Ip) keeps this
    // interface on the same code path as the NIC's, and is what smoltcp's own
    // loopback example uses.
    let hardware_addr = HardwareAddress::Ethernet(EthernetAddress([0x02, 0, 0, 0, 0, 1]));
    let mut config = Config::new(hardware_addr);
    config.random_seed = sched::ticks();

    let mut interface = Interface::new(config, &mut device,
        Instant::from_millis((sched::ticks() * 10) as i64));
    interface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
    });
    // No default route on purpose: 127.0.0.0/8 is on-link here, and nothing
    // else may ever leave through this interface.

    *LO_STACK.lock() = Some(NetStack {
        interface,
        socket_set: SocketSet::new(alloc::vec![]),
        dhcp_handle: None,
        loopback_dev: Some(device),
    });

    extern "C" { fn arch_serial_putc(b: u8); }
    let msg = b"[NET] Loopback interface 127.0.0.1/8 up\r\n";
    for &b in msg { unsafe { arch_serial_putc(b); } }
}

pub fn net_daemon() -> ! {
    loop {
        let timestamp = Instant::from_millis((sched::ticks() * 10) as i64);
        let mut device = VirtioNetDeviceWrapper { dev_idx: 0 };
        
        let mut readiness_changed = false;
        let dhcp_status = {
            let mut stack = NET_STACK.lock();
            if let Some(ref mut s) = *stack {
                readiness_changed |= s.interface.poll(timestamp, &mut device, &mut s.socket_set);

                match s.dhcp_handle {
                    Some(h) => {
                        let dhcp_socket =
                            s.socket_set.get_mut::<smoltcp::socket::dhcpv4::Socket>(h);
                        match dhcp_socket.poll() {
                            Some(smoltcp::socket::dhcpv4::Event::Configured(config)) => {
                                Some((config.address, config.router, config.dns_servers))
                            }
                            _ => None,
                        }
                    }
                    None => None,
                }
            } else {
                None
            }
        };

        // The loopback stack is independent of the NIC and exists even when no
        // NIC does. One poll carries a whole 127.0.0.1 exchange — ARP, SYN,
        // SYN-ACK, ACK — because Interface::poll loops until neither ingress
        // nor egress made progress, and the Loopback phy feeds every frame it
        // transmits straight back into its own receive queue.
        {
            let mut lo = LO_STACK.lock();
            if let Some(ref mut s) = *lo {
                if let Some(ref mut dev) = s.loopback_dev {
                    readiness_changed |= s.interface.poll(timestamp, dev, &mut s.socket_set);
                }
            }
        }

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

        // AF_INET sockets are level-triggered through NET_POLL, so a task
        // parked in poll/epoll_wait only re-reads them when someone publishes a
        // readiness edge. smoltcp just told us whether one may have happened;
        // publish it with every stack lock released.
        if readiness_changed { sched::wake_poll(); }

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
            // No fd names this one — it is bound on behalf of the system, and
            // the single reference is what keeps it alive for good.
            refs: 1,
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
        NET_SETSOCKOPT  => handle_setsockopt(caller_pid, arg(msg,0) as usize,
                                             arg(msg,1) as usize, arg(msg,2) as usize,
                                             arg(msg,3) as usize, arg(msg,4) as usize),
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
        reuseaddr:  false,                     // set by setsockopt, before bind
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
            // sockaddr_in is read out of user memory here, before any lock is
            // taken: a demand-paging fault under SOCK_TABLES would re-enter the
            // scheduler with a server lock held.
            let port_be = unsafe { ((addr_ptr + 2) as *const u16).read_unaligned() };
            let port = u16::from_be(port_be);
            let sin_addr = unsafe { ((addr_ptr + 4) as *const u32).read_unaligned() };
            let ip = IpAddress::from(smoltcp::wire::Ipv4Address::from_bytes(&sin_addr.to_ne_bytes()));

            // An address no interface owns is EADDRNOTAVAIL on Linux. INADDR_ANY
            // and 127.0.0.0/8 always bind; anything else must be an address the
            // NIC currently holds. Without this a bind to a bogus address
            // "succeeded" and then silently never received anything.
            if !ip.is_unspecified() && !is_loopback_addr(ip) {
                let held = {
                    let stack = NET_STACK.lock();
                    match *stack {
                        Some(ref s) => s.interface.ip_addrs().iter().any(|c| c.address() == ip),
                        None        => false,
                    }
                };
                if !held { return err_reply(-99); } // EADDRNOTAVAIL
            }

            // TIME_WAIT is a leaf lock, so its snapshot is taken before
            // SOCK_TABLES and never underneath it.
            let mut resv = [0u16; MAX_TIME_WAIT];
            let nresv = time_wait_snapshot(&mut resv);

            let mut tbls = SOCK_TABLES.lock();
            // SO_REUSEADDR is read through a shared borrow, before the mutable
            // one below.
            let reuseaddr = tbls.iter()
                .find(|t| t.in_use && t.pid == pid)
                .map(|t| slot < MAX_SOCKS && t.socks[slot].in_use && t.socks[slot].reuseaddr)
                .unwrap_or(false);
            // An explicit port whose last connection closed less than 2*MSL ago
            // is EADDRINUSE unless the caller asked for SO_REUSEADDR — the
            // Linux answer, and the reason every server sets that option.
            // Note this deliberately does NOT add a conflict check against
            // *live* bound ports: bind() has never had one, and giving it one
            // is a much larger behaviour change than the TIME_WAIT divergence.
            if port != 0 && !reuseaddr && resv[..nresv].contains(&port) {
                return err_reply(-98); // EADDRINUSE
            }
            // Port 0 means "any free port". Assigning it *here* rather than at
            // listen() is what makes bind("127.0.0.1:0") — i.e. every
            // mio/tokio TcpListener::bind — work: listen() rejects a still-zero
            // bound_port with EINVAL, which surfaced to the caller as
            // "bind failed: Invalid argument". Picking the port reads every
            // table, so it happens before this process's is borrowed mutably.
            let port = if port != 0 {
                port
            } else {
                // An automatic port skips TIME_WAIT regardless of SO_REUSEADDR:
                // the caller asked for "any free port", not for that one.
                match alloc_ephemeral_port(&*tbls, &resv[..nresv]) {
                    Some(p) => p,
                    None    => return err_reply(-98), // EADDRINUSE: range exhausted
                }
            };
            let tbl = match find_tbl(pid, &mut *tbls) {
                Some(t) => t, None => return err_reply(-9),
            };
            if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }

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
                                     sock_id, is_abstract, refs: 1,
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
        // bind() is what assigns the port (including the ephemeral one for a
        // bind to :0), so a socket that never bound has no InetBound state and
        // gets EINVAL — the same answer the old zero-bound_port check gave.
        let local = match tbl.socks[slot].state {
            SockState::InetBound { local_endpoint, .. } => local_endpoint,
            // A second listen() on an already-listening socket is legal on
            // Linux — `inet_listen` only updates sk_max_ack_backlog and
            // returns 0 — and matching `InetBound` alone answered EINVAL.
            // Answer success and change nothing: falling through to the
            // listen_on() calls below would be actively wrong, adding a second
            // pair of smoltcp sockets on the same port and orphaning the
            // handles the first listen() stored (dropping any half-open
            // connection with them). No backlog is recorded because nothing
            // would read it: smoltcp has no accept queue — `accept_on` takes
            // the listening socket over and arms a replacement — so
            // `_backlog` is already ignored on the first listen(), and storing
            // a number no code honours would only fake a knob.
            SockState::InetListening { .. } => return ok_reply(),
            _ => return err_reply(-22),
        };
        if local.port == 0 { return err_reply(-22); }

        // INADDR_ANY listens on both stacks, as it does on Linux; an explicit
        // address on exactly the one that owns it.
        let any = local.addr.is_unspecified();
        let main = if any || !is_loopback_addr(local.addr) { listen_on(false, local.port) } else { None };
        let lo   = if any ||  is_loopback_addr(local.addr) { listen_on(true,  local.port) } else { None };
        if main.is_none() && lo.is_none() { return err_reply(-100); } // ENETDOWN

        tbl.socks[slot].state = SockState::InetListening { main, lo, local };
        ok_reply()
    } else {
        // AF_UNIX. `unix_listen` (net/unix/af_unix.c) gates in this order:
        //
        //   1. sock->type is neither SOCK_STREAM nor SOCK_SEQPACKET
        //                                        -> EOPNOTSUPP (not EINVAL)
        //   2. u->addr is NULL (never bound)      -> EINVAL
        //   3. sk_state is neither TCP_CLOSE nor TCP_LISTEN
        //                                        -> EINVAL
        //
        // `sock_type` is stored as `sock_type as u8`, and both SOCK_CLOEXEC
        // (0x80000) and SOCK_NONBLOCK (0x800) live above bit 7, so the
        // truncation already leaves the bare type here.
        let ty = tbl.socks[slot].sock_type;
        if ty != SOCK_STREAM as u8 && ty != SOCK_SEQPACKET as u8 {
            return err_reply(-EOPNOTSUPP);
        }
        match tbl.socks[slot].state {
            // `handle_bind` is what marks an AF_UNIX socket UnixListening, so
            // this server has one state where Linux has two: "bound, still
            // TCP_CLOSE" and "already TCP_LISTEN". Linux answers 0 to both —
            // a repeat listen() only updates sk_max_ack_backlog — so
            // collapsing them onto one success arm is faithful, and listen()
            // stays idempotent. Nothing is re-armed: bind() already published
            // the address, and re-running any of it would be the same
            // orphaned-listener bug `07d461c` avoided on the AF_INET side.
            SockState::UnixListening { .. } => ok_reply(),
            // Everything else is EINVAL:
            //   Unbound            — never bound, Linux's `!u->addr` gate.
            //   UnixConnected      — a socketpair end, an accepted socket, or
            //                        a connector the listener has paired:
            //                        TCP_ESTABLISHED on Linux.
            //   UnixPendingAccept  — our connect() has *already* returned 0,
            //                        and `unix_stream_connect` sets
            //                        TCP_ESTABLISHED before it returns 0, so
            //                        this is TCP_ESTABLISHED too. AF_UNIX
            //                        stream connect never reports EINPROGRESS,
            //                        so there is no separate "connecting"
            //                        state to answer differently.
            //   Inet*/Icmp*/None   — unreachable for an AF_UNIX socket, but a
            //                        bind() handed a sockaddr_in on an AF_UNIX
            //                        fd can leave InetBound here; EINVAL is
            //                        the right answer for that too.
            _ => err_reply(-22),
        }
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

    let (state, _bound_port, sock_type) = {
        let tbls = SOCK_TABLES.lock();
        let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
        (tbl.socks[slot].state, tbl.socks[slot].bound_port, tbl.socks[slot].sock_type)
    };

    match state {
        SockState::InetListening { main, lo, local } => {
            // Try the NIC's stack, then loopback. `accept_on` holds one stack
            // lock at a time and hands back the established handle plus the
            // replacement listener it armed in its place.
            let (from_lo, established, replacement) =
                if let Some((e, r)) = accept_on(false, main, local.port) {
                    (false, e, r)
                } else if let Some((e, r)) = accept_on(true, lo, local.port) {
                    (true, e, r)
                } else {
                    return err_reply(-11); // EAGAIN: no completed handshake yet
                };

            let new_slot = {
                let mut tbls = SOCK_TABLES.lock();
                let tbl = match find_tbl(pid, &mut *tbls) {
                    Some(t) => t, None => return err_reply(-9),
                };
                tbl.socks[slot].state = if from_lo {
                    SockState::InetListening { main, lo: Some(replacement), local }
                } else {
                    SockState::InetListening { main: Some(replacement), lo, local }
                };
                let new_slot = match tbl.alloc() { Some(sn) => sn, None => return err_reply(-24) };
                tbl.socks[new_slot] = SockEntry {
                    state: SockState::InetConnected {
                        socket_handle: established, remote_endpoint: None, lo: from_lo },
                    in_use: true,
                    bound_port: 0,
                    domain: AF_INET as u8,
                    sock_type: SOCK_STREAM as u8,
                    cloexec: acc_cloexec,
                    nonblock: acc_nonblock,
                    // An accepted socket is never bind()ed, so the flag has no
                    // consumer on it.
                    reuseaddr: false,
                };
                new_slot
            };

            if addr_ptr != 0 && addrlen_ptr != 0 {
                // Read the peer endpoint under the stack lock, write it into
                // user memory only after that lock is gone.
                let peer = {
                    let mut stack = stack_for(from_lo);
                    stack.as_mut().and_then(|s|
                        s.socket_set.get_mut::<tcp::Socket>(established).remote_endpoint())
                };
                if let Some(endpoint) = peer {
                    unsafe { write_sockaddr_in(addr_ptr, addrlen_ptr, endpoint); }
                }
            }

            val_reply((new_slot + SOCK_FD_BASE) as u64)
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
                        // "A pending entry exists" no longer implies "the
                        // connection behind it does": a pending connector is
                        // multi-holder now that fork copies it, so the entry
                        // and the `UnixConn` are freed by different steps.
                        // `conn_idx` carries no generation, so adopting a
                        // freed slot would hand the caller a live-looking fd
                        // onto a connection a later connect() can reuse.
                        // Unreachable today — both teardown paths clear the
                        // holder's table slot BEFORE decrementing `refs_a`, so
                        // reaching zero implies no pending entry survives —
                        // which is exactly why it is checked rather than
                        // assumed. EAGAIN is the honest answer: there is
                        // nothing acceptable here.
                        if conn_idx >= MAX_CONNS || !c[conn_idx].in_use {
                            return err_reply(-11);
                        }
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
                        reuseaddr:  false,
                    };

                    // EVERY alias of this connection flips, in every table —
                    // no `break`. One connect() is now nameable by several
                    // entries (fork copies the connector, and `refs_a` counts
                    // exactly those copies), and leaving one behind as
                    // `UnixPendingAccept` would let the same `UnixConn` be
                    // found and accepted a SECOND time by the next accept on
                    // this listener: two end-B sockets on one connection, and
                    // an `refs_a` that no longer matches the number of live
                    // connector fds. Stopping at the first match happened to
                    // be safe only while a pending entry was unique.
                    for t in tbls.iter_mut() {
                        if !t.in_use { continue; }
                        for s in t.socks.iter_mut() {
                            if let SockState::UnixPendingAccept { conn_idx: pending_conn, .. } = s.state {
                                if pending_conn == conn_idx {
                                    s.state = SockState::UnixConnected { conn_idx, is_a: true };
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
                    dbg("[NET] accept pid="); dbg_u(pid as u64);
                    dbg(" sid="); dbg_u(listen_sock_id);
                    dbg(" conn="); dbg_u(conn_idx as u64);
                    dbg(" -> fd="); dbg_u((new_slot + SOCK_FD_BASE) as u64); dbg("\r\n");
                    // Wake the connector parked in poll/connect (K2).
                    sched::wake_poll();
                    val_reply((new_slot + SOCK_FD_BASE) as u64)
                }
                None => {
                    // SOCK_TABLES is still held by the scan above, and a
                    // per-byte serial write takes the console's lock — release
                    // first. The EAGAIN arm is also the hot one (every
                    // nonblocking accept loop turn on every AF_UNIX listener
                    // in the session lands here), so the trace is bounded the
                    // way servers/drm bounds its mmap trace.
                    drop(tbls);
                    if NET_DEBUG && ACCEPT_EAGAIN_SEEN.fetch_add(1, Ordering::Relaxed)
                        < ACCEPT_EAGAIN_LIMIT {
                        dbg("[NET] accept pid="); dbg_u(pid as u64);
                        dbg(" sid="); dbg_u(listen_sock_id); dbg(" EAGAIN\r\n");
                    }
                    err_reply(-11)
                }
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

        let remote_ip = IpAddress::from(smoltcp::wire::Ipv4Address::from_bytes(&sin_addr.to_ne_bytes()));
        let remote_endpoint = IpEndpoint::new(remote_ip, port);
        // 127.0.0.0/8 is served by the loopback stack, everything else by the
        // NIC's. Decided once, here, and recorded in the socket state so every
        // later send/recv/poll/close looks in the same socket set.
        let lo = is_loopback_addr(remote_ip);

        // TIME_WAIT snapshot before SOCK_TABLES — the leaf-lock rule again.
        let mut resv = [0u16; MAX_TIME_WAIT];
        let nresv = time_wait_snapshot(&mut resv);

        let mut tbls = SOCK_TABLES.lock();
        // Source port for a socket that never bound. Picked before this
        // process's table is borrowed mutably, since it reads every table.
        let ephemeral = alloc_ephemeral_port(&*tbls, &resv[..nresv]);
        let tbl = match find_tbl(pid, &mut *tbls) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }

        let sock_type = tbl.socks[slot].sock_type;
        let local_port = if tbl.socks[slot].bound_port != 0 {
            tbl.socks[slot].bound_port
        } else {
            match ephemeral {
                Some(p) => { tbl.socks[slot].bound_port = p; p }
                None    => return err_reply(-98), // EADDRINUSE: range exhausted
            }
        };

        let reply = {
        let mut stack = stack_for(lo);
        if let Some(ref mut s) = *stack {
            if sock_type == SOCK_STREAM as u8 {
                let rx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
                let tx_buffer = tcp::SocketBuffer::new(alloc::vec![0; 8192]);
                let mut socket = tcp::Socket::new(rx_buffer, tx_buffer);

                // A caller-supplied address must never panic the kernel: an
                // unroutable or malformed endpoint is EINVAL, not unwrap().
                if socket.connect(s.interface.context(), remote_endpoint, local_port).is_err() {
                    return err_reply(-22);
                }
                let handle = s.socket_set.add(socket);
                tbl.socks[slot].state = SockState::InetConnected {
                    socket_handle: handle, remote_endpoint: Some(remote_endpoint), lo };
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
                if socket.bind(local_port).is_err() { return err_reply(-22); }
                let handle = s.socket_set.add(socket);
                tbl.socks[slot].state = SockState::InetConnected {
                    socket_handle: handle, remote_endpoint: Some(remote_endpoint), lo };
                ok_reply()
            }
        } else {
            err_reply(-100)
        }
        };
        // Every server lock is released before the wake: the SYN only leaves on
        // the next daemon poll, and wake_poll must never run under one.
        drop(tbls);
        sched::wake_poll();
        reply
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
        // the /dev/shm and /run/user tmpfs mounts, S_IFSOCK check).
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
        let (conn_idx, orphans) = {
            let mut conns = UNIX_CONNS.lock();
            let idx = match conns.iter().position(|c| !c.in_use) {
                Some(i) => i, None => return err_reply(-12),
            };
            let orphans = conns[idx].take_fds();
            conns[idx] = UnixConn::new();
            conns[idx].in_use = true;
            conns[idx].cred_a = cred;
            (idx, orphans)
        };
        for x in orphans { xfer_drop(x); }

        let mut tbls = SOCK_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
        tbl.socks[slot].state = SockState::UnixPendingAccept { conn_idx, sock_id };
        drop(tbls);
        dbg("[NET] connect pid="); dbg_u(pid as u64);
        dbg(" fd="); dbg_u(fd as u64);
        dbg(" conn="); dbg_u(conn_idx as u64);
        dbg(" sid="); dbg_u(sock_id);
        dbg(if is_abstract { " abstract\r\n" } else { " path\r\n" });
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
    let (conn_idx, orphans) = {
        let mut conns = UNIX_CONNS.lock();
        let idx = match conns.iter().position(|c| !c.in_use) {
            Some(i) => i, None => return err_reply(-12),
        };
        let orphans = conns[idx].take_fds();
        conns[idx] = UnixConn::new();
        conns[idx].in_use = true;
        conns[idx].cred_a = cred;
        conns[idx].cred_b = cred;
        (idx, orphans)
    };
    for x in orphans { xfer_drop(x); }

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
        cloexec, nonblock, reuseaddr: false,
    };
    let slot_b = match tbl.alloc() { Some(s) => s, None => {
        tbl.socks[slot_a] = SockEntry::empty(); return err_reply(-24);
    }};
    tbl.socks[slot_b] = SockEntry {
        state: SockState::UnixConnected { conn_idx, is_a: false },
        in_use: true, bound_port: 0, domain: AF_UNIX as u8, sock_type: sock_type as u8,
        cloexec, nonblock, reuseaddr: false,
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
            // Half-close, per direction: our own write direction retired, or
            // the peer's read direction retired, is EPIPE as well. The peer
            // retiring its *write* direction is deliberately not — that only
            // makes our recvs see EOF, and treating it as a broken pipe is the
            // whole defect (the first send after the peer's OwnedWriteHalf
            // dropped got EPIPE instead of going through).
            if conn.wr_shut(is_a) || conn.rd_shut(!is_a) { return err_reply(-32); } // EPIPE
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
            // Pre-accept the connector is end A, so its own half-close counts
            // here too (accept preserves conn_idx, so the flag carries over).
            if conn.wr_shut(true) { return err_reply(-32); } // EPIPE
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
        SockState::InetConnected { socket_handle, remote_endpoint, lo } => {
            drop(tbls);
            // Touch user memory ONLY with every lock released. `buf_ptr`/`addr_ptr`
            // are demand-paged, so reading them can take a page fault, and a fault
            // taken under the stack spinlock re-enters the scheduler while holding
            // it — the hazard shape that once froze all four vCPUs (fixed 82d0cc3).
            // read_user_buf is deliberately NOT used here: it does not fault a page
            // in, and sys_sendto only validates the range, so a first-touch .rodata
            // send buffer would spuriously EFAULT. Same idiom as the ICMP arm below.
            let mut data = alloc::vec![0u8; len];
            if len > 0 {
                unsafe {
                    core::ptr::copy_nonoverlapping(buf_ptr as *const u8, data.as_mut_ptr(), len);
                }
            }
            // Destination sockaddr, likewise read before locking. Datagram-only:
            // the stream path never dereferenced `addr_ptr` and sys_sendto does not
            // validate it, so it must stay untouched there. Kept as an Option so the
            // EDESTADDRREQ/ENETDOWN precedence stays exactly as it was.
            let dest = if sock_type == SOCK_STREAM as u8 {
                None
            } else if addr_ptr != 0 && addrlen >= 8 {
                let port_be = unsafe { ((addr_ptr + 2) as *const u16).read_unaligned() };
                let port = u16::from_be(port_be);
                let sin_addr = unsafe { ((addr_ptr + 4) as *const u32).read_unaligned() };
                let ip = IpAddress::from(smoltcp::wire::Ipv4Address::from_bytes(&sin_addr.to_ne_bytes()));
                Some(IpEndpoint::new(ip, port))
            } else {
                remote_endpoint
            };

            let mut stack = stack_for(lo);
            if let Some(ref mut s) = *stack {
                if sock_type == SOCK_STREAM as u8 {
                    let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    if !socket.can_send() {
                        return err_reply(-11);
                    }
                    match socket.send_slice(&data) {
                        Ok(n) => val_reply(n as u64),
                        Err(_) => err_reply(-104),
                    }
                } else {
                    let socket = s.socket_set.get_mut::<udp::Socket>(socket_handle);
                    let endpoint = match dest {
                        Some(ep) => ep,
                        None => return err_reply(-89),
                    };
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
            // Our own read direction retired (shutdown(fd, SHUT_RD)): EOF at
            // once, queued bytes included — Linux discards them.
            if conn.rd_shut(is_a) { return val_reply(0); }
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
            // once the peer will never write again. An empty ring with a live
            // peer is EAGAIN — returning 0 here made tokio's signal driver see
            // "EOF on self-pipe" on its very first empty poll and panic. The
            // peer having retired just its write direction ends the stream the
            // same way a full close does, and must read as EOF, not an error.
            if n == 0 && len > 0 {
                let peer_closed = if is_a { conn.closed_b } else { conn.closed_a };
                if !peer_closed && !conn.wr_shut(!is_a) { return err_reply(-11); } // EAGAIN
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
            if conn.rd_shut(true) { return val_reply(0); } // own read half retired
            let n = conn.ring_ba.read(buf_ptr as *mut u8, len);
            if n == 0 && len > 0 && !conn.closed_b { return err_reply(-11); } // EAGAIN
            if n > 0 { conn.seq = conn.seq.wrapping_add(1); }
            drop(conns);
            if n > 0 { sched::wake_poll(); }
            val_reply(n as u64)
        }
        SockState::InetConnected { socket_handle, lo, .. } => {
            let mut stack = stack_for(lo);
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
    let mut batch: alloc::vec::Vec<XferFd> = alloc::vec::Vec::new();
    for i in 0..nfd {
        match xfer_export(pid, fd_nums[i] as usize) {
            Some(tf) => batch.push(tf),
            None => { for tf in batch { xfer_drop(tf); } return err_reply(-9); } // EBADF
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
        for tf in batch { xfer_drop(tf); }
        return val_reply(0);
    }

    // Write the data and attach the fd batch to the first byte written, under a
    // single UNIX_CONNS critical section so the stream offset can't race.
    let mut conns = UNIX_CONNS.lock();
    let conn = &mut conns[conn_idx];
    if !conn.in_use {
        drop(conns);
        for tf in batch { xfer_drop(tf); }
        return err_reply(-32);
    }
    let peer_closed = if is_a { conn.closed_b } else { conn.closed_a };
    if peer_closed {
        drop(conns);
        for tf in batch { xfer_drop(tf); }
        return err_reply(-32); // EPIPE
    }
    // Half-close, same rule as handle_send: our write direction or the peer's
    // read direction retired is EPIPE; the peer's write direction is not.
    if conn.wr_shut(is_a) || conn.rd_shut(!is_a) {
        drop(conns);
        for tf in batch { xfer_drop(tf); }
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
        for tf in batch { xfer_drop(tf); }
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
        for tf in batch { xfer_drop(tf); }
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
        // Empty ring: EOF only if the peer end has closed or retired its write
        // direction, or we retired our own read direction — else EAGAIN.
        // Mirrors handle_recv — tokio's self-pipe must not see a spurious EOF.
        let peer_closed = if is_a { conn.closed_b } else { conn.closed_a };
        if !peer_closed && conn.in_use && !conn.wr_shut(!is_a) && !conn.rd_shut(is_a) {
            drop(conns);
            return err_reply(-11); // EAGAIN — blocking wrapper retries
        }
    }

    let rtotal = if is_a { conn.ring_ba.rtotal } else { conn.ring_ab.rtotal };
    let deliver: Option<alloc::vec::Vec<XferFd>> = {
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
            let newfd = xfer_import(pid, fds[i], cloexec);
            if newfd < 0 {
                // Receiver's fd table is full: everything from here truncates.
                // `import_fd` consumes a descriptor only when it returns an fd,
                // so `fds[i]` is still ours — `fit = i` deliberately puts it
                // back in the drop loop below. Do NOT "fix" this to `i + 1`:
                // that leaks the reference instead.
                ctrunc = true;
                fit = i;
                break;
            }
            installed[i] = newfd as i32;
            i += 1;
        }
        // Close every fd that didn't fit — including the one whose import just
        // failed (Linux drops the overflow).
        for j in fit..nfds { xfer_drop(fds[j]); }
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

/// shutdown(2). Half-closes one *direction* of a connected socket. The fd stays
/// a perfectly valid socket, every other alias of the same end keeps working,
/// and the peer observes EOF on reads rather than an error.
///
/// This used to ignore `how` entirely and answer every call by setting the
/// end's `closed_*` flag — the "this end is fully gone" flag `handle_close`
/// only sets once `refs_*` reaches 0 — and then storing `SockEntry::empty()`
/// over the caller's slot. That made `shutdown(fd, SHUT_WR)` strictly more
/// destructive than `close(fd)`: it bypassed the dup refcount and it destroyed
/// the fd, so the caller's next recv answered EBADF and its epoll interest
/// answered POLLNVAL (which mio decodes as an empty readiness set, i.e. a hang,
/// not an error). An `InetConnected` socket fell through to the same slot
/// clearing with no smoltcp teardown at all, leaking the handle and the port
/// `handle_close` is careful to park.
fn handle_shutdown(pid: u32, fd: usize, how: usize) -> Message {
    if how != SHUT_RD && how != SHUT_WR && how != SHUT_RDWR { return err_reply(-22); } // EINVAL
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    // State is copied out and every table lock released before UNIX_CONNS or a
    // stack lock is taken (K2 lock order), exactly as handle_recv does.
    let (state, sock_type) = {
        let tbls = SOCK_TABLES.lock();
        let tbl = match tbls.iter().find(|t| t.in_use && t.pid == pid) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
        (tbl.socks[slot].state, tbl.socks[slot].sock_type)
    };

    let unix_end = match state {
        SockState::UnixConnected { conn_idx, is_a } => Some((conn_idx, is_a)),
        // A connect() not yet accepted is end A of its connection, and accept
        // preserves conn_idx and both rings, so the half-close carries over.
        SockState::UnixPendingAccept { conn_idx, .. } => Some((conn_idx, true)),
        _ => None,
    };
    if let Some((conn_idx, is_a)) = unix_end {
        {
            let mut conns = UNIX_CONNS.lock();
            let conn = &mut conns[conn_idx];
            if !conn.in_use { return err_reply(-107); } // ENOTCONN
            conn.shutdown_end(is_a, how);
            // A half-close is a poll-visible edge — the peer becomes
            // readable-at-EOF — so it bumps the same per-connection edge-seq
            // every other readiness change bumps. Without it an EPOLLET
            // registration (tokio's) never re-arms and the waiter sleeps
            // through the EOF it was waiting for.
            conn.seq = conn.seq.wrapping_add(1);
        } // release UNIX_CONNS before waking pollers (K2 lock order)
        sched::wake_poll();
        return ok_reply();
    }

    if let SockState::InetConnected { socket_handle, lo, .. } = state {
        // Retiring the write direction on TCP is the FIN and nothing else: the
        // fd, the smoltcp handle and the port stay this socket's until
        // handle_close tears them down (and parks the port in TIME_WAIT).
        // SHUT_RD has no wire effect, and a connected UDP socket has no
        // half-close at all — both are accepted and do nothing, as on Linux.
        // Only a stream socket has a `tcp::Socket` behind the handle; `get_mut`
        // on the wrong socket type panics.
        if sock_type == SOCK_STREAM as u8 && (how == SHUT_WR || how == SHUT_RDWR) {
            let mut stack = stack_for(lo);
            if let Some(ref mut s) = *stack {
                s.socket_set.get_mut::<tcp::Socket>(socket_handle).close();
            }
        }
        return ok_reply();
    }

    // Everything else — unbound, listening, ICMP — is not connected, and Linux
    // answers ENOTCONN. What it must never do, and what this did, is silently
    // retire the caller's fd out from under it.
    err_reply(-107) // ENOTCONN
}

/// getsockname(2). An AF_INET socket reports its real bound endpoint: the
/// ephemeral port a `bind(":0")` was handed is discoverable *only* here, and
/// every mio/tokio `TcpListener` asks for it straight after binding. Anything
/// else keeps the historical minimal answer (sa_family only, zeroed) — nothing
/// on LeandrOS reads a unix socket's own address back, and Linux reports an
/// unnamed unix socket exactly that way.
fn handle_getsockname(pid: u32, fd: usize, addr_ptr: usize, addrlen_ptr: usize) -> Message {
    if addr_ptr == 0 || addrlen_ptr == 0 { return err_reply(-14); }
    // Endpoint resolved first; user memory is written with no lock held.
    let endpoint = inet_local_endpoint(pid, fd);
    match endpoint {
        Some(ep) => unsafe { write_sockaddr_in(addr_ptr, addrlen_ptr, ep); },
        None => unsafe {
            core::ptr::write_bytes(addr_ptr as *mut u8, 0, 2);
            core::ptr::write(addrlen_ptr as *mut u32, 2);
        },
    }
    ok_reply()
}

fn handle_getpeername(pid: u32, fd: usize, addr_ptr: usize, addrlen_ptr: usize) -> Message {
    if addr_ptr == 0 || addrlen_ptr == 0 { return err_reply(-14); }
    let endpoint = inet_remote_endpoint(pid, fd);
    match endpoint {
        Some(ep) => unsafe { write_sockaddr_in(addr_ptr, addrlen_ptr, ep); },
        None => unsafe {
            core::ptr::write_bytes(addr_ptr as *mut u8, 0, 2);
            core::ptr::write(addrlen_ptr as *mut u32, 2);
        },
    }
    ok_reply()
}

/// setsockopt was a bare `ok_reply()` for every option, and stays that way for
/// every option but one: SO_REUSEADDR now has to be recorded, because bind()
/// consults it to decide whether a port still in TIME_WAIT may be taken.
/// Without that, adding TIME_WAIT would *break* the very restart it models —
/// Linux lets a server with SO_REUSEADDR rebind its port immediately.
///
/// Everything else keeps answering success. Unlike getsockopt (where a bogus
/// success makes the caller read an unwritten buffer — the zbus SO_PEERPIDFD
/// trap), a setsockopt that quietly ignores an option it does not implement
/// only loses a tuning knob, and turning those into ENOPROTOOPT would be an
/// unrelated behaviour change.
fn handle_setsockopt(pid: u32, fd: usize, level: usize, optname: usize,
                     optval_ptr: usize, optlen: usize) -> Message {
    const SO_REUSEADDR: usize = 2;
    if level == SOL_SOCKET as usize && optname == SO_REUSEADDR {
        // The int is read out of user memory before any server lock is taken:
        // a demand-paging fault under SOCK_TABLES would re-enter the scheduler
        // with a server lock held.
        let on = optval_ptr != 0 && optlen >= 4
            && unsafe { (optval_ptr as *const u32).read_unaligned() } != 0;
        let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
        let mut tbls = SOCK_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= MAX_SOCKS || !tbl.socks[slot].in_use { return err_reply(-9); }
        tbl.socks[slot].reuseaddr = on;
        return ok_reply();
    }
    ok_reply()
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
/// EOF/EPIPE accounting the parent observes.
///
/// Both AF_UNIX stream states are copied: `UnixConnected` and
/// `UnixPendingAccept`. The latter is a connect() the peer has not accepted
/// yet, and it is end A of a perfectly real `UnixConn` — it refcounts through
/// `refs_a` exactly like a connected end, and `handle_accept`'s fixup loop
/// converts *every* table's pending entry for that connection, so parent and
/// child both end up `UnixConnected { is_a: true }` with `refs_a == 2`.
///
/// Leaving it out is what stopped every `X-HostWaylandDisplay=true` COSMIC
/// applet from ever reaching `execve`. cosmic-panel creates the applet's
/// privileged socket by binding an abstract listener, marshalling it to
/// cosmic-comp over `wp_security_context_v1.create_listener`, and connecting
/// to it itself — all synchronously, before the Wayland request has even been
/// flushed. launch-pad then forks with that connector still in
/// `UnixPendingAccept`, and the child's `pre_exec` hook clears FD_CLOEXEC on
/// the inherited fd numbers. With the state dropped here that
/// `fcntl(F_GETFD)` returned EBADF, `pre_exec` failed, and `Command::spawn`
/// died *before* `execve` — silently, because cosmic-panel's `if let Ok(key)`
/// (cosmic-panel-bin/src/main.rs:225) discards the error. The applet then
/// looks like a live process that draws nothing when in fact it never ran.
/// Whether it happened at all was a race with cosmic-comp's accept, which is
/// why the applet spawned last in a batch worked and the first one did not.
///
/// Inet/listening sockets are NOT copied: their smoltcp handles have no
/// refcounting and a dup'd close would double-free them (fork children exec
/// immediately in practice).
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

    // TEMPORARY: (slot, conn_idx) of each inherited pending connector, printed
    // after both locks are released. `Vec::new` does not allocate, and nothing
    // is pushed unless NET_DEBUG, so a release build never heap-allocates here.
    let mut traced: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();

    // The entry copy and its refcount increment are ONE step, under both
    // locks. They used to be two passes with the table released in between,
    // which meant a child entry could be installed without a matching
    // increment (its connection having gone away between the passes) — and
    // since `conn_idx` carries no generation, that child's later close would
    // decrement, and could destroy, whatever connection had since been given
    // the slot. Copying only what was successfully refcounted removes the
    // window entirely. The SOCK_TABLES-outer / UNIX_CONNS-inner nesting is the
    // one `handle_dup` and `handle_accept` already use, and BOUND_PATHS — the
    // leaf — is never taken here: fork copies no listener.
    let mut conns = UNIX_CONNS.lock();
    for i in 0..MAX_SOCKS {
        let e = tbls[parent_pos].socks[i];
        if !e.in_use { continue; }
        match e.state {
            SockState::UnixConnected { conn_idx, is_a } => {
                if conn_idx >= MAX_CONNS || !conns[conn_idx].in_use { continue; }
                if is_a { conns[conn_idx].refs_a += 1; } else { conns[conn_idx].refs_b += 1; }
                tbls[child_pos].socks[i] = e;
            }
            // An unaccepted connect() is end A of a real connection — see the
            // doc comment for why dropping it here broke COSMIC's privileged
            // applet sockets.
            SockState::UnixPendingAccept { conn_idx, .. } => {
                if conn_idx >= MAX_CONNS || !conns[conn_idx].in_use { continue; }
                conns[conn_idx].refs_a += 1; // the connector is always end A
                tbls[child_pos].socks[i] = e;
                if NET_DEBUG { traced.push((i, conn_idx)); }
            }
            SockState::Unbound { .. } => { tbls[child_pos].socks[i] = e; }
            _ => {} // inet/listening: skipped (see doc comment)
        }
    }
    drop(conns);
    drop(tbls);
    // TEMPORARY trace, deliberately outside both critical sections: per-byte
    // serial writes take the console's own lock, and holding a server spinlock
    // across another subsystem's lock is the shape this tree already bans.
    for (i, conn_idx) in traced {
        dbg("[NET] fork "); dbg_u(parent as u64); dbg("->"); dbg_u(child as u64);
        dbg(" pending fd="); dbg_u((i + SOCK_FD_BASE) as u64);
        dbg(" conn="); dbg_u(conn_idx as u64); dbg(" COPIED\r\n");
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
    let cloexec = {
        let tbls = SOCK_TABLES.lock();
        match tbls.iter().find(|t| t.in_use && t.pid == pid) {
            Some(t) if slot < MAX_SOCKS && t.socks[slot].in_use => Some(t.socks[slot].cloexec),
            _ => None,
        }
    };
    match cloexec {
        Some(c) => val_reply(if c { 1 } else { 0 }),
        None => {
            // TEMPORARY: this is the exact failure the privileged-applet bug
            // produced — launch-pad's pre_exec clears FD_CLOEXEC on every
            // inherited fd, and an fd the fork did not copy answers EBADF,
            // which aborts Command::spawn before execve.
            dbg("[NET] getfd pid="); dbg_u(pid as u64);
            dbg(" fd="); dbg_u(sockfd as u64); dbg(" EBADF\r\n");
            err_reply(-9)
        }
    }
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
    // An AF_UNIX listener is refcounted through its bound address, so the two
    // fds share one listening queue and the address survives until the last of
    // them closes — Linux semantics, and the reason this is not the EINVAL arm
    // below any more. libwayland dups EVERY fd it marshals
    // (`wl_closure_marshal` -> `wl_os_dupfd_cloexec`), so refusing this made
    // `wp_security_context_v1.create_listener` unmarshallable, which poisons
    // the caller's whole Wayland connection rather than failing one request:
    // cosmic-panel died with exit 1 the moment any `X-HostWaylandDisplay=true`
    // applet (CosmicAppletTiling, CosmicAppletMinimize) was configured.
    else if let SockState::UnixListening { bound_idx } = entry.state {
        // BOUND_PATHS is a leaf lock and SOCK_TABLES is held here, so the
        // increment cannot be done inline; take it after dropping the table.
        // `new_slot` was chosen under the lock just dropped, so it is re-picked
        // below rather than trusted across the gap.
        drop(tbls);
        bound_ref_inc(bound_idx);
        let mut tbls = SOCK_TABLES.lock();
        let placed = match find_tbl(pid, &mut *tbls) {
            Some(t) => match t.alloc() {
                Some(s) => { let mut e = entry; e.cloexec = cloexec; t.socks[s] = e; Some(s) }
                None => None,
            },
            None => None,
        };
        drop(tbls);
        return match placed {
            Some(s) => val_reply((s + SOCK_FD_BASE) as u64),
            // Nothing was installed, so hand the reference straight back
            // rather than leaking the address for the life of the system.
            None => { free_bound_idx(bound_idx); err_reply(-24) } // EMFILE
        };
    }
    // Inet sockets alias fine at the table level, but their smoltcp handle is
    // only released by handle_close, which the refcountless copy would
    // double-free — so they stay unsupported.
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
    let state = tbl.socks[slot].state;
    // There is no port to release here, which is why `bound_port` is not read:
    // `alloc_ephemeral_port` derives "free" from the live tables (any socket
    // with `in_use && bound_port == p`), and every arm below stores
    // `SockEntry::empty()`, which zeroes `bound_port`. Clearing the slot *is*
    // the release; there is no separate pool to hand the port back to.
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
            let mut orphans = alloc::vec::Vec::new();
            if *refs == 0 {
                if is_a { c.closed_a = true; } else { c.closed_b = true; }
                c.seq = c.seq.wrapping_add(1);
                end_closed = true;
                if c.closed_a && c.closed_b { orphans = c.take_fds(); c.in_use = false; }
            }
            drop(conns);
            for x in orphans { xfer_drop(x); }
            let mut tbls2 = SOCK_TABLES.lock();
            if let Some(t2) = tbls2.iter_mut().find(|t| t.in_use && t.pid == pid) {
                t2.socks[slot] = SockEntry::empty();
            }
            drop(tbls2);
            // Peer sees POLLHUP/POLLIN (EOF) once this end really closed.
            if end_closed { sched::wake_poll(); }
        }
        SockState::InetConnected { socket_handle, lo, .. } => {
            let sock_type = tbl.socks[slot].sock_type;
            tbl.socks[slot] = SockEntry::empty();
            drop(tbls);
            // The port to park, decided under the stack lock and acted on after
            // it: TIME_WAIT is a leaf lock.
            let mut park = None;
            {
                let mut stack = stack_for(lo);
                if let Some(ref mut s) = *stack {
                    // Only a TCP socket has a `tcp::Socket` behind the handle —
                    // `get` on the wrong socket type panics, and a connected
                    // UDP socket lands in this same arm.
                    if sock_type == SOCK_STREAM as u8 {
                        let sk = s.socket_set.get::<tcp::Socket>(socket_handle);
                        // Only the side that closes an established connection
                        // *first* goes through TIME_WAIT. CloseWait/LastAck mean
                        // the peer's FIN already arrived, so this is the passive
                        // close and Linux goes straight to CLOSED. Closed/Listen/
                        // SynSent/SynReceived never established.
                        let active_close = matches!(sk.state(),
                            tcp::State::Established | tcp::State::FinWait1
                            | tcp::State::FinWait2  | tcp::State::Closing
                            | tcp::State::TimeWait);
                        // The local port is read from smoltcp rather than from
                        // `bound_port`, which `handle_accept` leaves at 0 on an
                        // accepted socket — and an accepted socket sharing the
                        // listener's port is exactly what makes a restarted
                        // server hit EADDRINUSE on Linux.
                        if active_close { park = sk.local_endpoint().map(|e| e.port); }
                    }
                    // The socket is still torn down here rather than being left
                    // to run smoltcp's own TIME-WAIT: what is modelled is the
                    // port reservation, not the protocol state.
                    s.socket_set.remove(socket_handle);
                }
            }
            if let Some(p) = park { time_wait_add(p); }
        }
        SockState::InetListening { main, lo, .. } => {
            tbl.socks[slot] = SockEntry::empty();
            drop(tbls);
            // An INADDR_ANY listener holds a socket on each stack; drop both,
            // one stack lock at a time.
            if let Some(h) = main {
                let mut stack = stack_for(false);
                if let Some(ref mut s) = *stack { s.socket_set.remove(h); }
            }
            if let Some(h) = lo {
                let mut stack = stack_for(true);
                if let Some(ref mut s) = *stack { s.socket_set.remove(h); }
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
            // A connect that has not been accepted yet is end A, and it is
            // refcounted like any other end: `handle_fork_dup` copies the fd
            // to a forked child, so the half-open connection must outlive
            // whichever holder closes first. Tearing it down unconditionally
            // meant cosmic-panel destroyed the applet's privileged socket the
            // instant launch-pad had forked — and cosmic-comp gets exactly one
            // shot at accepting it (smithay's SecurityContextListenerSource
            // removes itself as soon as the paired close_fd pipe reports
            // POLLERR, which it does immediately because the panel drops the
            // read end), so there is no second chance.
            let mut orphans = alloc::vec::Vec::new();
            let mut died = false;
            let mut refs = 0;
            {
                let mut conns = UNIX_CONNS.lock();
                if conn_idx < MAX_CONNS && conns[conn_idx].in_use {
                    let c = &mut conns[conn_idx];
                    c.refs_a = c.refs_a.saturating_sub(1);
                    refs = c.refs_a;
                    if c.refs_a == 0 {
                        orphans = c.take_fds();
                        c.in_use = false;
                        c.seq = c.seq.wrapping_add(1);
                        died = true;
                    }
                }
            }
            // UNIX_CONNS released first: an orphan may itself be a socket, and
            // releasing one takes BOUND_PATHS (see `take_fds`).
            for x in orphans { xfer_drop(x); }
            dbg("[NET] close-pending pid="); dbg_u(pid as u64);
            dbg(" fd="); dbg_u(sockfd as u64);
            dbg(" conn="); dbg_u(conn_idx as u64);
            dbg(" refs="); dbg_u(refs as u64);
            if died { dbg(" DESTROYED"); }
            dbg("\r\n");
            // A listener parked in poll must stop reporting POLLIN for a
            // connect that no longer exists.
            if died { sched::wake_poll(); }
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
            // A half-close asserts the same thing, but only for the direction
            // it retired: a recv that will return 0 does not block and must be
            // advertised readable, while a send that will raise EPIPE must not
            // be advertised writable.
            let read_eof   = peer_closed || conn.wr_shut(!is_a) || conn.rd_shut(is_a);
            let write_dead = peer_closed || conn.wr_shut(is_a)  || conn.rd_shut(!is_a);
            if readable > 0 || read_eof || !conn.in_use { ev |= POLLIN; }
            if conn.in_use && !write_dead && write_free > 0 { ev |= POLLOUT; }
            // POLLHUP stays reserved for a whole-connection teardown. Linux
            // reports a half-close as readable-at-EOF (POLLRDHUP), never as a
            // hangup, and asserting POLLHUP here would make an epoll client
            // discard a socket that is still writable in the other direction.
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
            // Pre-accept the connector is end A, so its own half-close applies
            // here the same way it does once the connection is established.
            if readable > 0 || !conn.in_use || conn.rd_shut(true) { ev |= POLLIN; }
            if conn.in_use && !conn.closed_b && !conn.wr_shut(true) && write_free > 0 { ev |= POLLOUT; }
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
        SockState::InetConnected { socket_handle, lo, .. } => {
            drop(tbls);
            let mut stack = stack_for(lo);
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
        SockState::InetListening { main, lo, .. } => {
            drop(tbls);
            // Readable as soon as either stack's listener has an established
            // connection waiting to be accepted. One stack lock at a time.
            let ready = |on_lo: bool, handle: Option<SocketHandle>| -> bool {
                let handle = match handle { Some(h) => h, None => return false };
                let mut stack = stack_for(on_lo);
                match stack.as_mut() {
                    Some(s) => {
                        let socket = s.socket_set.get_mut::<tcp::Socket>(handle);
                        socket.is_active() && socket.state() == tcp::State::Established
                    }
                    None => false,
                }
            };
            let ev = if ready(false, main) || ready(true, lo) { POLLIN } else { 0 };
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
        // Pending-accept half-open connections are end A of a real connection
        // and carry the same `refs_a` count a connected end does — fork copies
        // one (see handle_fork_dup) — so they are released, not force-freed,
        // matching handle_close's PendingAccept arm.
        let mut unix_pending_close: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
        // (on_loopback, handle) — an INADDR_ANY listener contributes one of each.
        let mut inet_to_close: alloc::vec::Vec<(bool, SocketHandle)> = alloc::vec::Vec::new();
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
                SockState::InetConnected { socket_handle, lo, .. } => {
                    inet_to_close.push((lo, socket_handle));
                }
                SockState::InetListening { main, lo, .. } => {
                    if let Some(h) = main { inet_to_close.push((false, h)); }
                    if let Some(h) = lo   { inet_to_close.push((true,  h)); }
                }
                SockState::IcmpBound { socket_handle } => {
                    inet_to_close.push((false, socket_handle));
                }
                _ => {}
            }
        }
        drop(tbls);

        let mut conns = UNIX_CONNS.lock();
        let mut peer_hup = false;
        // Every in-flight fd orphaned below, released after UNIX_CONNS is
        // dropped — an orphan may itself be a socket (see `take_fds`).
        let mut orphans: alloc::vec::Vec<XferFd> = alloc::vec::Vec::new();
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
                    if c.closed_a && c.closed_b { orphans.append(&mut c.take_fds()); c.in_use = false; }
                }
            }
        }
        // Pending-accept half-open connections: release this holder's
        // reference, and only tear the connection down once the last one is
        // gone. Force-freeing here undid the fork-inheritance fix above — a
        // forked child that exits (an exec failure, say) would have destroyed
        // the connector its parent still holds, and vice versa.
        for ci in unix_pending_close {
            if ci < MAX_CONNS && conns[ci].in_use {
                let c = &mut conns[ci];
                c.refs_a = c.refs_a.saturating_sub(1);
                if c.refs_a == 0 {
                    orphans.append(&mut c.take_fds());
                    c.in_use = false;
                    c.seq = c.seq.wrapping_add(1);
                    peer_hup = true;
                }
            }
        }
        drop(conns);
        for x in orphans { xfer_drop(x); }

        for bi in bound_to_free { free_bound_idx(bi); }

        // Each stack in turn; the two locks are never held together.
        for on_lo in [false, true] {
            let mut stack = stack_for(on_lo);
            if let Some(ref mut s) = *stack {
                for (h_lo, handle) in inet_to_close.iter() {
                    if *h_lo == on_lo { s.socket_set.remove(*handle); }
                }
            }
        }

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
