//! Net server — smoltcp and AF_UNIX sockets.

#![no_std]

extern crate alloc;

pub mod nftables;
/// `/dev/netctl` (docs/servers/netctl.md).
pub mod netctl;
mod procnet;
pub mod rtnetlink;
mod nlsock;
mod nfnl;
mod iface;
mod ip6;
mod vifsel;
mod vif;
pub mod tundev;
pub mod tunbe;
mod framedev;
mod l2;
mod fwd;
mod fwd6;
pub mod netwatch;

use iface::{Sid, LO, MAX_STACKS};
/// Interface state for /dev/netctl: see `iface` (module docs).
pub use iface::{any_ipv4_address, IfSnapshot, snapshot as iface_snapshot, set_hook as set_iface_hook, set_admin as set_iface_admin};

use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};
use ipc::Message;
use spin::Mutex;
use smoltcp::iface::{Config, Interface, SocketSet, SocketHandle};
use smoltcp::socket::{tcp, udp, icmp, raw};
use smoltcp::time::Instant;
use core::sync::atomic::{AtomicBool, AtomicU64};
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
/// ioctl FIONREAD (arg1 = 0) / TIOCOUTQ (arg1 = 1) on a socket fd.
pub const NET_QUEUE_LEN:  u64 = 0x49;

const POLLIN:  u64 = 0x0001;
const POLLOUT: u64 = 0x0004;
const POLLERR: u64 = 0x0008;
const POLLHUP: u64 = 0x0010;
const POLLRDHUP: u64 = 0x2000;

/// `net.bcastnet=1` restores the 100 ms broadcast safety net (A/B and escape
/// hatch). 0 = not read yet, 1 = off, 2 = on.
static BCAST_NET: AtomicU8 = AtomicU8::new(0);

fn bcast_net() -> bool {
    let mut v = BCAST_NET.load(Ordering::Relaxed);
    if v == 0 {
        v = if drivers::nvidia_probe::cmdline_opt("net.bcastnet") == Some(1) { 2 } else { 1 };
        BCAST_NET.store(v, Ordering::Relaxed);
    }
    v == 2
}

/// What a UDP socket's shutdown adds to poll (Linux `datagram_poll`): SHUT_RD
/// is readable-at-EOF, both directions a hangup.
fn udp_shut_events(shut: u8) -> u64 {
    let mut ev = 0;
    if shut & SHUT_RD_BIT != 0 { ev |= POLLIN | POLLRDHUP; }
    if shut & (SHUT_RD_BIT | SHUT_WR_BIT) == SHUT_RD_BIT | SHUT_WR_BIT { ev |= POLLHUP; }
    ev
}

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

/// Start (ms+1) of the current tx_ready()==false episode; 0 = none. A held
/// episode longer than 2 s is logged once (pigem-bulk diagnosis).
static TX_HELD_SINCE: AtomicU64 = AtomicU64::new(0);
static TX_HELD_LOGGED: AtomicBool = AtomicBool::new(false);
fn tx_held_episode(id: usize, now_ms: i64, held: u64) {
    let since = TX_HELD_SINCE.load(Ordering::Relaxed);
    if since == 0 { TX_HELD_SINCE.store(now_ms as u64 + 1, Ordering::Relaxed); TX_HELD_LOGGED.store(false, Ordering::Relaxed); return; }
    if now_ms as u64 + 1 - since >= 2000 && !TX_HELD_LOGGED.swap(true, Ordering::Relaxed) {
        extern "C" { fn arch_serial_putc(c: u8); }
        let _ = (id, held);
        for &b in b"netd: tx held >2 s (see tx_held in /proc/net)\n" { unsafe { arch_serial_putc(b); } }
    }
}

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
const SO_SNDBUF:        usize = 7;
const SO_PEERCRED:      usize = 17;
const SO_PASSCRED:      usize = 16;
const SCM_CREDENTIALS:  i32   = 2;
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
pub const AF_INET6:   usize = 10;
pub const AF_NETLINK: usize = 16;
/// connect(AF_UNSPEC): dissolve a UDP socket's association.
const AF_UNSPEC:      usize = 0;
pub const SOCK_STREAM: usize = 1;
pub const SOCK_DGRAM:  usize = 2;
pub const SOCK_RAW:    usize = 3;
pub const SOCK_SEQPACKET: usize = 5;
/// EOPNOTSUPP. `unix_listen` answers this — not EINVAL — for a socket type that
/// cannot accept, and it checks the type *before* the address, so even a bound
/// AF_UNIX SOCK_DGRAM socket gets EOPNOTSUPP.
const EOPNOTSUPP: i32 = 95;

pub const IPPROTO_ICMP: usize = 1;
pub const IPPROTO_ICMPV6: usize = 58;
/// Level 58 (SOL_ICMPV6) option: the 256-bit block mask of a raw ICMPv6 socket.
const ICMP6_FILTER: usize = 1;
const IPV6_UNICAST_HOPS: usize = 16;
const IPV6_MULTICAST_HOPS: usize = 18;
const IPV6_RECVERR: usize = 25;
const IPV6_MTU_DISCOVER: usize = 23;
const IPV6_MTU: usize = 24;
const IPV6_RECVPKTINFO: usize = 49;
const IPV6_PKTINFO: usize = 50;
const IPV6_RECVHOPLIMIT: usize = 51;
const IPV6_HOPLIMIT: usize = 52;
const IPV6_RECVTCLASS: usize = 66;
const IPV6_TCLASS: usize = 67;
/// SOL_TCP and the TCP-level options this server implements (Linux values).
const IPPROTO_TCP:   usize = 6;
const TCP_NODELAY:   usize = 1;
const TCP_INFO:      usize = 11;
const TCP_INFO_LEN:  usize = 104;
const TCP_KEEPIDLE:  usize = 4;
const TCP_KEEPINTVL: usize = 5;
const TCP_KEEPCNT:   usize = 6;
const SO_KEEPALIVE:  usize = 9;
const SO_BROADCAST:  usize = 6;
/// The timeval forms of SO_RCVTIMEO/SO_SNDTIMEO (`_OLD`) and the 64-bit time
/// ones (`_NEW`); on a 64-bit ABI both take the same 16-byte struct.
const SO_RCVTIMEO_OLD: usize = 20;
const SO_SNDTIMEO_OLD: usize = 21;
const SO_RCVTIMEO_NEW: usize = 66;
const SO_SNDTIMEO_NEW: usize = 67;
const SO_REUSEPORT:  usize = 15;
const SOL_UDP:       usize = 17;
const UDP_SEGMENT:   usize = 103;
const UDP_GRO:       usize = 104;
const IPPROTO_IP:    usize = 0;
const IPPROTO_IPV6:  usize = 41;
const IPV6_V6ONLY:   usize = 26;
const IP_TOS:        usize = 1;
const IP_TTL:        usize = 2;
const IP_PKTINFO:    usize = 8;
const IP_MTU_DISCOVER: usize = 10;
const IP_RECVERR:    usize = 11;
const IP_MTU:        usize = 14;
const IP_RECVTTL:    usize = 12;
const IP_RECVTOS:    usize = 13;
const IP_MULTICAST_IF:   usize = 32;
const IP_MULTICAST_TTL:  usize = 33;
const IP_MULTICAST_LOOP: usize = 34;
const IP_ADD_MEMBERSHIP: usize = 35;
const IP_DROP_MEMBERSHIP: usize = 36;
const IP_MULTICAST_ALL:  usize = 49;

/// First socket *handle*: `SOCK_FD_BASE + slot` names slot `slot` of a
/// process's socket table. Since lane fdmax a handle is not a descriptor: a
/// process reaches its sockets through `vfs::VnodeKind::SockAlias` entries of
/// its one fd table, which name these handles, and the kernel translates a
/// descriptor to its handle before routing a call here. (Sockets used to
/// have descriptor numbers of their own, [0x200, 0x3C7).) Every function of
/// this server that takes an `fd` takes a handle.
pub const SOCK_FD_BASE: usize = vfs::SOCK_HANDLE_BASE;
/// One past the last socket handle.
pub const SOCK_FD_END: usize = vfs::EPOLL_HANDLE_BASE;

/// Per-process socket fd cap. Raised 16→512 for a COSMIC-class workload (a
/// compositor holds a socket per client + the bus + internal socketpairs).
/// 510, not 512: that keeps a whole `ProcSockTable` (pid + flag + entries)
/// within 32 KiB, one order-3 buddy block instead of an order-4 one.
/// 510 -> 455 (lane epollofd, 2026-10-01): each entry grew 64 -> 72 bytes for
/// its open-file-description id (`SockEntry::ofd`), and 455 is what still
/// fits one order-3 block.
///
/// 455 -> grown on demand (lane fdmax): `ProcSockTable::socks` is a heap
/// vector, so a process's sockets are bounded by its RLIMIT_NOFILE (each one
/// is an fd-table entry) rather than by one fixed-size table.
const MAX_SOCKS:   usize = SOCK_FD_END - SOCK_FD_BASE;
/// Connection-pair pool. Raised 32→256 (K1 acceptance: 64 socketpairs + 32
/// listener connections concurrently; headroom for the desktop session).
/// 256 -> 512 (lane term20, 2026-09-27): a COSMIC session holds about 110 and
/// each cosmic-term adds 5 (its Wayland connection, its shell's and its own
/// socketpairs), so 20 terminals used 209. A `UnixConn` carries its two 4 KiB
/// rings inline and the pool is in .data (Vec::new is not all-zero), so this
/// costs about 4 MiB; 512 matches what the VFS fd table lets one compositor
/// serve (about 50 clients).
/// 512 -> grown on demand up to 8192 (lane pools, 2026-10-19): the pool is a
/// heap `Vec` that only ever grows (a slot index is a connection's identity,
/// so slots are reused, never moved out), and the rings are on the heap
/// already, so an idle slot costs ~200 bytes instead of a fixed 4 MiB table.
/// A full pool answers ENOMEM, once reported on the console.
const MAX_CONNS:   usize = 8192;

/// One serial line, the first time the global connection pool is empty.
fn report_conns_full() {
    static REPORTED: ::core::sync::atomic::AtomicBool = ::core::sync::atomic::AtomicBool::new(false);
    if REPORTED.swap(true, ::core::sync::atomic::Ordering::Relaxed) { return; }
    extern "C" { fn arch_serial_putc(c: u8); }
    for &b in b"\n[NET] unix connection pool FULL (net::MAX_CONNS) -- global, not this process's fd limit\n" {
        unsafe { arch_serial_putc(b); }
    }
}
/// Bound-address pool (abstract + pathname listeners). Raised 16→512.
const MAX_BOUND:   usize = 512;
/// AF_UNIX per-direction buffer. It starts at RING_INIT and grows on demand
/// (linearised, doubling) up to RING_MAX, and drops back to nothing once it
/// drains. RING_MAX is Linux's default `net.core.wmem_default` (212992), the
/// send budget a unix stream really has there. A fixed 4 KiB ring filled in
/// ~100 ms under cosmic-workspaces' ~40 KB/s capture loop whenever
/// cosmic-comp was briefly busy, and the client's `flush().unwrap()` then
/// panicked on EAGAIN (lane wwoverview, 2026-09-28).
const RING_INIT:   usize = 4096;
const RING_MAX:    usize = 212992;
/// Largest AF_UNIX datagram / seqpacket record: Linux's `sk_sndbuf - 32`
/// with the default send buffer (RING_MAX). Larger sends are EMSGSIZE.
const UNIX_DGRAM_MAX: usize = RING_MAX - 32;
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
/// NET_BIND / NET_CONNECT args 3 and 4: the host-absolute (backing) path the
/// kernel resolved a pathname AF_UNIX address to — the caller's cwd, chroot
/// and alias mounts applied, exactly as open(2) resolves a path — as a
/// kernel pointer and length. Absent (len 0) for other families, abstract
/// names and in-kernel callers; the raw sun_path is then used as given.
/// sun_path itself stays what the caller wrote: it is what getsockname and
/// the peer's accept report back, as on Linux.
fn host_path_arg(msg: &Message) -> Option<&'static [u8]> {
    let (ptr, len) = (arg(msg, 3) as usize, arg(msg, 4) as usize);
    if ptr == 0 || len == 0 { return None; }
    // SAFETY: a kernel buffer owned by the calling syscall, which stays
    // blocked in this call for the whole request (see the kernel's KPath).
    Some(unsafe { core::slice::from_raw_parts(ptr as *const u8, len) })
}

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
/// AF_UNIX socket, an inet socket: TCP, UDP, ICMP, listeners), 0 for
/// level-only sockets (AF_UNIX listeners, unbound TCP) so the epoll layer
/// treats them level-triggered. Mirrors vfs::poll_reply plus the has-seq flag.
fn net_poll_reply(revents: u64, seq: Option<u64>, tag: Option<sched::PollTag>) -> Message {
    let mut m = make_reply(revents as i64);
    if let Some(s) = seq {
        m.data[8..16].copy_from_slice(&s.to_le_bytes());
        m.data[16] = 1;
    }
    // Targeted-wake tag (see sched::poll_tag): data[24..32] + valid flag at
    // data[32]. `None` leaves data[32]==0 so the kernel prober treats the
    // socket as broadcast (POLL_TAG_ALL) — inet has no per-socket identity that
    // matches its (broadcast) wake_poll, so it always reports None here.
    // A connected AF_UNIX end also carries its exact keys (data[33..64], see
    // sched::PollSet), so a parked poller is not woken by every other
    // connection whose tag shares a bit with one of its own.
    if let Some(t) = tag {
        m.data[24..32].copy_from_slice(&t.bits.to_le_bytes());
        m.data[32] = 1;
        if t.n > 0 { sched::poll_reply_put_keys(&mut m.data, &t.keys[..t.n as usize]); }
        else if t.foreign { m.data[33] = sched::POLL_REPLY_FOREIGN; }
    }
    m
}

// ── Unix connection ring buffers ──────────────────────────────────────────────

struct UnixRing {
    buf:   alloc::vec::Vec<u8>,
    rpos:  usize,
    wpos:  usize,
    count: usize,
    /// Monotonic total bytes ever written / read on this direction. Unlike
    /// rpos/wpos these never wrap, so an SCM_RIGHTS fd batch can be pinned to
    /// the absolute stream offset of the byte it rides with (see
    /// `PendingFdBatch`) and delivered on the recv that consumes that byte.
    wtotal: u64,
    rtotal: u64,
    /// Datagrams queued (datagram/seqpacket rings only): each is charged
    /// `DGRAM_OVERHEAD` on top of its bytes against the sender's SO_SNDBUF.
    nrec: usize,
    /// A writer / reader is copying into / out of `buf` with UNIX_CONNS
    /// released (`reserve_write` / `reserve_read`, lane ipcfast). While set,
    /// `buf` must not be reallocated or freed, and nobody else may write /
    /// read this direction; the copier clears it in its commit.
    wbusy: bool,
    rbusy: bool,
}

/// A span of a ring handed to a copier that runs without UNIX_CONNS: `n`
/// bytes starting at `pos`, wrapping at `cap`. `buf` stays put while the
/// matching busy flag is set (see `UnixRing::wbusy`).
#[derive(Clone, Copy)]
struct RingSpan { base: *mut u8, cap: usize, pos: usize, n: usize }

impl RingSpan {
    /// Copy user/kernel iovecs into the span (writer side). Returns the bytes
    /// copied and whether a fault ended the copy early.
    fn fill(&self, iovs: &[(usize, usize)]) -> (usize, bool) {
        self.copy(iovs, true)
    }

    /// Copy the span out into iovecs (reader side).
    fn drain(&self, iovs: &[(usize, usize)]) -> (usize, bool) {
        self.copy(iovs, false)
    }

    fn copy(&self, iovs: &[(usize, usize)], into_ring: bool) -> (usize, bool) {
        let mut done = 0usize;
        for &(base, len) in iovs {
            let mut off = 0usize;
            while off < len && done < self.n {
                let pos = (self.pos + done) % self.cap;
                let chunk = (len - off).min(self.n - done).min(self.cap - pos);
                let left = unsafe {
                    if into_ring {
                        sched::uaccess::copy_raw(self.base.add(pos), (base + off) as *const u8, chunk)
                    } else {
                        sched::uaccess::copy_raw((base + off) as *mut u8, self.base.add(pos), chunk)
                    }
                };
                done += chunk - left;
                off += chunk - left;
                if left != 0 { return (done, true); }
            }
            if done >= self.n { break; }
        }
        (done, false)
    }
}

/// What one queued AF_UNIX datagram costs beyond its payload when measured
/// against SO_SNDBUF: Linux charges an skb's truesize (the sk_buff and its
/// shared info, ~576 bytes on 64-bit), so a socket fills up after a few
/// hundred small datagrams, not after RING_MAX / 4 of them.
const DGRAM_OVERHEAD: usize = 576;

impl UnixRing {
    const fn new() -> Self {
        Self { buf: alloc::vec::Vec::new(), rpos: 0, wpos: 0, count: 0, wtotal: 0, rtotal: 0, nrec: 0, wbusy: false, rbusy: false }
    }

    /// What the queued datagrams cost against SO_SNDBUF (see `DGRAM_OVERHEAD`).
    fn dgram_charge(&self) -> usize { self.count + self.nrec * DGRAM_OVERHEAD }

    /// Append up to `len` bytes from `data` — the caller's buffer, user or
    /// kernel memory. Callers hold UNIX_CONNS, so the copy is the
    /// fault-tolerant one: a page fault it cannot resolve ends the copy
    /// instead of killing the caller with the lock held (every later unix
    /// socket operation would then spin on it forever). Only bytes actually
    /// copied are committed. `Err(EFAULT)` when a fault stopped the copy
    /// before its first byte.
    fn write(&mut self, data: *const u8, len: usize) -> Result<usize, i32> {
        if self.wbusy { return Ok(0); }
        if !self.rbusy { self.grow(len); }
        let cap = self.buf.len();
        let n = len.min(cap - self.count);
        let mut done = 0usize;
        let mut faulted = false;
        while done < n {
            let chunk = (n - done).min(cap - self.wpos);
            let left = unsafe {
                sched::uaccess::copy_raw(self.buf.as_mut_ptr().add(self.wpos), data.add(done), chunk)
            };
            let copied = chunk - left;
            self.wpos = (self.wpos + copied) % cap;
            done += copied;
            if left != 0 { faulted = true; break; }
        }
        self.count += done;
        self.wtotal += done as u64;
        if faulted && done == 0 { Err(-14) } else { Ok(done) }
    }

    /// Consume up to `len` bytes into `data` (user or kernel memory), same
    /// fault rules as [`write`](Self::write): only bytes that reached the
    /// caller are consumed, and `Err(EFAULT)` means none did.
    fn read(&mut self, data: *mut u8, len: usize) -> Result<usize, i32> {
        if self.rbusy { return Ok(0); }
        let n = len.min(self.count);
        let cap = self.buf.len();
        let mut done = 0usize;
        let mut faulted = false;
        while done < n {
            let chunk = (n - done).min(cap - self.rpos);
            let left = unsafe {
                sched::uaccess::copy_raw(data.add(done), self.buf.as_ptr().add(self.rpos), chunk)
            };
            let copied = chunk - left;
            self.rpos = (self.rpos + copied) % cap;
            done += copied;
            if left != 0 { faulted = true; break; }
        }
        self.count -= done;
        self.rtotal += done as u64;
        self.shrink_if_drained();
        if faulted && done == 0 { Err(-14) } else { Ok(done) }
    }

    /// Drained: give a grown buffer back (the next write re-allocates) —
    /// unless a writer is copying into it right now.
    fn shrink_if_drained(&mut self) {
        if self.count == 0 && self.buf.len() > RING_INIT && !self.wbusy && !self.rbusy {
            self.buf = alloc::vec::Vec::new();
            self.rpos = 0;
            self.wpos = 0;
        }
    }

    /// Writer: claim room for up to `len` bytes to be copied with
    /// UNIX_CONNS released. `None` while another writer holds this
    /// direction; a span with `n == 0` when the ring is full.
    fn reserve_write(&mut self, len: usize) -> Option<RingSpan> {
        if self.wbusy { return None; }
        if !self.rbusy { self.grow(len); }
        let cap = self.buf.len();
        let n = len.min(cap - self.count);
        if n > 0 { self.wbusy = true; }
        Some(RingSpan { base: self.buf.as_mut_ptr(), cap, pos: self.wpos, n })
    }

    /// Writer: publish `done` bytes of a span from `reserve_write`.
    fn commit_write(&mut self, sp: &RingSpan, done: usize) {
        if sp.n == 0 { return; }
        self.wbusy = false;
        self.wpos = (sp.pos + done) % sp.cap;
        self.count += done;
        self.wtotal += done as u64;
        self.shrink_if_drained();
    }

    /// Reader: claim up to `len` queued bytes to be copied out with
    /// UNIX_CONNS released. `None` while another reader holds this direction.
    fn reserve_read(&mut self, len: usize) -> Option<RingSpan> {
        if self.rbusy { return None; }
        let n = len.min(self.count);
        if n > 0 { self.rbusy = true; }
        Some(RingSpan { base: self.buf.as_mut_ptr(), cap: self.buf.len(), pos: self.rpos, n })
    }

    /// Reader: consume `done` bytes of a span from `reserve_read`.
    fn commit_read(&mut self, sp: &RingSpan, done: usize) {
        if sp.n == 0 { return; }
        self.rbusy = false;
        self.rpos = (sp.pos + done) % sp.cap;
        self.count -= done;
        self.rtotal += done as u64;
        self.shrink_if_drained();
    }

    /// Copy up to `len` queued bytes, starting `skip` bytes past the read
    /// position, without consuming them (MSG_PEEK). Same fault rules as
    /// [`read`](Self::read). `mark`/`reset` around a `read` cannot do this:
    /// a read that drains the ring also frees its buffer.
    fn peek(&self, skip: usize, data: *mut u8, len: usize) -> Result<usize, i32> {
        if skip >= self.count { return Ok(0); }
        let n = len.min(self.count - skip);
        let cap = self.buf.len();
        let mut pos = (self.rpos + skip) % cap;
        let mut done = 0usize;
        while done < n {
            let chunk = (n - done).min(cap - pos);
            let left = unsafe {
                sched::uaccess::copy_raw(data.add(done), self.buf.as_ptr().add(pos), chunk)
            };
            let copied = chunk - left;
            pos = (pos + copied) % cap;
            done += copied;
            if left != 0 { return if done == 0 { Err(-14) } else { Ok(done) }; }
        }
        Ok(done)
    }

    /// The datagram at the head, copied (truncated to `len`) and left
    /// queued: MSG_PEEK on a unix datagram socket. `(copied, full length)`.
    fn peek_dgram(&self, data: *mut u8, len: usize) -> Result<Option<(usize, usize)>, i32> {
        if self.count < 4 { return Ok(None); }
        let mut len_bytes = [0u8; 4];
        let _ = self.peek(0, len_bytes.as_mut_ptr(), 4);
        let full = u32::from_le_bytes(len_bytes) as usize;
        let to_read = full.min(len);
        if to_read > 0 && self.peek(4, data, to_read) != Ok(to_read) { return Err(-14); }
        Ok(Some((to_read, full)))
    }

    /// Bytes that can still be queued before a send would see EAGAIN.
    fn space(&self) -> usize {
        RING_MAX - self.count
    }

    /// Make room for up to `want` more bytes (bounded by RING_MAX), growing
    /// the buffer and linearising its contents. An allocation failure keeps
    /// the current buffer; the write then places what fits (or EAGAINs).
    fn grow(&mut self, want: usize) {
        let need = (self.count + want).min(RING_MAX);
        let cap = self.buf.len();
        if need <= cap { return; }
        let mut new_cap = cap.max(RING_INIT);
        while new_cap < need { new_cap = new_cap.saturating_mul(2); }
        let new_cap = new_cap.min(RING_MAX);
        let mut nb: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        if nb.try_reserve_exact(new_cap).is_err() { return; }
        // No zero-fill (it was a memset of up to RING_MAX per grow, and every
        // drained ring re-grows): ring bytes are only ever read after a write
        // put them there. Copy the queued bytes out in stream order.
        let first = self.count.min(cap.saturating_sub(self.rpos));
        let rest = self.count - first;
        unsafe {
            let dst = nb.as_mut_ptr();
            if first > 0 { core::ptr::copy_nonoverlapping(self.buf.as_ptr().add(self.rpos), dst, first); }
            if rest > 0 { core::ptr::copy_nonoverlapping(self.buf.as_ptr(), dst.add(first), rest); }
            nb.set_len(new_cap);
        }
        self.buf = nb;
        self.rpos = 0;
        self.wpos = self.count % new_cap;
    }

    /// Ring position, for undoing a datagram that faulted half-way.
    fn mark(&self) -> (usize, usize, usize, u64, u64) {
        (self.rpos, self.wpos, self.count, self.wtotal, self.rtotal)
    }
    fn reset(&mut self, m: (usize, usize, usize, u64, u64)) {
        (self.rpos, self.wpos, self.count, self.wtotal, self.rtotal) = m;
    }

    /// Queue one datagram, whole or not at all: `Ok(None)` when it does not
    /// fit, `Err(EFAULT)` when its bytes could not all be read.
    /// `sndbuf` is the sender's SO_SNDBUF (Linux's doubled value).
    fn write_dgram(&mut self, data: *const u8, len: usize, sndbuf: usize) -> Result<Option<usize>, i32> {
        // A datagram that could never fit, even into an empty queue, is
        // EMSGSIZE (Linux `unix_dgram_sendmsg`: `len > sk_sndbuf - 32`), not
        // EAGAIN: a blocking sender retries EAGAIN forever (greetd's
        // parent-to-worker datagram did, back when the ring was 4 KiB).
        if len > UNIX_DGRAM_MAX || len > sndbuf.saturating_sub(32) {
            return Err(-90);
        }
        if self.space() < 4 + len {
            return Ok(None);
        }
        // Linux's sock_alloc_send_pskb: the send waits while what the socket
        // has in flight already reaches SO_SNDBUF (it may overshoot by one).
        if self.nrec > 0 && self.dgram_charge() >= sndbuf {
            return Ok(None);
        }
        self.grow(4 + len);
        if self.buf.len() - self.count < 4 + len {
            return Ok(None); // could not grow
        }
        let m = self.mark();
        let len_bytes = (len as u32).to_le_bytes();
        let _ = self.write(len_bytes.as_ptr(), 4);
        if self.write(data, len) != Ok(len) {
            self.reset(m);
            return Err(-14);
        }
        self.nrec += 1;
        Ok(Some(len))
    }

    /// Dequeue one datagram (truncated to `len`): `Ok(None)` when none is
    /// queued, `Err(EFAULT)` — datagram left queued — when it could not be
    /// stored.
    /// `(copied, full length)`.
    fn read_dgram(&mut self, data: *mut u8, len: usize) -> Result<Option<(usize, usize)>, i32> {
        if self.count < 4 {
            return Ok(None);
        }
        let m = self.mark();
        let mut len_bytes = [0u8; 4];
        let _ = self.read(len_bytes.as_mut_ptr(), 4);
        let dgram_len = u32::from_le_bytes(len_bytes) as usize;

        let to_read = dgram_len.min(len);
        if to_read > 0 && self.read(data, to_read) != Ok(to_read) {
            self.reset(m);
            return Err(-14);
        }
        if dgram_len > to_read {
            let discard = dgram_len - to_read;
            let mut discard_buf = [0u8; 128];
            let mut remaining = discard;
            while remaining > 0 {
                let chunk = remaining.min(128);
                let _ = self.read(discard_buf.as_mut_ptr(), chunk);
                remaining -= chunk;
            }
        }
        self.nrec = self.nrec.saturating_sub(1);
        Ok(Some((to_read, dgram_len)))
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
#[derive(Clone)]
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
///
/// `end_byte` is one past the last byte that sendmsg wrote. A read that
/// consumes any byte of `seq_byte..end_byte` takes the fds and stops at
/// `end_byte`, so bytes sent after them never come back in the same call.
/// Linux works the same way: unix_stream_read_generic detaches the fds of an
/// skb and breaks out once that skb is used up.
struct PendingFdBatch {
    seq_byte: u64,
    end_byte: u64,
    fds:      alloc::vec::Vec<XferFd>,
    /// SCM_CREDENTIALS captured at send time (host pid/uid/gid).
    cred:     Option<Ucred>,
}

/// In-flight copies of AF_UNIX sockets that can take part in a reference
/// cycle: connected ends and listeners (`XferFd::Sock` in either state).
/// They are counted from `xfer_export` or a MSG_PEEK clone to the matching
/// `xfer_import` or `xfer_drop`. The counter is only a trigger. While it is
/// zero no queue holds such a socket, no cycle can exist, and `unix_gc`
/// returns at once.
static INFLIGHT_SOCKS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// The listeners among those. The collector only scans the per-process
/// tables for embryonic connections while one is in flight, because that
/// scan is the expensive part.
static INFLIGHT_LISTENERS: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Count one in-flight copy of `x` (`up`) or retire one.
fn inflight_adj(x: &XferFd, up: bool) {
    use core::sync::atomic::Ordering::Relaxed;
    let listener = match x {
        XferFd::Sock(SockEntry { state: SockState::UnixConnected { .. }, .. }) => false,
        XferFd::Sock(SockEntry { state: SockState::UnixListening { .. }, .. }) => true,
        _ => return,
    };
    if up {
        INFLIGHT_SOCKS.fetch_add(1, Relaxed);
        if listener { INFLIGHT_LISTENERS.fetch_add(1, Relaxed); }
    } else {
        INFLIGHT_SOCKS.fetch_sub(1, Relaxed);
        if listener { INFLIGHT_LISTENERS.fetch_sub(1, Relaxed); }
    }
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
    /// SO_PASSCRED of each end: a sender attaches SCM_CREDENTIALS for it.
    pc_a: bool,
    pc_b: bool,
    /// A pending ECONNRESET for end A (Linux `sk_err`): its listener was
    /// closed before accepting it (`unix_listener_gone`). Reported as
    /// POLLERR while set; the first recv that finds no data answers
    /// ECONNRESET and clears it, later ones read EOF.
    reset_a: bool,
    /// The listener (`BoundPath::sock_id`) this connection waits on while
    /// nobody has accept()ed it; 0 once accepted (or never pending). The
    /// backlog is the set of connections carrying a listener's id, so a
    /// connection whose connector closed before the accept is still in it
    /// (Linux keeps the embryo on the listener's queue: accept returns it
    /// and reads see the data sent before the close, then EOF).
    pending_sid: u64,
    /// Arrival order on that backlog (`PENDING_ORD`): accept() takes the
    /// oldest, as Linux's queue does.
    pending_ord: u64,
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
    /// A copier is inside one of the rings with UNIX_CONNS released: the
    /// slot must not be reset or reused until it commits.
    fn copying(&self) -> bool {
        self.ring_ab.wbusy || self.ring_ab.rbusy || self.ring_ba.wbusy || self.ring_ba.rbusy
    }

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
            pc_a: false,
            pc_b: false,
            reset_a: false,
            pending_sid: 0,
            pending_ord: 0,
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

    /// The fd batches waiting to be received by end `is_a` (what the peer
    /// sent it). Linux calls this the socket's receive queue.
    fn rx_fdq(&mut self, is_a: bool) -> &mut alloc::vec::Vec<PendingFdBatch> {
        if is_a { &mut self.fdq_ba } else { &mut self.fdq_ab }
    }

    /// Lift every fd still queued for end `is_a`. Same release rule as
    /// `take_fds`: the caller passes the result to `xfer_drop` after it drops
    /// UNIX_CONNS.
    #[must_use = "the lifted descriptors must be passed to xfer_drop"]
    fn take_rx_fds(&mut self, is_a: bool) -> alloc::vec::Vec<XferFd> {
        let mut out = alloc::vec::Vec::new();
        for b in self.rx_fdq(is_a).drain(..) { out.extend(b.fds); }
        out
    }

    /// Drop one reference to end `is_a`. On the last one the end is closed.
    /// The fds still queued for it are lifted out, because nobody can receive
    /// them any more. Linux does the same: unix_release_sock purges the
    /// receive queue. Without this, an end sent to a peer that closed without
    /// reading stayed alive as long as the sender's end did. When both ends
    /// are closed the connection is freed. Returns (end closed, lifted fds).
    /// The fds go to `xfer_drop` once UNIX_CONNS is released.
    fn end_put(&mut self, is_a: bool) -> (bool, alloc::vec::Vec<XferFd>) {
        let refs = if is_a { &mut self.refs_a } else { &mut self.refs_b };
        *refs = refs.saturating_sub(1);
        if *refs != 0 { return (false, alloc::vec::Vec::new()); }
        if is_a { self.closed_a = true; } else { self.closed_b = true; }
        self.seq = self.seq.wrapping_add(1);
        let mut orphans = self.take_rx_fds(is_a);
        if self.closed_a && self.closed_b {
            orphans.append(&mut self.take_fds());
            self.in_use = false;
        }
        (true, orphans)
    }
}

static UNIX_CONNS: sched::lockwatch::TrackedMutex<alloc::vec::Vec<UnixConn>> =
    sched::lockwatch::TrackedMutex::new(sched::lockwatch::L_UNIX_CONNS, alloc::vec::Vec::new());

/// A free connection slot, growing the pool when every slot is in use.
/// `None` once `MAX_CONNS` slots exist and all are in use (or the heap is
/// out of memory).
fn unix_conn_slot(conns: &mut alloc::vec::Vec<UnixConn>) -> Option<usize> {
    if let Some(i) = conns.iter().position(|c| !c.in_use && !c.copying()) { return Some(i); }
    if conns.len() >= MAX_CONNS { return None; }
    let want = (conns.len().max(32)).min(MAX_CONNS - conns.len());
    if conns.try_reserve(want).is_err() { return None; }
    conns.push(UnixConn::new());
    Some(conns.len() - 1)
}

/// Arrival order of connections on AF_UNIX backlogs (`UnixConn::pending_ord`).
static PENDING_ORD: AtomicU64 = AtomicU64::new(1);

/// The oldest connection waiting on the backlog of listener `sock_id`.
fn unix_backlog_head(conns: &[UnixConn], sock_id: u64) -> Option<usize> {
    if sock_id == 0 { return None; }
    conns.iter().enumerate()
        .filter(|(_, c)| c.in_use && c.pending_sid == sock_id)
        .min_by_key(|(_, c)| c.pending_ord)
        .map(|(i, _)| i)
}

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
    /// Network namespace of the binder: abstract names are scoped by it.
    netns:      u16,
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
               sock_id: 0, is_abstract: false, netns: 0, refs: 0,
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
    /// A listening TCP socket: listener `lid` on every stack it listens on
    /// (`listen::stacks_for`), each with its own LISTEN socket and accept
    /// queue held by that stack (see `listen`). The state never changes
    /// after listen(), so holders sharing it have nothing to publish.
    /// `local` is the bound endpoint, kept for getsockname.
    InetListening { lid: u32, local: IpEndpoint },
    /// `sid` says which stack owns `socket_handle` — see `stack_for`.
    InetConnected { socket_handle: SocketHandle, remote_endpoint: Option<IpEndpoint>, sid: Sid },
    IcmpUnbound,
    IcmpBound { socket_handle: SocketHandle, sid: Sid },
}

#[derive(Clone)]
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
    /// The open file description (`vfs::ofd`): new at socket/socketpair/
    /// accept, shared by dup/fork/SCM_RIGHTS copies, one reference per entry.
    ofd:        u32,
    /// This slot backs a VFS-range descriptor (`dup2(sock, 3)`; see
    /// `vfs::VnodeKind::SockAlias`). Its own number is not a descriptor the
    /// process holds, so a user `close()` of it answers EBADF: Firefox's
    /// child-side fd sweep must not tear down the socket it just dup2'd. Kept
    /// by fork, set or cleared by dup, cleared on SCM_RIGHTS receipt. A field
    /// (it fits the entry's padding after `ofd`), not a per-table bitmap: the
    /// table is at its 32 KiB budget.
    hidden:     bool,
    /// A TCP connect() whose outcome the caller has not been told yet. Set by
    /// connect(), cleared once the handshake is seen to complete or fail; a
    /// connection that ends in Closed while it is still set was refused,
    /// which is what SO_ERROR, POLLERR and send/recv report.
    connecting: bool,
    /// Network namespace captured at socket() (Linux sock_net): every later
    /// op uses this, never the calling task's. Travels with SCM_RIGHTS copies.
    /// Counted reference: keeps the namespace id from being freed and reused
    /// while this socket (or an in-flight copy of it) exists.
    ns:         sched::ns::NsRef,
}

impl SockEntry {
    fn netns(&self) -> u16 { self.ns.id() }
    const fn empty() -> Self {
        Self { state: SockState::None, in_use: false, bound_port: 0, domain: 0,
               sock_type: 0, cloexec: false, nonblock: false, reuseaddr: false, ofd: 0, hidden: false,
               connecting: false, ns: sched::ns::NsRef::INIT }
    }
}

struct ProcSockTable {
    pid:    u32,
    /// Slot `i` is handle `SOCK_FD_BASE + i`. Grows (until the table is
    /// reset) when no slot is free; an index past the end is simply not open.
    socks:  alloc::vec::Vec<SockEntry>,
    in_use: bool,
}

impl ProcSockTable {
    fn is_hidden(&self, slot: usize) -> bool {
        slot < self.socks.len() && self.socks[slot].hidden
    }

    fn set_hidden(&mut self, slot: usize, on: bool) {
        if slot < self.socks.len() { self.socks[slot].hidden = on; }
    }

    /// Forget every entry (and the memory they took).
    fn reset(&mut self) {
        self.pid = 0;
        self.in_use = false;
        self.socks = alloc::vec::Vec::new();
    }

    /// A free slot, growing the table by one when none is.
    fn alloc(&mut self) -> Option<usize> {
        if let Some(i) = self.socks.iter().position(|s| !s.in_use) { return Some(i); }
        if self.socks.len() >= MAX_SOCKS || self.socks.try_reserve(1).is_err() { return None; }
        self.socks.push(SockEntry::empty());
        Some(self.socks.len() - 1)
    }
}

/// The per-process socket tables, each allocated on first use and then kept
/// for reuse: a freed table is only marked `!in_use`. A heap vector of boxes,
/// so neither the image nor this struct is sized by the process limit.
///
/// The k-th table `iter()` yields is `self[k]` — which is what lets the call
/// sites keep their `iter().position(..)` + index pattern.
struct SockTables {
    slots: alloc::vec::Vec<TableBox>,
    /// Canonical state of each inet socket changed while shared, by open
    /// file description (see "Shared inet sockets"); removed by the last put.
    inet:  alloc::collections::BTreeMap<u32, InetShared>,
}

type TableBox = alloc::boxed::Box<ProcSockTable>;
type SockIter<'a> = ::core::iter::Map<::core::slice::Iter<'a, TableBox>, fn(&TableBox) -> &ProcSockTable>;
type SockIterMut<'a> = ::core::iter::Map<::core::slice::IterMut<'a, TableBox>, fn(&mut TableBox) -> &mut ProcSockTable>;

impl SockTables {
    const fn new() -> Self { Self { slots: alloc::vec::Vec::new(), inet: alloc::collections::BTreeMap::new() } }

    // Named iterator types, not `impl Iterator`: an opaque type may have a
    // destructor, which would stretch every `match tbls.iter().find(..)`
    // borrow to the end of the statement and break the call sites.
    fn iter(&self) -> SockIter<'_> {
        fn deref(b: &TableBox) -> &ProcSockTable { b }
        self.slots.iter().map(deref as fn(&TableBox) -> &ProcSockTable)
    }

    fn iter_mut(&mut self) -> SockIterMut<'_> {
        fn deref(b: &mut TableBox) -> &mut ProcSockTable { b }
        self.slots.iter_mut().map(deref as fn(&mut TableBox) -> &mut ProcSockTable)
    }

    /// A free table, allocating one if every allocated table is live.
    fn claim(&mut self) -> Option<usize> {
        if let Some(pos) = self.iter().position(|t| !t.in_use) { return Some(pos); }
        if self.slots.try_reserve(1).is_err() { return None; }
        self.slots.push(alloc::boxed::Box::new(ProcSockTable { pid: 0, socks: alloc::vec::Vec::new(), in_use: false }));
        Some(self.slots.len() - 1)
    }
}

impl ::core::ops::Index<usize> for SockTables {
    type Output = ProcSockTable;
    fn index(&self, i: usize) -> &ProcSockTable { &self.slots[i] }
}

impl ::core::ops::IndexMut<usize> for SockTables {
    fn index_mut(&mut self, i: usize) -> &mut ProcSockTable { &mut self.slots[i] }
}

static SOCK_TABLES: sched::lockwatch::TrackedRwLock<SockTables> =
    sched::lockwatch::TrackedRwLock::new(sched::lockwatch::L_SOCK_TABLES, SockTables::new());

// ── Per-description socket options ──────────────────────────────────────────

/// The options Linux keeps on the socket and this server has to remember
/// before there is a smoltcp socket to hold them: TCP_NODELAY and the
/// keepalive set (SO_KEEPALIVE, TCP_KEEPIDLE/INTVL/CNT). Apps set NODELAY
/// right after socket() (NSPR, curl), and an accepted socket inherits the
/// listener's, so "apply it to the connected smoltcp socket" alone is not
/// enough. The UDP side (SO_REUSEPORT, SO_BROADCAST, UDP_SEGMENT and a
/// shutdown's directions) lives here for the same reason.
///
/// Kept in a side map keyed by the open file description (`SockEntry::ofd`),
/// not in `SockEntry`: the entry is 72 bytes with 2 bytes of padding and the
/// table is at its 32 KiB budget (455 entries), while this needs 12 bytes.
/// The description is also the right key — on Linux the options belong to
/// the socket, which dup/fork/SCM_RIGHTS copies share — and only sockets
/// that changed an option from its default have a row.
#[derive(Clone, Copy, PartialEq, Eq)]
struct SockOpts {
    nodelay:   bool,
    keepalive: bool,
    /// Seconds; Linux defaults and ranges (1..=32767, 1..=32767, 1..=127).
    keepidle:  u16,
    keepintvl: u16,
    keepcnt:   u8,
    /// SO_REUSEPORT: bind() lets sockets that both set it share a port.
    reuseport: bool,
    /// SO_BROADCAST: without it, sending to (or connecting to) a broadcast
    /// address is EACCES, as on Linux.
    broadcast: bool,
    /// UDP_SEGMENT (UDP GSO) as a socket option: a send longer than this
    /// leaves as datagrams of this size. 0 = off.
    gso:       u16,
    /// shutdown() of a UDP socket: `SHUT_RD_BIT` / `SHUT_WR_BIT`.
    shut:      u8,
    /// IP_MULTICAST_LOOP turned off (Linux default: on).
    mc_noloop: bool,
    /// IP_MULTICAST_TTL (Linux default 1).
    mc_ttl:    u8,
    /// IP_MULTICAST_IF: the interface (stack id + 1; 0 = by route) and the
    /// address it was named by, which getsockopt reads back.
    mc_if:     u8,
    mc_addr:   [u8; 4],
    /// IP_TTL (0 = default, 64) and IP_TOS (recorded and read back).
    ttl:       u8,
    tos:       u8,
    /// IP_MTU_DISCOVER + 1 (0 = never set: Linux's default, IP_PMTUDISC_WANT).
    pmtud1:    u8,
    /// Ancillary data recvmsg adds (`RECV_*`): IP_PKTINFO, IP_RECVTOS,
    /// IP_RECVTTL, UDP_GRO; and IP_RECVERR (accepted, read back).
    recv:      u8,
    /// What bind() fixed (`BIND_ADDR_LOCK`, `BIND_PORT_LOCK`: Linux's
    /// SOCK_BINDADDR_LOCK / SOCK_BINDPORT_LOCK). connect(AF_UNSPEC) gives up
    /// a port nobody chose and an address nobody bound.
    bind_lock: u8,
    /// SO_RCVTIMEO / SO_SNDTIMEO in nanoseconds; 0 = none (wait forever).
    /// A negative tv_sec, which Linux turns into "do not wait", is 1 ns.
    /// The kernel's blocking loops read them through `sock_timeo`.
    rcvtimeo:  u64,
    sndtimeo:  u64,
    /// SO_SNDBUF as Linux stores it (twice the value set, clamped); 0 =
    /// never set (`UNIX_SNDBUF_DEFAULT`). Enforced on AF_UNIX datagrams.
    sndbuf:    u32,
    /// SO_PASSCRED: recvmsg returns the sender's SCM_CREDENTIALS.
    passcred:  bool,
    /// IPV6_V6ONLY on an AF_INET6 socket: it hears and reaches only v6.
    v6only:    bool,
    /// IPv6 ancillary data recvmsg adds (`RECV6_*`).
    recv6:     u8,
    /// ICMP6_FILTER of a raw ICMPv6 socket: a set bit blocks that type.
    icmp6f:    [u32; 8],
}

/// Linux's `net.core.wmem_default` / `rmem_default`: an AF_UNIX socket's
/// SO_SNDBUF / SO_RCVBUF before anyone sets them.
const UNIX_SNDBUF_DEFAULT: u32 = 212992;
/// `SOCK_MIN_SNDBUF` (2 * (2048 + sizeof(struct sk_buff))) and twice
/// `net.core.wmem_max`: the range sock_setsockopt clamps SO_SNDBUF into.
const SNDBUF_MIN: u32 = 4608;
const SNDBUF_MAX: u32 = 2 * 212992;

const SHUT_RD_BIT: u8 = 1;
const SHUT_WR_BIT: u8 = 2;

const RECV_PKTINFO: u8 = 1;
const RECV_TOS: u8 = 2;
const RECV_TTL: u8 = 4;
const RECV_GRO: u8 = 8;
const RECV_ERR: u8 = 16;

const RECV6_PKTINFO: u8 = 1;
const RECV6_HOPLIMIT: u8 = 2;
const RECV6_TCLASS: u8 = 4;
const RECV6_ERR: u8 = 8;

const BIND_ADDR_LOCK: u8 = 1;
const BIND_PORT_LOCK: u8 = 2;

/// The path MTU towards `d` over stack `sid`: the link's, or what an ICMP
/// "fragmentation needed" / ICMPv6 "packet too big" taught the cache (scoped
/// to the stack, hence the network namespace), if smaller.
fn path_mtu_ip(sid: Sid, d: IpAddress) -> u32 {
    let link: u32 = if sid == iface::LO { 65536 } else {
        // A namespace's own loopback (a dynamic sid) is as big as lo.
        match vif::VIFS.lock().get(sid as usize).and_then(|x| x.as_ref()) {
            Some(x) if matches!(x.kind, vif::VifKind::Lo) => x.mtu as u32,
            _ => 1500,
        }
    };
    let now = sched::monotonic_ns() / 1_000_000;
    let key = match d {
        IpAddress::Ipv4(a) => smoltcp::pmtu::v4(a.0),
        IpAddress::Ipv6(a) => a.0,
    };
    match smoltcp::pmtu::lookup(sid as u16, key, now) {
        Some(m) => link.min(m as u32),
        None => link,
    }
}

fn path_mtu(sid: Sid, d: smoltcp::wire::Ipv4Address) -> u32 { path_mtu_ip(sid, IpAddress::Ipv4(d)) }

impl SockOpts {
    /// IP_MTU_DISCOVER as Linux reports it (default IP_PMTUDISC_WANT).
    fn pmtud_mode(&self) -> u8 { if self.pmtud1 == 0 { 1 } else { self.pmtud1 - 1 } }
    const DEFAULT: Self = Self { nodelay: false, keepalive: false, keepidle: 7200, keepintvl: 75, keepcnt: 9,
                                 reuseport: false, broadcast: false, gso: 0, shut: 0,
                                 mc_noloop: false, mc_ttl: 1, mc_if: 0, mc_addr: [0; 4], ttl: 0, tos: 0, pmtud1: 0,
                                 recv: 0, bind_lock: 0, rcvtimeo: 0, sndtimeo: 0, sndbuf: 0, passcred: false, v6only: false, recv6: 0, icmp6f: [0; 8] };
}

/// Rows for live descriptions with a non-default `SockOpts`. Leaf lock: taken
/// under SOCK_TABLES and a stack lock, takes only `vfs::ofd`'s leaf lock.
static SOCK_OPTS: Mutex<alloc::collections::BTreeMap<u32, SockOpts>> =
    Mutex::new(alloc::collections::BTreeMap::new());

fn sock_opts(ofd: u32) -> SockOpts {
    if ofd == 0 { return SockOpts::DEFAULT; }
    SOCK_OPTS.lock().get(&ofd).copied().unwrap_or(SockOpts::DEFAULT)
}

/// The SO_SNDBUF an AF_UNIX datagram sender on `ofd` is bounded by.
fn unix_sndbuf(ofd: u32) -> usize {
    match sock_opts(ofd).sndbuf { 0 => UNIX_SNDBUF_DEFAULT as usize, v => v as usize }
}

/// Record `o` for description `ofd` (0 = no description: nothing to key by,
/// so the options are applied but not remembered). A new row first drops the
/// rows of descriptions that have died, which bounds the map by the live
/// sockets that set an option; ids carry a generation, so a dead row can
/// never be mistaken for a new socket's.
fn set_sock_opts(ofd: u32, o: SockOpts) {
    if ofd == 0 { return; }
    let mut m = SOCK_OPTS.lock();
    if o == SockOpts::DEFAULT { m.remove(&ofd); return; }
    if !m.contains_key(&ofd) { m.retain(|&k, _| vfs::ofd::live(k)); }
    m.insert(ofd, o);
}

/// Push `o` into a TCP socket. Keepalive is Linux's (the vendored smoltcp's
/// `set_keep_alive_probes`, see servers/net/vendor/smoltcp/LEANDROS-PATCH.md):
/// the first probe after `keepidle` seconds of quiet, then one every
/// `keepintvl`, and the connection is dropped when `keepcnt` of them went
/// unanswered — `keepidle + keepintvl * keepcnt` after the peer last spoke.
/// smoltcp's own "no packet for this long" timeout is not used: it fired at
/// that deadline but probed only every `keepidle`, so with Linux's defaults
/// one probe went out before the 7875 s deadline instead of nine.
/// `_established` is kept for the callers: the probes, unlike that timeout,
/// never bound a handshake.
fn apply_tcp_opts(t: &mut tcp::Socket, o: SockOpts, _established: bool) {
    use smoltcp::time::Duration;
    t.set_nagle_enabled(!o.nodelay);
    if o.keepalive {
        let idle = Duration::from_secs(o.keepidle as u64);
        t.set_keep_alive_probes(Some(Duration::from_secs(o.keepintvl as u64)), o.keepcnt);
        // set_keep_alive winds the timer up when the interval appears, so
        // only call it on a change.
        if t.keep_alive() != Some(idle) { t.set_keep_alive(Some(idle)); }
    } else {
        t.set_keep_alive(None);
        t.set_keep_alive_probes(None, 0);
    }
    t.set_timeout(None);
}

/// One serial line, the first time no socket table can be had for a process.
fn report_sock_tables_full() {
    static REPORTED: ::core::sync::atomic::AtomicBool = ::core::sync::atomic::AtomicBool::new(false);
    if REPORTED.swap(true, ::core::sync::atomic::Ordering::Relaxed) { return; }
    extern "C" { fn arch_serial_putc(c: u8); }
    for &b in b"\n[NET] socket-table pool FULL (ENFILE) -- heap, not this process's fd limit\n" {
        unsafe { arch_serial_putc(b); }
    }
}

// ── Table helpers ─────────────────────────────────────────────────────────────

/// The fd-table key socket `pid` lives under: the VFS table id, so
/// CLONE_FILES sharers resolve one fd number to one socket (`sched::fdkey`).
#[inline]
fn tk(pid: u32) -> u32 {
    if pid & sched::fdkey::FD_KEY_PRIVATE != 0 { pid } else { vfs::tbl_key(sched::tgid_of(pid)) }
}

/// Move the socket table `from` to key `to` (the table follows its fd table).
pub fn rekey_table(from: u32, to: u32) {
    let mut tbls = SOCK_TABLES.lock();
    if let Some(t) = tbls.iter_mut().find(|t| t.in_use && t.pid == from) { t.pid = to; }
}

/// Move the socket at `slot` of table `src` into a free slot of table `dst`
/// (no refcount change). Returns the new handle. Used after a dup within
/// `src` to hand the new reference to a split-off table.
pub fn move_slot(src: u32, slot_fd: usize, dst: u32) -> isize {
    let Some(slot) = fd_to_slot(slot_fd) else { return -9 };
    let mut tbls = SOCK_TABLES.lock();
    let (entry, hidden) = match tbls.iter().find(|t| t.in_use && t.pid == src) {
        Some(t) if slot < t.socks.len() && t.socks[slot].in_use => (t.socks[slot].clone(), t.is_hidden(slot)),
        _ => return -9,
    };
    let Some(d) = get_or_create(dst, &mut *tbls) else { return -23 };
    let Some(ns) = d.alloc() else { return -24 };
    d.socks[ns] = entry; d.set_hidden(ns, hidden);
    if let Some(t) = tbls.iter_mut().find(|t| t.in_use && t.pid == src) { t.socks[slot] = SockEntry::empty(); }
    (ns + SOCK_FD_BASE) as isize
}

fn find_tbl_key<'a>(key: u32, tbls: &'a mut SockTables) -> Option<&'a mut ProcSockTable> {
    tbls.iter_mut().find(|t| t.in_use && t.pid == key)
}

/// Duplicate socket `fd` inside table `key` (exact key), returning the new fd.
pub fn dup_in(key: u32, fd: usize, cloexec: bool) -> isize {
    let m = handle_dup(key, fd, cloexec, false);
    let b: [u8; 8] = m.data[0..8].try_into().unwrap_or([0u8; 8]);
    i64::from_le_bytes(b) as isize
}

/// FD_CLOEXEC of socket `fd` in table `key` (exact key).
pub fn sock_cloexec(key: u32, fd: usize) -> bool {
    let Some(slot) = fd_to_slot(fd) else { return false };
    let tbls = SOCK_TABLES.read();
    tbls.iter().find(|t| t.in_use && t.pid == key)
        .map_or(false, |t| slot < t.socks.len() && t.socks[slot].in_use && t.socks[slot].cloexec)
}

fn find_tbl<'a>(pid: u32, tbls: &'a mut SockTables) -> Option<&'a mut ProcSockTable> {
    tbls.iter_mut().find(|t| t.in_use && t.pid == tk(pid))
}

fn get_or_create<'a>(pid: u32, tbls: &'a mut SockTables) -> Option<&'a mut ProcSockTable> {
    if let Some(pos) = tbls.iter().position(|t| t.in_use && t.pid == tk(pid)) {
        return Some(&mut tbls[pos]);
    }
    if let Some(pos) = tbls.claim() {
        tbls[pos].reset();
        tbls[pos].in_use = true;
        tbls[pos].pid    = pid;
        return Some(&mut tbls[pos]);
    }
    report_sock_tables_full();
    None
}

/// Every open socket fd of `pid`'s process (close_range).
pub fn open_fds(pid: u32) -> alloc::vec::Vec<usize> {
    let pid = sched::tgid_of(pid);
    let tbls = SOCK_TABLES.read();
    match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) {
        Some(t) => t.socks.iter().enumerate().filter(|(_, e)| e.in_use).map(|(i, _)| i + SOCK_FD_BASE).collect(),
        None => alloc::vec::Vec::new(),
    }
}

/// The open file description socket `fd` names in `pid`'s process (`Some(0)`:
/// none), or `None` when `fd` is not an open socket.
pub fn fd_ofd(pid: u32, fd: usize) -> Option<u32> {
    let pid = sched::tgid_of(pid);
    let slot = fd_to_slot(fd)?;
    let tbls = SOCK_TABLES.read();
    let t = tbls.iter().find(|t| t.in_use && t.pid == tk(pid))?;
    if slot < t.socks.len() && t.socks[slot].in_use { Some(t.socks[slot].ofd) } else { None }
}

/// A copy of the in-flight SCM_RIGHTS descriptor that carries VFS
/// description `id`, if one is queued on an AF_UNIX stream: what the epoll
/// layer probes when no process's fd names the description (Linux still
/// reports such an item). The copy carries no reference of its own — the
/// caller re-checks `vfs::ofd::live` after using it.
pub fn inflight_vfs(id: u32) -> Option<vfs::TransferFd> {
    if id == 0 { return None; }
    let conns = UNIX_CONNS.lock();
    for c in conns.iter().filter(|c| c.in_use) {
        for b in c.fdq_ab.iter().chain(c.fdq_ba.iter()) {
            for x in b.fds.iter() {
                if let XferFd::Vfs(tf) = x { if tf.ofd() == id { return Some(*tf); } }
            }
        }
    }
    None
}

/// Some process's socket fd naming description `id`, as (process id, fd),
/// preferring `pid`'s own process (see `vfs::ofd_holder`).
pub fn ofd_holder(pid: u32, id: u32) -> Option<(u32, usize)> {
    if id == 0 { return None; }
    let pid = sched::tgid_of(pid);
    let tbls = SOCK_TABLES.read();
    let find = |t: &ProcSockTable| t.socks.iter().position(|e| e.in_use && e.ofd == id)
        .map(|s| (t.pid, s + SOCK_FD_BASE));
    if let Some(r) = tbls.iter().find(|t| t.in_use && t.pid == tk(pid)).and_then(find) { return Some(r); }
    tbls.iter().filter(|t| t.in_use && t.pid != tk(pid)).find_map(find)
}

/// Network namespace captured when the socket at `slot` was created. Takes the
/// table read lock: never call it with SOCK_TABLES held.
fn sock_netns(pid: u32, slot: usize) -> u16 {
    SOCK_TABLES.read().iter().find(|t| t.in_use && t.pid == tk(pid))
        .and_then(|t| t.socks.get(slot)).filter(|e| e.in_use)
        .map(|e| e.netns()).unwrap_or_else(|| sched::ns::netns_of(pid))
}

fn fd_to_slot(fd: usize) -> Option<usize> {
    if fd >= SOCK_FD_BASE && fd < SOCK_FD_END { Some(fd - SOCK_FD_BASE) } else { None }
}

// ── Shared inet sockets ──────────────────────────────────────────────────────
//
// An AF_INET (or ICMP) socket's object is a set of smoltcp handles named by
// value from its `SockState`; unlike an AF_UNIX end it has no refcount of its
// own. Several holders — dup, fork, SCM_RIGHTS, pidfd_getfd — each keep a
// copy of the entry, and what ties the copies together is the open file
// description they share (`SockEntry::ofd`):
//
// - The description's refcount is the socket's. The holder whose put drops
//   it to zero releases the handles (`inet_put` + `inet_put_finish`). A
//   socket with no description (id 0: the description table was full) is
//   never shared, so its one holder releases.
// - A change to the socket's state through any holder (bind, listen, connect
//   and its outcome, accept re-arming a listener, SO_REUSEADDR, an ICMP
//   socket's first send) is copied, under SOCK_TABLES, into every table entry
//   naming the description and into a canonical row (`SockTables::inet`)
//   (`share_state`). A copy outside every table — queued by SCM_RIGHTS — is
//   refreshed from that row when it is received or dropped, so whichever
//   holder acts next, or puts last, works on the live handles.
//
// Until lane fdgaps an inet socket could not be dup'd, passed or inherited at
// all: a second copy of the entry meant a double free of its handles.

/// The socket-level part of an inet entry, as `share_state` last published it.
#[derive(Clone, Copy)]
struct InetShared {
    state:      SockState,
    bound_port: u16,
    connecting: bool,
    reuseaddr:  bool,
}

impl InetShared {
    fn of(e: &SockEntry) -> Self {
        Self { state: e.state, bound_port: e.bound_port, connecting: e.connecting, reuseaddr: e.reuseaddr }
    }
    fn apply(&self, e: &mut SockEntry) {
        e.state = self.state;
        e.bound_port = self.bound_port;
        e.connecting = self.connecting;
        e.reuseaddr = self.reuseaddr;
    }
}

/// States whose object is a set of smoltcp handles (see above).
fn is_inet_state(s: &SockState) -> bool {
    matches!(s, SockState::InetBound { .. } | SockState::InetListening { .. }
        | SockState::InetConnected { .. } | SockState::IcmpUnbound | SockState::IcmpBound { .. })
}

/// An inet socket, or an AF_INET one not bound yet: the entries whose state
/// `share_state` publishes and whose last put goes through `inet_put`.
/// AF_INET or AF_INET6 (the socket's `domain` byte).
#[inline]
fn is_inet(d: u8) -> bool { d == AF_INET as u8 || d == AF_INET6 as u8 }

fn inet_family(e: &SockEntry) -> bool {
    is_inet_state(&e.state)
        || matches!(e.state, SockState::Unbound { .. }) && is_inet(e.domain)
}

/// May this entry be copied to a second holder (fork, dup, SCM_RIGHTS)? Not
/// an inet socket with no description: nothing would tell its holders apart.
fn inet_shareable(e: &SockEntry) -> bool {
    !(is_inet_state(&e.state) && e.ofd == 0)
}

/// Publish the socket-level fields of the entry at (`pid`, `slot`) — its
/// state, port, pending-connect flag and SO_REUSEADDR — to every other holder
/// of its description (see "Shared inet sockets"). Call after changing any of
/// them, with SOCK_TABLES held (it is the `tbls` passed) and no stack lock.
/// An unshared socket costs one leaf-lock probe.
fn share_state(tbls: &mut SockTables, pid: u32, slot: usize) {
    let Some(src) = tbls.iter().find(|t| t.in_use && t.pid == tk(pid))
        .and_then(|t| t.socks.get(slot).cloned()) else { return };
    // Only inet states (and an AF_INET socket not bound yet, for
    // SO_REUSEADDR): an AF_UNIX object (a bound address, a connection end) is
    // refcounted per holder, so copying one into a holder that took no
    // reference on it would double-release it.
    if !src.in_use || !inet_family(&src) || src.ofd == 0 { return; }
    // A row from an earlier shared period must not go stale, even if the
    // other holders are gone by now.
    if !vfs::ofd::shared(src.ofd) && !tbls.inet.contains_key(&src.ofd) { return; }
    let sh = InetShared::of(&src);
    tbls.inet.insert(src.ofd, sh);
    for t in tbls.iter_mut().filter(|t| t.in_use) {
        for e in t.socks.iter_mut().filter(|e| e.in_use && e.ofd == src.ofd) { sh.apply(e); }
    }
}

/// Refresh a copy that was outside every table (an SCM_RIGHTS copy) from
/// the canonical row, if the socket was ever changed while shared.
fn inet_refresh(tbls: &SockTables, e: &mut SockEntry) {
    if e.ofd == 0 { return; }
    if let Some(sh) = tbls.inet.get(&e.ofd) { sh.apply(e); }
}

/// What one holder's put of an inet socket left to do once SOCK_TABLES is
/// released: the state to tear down if it was the last holder, and whether
/// the description's release hook must run.
#[must_use]
struct InetPut { ofd: u32, release: Option<(SockState, u8)>, hook: bool, last: bool }

/// Drop one holder's reference to the inet socket `e` names (its table entry
/// or in-flight copy is already gone). Called with SOCK_TABLES held, which is
/// what makes "the last holder" see the state every other holder published.
fn inet_put(tbls: &mut SockTables, e: &SockEntry) -> InetPut {
    let (last, hook) = vfs::ofd::put_deferred(e.ofd);
    let mut release = None;
    if last {
        let mut cur = e.clone();
        if let Some(sh) = tbls.inet.remove(&e.ofd) { sh.apply(&mut cur); }
        if is_inet_state(&cur.state) { release = Some((cur.state, cur.sock_type)); }
    }
    InetPut { ofd: e.ofd, release, hook, last }
}

/// The rest of `inet_put`, with no net lock held. `park`: see
/// `release_inet_state`.
fn inet_put_finish(p: InetPut, park: bool) {
    // Its multicast groups are left (Linux ip_mc_drop_socket), also for a
    // socket that joined before it was ever bound.
    if p.last { mcast::release(p.ofd); }
    if let Some((state, ty)) = p.release {
        // A wildcard UDP server's mirrors go with it (`udpany`).
        if let (SockState::InetConnected { sid, .. }, true) = (state, ty == SOCK_DGRAM as u8) {
            udpany::release(p.ofd, Some(sid));
        }
        release_inet_state(state, ty, park);
    }
    if p.hook { vfs::ofd::release(p.ofd); }
}

/// The smoltcp handles an inet state holds, as (stack, handle).
fn inet_handles(s: &SockState) -> alloc::vec::Vec<(Sid, SocketHandle)> {
    let mut v = alloc::vec::Vec::new();
    match *s {
        SockState::InetConnected { socket_handle, sid, .. } => v.push((sid, socket_handle)),
        // A listener's sockets are its stacks' (`listen::destroy`).
        SockState::InetListening { .. } => {}
        SockState::IcmpBound { socket_handle, sid } => v.push((sid, socket_handle)),
        _ => {}
    }
    v
}

/// Tear down the socket an inet state names, after its last holder let go.
/// `park`: a TCP connection this side closes first parks its local port in
/// TIME_WAIT (close(2); not process exit, as before). Caller holds no net lock.
fn release_inet_state(state: SockState, sock_type: u8, park: bool) {
    if let SockState::InetListening { lid, .. } = state {
        listen::destroy_all(lid);
        return;
    }
    let mut park_port = None;
    for (sid, h) in inet_handles(&state) {
        let mut stack = stack_for(sid);
        let Some(ref mut s) = *stack else { continue };
        if !s.socket_set.contains(h) { continue; }
        if park && sock_type == SOCK_STREAM as u8
            && matches!(state, SockState::InetConnected { .. })
        {
            let sk = s.socket_set.get::<tcp::Socket>(h);
            // Only the side that closes an established connection *first*
            // goes through TIME_WAIT. CloseWait/LastAck mean the peer's FIN
            // already arrived, so this is the passive close and Linux goes
            // straight to CLOSED. Closed/Listen/SynSent/SynReceived never
            // established. The port is read from smoltcp rather than from
            // `bound_port`, which `handle_accept` leaves at 0 on an accepted
            // socket — and an accepted socket sharing the listener's port is
            // exactly what makes a restarted server hit EADDRINUSE on Linux.
            let active_close = matches!(sk.state(),
                tcp::State::Established | tcp::State::FinWait1
                | tcp::State::FinWait2  | tcp::State::Closing
                | tcp::State::TimeWait);
            if active_close { park_port = sk.local_endpoint().map(|e| e.port); }
        }
        // The connection is shut down (FIN, or RST over unread data) but
        // smoltcp's own TIME-WAIT is not kept: what is modelled is the port
        // reservation, not the protocol state. A listener just goes.
        release_inet_socket(s, h);
    }
    // TIME_WAIT is a leaf lock, taken after the stack lock is gone.
    if let Some(p) = park_port { time_wait_add(p); }
}

/// End of stream on a TCP socket whose receive queue is empty: the peer's FIN
/// has been received (CloseWait, LastAck, Closing, TimeWait) or the
/// connection is gone (Closed, after an RST or a timeout). `is_active()` alone
/// is not this: it stays true through CloseWait, so a server that answered
/// and closed (HTTP/1.0, `Connection: close`) left recv() returning EAGAIN and
/// poll() never reporting POLLIN, and the reader waited forever for an EOF
/// that had already arrived. A connect still in flight (SynSent/SynReceived)
/// has not received anything yet and is not at EOF.
/// The peer's FIN has been received (whether or not unread data remains).
fn tcp_peer_fin(socket: &tcp::Socket) -> bool {
    matches!(socket.state(), tcp::State::CloseWait | tcp::State::LastAck
        | tcp::State::Closing | tcp::State::TimeWait | tcp::State::Closed)
}

fn tcp_rx_eof(socket: &tcp::Socket) -> bool {
    !socket.may_recv()
        && !matches!(socket.state(), tcp::State::SynSent | tcp::State::SynReceived)
}

/// True while a TCP connect() on `fd` has an outcome nobody has read yet
/// (the `connecting` flag). A non-blocking connect() answers EINPROGRESS
/// whenever this holds, without looking at the handshake: see sys_connect.
pub fn tcp_connect_pending(pid: u32, fd: usize) -> bool {
    let pid = sched::tgid_of(pid);
    let Some(slot) = fd_to_slot(fd) else { return false };
    let mut tbls = SOCK_TABLES.lock();
    let Some(tbl) = find_tbl(pid, &mut *tbls) else { return false };
    slot < tbl.socks.len() && tbl.socks[slot].in_use && tbl.socks[slot].connecting
}

/// Where a TCP connect() on `fd` stands: 0 once the handshake completed (and
/// for any fd that is not a TCP socket with a connect outstanding), -EAGAIN
/// while it is in flight, -ECONNREFUSED when the connection ended before it
/// was ever seen established. A final answer clears `connecting`, so a
/// refusal is reported once, as Linux reports a pending socket error once.
/// The kernel's connect() waits on this for a blocking socket; SO_ERROR,
/// send and recv use it to name the error.
pub fn tcp_connect_status(pid: u32, fd: usize) -> isize {
    let pid = sched::tgid_of(pid);
    let Some(slot) = fd_to_slot(fd) else { return 0 };
    let mut tbls = SOCK_TABLES.lock();
    let Some(tbl) = find_tbl(pid, &mut *tbls) else { return 0 };
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use || !tbl.socks[slot].connecting { return 0; }
    let (handle, sid) = match tbl.socks[slot].state {
        SockState::InetConnected { socket_handle, sid, .. } => (socket_handle, sid),
        _ => { tbl.socks[slot].connecting = false; return 0; }
    };
    let opts = sock_opts(tbl.socks[slot].ofd);
    let (st, icmp) = {
        let mut stack = stack_for(sid);
        match stack.as_mut() {
            Some(s) => {
                let Some(t) = s.socket_set.try_get_mut::<tcp::Socket>(handle) else {
                    tbl.socks[slot].connecting = false;
                    return -(111i32 as isize); // ECONNREFUSED: the socket is gone
                };
                let st = t.state();
                // The handshake is over: arm the keepalive timeout that
                // connect() held back (see apply_tcp_opts).
                if opts.keepalive && !matches!(st, tcp::State::SynSent | tcp::State::SynReceived | tcp::State::Closed) {
                    apply_tcp_opts(t, opts, true);
                }
                (st, t.icmp_errno())
            }
            None => (tcp::State::Closed, 0),
        }
    };
    let r = match st {
        tcp::State::SynSent | tcp::State::SynReceived => return -11,
        // An ICMP error ended the attempt (EHOSTUNREACH, ECONNREFUSED, ...:
        // Linux tcp_v4_err in SYN-SENT); else a reset refused it.
        tcp::State::Closed if icmp != 0 => -(icmp as isize),
        tcp::State::Closed => -111,
        _ => 0,
    };
    // The outcome is the socket's, reported once to whichever holder asks.
    tbl.socks[slot].connecting = false;
    share_state(&mut *tbls, pid, slot);
    r
}

/// The stack that owns a socket's smoltcp handle (`LO`, or a NIC's stack
/// id; see `iface`). No two stacks are ever locked at the same time — every
/// caller takes one, finishes with it, and only then takes another.
#[inline]
fn stack_for(sid: Sid) -> spin::MutexGuard<'static, Option<NetStack>> {
    STACKS[(sid as usize).min(MAX_STACKS - 1)].lock()
}

/// A socket whose smoltcp socket is gone (its stack was freed by a link
/// delete / namespace reap, or the slot was reused) must fail like a socket on
/// a dead link, never reach `SocketSet::get`, which panics on a stale handle.
/// Called before every fd operation: a stale connected/ICMP entry (and every
/// holder sharing its description) is demoted to the unconnected state the
/// handlers already answer with an error. Common case: one table read.
fn heal_stale(pid: u32, fd: usize) {
    let Some(e) = sock_entry(pid, fd) else { return };
    let (sid, h) = match e.state {
        SockState::InetConnected { socket_handle, sid, .. } => (sid, socket_handle),
        SockState::IcmpBound { socket_handle, sid } => (sid, socket_handle),
        _ => return,
    };
    if stack_for(sid).as_ref().map_or(false, |s| s.socket_set.contains(h)) { return; }
    let Some(slot) = fd_to_slot(fd) else { return };
    let mut tbls = SOCK_TABLES.lock();
    {
        let Some(tbl) = find_tbl(pid, &mut *tbls) else { return };
        let Some(e) = tbl.socks.get_mut(slot).filter(|e| e.in_use) else { return };
        let same = match e.state {
            SockState::InetConnected { socket_handle, sid: s2, .. } => s2 == sid && socket_handle == h,
            SockState::IcmpBound { socket_handle, sid: s2 } => s2 == sid && socket_handle == h,
            _ => false,
        };
        if !same { return; }
        e.state = match e.state {
            SockState::InetConnected { .. } => SockState::InetBound {
                domain: e.domain, sock_type: e.sock_type,
                local_endpoint: IpEndpoint::new(IpAddress::v4(0, 0, 0, 0), e.bound_port),
            },
            _ => SockState::IcmpUnbound,
        };
        e.connecting = false;
    }
    share_state(&mut *tbls, pid, slot);
}

/// True for an address the loopback stack owns (127.0.0.0/8).
#[inline]
fn is_loopback_addr(addr: IpAddress) -> bool {
    iface::is_loopback(addr) || matches!(addr, IpAddress::Ipv6(a) if ip6::lo_static(&a))
}

/// The NIC stack whose interface holds `ip` now. One stack lock at a time.
fn stack_holding(ns: u16, ip: IpAddress) -> Option<Sid> {
    let nics: alloc::vec::Vec<Sid> = if ns == 0 { iface::nic_sids().collect() } else { alloc::vec::Vec::new() };
    nics.into_iter().chain(vif::ns_sids_dyn(ns)).find(|&sid| match *stack_for(sid) {
        Some(ref s) => s.interface.ip_addrs().iter().any(|c| c.address() == ip),
        None => false,
    })
}

/// Does a bind of (`ip`, `port`) by socket `me` (of process `pid`) collide
/// with a port another socket holds? Linux's rule: a socket of the same
/// protocol bound to the port with an overlapping address (equal, or either
/// one INADDR_ANY) conflicts, unless both set SO_REUSEPORT (and have the same
/// owner), or both set SO_REUSEADDR — which for TCP does not reach a socket
/// that is listening. Holders of the same description (dup, fork) are the
/// same socket. Caller holds SOCK_TABLES; this takes stack locks (the
/// documented order) for the address of a connected socket, and SOCK_OPTS.
/// Do two bound addresses claim a common local address? Within a family a wildcard
/// overlaps everything; across families only a v6 wildcard (dual-stack `::`) reaches
/// the v4 side, so 0.0.0.0:P and [::1]:P coexist (Linux, bindv6only=0).
fn bind_addrs_overlap(a: IpAddress, a_v6only: bool, b: IpAddress, b_v6only: bool) -> bool {
    match (a, b) {
        (IpAddress::Ipv4(_), IpAddress::Ipv4(_)) | (IpAddress::Ipv6(_), IpAddress::Ipv6(_)) => {
            a.is_unspecified() || b.is_unspecified() || a == b
        }
        (IpAddress::Ipv6(_), _) => a.is_unspecified() && !a_v6only,
        (_, IpAddress::Ipv6(_)) => b.is_unspecified() && !b_v6only,
    }
}

fn bind_conflict(tbls: &SockTables, pid: u32, me: &SockEntry, ip: IpAddress, port: u16) -> bool {
    let tcp = me.sock_type == SOCK_STREAM as u8;
    // A port a NAT flow (fwd.rs) holds on the host's NIC address is taken. FWD is a leaf lock.
    if me.netns() == 0 && is_inet(me.domain) && fwd::reserved(tcp, port) {
        if ip.is_unspecified() || matches!(ip, IpAddress::Ipv4(a) if iface::is_nic_addr(a.0)) { return true; }
    }
    let my_opts = sock_opts(me.ofd);
    let my_reuseport = my_opts.reuseport;
    let my_v6only = my_opts.v6only;
    let my_uid = sched::euid_of(pid);
    for t in tbls.iter().filter(|t| t.in_use) {
        for e in t.socks.iter() {
            if !e.in_use || e.bound_port != port || !is_inet(e.domain) || e.sock_type != me.sock_type { continue; }
            // Each network namespace has its own port space.
            if e.netns() != me.netns() { continue; }
            if me.ofd != 0 && e.ofd == me.ofd { continue; }
            let (addr, listening) = match e.state {
                SockState::InetBound { local_endpoint, .. } => (local_endpoint.addr, false),
                SockState::InetListening { local, .. } => (local.addr, true),
                SockState::InetConnected { socket_handle, sid, .. } => {
                    let mut stack = stack_for(sid);
                    let a = stack.as_mut().and_then(|s| if tcp {
                        s.socket_set.get_mut::<tcp::Socket>(socket_handle).local_endpoint().map(|ep| ep.addr)
                    } else {
                        s.socket_set.get_mut::<udp::Socket>(socket_handle).endpoint().addr
                    });
                    (a.unwrap_or(IpAddress::v4(0, 0, 0, 0)), false)
                }
                _ => continue,
            };
            if !bind_addrs_overlap(addr, sock_opts(e.ofd).v6only, ip, my_v6only) { continue; }
            if my_reuseport && sock_opts(e.ofd).reuseport && sched::euid_of(t.pid) == my_uid { continue; }
            if me.reuseaddr && e.reuseaddr && !(tcp && listening) { continue; }
            return true;
        }
    }
    false
}

/// True when a host (netns 0) socket of the given protocol is bound to `port`.
/// Takes SOCK_TABLES; the caller holds no stack or FWD lock (fwd.rs NAT port pick).
pub(crate) fn host_port_busy(tcp: bool, port: u16) -> bool {
    let st = if tcp { SOCK_STREAM as u8 } else { SOCK_DGRAM as u8 };
    let tbls = SOCK_TABLES.read();
    tbls.iter().any(|t| t.in_use && t.socks.iter().any(|e| e.in_use && e.bound_port == port
        && is_inet(e.domain) && e.sock_type == st && e.netns() == 0))
}

/// Pick a free local port from Linux's ephemeral range for a `bind()` to port 0
/// or a `connect()` from an unbound socket. "Free" means no socket in any
/// process table claims it. Caller holds SOCK_TABLES; this only reads it, so it
/// must run before the caller borrows its own table mutably.
/// `reserved` is a `time_wait_snapshot` taken by the caller before it locked
/// SOCK_TABLES: an automatic port must skip a port still in TIME_WAIT, and the
/// snapshot is what keeps TIME_WAIT from being locked underneath SOCK_TABLES.
///
/// Where the search starts: a cursor seeded at random once (`init`) that moves
/// past every port it hands out, the way Linux's `__inet_hash_connect` walks
/// its perturbation table forward. The search used to start at
/// `ticks % span`, so every allocation within one 10 ms tick began at the
/// same port and a port released a moment ago was handed straight back
/// (close + reconnect reused the 4-tuple of the connection just closed).
fn alloc_ephemeral_port(tbls: &SockTables, reserved: &[u16]) -> Option<u16> {
    let start = EPH_CURSOR.fetch_add(1, Ordering::Relaxed) % EPH_SPAN;
    for i in 0..EPH_SPAN {
        let p = (EPH_LO + (start + i) % EPH_SPAN) as u16;
        if reserved.contains(&p) { continue; }
        let taken = tbls.iter().any(|t| t.in_use
            && t.socks.iter().any(|s| s.in_use && s.bound_port == p));
        if !taken {
            // Skip what this search walked over, so the next one starts
            // past the port just handed out.
            if i != 0 { EPH_CURSOR.fetch_add(i, Ordering::Relaxed); }
            return Some(p);
        }
    }
    None
}

/// Linux's default `ip_local_port_range` (32768-60999).
const EPH_LO: u32 = 32768;
const EPH_SPAN: u32 = 60999 - EPH_LO + 1;
/// Next ephemeral port to try (offset into the range, taken mod `EPH_SPAN`).
static EPH_CURSOR: AtomicU32 = AtomicU32::new(0);

/// Seed `EPH_CURSOR` from the kernel CSPRNG (at init: `random::fill` may read
/// an entropy device, which must not happen under SOCK_TABLES).
fn seed_ephemeral() {
    let mut b = [0u8; 4];
    sched::random::fill(&mut b);
    EPH_CURSOR.store(u32::from_ne_bytes(b) % EPH_SPAN, Ordering::Relaxed);
}

/// Write a `sockaddr_in` (Linux ABI: sa_family(2) + sin_port BE(2) + sin_addr
/// BE(4) + 8 pad) and its length. `addrlen_ptr` is in/out, as on Linux: its
/// incoming value caps how much of the caller's buffer may be written, and it
/// always reports the full address length back. The caller must hold NO server
/// lock — this touches user memory, which can demand-page and re-enter the
/// scheduler.
unsafe fn write_sockaddr_in(dom: usize, addr_ptr: usize, addrlen_ptr: usize, endpoint: IpEndpoint) {
    sa::write(dom, addr_ptr, addrlen_ptr, endpoint);
}

/// The domain (AF_INET / AF_INET6) of the inet socket at `fd`; AF_INET when unknown.
fn sock_dom(pid: u32, fd: usize) -> usize {
    sock_entry(pid, fd).map_or(AF_INET, |e| e.domain as usize)
}

/// The local endpoint of an AF_INET socket, for getsockname. `None` for
/// anything that is not an inet socket (the AF_UNIX answer is written by the
/// caller). Takes SOCK_TABLES and then at most one stack lock, never nested
/// the other way round.
fn inet_local_endpoint(pid: u32, fd: usize) -> Option<IpEndpoint> {
    let (state, sock_type, bound_port) = inet_sock_info(pid, fd)?;
    match state {
        // An AF_INET socket not bound yet: 0.0.0.0:0 (it answered with an
        // AF_UNSPEC address).
        SockState::Unbound { domain, .. } if is_inet(domain) => Some(IpEndpoint::new(IpAddress::v4(0, 0, 0, 0), 0)),
        SockState::InetBound { local_endpoint, .. }  => Some(local_endpoint),
        SockState::InetListening { local, .. }       => Some(local),
        // Only a TCP socket may be fetched as a tcp::Socket — `SocketSet::get_mut`
        // panics on a type mismatch, so a connected UDP socket answers from the
        // fd table instead.
        SockState::InetConnected { socket_handle, sid, .. }
            if sock_type == SOCK_STREAM as u8 =>
        {
            let mut stack = stack_for(sid);
            let s = stack.as_mut()?;
            s.socket_set.get_mut::<tcp::Socket>(socket_handle).local_endpoint()
        }
        // UDP: the address the smoltcp socket is bound to (bind() to
        // 127.0.0.1 reports 127.0.0.1, as on Linux); a connected socket
        // bound to none reports the source address its route uses, which
        // Linux fixes at connect() — the "which local address reaches X"
        // probe (WebRTC's host candidates, musl's getaddrinfo sort) reads
        // exactly that; else INADDR_ANY.
        SockState::InetConnected { socket_handle, sid, remote_endpoint } => {
            let addr = {
                let mut stack = stack_for(sid);
                stack.as_mut().and_then(|s| {
                    let bound = s.socket_set.get_mut::<udp::Socket>(socket_handle).endpoint().addr;
                    bound.or_else(|| remote_endpoint.and_then(|_| stack_src_addr(s, sid)))
                })
            };
            Some(IpEndpoint::new(addr.unwrap_or(IpAddress::v4(0, 0, 0, 0)), bound_port))
        }
        _ => None,
    }
}

/// The IPv4 source address of stack `sid`'s traffic: 127.0.0.1 on loopback,
/// the interface's (first) address on a NIC stack.
fn stack_src_addr(s: &NetStack, sid: Sid) -> Option<IpAddress> {
    if sid == LO { return Some(IpAddress::v4(127, 0, 0, 1)); }
    s.interface.ip_addrs().iter().map(|c| c.address())
        .find(|a| matches!(a, IpAddress::Ipv4(_)) && !a.is_unspecified())
}

/// The remote endpoint of a connected AF_INET socket, for getpeername.
fn inet_remote_endpoint(pid: u32, fd: usize) -> Option<IpEndpoint> {
    let (state, sock_type, _) = inet_sock_info(pid, fd)?;
    match state {
        SockState::InetConnected { socket_handle, remote_endpoint, sid }
            if sock_type == SOCK_STREAM as u8 =>
        {
            let mut stack = stack_for(sid);
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

/// The targeted-wake tag a blocking send/recv/connect on `fd` may park under
/// (see `sched::poll_tag`), or `None` for a socket whose producers still
/// broadcast (an AF_UNIX listener) — the caller then parks on the broadcast
/// mask with a tick deadline. Unix connections: every ring write, half/whole
/// close and death on a connection wakes `poll_tag(UNIX, conn_idx)`, the same
/// tag `handle_poll` reports for it. Everything else: `sock_keys`, the same
/// keys `handle_poll` answers. `pid` may be any thread's.
///
/// The answer carries the exact keys, not just their bloom bits, so the park
/// is refined exactly as a poll() of the fd is. Bits alone left the park
/// unkeyed: every keyed AF_UNIX/pipe/eventfd data wake anywhere in the
/// system whose key shared a bloom bit with this socket's woke it (1 in 64
/// per key), which is what made nettag2 fail 2 runs in 3 with a different
/// case each boot (the bit follows the socket's key).
pub fn fd_wake_tag(pid: u32, fd: usize) -> Option<sched::PollTag> {
    let e = sock_entry(sched::tgid_of(pid), fd)?;
    match e.state {
        SockState::UnixConnected { conn_idx, is_a } => Some(unix_end_keys(conn_idx, is_a, 0)),
        SockState::UnixPendingAccept { conn_idx, .. } => Some(unix_end_keys(conn_idx, true, 0)),
        // Every inet socket (and an AF_UNIX one with no object yet): the keys
        // its poll answers (`sock_keys`). A blocking recv/send/connect used
        // to park broadcast here, woken by every poll wake in the system plus
        // a 100 Hz re-probe.
        _ => sock_tag(&e),
    }
}

/// `(state, sock_type, bound_port)` of one fd, read under SOCK_TABLES and with
/// the lock released before the caller touches any stack.
fn inet_sock_info(pid: u32, fd: usize) -> Option<(SockState, u8, u16)> {
    let slot = fd_to_slot(fd)?;
    let tbls = SOCK_TABLES.read();
    let tbl = tbls.iter().find(|t| t.in_use && t.pid == tk(pid))?;
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return None; }
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
    let mut gone = None;
    {
        let mut bound = BOUND_PATHS.lock();
        if bound_idx < MAX_BOUND && bound[bound_idx].in_use {
            bound[bound_idx].refs = bound[bound_idx].refs.saturating_sub(1);
            if bound[bound_idx].refs == 0 {
                gone = Some(bound[bound_idx].sock_id);
                bound[bound_idx] = BoundPath::new();
            }
        }
    }
    if let Some(sock_id) = gone { unix_listener_gone(sock_id); }
}

/// The last reference to the listener bound as `sock_id` went away: every
/// connect() still waiting for its accept() is reset, as Linux's
/// `unix_release_sock` does for the embryos on a closing listener's queue —
/// the connector's peer is gone (`closed_b`), so it polls
/// POLLIN|POLLHUP|POLLERR, its first recv answers ECONNRESET and later ones
/// EOF, and a send answers EPIPE. Bytes and fds it wrote before the accept
/// are discarded with the embryo. Every alias of each connector (fork
/// copies) becomes an ordinary end A, so the connected-end paths apply.
///
/// The readiness change is woken on the connection's state key
/// (`poll_tag(UNIX, conn)`), which every end-A probe reports — before this,
/// such a connector never heard of the close at all (invariant 16).
///
/// Caller holds no net lock (SOCK_TABLES > UNIX_CONNS > BOUND_PATHS order).
fn unix_listener_gone(sock_id: u64) {
    if sock_id == 0 { return; }
    let mut hit: alloc::vec::Vec<usize> = alloc::vec::Vec::new();
    let mut orphans: alloc::vec::Vec<XferFd> = alloc::vec::Vec::new();
    {
        let mut tbls = SOCK_TABLES.lock();
        for t in tbls.iter_mut() {
            if !t.in_use { continue; }
            for e in t.socks.iter_mut() {
                if let SockState::UnixPendingAccept { conn_idx, sock_id: sid } = e.state {
                    if sid == sock_id {
                        e.state = SockState::UnixConnected { conn_idx, is_a: true };
                        if !hit.contains(&conn_idx) { hit.push(conn_idx); }
                    }
                }
            }
        }
        let mut conns = UNIX_CONNS.lock();
        // Connections on the backlog whose connector already closed have no
        // table entry: nobody can see them any more, so they just go.
        for c in conns.iter_mut() {
            if c.in_use && c.pending_sid == sock_id && c.closed_a {
                orphans.append(&mut c.take_fds());
                c.pending_sid = 0;
                c.in_use = false;
                c.seq = c.seq.wrapping_add(1);
            }
        }
        for &ci in hit.iter() {
            if ci >= conns.len() || !conns[ci].in_use { continue; }
            let c = &mut conns[ci];
            c.pending_sid = 0;
            c.closed_b = true;
            c.refs_b = 0;
            c.reset_a = true;
            // Never under a copier working with UNIX_CONNS released.
            if !c.ring_ab.wbusy && !c.ring_ab.rbusy { c.ring_ab = UnixRing::new(); }
            for b in c.fdq_ab.drain(..) { orphans.extend(b.fds); }
            c.seq = c.seq.wrapping_add(1);
        }
    }
    for x in orphans { xfer_drop(x); }
    for ci in hit {
        dbg("[NET] listener gone sid="); dbg_u(sock_id);
        dbg(" reset conn="); dbg_u(ci as u64); dbg("\r\n");
        sched::wake_poll_tagged(sched::poll_tag(sched::poll_class::UNIX, ci as u32));
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
    // A VFS-range descriptor that aliases a socket (`dup2(sock, 3)`) sends
    // the socket it names.
    // A raw handle number from the sender's control message is not one of
    // its descriptors.
    if vfs::is_handle(fd) { return None; }
    let fd = vfs::sock_alias_of(pid, fd).unwrap_or(fd);
    let Some(slot) = fd_to_slot(fd) else {
        // Ordinary VFS descriptor (memfd, pipe, file).
        return vfs::export_fd(pid, fd).map(XferFd::Vfs);
    };
    let entry = {
        let tbls = SOCK_TABLES.read();
        let tbl = tbls.iter().find(|t| t.in_use && t.pid == tk(pid))?;
        if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return None; }
        let e = tbl.socks[slot].clone();
        // An inet socket: the in-flight copy is one more holder of its
        // description, which is the socket's refcount (see "Shared inet
        // sockets"), so the reference is taken in the same SOCK_TABLES hold
        // the entry was read in — a concurrent last close cannot release the
        // handles in between. One with no description cannot be shared.
        if !inet_shareable(&e) { return None; }
        if inet_family(&e) { vfs::ofd::get(e.ofd); }
        e
    };
    match entry.state {
        // The case this exists for: handing a peer a listening socket.
        SockState::UnixListening { bound_idx } => bound_ref_inc(bound_idx),
        // Nothing underneath yet — the entry itself is the whole state.
        SockState::Unbound { .. } => {}
        // A connected end, refcounted exactly as `handle_dup` does: the queued
        // descriptor is one more alias of that end. Firefox passes these all
        // the time — every new IPC channel is a socketpair whose far end is
        // sent to the process that will use it — and an EBADF here fails the
        // whole sendmsg, which its IPC layer treats as a dead channel.
        //
        // An end queued on its own connection, or on a ring of connections
        // that are only reachable from each other's queues, is a cycle that
        // no close() breaks. `unix_gc` collects those.
        SockState::UnixConnected { conn_idx, is_a } => {
            let mut conns = UNIX_CONNS.lock();
            if conn_idx >= conns.len() || !conns[conn_idx].in_use { return None; }
            if is_a { conns[conn_idx].refs_a += 1; } else { conns[conn_idx].refs_b += 1; }
        }
        // Referenced above.
        ref s if is_inet_state(s) => {}
        _ => return None,
    }
    // The in-flight copy names the same description.
    if !inet_family(&entry) { vfs::ofd::get(entry.ofd); }
    let x = XferFd::Sock(entry);
    inflight_adj(&x, true);
    Some(x)
}

/// Install an in-flight descriptor into `pid`'s table, consuming its reference.
/// Returns the new fd, or a negative errno **without consuming** it — the
/// caller still owns it and must `xfer_drop` it (see the recvmsg overflow path).
fn xfer_import(pid: u32, x: XferFd, cloexec: bool) -> isize {
    match &x {
        XferFd::Vfs(tf) => vfs::import_fd(pid, *tf, cloexec),
        XferFd::Sock(entry) => {
            let slot = {
                let mut tbls = SOCK_TABLES.lock();
                let Some(tbl) = get_or_create(pid, &mut *tbls) else { return -23 }; // ENFILE
                let Some(slot) = tbl.alloc() else { return -24 };                   // EMFILE
                let mut e = entry.clone();
                e.cloexec = cloexec;
                e.hidden = false; // the receiver holds a plain socket
                tbl.socks[slot] = e.clone();
                // An inet socket changed by another holder while this copy
                // was queued: take the state they published.
                if let Some(row) = tbls.inet.get(&e.ofd).copied() {
                    if e.ofd != 0 {
                        if let Some(t) = find_tbl(pid, &mut *tbls) { row.apply(&mut t.socks[slot]); }
                    }
                }
                slot
            };
            // The receiver reaches it through a descriptor of its own fd table.
            // No room there (RLIMIT_NOFILE): take the slot back untouched, so
            // the descriptor is still the caller's to drop, as the contract says.
            let fd = vfs::install_handle(pid, slot + SOCK_FD_BASE, 0);
            if fd < 0 {
                let mut tbls = SOCK_TABLES.lock();
                if let Some(tbl) = find_tbl(pid, &mut *tbls) {
                    if slot < tbl.socks.len() { tbl.socks[slot] = SockEntry::empty(); }
                }
                return fd;
            }
            inflight_adj(&x, false);
            fd
        }
    }
}

/// Release an in-flight descriptor that never reached a receiver.
/// Caller must hold neither UNIX_CONNS nor BOUND_PATHS.
fn xfer_drop(x: XferFd) {
    inflight_adj(&x, false);
    match x {
        XferFd::Vfs(tf) => vfs::drop_transfer(tf),
        XferFd::Sock(mut entry) => {
            // An inet socket — or an unbound one, which another holder may
            // have bound or connected while this copy was queued — puts its
            // reference under SOCK_TABLES with the state the holders
            // published (see "Shared inet sockets").
            if is_inet_state(&entry.state) || matches!(entry.state, SockState::Unbound { .. }) {
                let p = {
                    let mut tbls = SOCK_TABLES.lock();
                    inet_refresh(&tbls, &mut entry);
                    inet_put(&mut *tbls, &entry)
                };
                inet_put_finish(p, false);
                return;
            }
            match entry.state {
                SockState::UnixListening { bound_idx } => { free_bound_idx(bound_idx); unix_gc(); }
                SockState::UnixConnected { conn_idx, is_a } => unix_end_release(conn_idx, is_a),
                // `xfer_export` admits no other reference-holding state.
                _ => {}
            }
            vfs::ofd::put(entry.ofd);
        }
    }
}

/// pidfd_getfd(2): duplicate descriptor `fd` of process `src` into process
/// `dst`'s table, close-on-exec — an SCM_RIGHTS transfer without the socket,
/// so sockets and VFS descriptors alike keep sharing their open description.
/// EBADF for a descriptor that cannot be transferred.
pub fn dup_fd_across(src: u32, fd: usize, dst: u32) -> isize {
    let Some(x) = xfer_export(src, fd) else { return -9 };
    let r = xfer_import(dst, x.clone(), true);
    if r < 0 { xfer_drop(x); }
    r
}

/// A second in-flight reference to what `x` names, for a MSG_PEEK that
/// returns fds while they stay queued. Linux does the same in unix_peek_fds.
/// The caller holds UNIX_CONNS and passes it as `conns`, because a queued
/// connected end is refcounted there. The other locks taken here (BOUND_PATHS,
/// the description table, the VFS pipe/tmpfs refcounts) are leaves. Returns
/// `None` when what `x` names is already gone, and then takes no reference.
fn xfer_clone_locked(conns: &mut [UnixConn], x: &XferFd) -> Option<XferFd> {
    match x {
        XferFd::Vfs(tf) => Some(XferFd::Vfs(vfs::clone_transfer(tf))),
        XferFd::Sock(entry) => {
            match entry.state {
                SockState::UnixListening { bound_idx } => bound_ref_inc(bound_idx),
                SockState::UnixConnected { conn_idx, is_a } => {
                    if conn_idx >= conns.len() || !conns[conn_idx].in_use { return None; }
                    let c = &mut conns[conn_idx];
                    if is_a { c.refs_a += 1; } else { c.refs_b += 1; }
                }
                _ => {}
            }
            vfs::ofd::get(entry.ofd);
            inflight_adj(x, true);
            Some(XferFd::Sock(entry.clone()))
        }
    }
}

// ── AF_UNIX garbage collector ─────────────────────────────────────────────────
//
// An SCM_RIGHTS fd in a queue holds a reference on what it names. When that is
// a socket, references can form a cycle. Examples: end b queued for b itself;
// two connections each queued in the other; a listener queued on a connection
// that is still waiting in that same listener's backlog. Once every process fd
// is closed, the only references left are inside the cycle. No close() will
// ever release them, so the sockets, and every fd queued on them, leak.
//
// The fix is the one Linux uses (net/unix/garbage.c, unix_gc). The graph's
// nodes are connected ends and listeners:
//   1. Count, for each node, the in-flight copies queued anywhere. A node whose
//      references are *all* in flight (refs == in-flight) is a candidate. Any
//      other live node is a root: some process still holds it.
//   2. Walk outward from the roots. An end's edges are the sockets queued for
//      it (its receive queue). A listener's edges are its embryonic
//      connections: the end B of a connect() not yet accepted. That end has no
//      fd, and only an accept() on the listener can ever reach what is queued
//      for it. Linux's listener receive queue holds exactly these embryos. An
//      embryo of a candidate listener is itself treated as a candidate.
//   3. Candidates not reached are garbage. The fds queued for each such end are
//      lifted out and released. Every socket in the cycle loses its last
//      reference that way, so the ordinary close paths tear them down.
//
// The scan and the lift happen in one UNIX_CONNS critical section, and every
// queue and refcount it reads is guarded by that lock, so the view is
// consistent. While a listener is in flight, SOCK_TABLES (where pending
// connects are recorded) and BOUND_PATHS (listener refcounts) are held as
// well, in the usual SOCK_TABLES > UNIX_CONNS > BOUND_PATHS order. A
// reference in transit is safe too. One example is a batch that recvmsg
// dequeued but has not imported yet; another is an export waiting to be
// queued. That reference is counted in `refs` but in no queue, so its node is
// not a candidate. The cost is a pass over the connection table plus every
// queued fd. The per-process tables are scanned only while a listener is in
// flight. The collector runs only while INFLIGHT_SOCKS is non-zero.

static GC_ACTIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static GC_AGAIN:  core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Node numbers: connected ends first (two per connection), then listeners.
#[inline]
fn gc_node(conn_idx: usize, is_a: bool) -> usize { conn_idx * 2 + (!is_a) as usize }
const GC_LISTENER_BASE: usize = MAX_CONNS * 2;
const GC_NODES: usize = GC_LISTENER_BASE + MAX_BOUND;

/// The graph node an in-flight descriptor names, if it is one.
fn gc_target(x: &XferFd) -> Option<usize> {
    match *x {
        XferFd::Sock(SockEntry { state: SockState::UnixConnected { conn_idx, is_a }, .. })
            if conn_idx < MAX_CONNS => Some(gc_node(conn_idx, is_a)),
        XferFd::Sock(SockEntry { state: SockState::UnixListening { bound_idx }, .. })
            if bound_idx < MAX_BOUND => Some(GC_LISTENER_BASE + bound_idx),
        _ => None,
    }
}

/// Run the collector. It is called after any socket loses a reference, which
/// is the only event that can turn a reachable cycle into garbage. Caller
/// holds no net lock. Re-entry from its own `xfer_drop`, or a run from another
/// CPU, only sets GC_AGAIN, and the active run then does one more pass.
fn unix_gc() {
    use core::sync::atomic::Ordering::{Acquire, Release, AcqRel};
    loop {
        if INFLIGHT_SOCKS.load(Acquire) == 0 { return; }
        if GC_ACTIVE.swap(true, AcqRel) { GC_AGAIN.store(true, Release); return; }
        GC_AGAIN.store(false, Release);
        let dead = unix_gc_collect();
        for x in dead { xfer_drop(x); }
        GC_ACTIVE.store(false, Release);
        if !GC_AGAIN.swap(false, AcqRel) { return; }
    }
}

/// Steps 1 to 3 above. Returns the fds lifted off the garbage ends' queues.
fn unix_gc_collect() -> alloc::vec::Vec<XferFd> {
    let mut out = alloc::vec::Vec::new();
    let listeners = INFLIGHT_LISTENERS.load(core::sync::atomic::Ordering::Acquire) != 0;
    // Pending connects as (connection, listener sock_id). They are read under
    // SOCK_TABLES, which stays held to the end so that no accept() can turn
    // an embryo into an ordinary end (and no connection slot can be freed
    // and reused) while the collector works.
    let tbls = if listeners { Some(SOCK_TABLES.lock()) } else { None };
    let mut conns = UNIX_CONNS.lock();
    let mut pending: alloc::vec::Vec<(usize, u64)> = alloc::vec::Vec::new();
    if listeners {
        for (ci, c) in conns.iter().enumerate() {
            if c.in_use && c.pending_sid != 0 { pending.push((ci, c.pending_sid)); }
        }
    }
    let bound = if listeners { Some(BOUND_PATHS.lock()) } else { None };

    // 1. In-flight copies per node.
    let mut inflight = alloc::vec![0u32; GC_NODES];
    let mut any = false;
    for c in conns.iter().filter(|c| c.in_use) {
        for b in c.fdq_ab.iter().chain(c.fdq_ba.iter()) {
            for x in b.fds.iter() {
                if let Some(n) = gc_target(x) { inflight[n] += 1; any = true; }
            }
        }
    }
    if !any { return out; }

    // Node state: 0 = root or not a node, 1 = candidate not reached yet,
    // 2 = candidate reached.
    let mut st = alloc::vec![0u8; GC_NODES];
    let mut ncand = 0usize;
    for (n, &k) in inflight.iter().enumerate() {
        if k == 0 { continue; }
        // refs < k would mean an in-flight copy that holds no reference.
        // That is never true, but if it were, collecting would free a socket
        // still in use, so such a node stays a root.
        let refs = if n < GC_LISTENER_BASE {
            let c = &conns[n / 2];
            if !c.in_use { continue; }
            if n % 2 == 0 { c.refs_a } else { c.refs_b }
        } else {
            match bound.as_ref() {
                Some(bp) if bp[n - GC_LISTENER_BASE].in_use => bp[n - GC_LISTENER_BASE].refs,
                _ => continue,
            }
        };
        if refs != 0 && refs == k { st[n] = 1; ncand += 1; }
    }
    if ncand == 0 { return out; }

    // Embryos of candidate listeners: (listener node, end B node). The end
    // B of a pending connect has no fd and no in-flight copy; its only
    // holder is the backlog. With a root listener it stays a root.
    let mut embryos: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
    if let Some(bp) = bound.as_ref() {
        for &(ci, sock_id) in pending.iter() {
            if !conns[ci].in_use { continue; }
            let Some(bi) = bp.iter().position(|b| b.in_use && b.sock_id == sock_id) else { continue };
            let ln = GC_LISTENER_BASE + bi;
            let en = gc_node(ci, false);
            if st[ln] == 1 && st[en] == 0 { st[en] = 1; embryos.push((ln, en)); }
        }
    }

    // 2. Mark everything reachable from a root.
    fn reach(n: usize, conns: &[UnixConn], embryos: &[(usize, usize)],
             st: &mut [u8], stack: &mut alloc::vec::Vec<usize>) {
        let mut visit = |m: usize, st: &mut [u8]| { if st[m] == 1 { st[m] = 2; stack.push(m); } };
        if n >= GC_LISTENER_BASE {
            for &(ln, en) in embryos.iter() { if ln == n { visit(en, st); } }
            return;
        }
        let c = &conns[n / 2];
        let q = if n % 2 == 0 { &c.fdq_ba } else { &c.fdq_ab };
        for b in q.iter() {
            for x in b.fds.iter() {
                if let Some(m) = gc_target(x) { visit(m, st); }
            }
        }
    }
    let mut stack = alloc::vec::Vec::new();
    for ci in (0..conns.len()).filter(|&ci| conns[ci].in_use) {
        for is_a in [true, false] {
            let n = gc_node(ci, is_a);
            if st[n] == 0 { reach(n, &*conns, &embryos, &mut st, &mut stack); }
        }
    }
    // A root listener's embryos are roots already (see above), so listeners
    // need no seeding of their own.
    while let Some(n) = stack.pop() {
        reach(n, &*conns, &embryos, &mut st, &mut stack);
    }

    // 3. Purge the receive queues of the ends nothing reached. An unreached
    // listener has no queue of its own; its embryos are ends and are purged
    // here, which drops the copies that kept it alive.
    for n in 0..GC_LISTENER_BASE {
        if st[n] == 1 { out.append(&mut conns[n / 2].take_rx_fds(n % 2 == 0)); }
    }
    drop(bound);
    drop(conns);
    drop(tbls);
    out
}

// ── Smoltcp Integration ───────────────────────────────────────────────────────

pub struct NetStack {
    pub interface: Interface,
    pub socket_set: SocketSet<'static>,
    /// `Some` only on the NIC's stack. The loopback stack runs no DHCP client:
    /// a dhcpv4 socket there would broadcast DISCOVERs into its own queue and
    /// then receive them back forever.
    pub dhcp_handle: Option<SocketHandle>,
    /// `Some` only on the loopback stack. `syngate::LoopDev` owns the queue that
    /// carries a frame from transmit back to receive, so it has to persist
    /// across polls; the virtio wrapper is a stateless `dev_idx` and is rebuilt
    /// at every poll instead.
    pub loopback_dev: Option<syngate::LoopDev>,
    /// TCP sockets whose fd is closed but whose connection is still shutting
    /// down (FIN or RST not yet out, or not yet acknowledged), each with the
    /// tick after which it is dropped regardless. `reap_orphans` removes
    /// them once smoltcp reaches Closed or TimeWait.
    pub orphans: alloc::vec::Vec<(SocketHandle, u64)>,
    /// One datagram per ICMP socket that a MSG_PEEK (or FIONREAD) took out of
    /// smoltcp, which has no peek for `icmp::Socket`. Every read, poll and
    /// FIONREAD of that socket consults it first; a non-peek read hands it
    /// out and clears it, and releasing the socket drops it (handles are
    /// reused). Keyed by handle, so dup/fork copies share it like the queue.
    pub icmp_peeked: alloc::collections::BTreeMap<SocketHandle, (alloc::vec::Vec<u8>, IpAddress)>,
    /// The TCP listeners on this stack, each with its accept queue.
    pub listens: alloc::vec::Vec<listen::Listen>,
    /// EPOLLET edge-seq of each TCP, UDP and ICMP socket: bumped (from
    /// `INET_SEQ_GEN`, so a reused handle never repeats a value) by every
    /// daemon pass in which the network moved the socket or handed it a
    /// datagram (see `readiness_moved`), and by a shutdown. `handle_poll`
    /// reports it so an EPOLLET interest fires once per edge instead of on
    /// every epoll_wait while the socket stays writable. A socket that never
    /// moved reports 0. Listeners keep theirs in `listen::Listen::seq`.
    pub inet_seq: alloc::collections::BTreeMap<SocketHandle, u64>,
}

/// Source of `NetStack::inet_seq` values: global and monotonic, never
/// u64::MAX (epoll's "not yet delivered" mark).
static INET_SEQ_GEN: AtomicU64 = AtomicU64::new(1);

/// smoltcp buffer sizes for TCP sockets. The receive buffer is the
/// advertised window, and a connection moves at most one window per round
/// trip: 64 KiB (the previous size) capped the real PC at 2.0 Mbit/s from a
/// server ~250 ms away (4464 frames in 24.7 s, none dropped) and a CDN at
/// ~20 ms at ~25 Mbit/s. 2 MiB (window scale 6, negotiated by smoltcp from
/// the capacity) allows ~64 Mbit/s at 250 ms and the link rate nearby.
/// The buffers are allocated up front, so past `TCP_BIG_SOCKETS` TCP sockets
/// on a stack new ones get the small sizes, and an allocation that fails
/// falls back by halves (a 2 MiB buffer is one physically contiguous block).
const TCP_RX_BUF: usize = 2 * 1024 * 1024;
const TCP_TX_BUF_DEFAULT: usize = 256 * 1024;
/// Send buffer of a big TCP socket: `tcp.sndbuf_kb=` on the kernel command line.
static TCP_TX_BUF_CFG: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(TCP_TX_BUF_DEFAULT);
/// Active polled-NIC pass cadence in ns (`net.cadence_us=`).
static NET_CADENCE_NS: AtomicU64 = AtomicU64::new(1_000_000);
fn tcp_tx_buf() -> usize { TCP_TX_BUF_CFG.load(Ordering::Relaxed) }

/// Read the TCP / cadence knobs once (kernel command line).
fn read_net_knobs() {
    use smoltcp::socket::tcp as t;
    match drivers::nvidia_probe::cmdline_word("tcp.cc") {
        Some("reno") => t::CC_MODE.store(1, Ordering::Relaxed),
        Some("cubic") => t::CC_MODE.store(2, Ordering::Relaxed),
        Some("none") => t::CC_MODE.store(0, Ordering::Relaxed),
        None => {} // default: CUBIC (smoltcp CC_MODE)
        Some(_) => {
            extern "C" { fn arch_serial_putc(c: u8); }
            for &b in b"netd: unknown tcp.cc= value (want none|reno|cubic), using cubic\n" { unsafe { arch_serial_putc(b); } }
        }
    }
    if let Some(v) = drivers::nvidia_probe::cmdline_opt("tcp.iw") { t::CC_IW.store(v.clamp(1, 1000) as u32, Ordering::Relaxed); }
    if let Some(v) = drivers::nvidia_probe::cmdline_opt("tcp.rto_min_ms") { t::RTO_MIN_MS.store(v.clamp(1, 5000) as u32, Ordering::Relaxed); }
    if let Some(v) = drivers::nvidia_probe::cmdline_opt("tcp.sndbuf_kb") { TCP_TX_BUF_CFG.store((v.clamp(64, 16384) as usize) * 1024, Ordering::Relaxed); }
    if let Some(v) = drivers::nvidia_probe::cmdline_opt("net.cadence_us") { NET_CADENCE_NS.store(v.clamp(50, 100_000) * 1000, Ordering::Relaxed); }
}
const TCP_RX_BUF_SMALL: usize = 256 * 1024;
const TCP_TX_BUF_SMALL: usize = 64 * 1024;
const TCP_BUF_MIN: usize = 64 * 1024;
const TCP_BIG_SOCKETS: usize = 48;
/// Initial advertised receive window of the autotuning (Linux starts near 64 KiB too).
const TCP_RX_WIN_INIT: usize = 64 * 1024;

/// A zeroed buffer of `want` bytes, or the largest power-of-two fraction of
/// it down to `min` that the allocator can give.
fn alloc_net_buf(want: usize, min: usize) -> alloc::vec::Vec<u8> {
    let mut size = want;
    loop {
        let mut v: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        if v.try_reserve_exact(size).is_ok() {
            v.resize(size, 0);
            return v;
        }
        if size <= min { return alloc::vec![0; min]; }
        size /= 2;
    }
}

/// A new TCP socket for `set`, with buffers sized as above.
fn new_tcp_socket(set: &SocketSet<'static>) -> tcp::Socket<'static> {
    let ntcp = set.iter().filter(|(_, sk)| matches!(sk, smoltcp::socket::Socket::Tcp(_))).count();
    let (rx, tx) = if ntcp < TCP_BIG_SOCKETS { (TCP_RX_BUF, tcp_tx_buf()) } else { (TCP_RX_BUF_SMALL, TCP_TX_BUF_SMALL) };
    let rxb = alloc_net_buf(rx, TCP_BUF_MIN);
    let txb = alloc_net_buf(tx, TCP_BUF_MIN / 4);
    procnet::TCP_RXBUF_LAST.store(rxb.len() as u64, Ordering::Relaxed);
    let mut s = tcp::Socket::new(tcp::SocketBuffer::new(rxb), tcp::SocketBuffer::new(txb));
    // Receive-window autotuning: advertise TCP_RX_WIN_INIT first and grow
    // toward the buffer only while the reader drains a window per RTT, so a
    // sender's slow start cannot overshoot a slow link's queue (a 256-frame
    // driver RX queue, an AP) by the whole 2 MiB.
    s.set_rx_autotune(Some(TCP_RX_WIN_INIT));
    s
}

/// A new UDP socket. The receive side holds a burst: QUIC (YouTube over
/// HTTP/3) arrives in trains of tens of datagrams, and the old 8/16-slot
/// buffers dropped the rest of a train before the reader ran.
fn new_udp_socket() -> udp::Socket<'static> {
    let rx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 512], alloc_net_buf(UDP_RX_BUF, 64 * 1024));
    let tx = udp::PacketBuffer::new(alloc::vec![udp::PacketMetadata::EMPTY; 128], alloc_net_buf(UDP_TX_BUF, 64 * 1024));
    udp::Socket::new(rx, tx)
}

/// A UDP socket's payload buffers (SO_RCVBUF / SO_SNDBUF report them).
const UDP_RX_BUF: usize = 512 * 1024;
const UDP_TX_BUF: usize = 128 * 1024;

/// The poll tag of one smoltcp socket (`handle` on stack `sid`).
/// `handle_poll` reports it and the daemon wakes it when that socket's
/// readiness moved during a pass.
fn inet_tag(sid: Sid, handle: SocketHandle) -> u64 {
    sched::poll_key_bit(inet_key(sid, handle))
}

/// The poll tag of a set of exact keys (a wildcard UDP server's, one per
/// stack socket, or any `sock_keys`). `PollTag` holds three keys; past that,
/// every key's bloom bit goes in keyless instead. Keeping three keys and
/// adding the rest as extra bits would be unsound: an extra key whose bit
/// equals a kept key's bit would be refined by the kept key's fingerprint,
/// and its wake dropped.
fn keys_tag(keys: &[u64]) -> sched::PollTag {
    if keys.len() <= 3 { return sched::PollTag::keyed(keys, 0); }
    sched::PollTag::bits(keys.iter().fold(0u64, |b, &k| b | sched::poll_key_bit(k)))
}

/// The SOCKSTATE key of socket description `ofd` (`poll_class::SOCKSTATE`):
/// the key of a socket that has no object to be woken through yet (unbound,
/// bound but not listening, an ICMP socket before its first send).
fn sock_state_key(ofd: u32) -> u64 { sched::poll_key(sched::poll_class::SOCKSTATE, ofd) }

/// The exact keys the readiness of socket `e` is published under, for every
/// state but the AF_UNIX objects (which have their own, direction-aware keys:
/// `unix_end_keys`, the listener's foreign tag). The one helper behind the
/// poll answer (`handle_poll`), the park of a blocking send/recv/connect
/// (`fd_wake_tag`) and the wake of a state change (`sock_state_moved`), so
/// what a poller parks under and what is woken cannot drift apart:
///
/// - a socket with a smoltcp object: that object's `inet_key` (each stack's
///   for a wildcard UDP server, `udpany`), woken by the daemon's per-pass
///   `readiness_moved`;
/// - a TCP listener: `listen::wake_key`, woken by `listen::maintain`;
/// - anything else: `sock_state_key`, woken when the state changes.
///
/// `None`: not one of these (an AF_UNIX connection or listener, or no
/// description to key on). Caller holds no net lock (`udpany::of`).
fn sock_keys(e: &SockEntry) -> Option<alloc::vec::Vec<u64>> {
    let v = match e.state {
        SockState::InetConnected { socket_handle, sid, .. } => {
            match udpany::of(e.ofd).filter(|_| e.sock_type == SOCK_DGRAM as u8) {
                Some(hs) => udpany::order(&hs, (sid, socket_handle)).iter()
                    .map(|&(sd, h)| inet_key(sd, h)).collect(),
                None => alloc::vec![inet_key(sid, socket_handle)],
            }
        }
        SockState::IcmpBound { socket_handle, sid } => alloc::vec![inet_key(sid, socket_handle)],
        SockState::InetListening { lid, .. } => alloc::vec![listen::wake_key(lid)],
        SockState::UnixConnected { .. } | SockState::UnixPendingAccept { .. }
        | SockState::UnixListening { .. } => return None,
        SockState::None | SockState::Unbound { .. } | SockState::InetBound { .. }
        | SockState::IcmpUnbound => {
            if e.ofd == 0 { return None; }
            alloc::vec![sock_state_key(e.ofd)]
        }
    };
    Some(v)
}

/// The poll tag of socket `e`: its `sock_keys`, keyed (`keys_tag`).
fn sock_tag(e: &SockEntry) -> Option<sched::PollTag> { sock_keys(e).map(|k| keys_tag(&k)) }

/// What a state change must wake: the keys socket `e` was polled under
/// before it (`sock_keys`), and its description's SOCKSTATE key. Taken
/// before the change, while a wildcard server's mirrors still exist. Empty
/// for an AF_UNIX connection or listener: their own producers wake them.
fn sock_wake_set(e: &SockEntry) -> alloc::vec::Vec<u64> {
    let Some(mut v) = sock_keys(e) else { return alloc::vec::Vec::new() };
    if !v.contains(&sock_state_key(e.ofd)) { v.push(sock_state_key(e.ofd)); }
    v
}

/// A socket's state changed (or a shutdown/close changed what its poll
/// answers): every poller and blocked sender/receiver re-probes. `keys` is
/// `sock_wake_set` of the old state. The retag comes first, so a poller that
/// collected the old keys and parks after these wakes re-probes instead
/// (`sched::poll_retag`). Caller holds no net lock.
fn sock_state_moved(keys: &[u64]) {
    if keys.is_empty() { return; }
    sched::poll_retag();
    for &k in keys { sched::wake_poll_key(k); }
}

/// Run a handler that may change `fd`'s state (bind, connect, listen) or what
/// its poll answers (`always`: shutdown, close), then wake whatever was
/// polled or parked under the old state (`sock_state_moved`). This is the
/// producer for the SOCKSTATE keys and for a socket whose object changes; the
/// paths that change a state outside these calls (the first sendto/send,
/// `udp_migrate`) wake on their own.
fn with_state_wake(pid: u32, fd: usize, always: bool, f: impl FnOnce() -> Message) -> Message {
    let before = sock_entry(pid, fd).map(|e| { let k = sock_wake_set(&e); (e, k) });
    let r = f();
    if let Some((b, keys)) = before {
        let moved = always || sock_entry(pid, fd).map_or(true, |a| a.state != b.state);
        if moved { sock_state_moved(&keys); }
    }
    r
}

/// The exact key of an inet socket (lane deskbusy2): what its poll reports and
/// what the daemon's per-pass readiness wake uses (`readiness_moved`), so a
/// streaming socket's edges no longer wake every poller sharing its bloom
/// bit (~1 000 spurious wakes/s on the laptop during YouTube).
fn inet_key(sid: Sid, handle: SocketHandle) -> u64 {
    use core::hash::{Hash, Hasher};
    struct Fnv(u64);
    impl Hasher for Fnv {
        fn finish(&self) -> u64 { self.0 }
        fn write(&mut self, b: &[u8]) {
            for &x in b { self.0 ^= x as u64; self.0 = self.0.wrapping_mul(0x100_0000_01b3); }
        }
    }
    let mut f = Fnv(0xcbf2_9ce4_8422_2325);
    handle.hash(&mut f);
    sched::poll_key(sched::poll_class::INET, (f.finish() as u32 & 0x1fff_ffff) | (((sid as u32) & 7) << 29))
}

/// How long a closed TCP connection may take to finish its shutdown before
/// its socket is dropped anyway (ticks are 10 ms).
const ORPHAN_TICKS: u64 = 60 * 100;

/// Release the smoltcp socket behind a closed fd. A TCP connection ends the
/// way close(2) ends it on Linux: a FIN after whatever was still queued to
/// send, or an RST when received data was never read. The socket stays in the
/// set as an orphan until that exchange is done. Removing it outright, as
/// this server used to, sent nothing at all: the peer never learned the
/// connection was gone (a server's reply never reached EOF at the client, a
/// client's close left the server holding the connection until it timed out).
/// Every other socket kind is removed at once.
fn release_inet_socket(s: &mut NetStack, handle: SocketHandle) {
    use smoltcp::socket::AnySocket;
    let mut orphan = false;
    if let Some((_, sock)) = s.socket_set.iter_mut().find(|(h, _)| *h == handle) {
        // A UDP socket closed right after a send: on Linux the datagram is
        // on its way by then; here it still waits in smoltcp for the
        // daemon's next pass, so the socket stays until its queue is out
        // (`reap_orphans`). Dropping it lost the datagram (lane udp2).
        if let Some(u) = udp::Socket::downcast_mut(sock) {
            if u.send_pending() { orphan = true; }
        }
        // Same for a raw/ICMP socket: close() right after sendto must still
        // transmit the queued packet (Linux sends synchronously).
        if let Some(i) = icmp::Socket::downcast_mut(sock) {
            if i.send_pending() { orphan = true; }
        }
        if let Some(r) = raw::Socket::downcast_mut(sock) {
            if r.send_pending() { orphan = true; }
        }
        if let Some(t) = tcp::Socket::downcast_mut(sock) {
            match t.state() {
                tcp::State::Established | tcp::State::CloseWait | tcp::State::SynReceived => {
                    if t.recv_queue() > 0 { t.abort(); } else { t.close(); }
                    orphan = true;
                }
                // shutdown(SHUT_WR) already sent the FIN; let it finish.
                tcp::State::FinWait1 | tcp::State::FinWait2
                | tcp::State::Closing | tcp::State::LastAck => orphan = true,
                _ => {}
            }
        }
    }
    if orphan {
        s.orphans.push((handle, sched::ticks() + ORPHAN_TICKS));
    } else {
        s.icmp_peeked.remove(&handle);
        s.socket_set.remove(handle);
    }
}

/// The destination of a ping socket's sendto: a sockaddr_in, or for an
/// AF_INET6 ping socket a sockaddr_in6 (a v4-mapped address is EINVAL).
fn icmp_dest(domain: usize, addr_ptr: usize, addrlen: usize) -> Result<IpAddress, i32> {
    if domain == AF_INET6 {
        let a = sa::parse_for(AF_INET6, addr_ptr, addrlen)?;
        return match a.ip { IpAddress::Ipv6(_) => Ok(a.ip), _ => Err(-22) };
    }
    let sin_addr = sched::uaccess::read_user::<u32>(addr_ptr + 4).ok_or(-14)?;
    Ok(IpAddress::from(smoltcp::wire::Ipv4Address::from_bytes(&sin_addr.to_ne_bytes())))
}

/// `icmp_recv_locked` for a socket that may carry an ICMP6_FILTER: datagrams
/// whose type the filter blocks are dropped (also under `peek`, which would
/// otherwise see the same one forever). `v6` sockets read the bare ICMPv6
/// message (raw(7): no IPv6 header on raw ICMPv6 sockets).
fn icmp_recv_filtered(s: &mut NetStack, h: SocketHandle, cap: usize, peek: bool, raw: bool, v6: bool,
                      filt: &[u32; 8]) -> Result<(alloc::vec::Vec<u8>, usize, IpAddress), i32>
{
    let raw = raw && !v6;
    if !v6 || *filt == [0u32; 8] { return icmp_recv_locked(s, h, cap, peek, raw); }
    loop {
        let (d, full, from) = icmp_recv_locked(s, h, usize::MAX, peek, raw)?;
        let ty = d.first().copied().unwrap_or(0) as usize;
        if filt[ty >> 5] & (1u32 << (ty & 31)) != 0 {
            if peek { let _ = icmp_recv_locked(s, h, 0, false, raw); }
            continue;
        }
        let n = full.min(cap);
        return Ok((d[..n].to_vec(), full, from));
    }
}

/// The next datagram of an ICMP socket: its first `cap` bytes, its full
/// length and its source; EAGAIN when none is queued. `peek` leaves it
/// queued — in `icmp_peeked`, since smoltcp's `icmp::Socket` cannot peek.
fn icmp_recv_locked(s: &mut NetStack, h: SocketHandle, cap: usize, peek: bool, raw: bool)
    -> Result<(alloc::vec::Vec<u8>, usize, IpAddress), i32>
{
    if raw {
        // A SOCK_RAW socket reads the whole IP datagram (raw(7)): put back the
        // IPv4 header smoltcp's ICMP socket consumed. smoltcp keeps no
        // TTL/TOS/id per datagram, so they read 64/0/0.
        let (d, _, from) = icmp_recv_locked(s, h, usize::MAX, peek, false)?;
        let dst = s.interface.ipv4_addr().map(|a| a.0).unwrap_or([0; 4]);
        #[allow(unreachable_patterns)]
        let src = match from { IpAddress::Ipv4(a) => a.0, _ => [0; 4] };
        let total = (20 + d.len()).min(0xffff) as u16;
        let mut v = alloc::vec::Vec::with_capacity(20 + d.len());
        v.extend_from_slice(&[0x45, 0, (total >> 8) as u8, total as u8, 0, 0, 0x40, 0, 64, 1, 0, 0]);
        v.extend_from_slice(&src);
        v.extend_from_slice(&dst);
        let mut sum: u32 = 0;
        for c in v.chunks(2) { sum += u16::from_be_bytes([c[0], c[1]]) as u32; }
        while sum > 0xffff { sum = (sum & 0xffff) + (sum >> 16); }
        let ck = !(sum as u16);
        v[10] = (ck >> 8) as u8; v[11] = ck as u8;
        v.extend_from_slice(&d);
        let full = v.len();
        v.truncate(cap);
        return Ok((v, full, from));
    }
    let stashed = if peek { s.icmp_peeked.get(&h).cloned() } else { s.icmp_peeked.remove(&h) };
    let (data, from) = match stashed {
        Some(d) => d,
        None => {
            let (p, from) = s.socket_set.get_mut::<icmp::Socket>(h).recv().map_err(|_| -11i32)?;
            let d = p.to_vec();
            if peek { s.icmp_peeked.insert(h, (d.clone(), from)); }
            (d, from)
        }
    };
    let full = data.len();
    Ok((data[..full.min(cap)].to_vec(), full, from))
}

/// POLLIN for an ICMP socket: a datagram in smoltcp or one already peeked.
fn icmp_readable(s: &mut NetStack, h: SocketHandle) -> bool {
    s.icmp_peeked.contains_key(&h) || s.socket_set.get_mut::<icmp::Socket>(h).can_recv()
}

/// Drop the orphaned TCP sockets whose shutdown has finished (or timed out),
/// and the closed UDP sockets whose last datagrams left. Runs after each
/// daemon poll, so an abort's RST has already been sent.
fn reap_orphans(s: &mut NetStack) {
    use smoltcp::socket::AnySocket;
    if s.orphans.is_empty() { return; }
    let now = sched::ticks();
    let set = &mut s.socket_set;
    s.orphans.retain(|&(h, deadline)| {
        let done = match set.iter().find(|(hh, _)| *hh == h).map(|(_, sk)| sk) {
            Some(sk) => match tcp::Socket::downcast(sk) {
                Some(t) => matches!(t.state(), tcp::State::Closed | tcp::State::TimeWait),
                None => if let Some(u) = udp::Socket::downcast(sk) { !u.send_pending() }
                    else if let Some(i) = icmp::Socket::downcast(sk) { !i.send_pending() }
                    else if let Some(r) = raw::Socket::downcast(sk) { !r.send_pending() }
                    else { true },
            },
            None => return false,
        } || now >= deadline;
        if done { set.remove(h); }
        !done
    });
}

/// Every stack, indexed by stack id (see `iface`): `STACKS[LO]` is loopback
/// (127.0.0.0/8), `STACKS[netif id + 1]` the stack of each netif in
/// `drivers::netdev`'s registry.
///
/// Each is a separate `Interface` with its own `SocketSet`, not a second
/// address on one interface: two interfaces sharing one socket set would let
/// whichever polls first claim a socket's pending segment, so a loopback SYN
/// could be emitted onto the wire and never delivered (and a flow routed via
/// one NIC could leave through another). Separate sets make the choice
/// explicit and make it once, at bind/connect time.
pub static STACKS: [Mutex<Option<NetStack>>; MAX_STACKS] = [const { Mutex::new(None) }; MAX_STACKS];
pub static NFTABLES: Mutex<nftables::NftablesEngine> = Mutex::new(nftables::NftablesEngine::new());

/// smoltcp's view of one netif for one daemon pass (rebuilt every pass; the
/// driver keeps all device state).
pub struct NicDevice {
    pub netif: drivers::netdev::Netif,
    /// Frames handed to smoltcp since the last `None` (see `RX_BATCH`).
    pub batch: u32,
    /// Frames taken from the NIC during this daemon pass.
    pub pass_rx: u64,
}

impl NicDevice {
    pub fn new(netif: drivers::netdev::Netif) -> Self { Self { netif, batch: 0, pass_rx: 0 } }
}

/// smoltcp's ingress loop takes frames until the device says `None`, and only
/// then runs egress (ACKs, window updates). Ending a run every `RX_BATCH`
/// frames lets one ACK go out per 64 frames of a burst rather than one per
/// pass; `Interface::poll` loops back into ingress while either side
/// progressed, so nothing is left behind.
const RX_BATCH: u32 = 64;

/// `monotonic_ns()` of the last frame received or sent on any NIC: the
/// daemon polls at 1 kHz for `ACTIVE_NS` after it.
static LAST_TRAFFIC_NS: AtomicU64 = AtomicU64::new(0);
const ACTIVE_NS: u64 = 200_000_000;
/// Longest idle sleep of the net daemon when every NIC's receive path kicks it
/// (see the end of `net_daemon`).
const IDLE_CAP_NS: u64 = 1_000_000_000;
/// Fallback sleep while a NIC refuses frames (`tx_ready` false) and smoltcp
/// only wants to transmit: the driver's drain kick normally ends it first.
const TX_HELD_FALLBACK_NS: u64 = 10_000_000;

impl<'a> smoltcp::phy::Device for NicDevice {
    type RxToken<'b> = RxToken where Self: 'b;
    type TxToken<'b> = TxToken where Self: 'b;

    fn receive(&mut self, _timestamp: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        if self.batch >= RX_BATCH {
            self.batch = 0;
            return None;
        }
        // This host's own multicast and broadcast first (see "Local copies").
        if let Some(f) = take_injected(self.netif.id) {
            self.batch += 1;
            return Some((RxToken { buf: f }, TxToken { netif: self.netif }));
        }
        let mut buf = [0u8; 2048];
        if let Some(len) = self.netif.recv(&mut buf) {
            self.batch += 1;
            self.pass_rx += 1;
            let stats = &iface::STATS[self.netif.id as usize];
            stats.rx(len);
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
                            stats.rx_drop.fetch_add(1, Ordering::Relaxed);
                            return None;
                        }
                    }
                }
            }

            if fwd::nic_rx(self.netif.id, &buf[..len]) { return self.receive(_timestamp); }
            let mut data = alloc::vec![0u8; len];
            data.copy_from_slice(&buf[..len]);
            // Weak host: a unicast packet for another interface's address
            // goes to that interface's stack, addressed to its MAC.
            if len >= 34 && u16::from_be_bytes([buf[12], buf[13]]) == 0x0800 && buf[0] & 1 == 0 {
                let dst = [buf[30], buf[31], buf[32], buf[33]];
                if let Some(sid) = iface::weak_host_holder(self.netif.id, dst) {
                    if let Some(mac) = drivers::netdev::get((sid - 1) as usize).and_then(|n| n.mac()) {
                        data[..6].copy_from_slice(&mac);
                        stats.rx_weak_host.fetch_add(1, Ordering::Relaxed);
                        inject_frame(sid, data);
                        return self.receive(_timestamp);
                    }
                }
            }
            Some((RxToken { buf: data }, TxToken { netif: self.netif }))
        } else {
            None
        }
    }

    fn transmit(&mut self, timestamp: Instant) -> Option<Self::TxToken<'_>> {
        // Backpressure: a driver whose queue is past its high-water mark
        // gets nothing more; smoltcp keeps the segments (TCP in its send
        // buffer, UDP in the socket's) and retries on a later pass. The
        // driver kicks the daemon when its queue drains.
        if !self.netif.tx_ready() {
            let held = iface::STATS[self.netif.id as usize].tx_held.fetch_add(1, Ordering::Relaxed) + 1;
            tx_held_episode(self.netif.id as usize, timestamp.total_millis(), held);
            return None;
        }
        TX_HELD_SINCE.store(0, Ordering::Relaxed);
        Some(TxToken { netif: self.netif })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        // The Ethernet frame: smoltcp's Ethernet MTU includes the 14-byte
        // header, so this is IP MTU 1500 (it was 1500, i.e. IP MTU 1486,
        // which also made every fragment's payload a non-multiple of 8).
        caps.max_transmission_unit = 1514;
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
    netif: drivers::netdev::Netif,
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

        let stats = &iface::STATS[self.netif.id as usize];
        // A host talking to its own interface address (see `local_delivery`).
        if transmit_allowed && local_delivery(self.netif.id, &buf) {
            return result;
        }
        if transmit_allowed {
            if self.netif.send(&buf) {
                stats.tx(len);
            } else {
                stats.tx_drop.fetch_add(1, Ordering::Relaxed);
            }
            LAST_TRAFFIC_NS.store(sched::monotonic_ns(), Ordering::Relaxed);
        } else {
            stats.tx_drop.fetch_add(1, Ordering::Relaxed);
        }
        result
    }
}

/// A frame smoltcp built for one of this host's own addresses never reaches
/// a wire: the neighbour of our own IP is ourselves. An ARP request for an
/// address a local interface holds is answered here (the reply is queued for
/// the sender's stack), and an IPv4 frame addressed to a local interface's
/// MAC (which that answer makes smoltcp use) is queued for that interface's
/// stack, as Linux's `local` routing table delivers it. Without the answer
/// the datagram waited forever for an ARP reply that nobody sends, and
/// blocked its socket's later sends. True when `buf` was consumed.
fn local_delivery(from: u8, buf: &[u8]) -> bool {
    if buf.len() < 14 { return false; }
    let proto = u16::from_be_bytes([buf[12], buf[13]]);
    if proto == 0x0806 && buf.len() >= 42 && u16::from_be_bytes([buf[20], buf[21]]) == 1 {
        let tip = smoltcp::wire::Ipv4Address::from_bytes(&buf[38..42]);
        let Some(holder) = iface::holder_of(IpAddress::Ipv4(tip)) else { return false };
        if holder == LO { return false; }
        let Some(hmac) = drivers::netdev::get((holder - 1) as usize).and_then(|n| n.mac()) else { return false };
        let mut r = alloc::vec![0u8; 42];
        r[0..6].copy_from_slice(&buf[6..12]);
        r[6..12].copy_from_slice(&hmac);
        r[12..14].copy_from_slice(&[0x08, 0x06]);
        r[14..20].copy_from_slice(&buf[14..20]);
        r[20..22].copy_from_slice(&[0, 2]);
        r[22..28].copy_from_slice(&hmac);
        r[28..32].copy_from_slice(&tip.0);
        r[32..38].copy_from_slice(&buf[22..28]);
        r[38..42].copy_from_slice(&buf[28..32]);
        inject_frame(from as Sid + 1, r);
        return true;
    }
    // IPv6: a neighbour solicitation for an address a local interface holds
    // is answered here (the same shortcut as ARP above), and a unicast frame
    // for a local interface's MAC is that interface's own, as for IPv4.
    if proto == 0x86dd && buf.len() >= 78 && buf[20] == 58 && buf[54] == 135 {
        let tgt: [u8; 16] = buf[62..78].try_into().unwrap();
        let src: [u8; 16] = buf[22..38].try_into().unwrap();
        if src == [0; 16] { return false; }
        let Some(a) = ip6::addrs6().into_iter().find(|a| a.addr == tgt && a.index >= 2) else { return false };
        let Some(hmac) = drivers::netdev::get((a.index - 2) as usize).and_then(|n| n.mac()) else { return false };
        let mut r = alloc::vec![0u8; 14 + 40 + 32];
        r[0..6].copy_from_slice(&buf[6..12]);
        r[6..12].copy_from_slice(&hmac);
        r[12..14].copy_from_slice(&[0x86, 0xdd]);
        r[14] = 0x60;
        r[18..20].copy_from_slice(&32u16.to_be_bytes());
        r[20] = 58;
        r[21] = 255;
        r[22..38].copy_from_slice(&tgt);
        r[38..54].copy_from_slice(&src);
        r[54] = 136;
        r[58] = 0x60; // solicited | override
        r[62..78].copy_from_slice(&tgt);
        r[78] = 2;
        r[79] = 1;
        r[80..86].copy_from_slice(&hmac);
        let mut ic = smoltcp::wire::Icmpv6Packet::new_unchecked(&mut r[54..]);
        ic.fill_checksum(&IpAddress::Ipv6(smoltcp::wire::Ipv6Address(tgt)), &IpAddress::Ipv6(smoltcp::wire::Ipv6Address(src)));
        inject_frame(from as Sid + 1, r);
        return true;
    }
    if (proto == 0x0800 || proto == 0x86dd) && buf[0] & 1 == 0 {
        for i in 0..drivers::netdev::count() {
            if drivers::netdev::get(i).and_then(|n| n.mac()).map_or(false, |m| m[..] == buf[0..6]) {
                inject_frame(i as Sid + 1, buf.to_vec());
                return true;
            }
        }
    }
    false
}

// ── Local copies of multicast and broadcast ──────────────────────────────────
//
// Linux hands this host a copy of every broadcast it sends and of every
// multicast datagram whose socket has IP_MULTICAST_LOOP on (the default):
// ip_mc_output loops it back through the receive path, where the sockets
// bound to its port (and members of its group) get it like any other. Here
// the send path builds that copy as the Ethernet frame it would be on the
// wire and queues it for the interface's own receive side (`INJECT`), so
// smoltcp's demultiplexing — every socket on the port, group membership —
// decides who gets it, as for a datagram from the network.

/// Frames waiting to be received by netif `.0`'s stack.
static INJECT: Mutex<alloc::collections::VecDeque<(u8, alloc::vec::Vec<u8>)>> =
    Mutex::new(alloc::collections::VecDeque::new());
/// How many frames `INJECT` holds: the receive path skips the lock while 0.
static INJECT_N: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// A sender looping more than this many frames before the daemon's next
/// pass loses the oldest (a full socket buffer would drop them anyway).
const INJECT_MAX: usize = 256;

/// The frame stack `s` would receive for a UDP datagram from `src`:`sport`
/// to `dst` carrying `payload` (multicast MAC for a group, broadcast MAC
/// else). `None` for a datagram no IPv4 packet can carry.
fn loop_frame(s: &NetStack, src: IpAddress, sport: u16, dst: IpEndpoint, ttl: u8, tos: u8, payload: &[u8])
    -> Option<alloc::vec::Vec<u8>>
{
    use smoltcp::wire::{EthernetFrame, EthernetProtocol, IpProtocol, Ipv4Packet, Ipv4Repr, UdpPacket, UdpRepr};
    #[allow(irrefutable_let_patterns)]
    let (IpAddress::Ipv4(s4), IpAddress::Ipv4(d4)) = (src, dst.addr) else { return None };
    #[allow(irrefutable_let_patterns)]
    let HardwareAddress::Ethernet(mac) = s.interface.hardware_addr() else { return None };
    if 20 + 8 + payload.len() > 65535 { return None; }
    let dst_mac = if d4.is_multicast() {
        EthernetAddress([0x01, 0x00, 0x5e, d4.0[1] & 0x7f, d4.0[2], d4.0[3]])
    } else {
        EthernetAddress::BROADCAST
    };
    let caps = smoltcp::phy::ChecksumCapabilities::default();
    let mut buf = alloc::vec![0u8; 14 + 20 + 8 + payload.len()];
    {
        let mut eth = EthernetFrame::new_unchecked(&mut buf[..]);
        eth.set_dst_addr(dst_mac);
        eth.set_src_addr(mac);
        eth.set_ethertype(EthernetProtocol::Ipv4);
    }
    let ip = Ipv4Repr { src_addr: s4, dst_addr: d4, next_header: IpProtocol::Udp,
                        payload_len: 8 + payload.len(), hop_limit: ttl.max(1) };
    {
        let mut p = Ipv4Packet::new_unchecked(&mut buf[14..]);
        ip.emit(&mut p, &caps);
        p.set_dscp(tos >> 2);
        p.set_ecn(tos & 3);
        p.fill_checksum();
    }
    let udp = UdpRepr { src_port: sport, dst_port: dst.port };
    udp.emit(&mut UdpPacket::new_unchecked(&mut buf[34..]), &src, &dst.addr, payload.len(),
             |b| b.copy_from_slice(payload), &caps);
    Some(buf)
}

/// Queue `frame` for stack `sid`'s receive side and wake the daemon.
fn inject_frame(sid: Sid, frame: alloc::vec::Vec<u8>) {
    if sid == LO { return; }
    {
        let mut q = INJECT.lock();
        if q.len() >= INJECT_MAX { q.pop_front(); } else { INJECT_N.fetch_add(1, Ordering::Relaxed); }
        q.push_back((sid - 1, frame));
    }
    drivers::netdev::kick();
}

/// The next frame looped back to netif `id`, if any.
fn take_injected(id: u8) -> Option<alloc::vec::Vec<u8>> {
    if INJECT_N.load(Ordering::Relaxed) == 0 { return None; }
    let mut q = INJECT.lock();
    let pos = q.iter().position(|(i, _)| *i == id)?;
    let f = q.remove(pos)?.1;
    INJECT_N.fetch_sub(1, Ordering::Relaxed);
    Some(f)
}

/// IP_MULTICAST_IF's stack for a datagram to `dst` (multicast or the
/// limited broadcast) from description `ofd`, if one is set and exists.
fn mcast_if_sid(ofd: u32, dst: IpAddress) -> Option<Sid> {
    #[allow(irrefutable_let_patterns)]
    let IpAddress::Ipv4(d) = dst else { return None };
    if !d.is_multicast() && d.0 != [255; 4] { return None; }
    let ifx = sock_opts(ofd).mc_if;
    if ifx == 0 { return None; }
    let sid = ifx - 1;
    (sid != LO && (sid as usize) < MAX_STACKS && stack_for(sid).is_some()).then_some(sid)
}

mod listen;
pub mod rxpolicy;
pub mod latestack;
mod mcast;
mod udpany;
mod syngate;
mod sa;

pub fn init() {
    sched::register_dump_hook(dump_sockets);
    vfs::set_proc_net_gen(procnet::generate);
    seed_ephemeral();
    init_loopback();
    for nif in drivers::netdev::iter() { init_nic(nif); }
}

/// Stand up one netif's stack, with its DHCP client.
fn init_nic(nif: drivers::netdev::Netif) {
    let Some(mac) = nif.mac() else { return };
    let mut device = NicDevice::new(nif);
    let hardware_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
    let mut config = Config::new(hardware_addr);
    config.random_seed = sched::ticks() ^ sched::monotonic_ns();

    let mut interface = Interface::new(config, &mut device, stack_now());
    // Linux's net.ipv4.ipfrag_time: an incomplete datagram's fragments are
    // dropped after 30 s (smoltcp's default is 60 s).
    interface.set_reassembly_timeout(smoltcp::time::Duration::from_secs(30));

    // QEMU's user-mode network always hands the first NIC 10.0.2.15/24 via
    // 10.0.2.2, so that one starts configured (traffic flows before the
    // lease, as it always has). Every other netif starts unaddressed and
    // takes no flows until its DHCP lease arrives.
    let prelease = nif.driver() == "virtio-net" && nif.dev == 0;
    if prelease {
        interface.update_ip_addrs(|addrs| {
            addrs.push(IpCidr::new(IpAddress::v4(10, 0, 2, 15), 24)).unwrap();
        });
        interface.routes_mut().add_default_ipv4_route(smoltcp::wire::Ipv4Address::new(10, 0, 2, 2)).unwrap();
    }
    {
        let mut t = iface::IFRT.lock();
        let r = &mut t[nif.id as usize];
        r.metric = iface::default_metric(&nif);
        r.carrier = nif.link().0;
        r.cfg = if prelease {
            Some(iface::IfCfg { addr: [10, 0, 2, 15], prefix: 24, gateway: Some([10, 0, 2, 2]) })
        } else {
            None
        };
    }

    let mut socket_set = SocketSet::new(alloc::vec![]);
    let dhcp_socket = smoltcp::socket::dhcpv4::Socket::new();
    let dhcp_handle = socket_set.add(dhcp_socket);
    ip6::attach(nif.id, &mut socket_set);

    let stack = NetStack {
        interface,
        socket_set,
        dhcp_handle: Some(dhcp_handle),
        loopback_dev: None,
        orphans: alloc::vec::Vec::new(),
        icmp_peeked: alloc::collections::BTreeMap::new(),
        listens: alloc::vec::Vec::new(),
        inet_seq: alloc::collections::BTreeMap::new(),
    };
    let sid = iface::sid_of(nif.id);
    let mut stack = stack;
    stack.interface.set_pmtu_scope(sid as u16);
    smoltcp::pmtu::clear_scope(sid as u16);
    *stack_for(sid) = Some(stack);
    // A stack built after sockets that belong on every stack (wifi0: the
    // MAC comes late) takes them over; at boot there are none yet.
    let (nl, nu) = (listen::adopt(sid), udpany::adopt(sid));

    iface::serial(&[b"[NET] ", nif.name().as_bytes(), b": stack up (", nif.driver().as_bytes(),
                    if prelease { b", 10.0.2.15/24 until the lease)\r\n" as &[u8] } else { b")\r\n" }]);
    if nl + nu != 0 {
        let line = alloc::format!("[NET] {}: adopted {} listener(s), {} udp server(s)\r\n", nif.name(), nl, nu);
        iface::serial(&[line.as_bytes()]);
    }
}

/// Stand up the loopback stack. Unconditional, unlike the NICs': 127.0.0.1 has
/// to work on a guest with no network device at all.
fn init_loopback() {
    { let mut st = make_loopback_stack(); st.interface.set_pmtu_scope(LO as u16); *stack_for(LO) = Some(st); }
    extern "C" { fn arch_serial_putc(b: u8); }
    let msg = b"[NET] Loopback interface 127.0.0.1/8 up\r\n";
    for &b in msg { unsafe { arch_serial_putc(b); } }
}

/// A fresh loopback stack: 127.0.0.1/8 on a `LoopDev`. netns 0's `LO` and
/// every other netns's lo (`vif::set_lo`) are built the same way.
pub(crate) fn make_loopback_stack() -> NetStack {
    let mut device = syngate::LoopDev::new();
    // A locally-administered MAC, distinct from the NIC's. It is never seen off
    // the box; smoltcp needs one only so the interface can answer its own ARP
    // request for 127.0.0.1. Ethernet medium (rather than Medium::Ip) keeps this
    // interface on the same code path as the NIC's, and is what smoltcp's own
    // loopback example uses.
    let hardware_addr = HardwareAddress::Ethernet(EthernetAddress([0x02, 0, 0, 0, 0, 1]));
    let mut config = Config::new(hardware_addr);
    config.random_seed = sched::ticks();

    let mut interface = Interface::new(config, &mut device, stack_now());
    interface.update_ip_addrs(|addrs| {
        addrs.push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8)).unwrap();
        addrs.push(IpCidr::new(IpAddress::Ipv6(smoltcp::wire::Ipv6Address::LOOPBACK), 128)).unwrap();
    });
    // No default route on purpose: 127.0.0.0/8 is on-link here, and nothing
    // else may ever leave through this interface.

    NetStack {
        interface,
        socket_set: SocketSet::new(alloc::vec![]),
        dhcp_handle: None,
        loopback_dev: Some(device),
        orphans: alloc::vec::Vec::new(),
        icmp_peeked: alloc::collections::BTreeMap::new(),
        listens: alloc::vec::Vec::new(),
        inet_seq: alloc::collections::BTreeMap::new(),
    }
}

/// What one stack's DHCP client reported during a pass.
enum DhcpEv {
    Configured(smoltcp::wire::Ipv4Cidr, Option<smoltcp::wire::Ipv4Address>, alloc::vec::Vec<[u8; 4]>),
    Deconfigured,
}

/// Install a lease on a netif's stack, then record it for routing, DNS and
/// /proc. A lease without a router keeps the previous default route, as the
/// stack does. Takes the stack lock once, then leaf locks only.
fn apply_lease(nif: drivers::netdev::Netif, addr: smoltcp::wire::Ipv4Cidr,
               router: Option<smoltcp::wire::Ipv4Address>, servers: &[[u8; 4]]) {
    {
        let mut stack = stack_for(iface::sid_of(nif.id));
        if let Some(ref mut s) = *stack {
            ip6::replace_v4(&mut s.interface, Some(IpCidr::new(IpAddress::Ipv4(addr.address()), addr.prefix_len())));
            if let Some(gateway) = router {
                s.interface.routes_mut().add_default_ipv4_route(gateway).unwrap();
            }
        }
    }
    {
        let mut t = iface::IFRT.lock();
        let r = &mut t[nif.id as usize];
        let gw = router.map(|g| g.0).or(r.cfg.and_then(|c| c.gateway));
        r.cfg = Some(iface::IfCfg { addr: addr.address().0, prefix: addr.prefix_len(), gateway: gw });
        let n = servers.len().min(3);
        r.dns[..n].copy_from_slice(&servers[..n]);
        r.ndns = n as u8;
    }
    // /etc/resolv.conf names the leases' DNS servers from now on.
    let merged = iface::merged_dns();
    if !merged.is_empty() { vfs::set_dhcp_dns(&merged); }
    iface::note_dhcp_lease(nif.id, addr.address().0, addr.prefix_len(), router.map(|g| g.0), servers);

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
    for &b in b" (" { unsafe { arch_serial_putc(b); } }
    for &b in nif.name().as_bytes() { unsafe { arch_serial_putc(b); } }
    for &b in b")\r\n" { unsafe { arch_serial_putc(b); } }
    iface::notify(nif.id);
}

/// How long a netif may be without carrier before its address goes (and the
/// connections using it are reset). A shorter blip keeps everything; the
/// interface takes no new flows meanwhile.
const CARRIER_GRACE_NS: u64 = 4_000_000_000;
/// After a link comes back up the DHCP client restarts; the old address is
/// kept this long for the new lease to confirm it.
const REBIND_NS: u64 = 10_000_000_000;

/// The daemon's view of one netif between passes.
#[derive(Clone, Copy)]
struct NicTrack {
    /// The link generation the DHCP client last saw.
    gen: u32,
    carrier: bool,
    /// `monotonic_ns` when the carrier went away.
    down_since: u64,
    /// While nonzero (a deadline): the DHCP client was restarted by a link
    /// change and the address is kept until a lease confirms or replaces it.
    rebind_until: u64,
    /// Administratively down as of the last pass (`iface::set_admin`).
    admin_down: bool,
}

/// Apply a netif's administrative state, once per pass. Down: the address
/// and routes go at once (no carrier grace) and the stack is not polled, so
/// the DHCP client sends nothing. Up: the DHCP client restarts as on a link
/// change. Returns true while the netif is down (skip it this pass).
fn track_admin(nif: drivers::netdev::Netif, t: &mut NicTrack) -> bool {
    let down = iface::admin_down(nif.id);
    if down != t.admin_down {
        t.admin_down = down;
        iface::serial(&[b"[NET] ", nif.name().as_bytes(), if down { b": admin down\r\n" as &[u8] } else { b": admin up\r\n" }]);
        if down {
            t.rebind_until = 0;
            if iface::rt(nif.id).cfg.is_some() {
                deconfigure(nif, "admin down");
            } else {
                iface::notify(nif.id);
            }
        } else {
            // A generation the DHCP client has not seen: it resets and sends a
            // DISCOVER on this pass.
            t.gen = nif.link_generation().wrapping_add(1);
            iface::notify(nif.id);
        }
    }
    down
}

/// Take a netif's address and default route away: its lease is gone (or its
/// link has been down past the grace period). Every TCP connection on its
/// stack is reset, as the address it used no longer exists here; listeners
/// stay armed for the next lease. Returns after the stack lock is released.
fn deconfigure(nif: drivers::netdev::Netif, why: &str) {
    let mut reset = 0u32;
    {
        let mut stack = stack_for(iface::sid_of(nif.id));
        if let Some(ref mut s) = *stack {
            ip6::replace_v4(&mut s.interface, None);
            s.interface.routes_mut().remove_default_ipv4_route();
            use smoltcp::socket::AnySocket;
            for (_, sock) in s.socket_set.iter_mut() {
                if let Some(t) = tcp::Socket::downcast_mut(sock) {
                    if !matches!(t.state(), tcp::State::Listen | tcp::State::Closed | tcp::State::TimeWait) {
                        t.abort();
                        reset += 1;
                    }
                }
            }
        }
    }
    let old = {
        let mut t = iface::IFRT.lock();
        let r = &mut t[nif.id as usize];
        r.ndns = 0;
        r.cfg.take()
    };
    iface::note_dhcp_lost(nif.id);
    let merged = iface::merged_dns();
    if !merged.is_empty() { vfs::set_dhcp_dns(&merged); }
    if let Some(c) = old {
        let a = alloc::format!("[NET] {}: address {}.{}.{}.{}/{} removed ({}), {} connection(s) reset\r\n",
                               nif.name(), c.addr[0], c.addr[1], c.addr[2], c.addr[3], c.prefix, why, reset);
        iface::serial(&[a.as_bytes()]);
    }
    iface::notify(nif.id);
    sched::wake_poll();
}

/// Passes where `rx_rearm` stayed pending past `rxpolicy::REARM_MAX`.
pub static REARM_CAPS: AtomicU64 = AtomicU64::new(0);
/// Frames dropped because their netif had no stack or was admin down.
pub static RX_NO_STACK_DROPS: AtomicU64 = AtomicU64::new(0);

/// Drop up to `DISCARD_MAX` frames a netif received that no stack will read.
fn discard_rx(nif: drivers::netdev::Netif) {
    let mut buf = [0u8; 2048];
    let mut n = 0u64;
    while (n as usize) < rxpolicy::DISCARD_MAX && nif.recv(&mut buf).is_some() { n += 1; }
    if n > 0 {
        RX_NO_STACK_DROPS.fetch_add(n, Ordering::Relaxed);
        iface::STATS[nif.id as usize].rx_drop.fetch_add(n as _, Ordering::Relaxed);
    }
}

/// Carrier bookkeeping for one netif, once per pass, no lock held.
fn track_carrier(nif: drivers::netdev::Netif, t: &mut NicTrack, now: u64) {
    let (up, speed, _) = nif.link();
    if up != t.carrier {
        t.carrier = up;
        if !up { t.down_since = now; }
        iface::IFRT.lock()[nif.id as usize].carrier = up;
        iface::serial(&[b"[NET] ", nif.name().as_bytes(), if up { b": carrier up\r\n" as &[u8] } else { b": carrier down\r\n" }]);
        // The default route may have moved: DNS follows it.
        let merged = iface::merged_dns();
        if !merged.is_empty() { vfs::set_dhcp_dns(&merged); }
        drivers::netdev::note_link(nif.id, if up {
            drivers::netdev::LinkEvent::Up { speed }
        } else {
            drivers::netdev::LinkEvent::Down
        });
        iface::notify(nif.id);
    }
    if !t.carrier && now.saturating_sub(t.down_since) >= CARRIER_GRACE_NS && iface::rt(nif.id).cfg.is_some() {
        t.rebind_until = 0;
        deconfigure(nif, "no carrier");
    }
    if t.rebind_until != 0 && now >= t.rebind_until {
        t.rebind_until = 0;
        if iface::rt(nif.id).cfg.is_some() { deconfigure(nif, "no lease after the link came back"); }
    }
}

/// One stack's share of a daemon pass: poll, readiness edges, listener
/// upkeep. The device goes through a `SynGate`, so a listener's second SYN in
/// one poll is held until `listen::maintain` has re-armed it, and the stack
/// is polled again for it (see `syngate.rs`). Returns (smoltcp reported
/// activity, wake mask).
fn poll_stack<D: syngate::Frames>(s: &mut NetStack, dev: &mut D, timestamp: Instant,
                                  sg: &mut alloc::vec::Vec<Sig>, sid: Sid,
                                  keys: &mut sched::WakeKeys) -> (bool, u64) {
    let mut gate = syngate::SynGate::new(dev);
    let (mut changed, mut mask) = (false, 0u64);
    let mut repolls = 0;
    loop {
        readiness_sigs(&mut s.socket_set, sg);
        changed |= s.interface.poll(timestamp, &mut gate, &mut s.socket_set);
        mask |= readiness_moved(&s.socket_set, &mut s.inet_seq, sg, sid, keys);
        mask |= listen::maintain(s, sched::monotonic_ns(), keys);
        if !gate.again() || repolls >= syngate::MAX_REPOLLS { break; }
        repolls += 1;
    }
    (changed, mask)
}

pub fn net_daemon() -> ! {
    vif::init();
    tunbe::init();
    // Readiness snapshots of each stack's sockets taken just before a poll
    // (see `readiness_sigs`), reused across passes.
    let mut sigs: [alloc::vec::Vec<Sig>; MAX_STACKS] = [const { alloc::vec::Vec::new() }; MAX_STACKS];
    let mut track = [NicTrack { gen: 0, carrier: true, down_since: 0, rebind_until: 0, admin_down: false }; drivers::netdev::MAX_NETIF];
    for nif in drivers::netdev::iter() { track[nif.id as usize].carrier = iface::rt(nif.id).carrier; }
    let mut last_broadcast = 0u64;
    let mut immediate_streak = 0u32;
    let mut rearm = rxpolicy::RearmGuard::new();
    let mut busy_marked = false;
    read_net_knobs();
    let (mut prev_tx, mut prev_rx) = (0u64, 0u64);
    loop {
        vif::daemon_pass();
        // This task runs with IRQs masked between parks: let the tick in on
        // every pass, so a pass that keeps restarting cannot starve the CPU
        // and the wedge scan / watchdog can still run on it (1-CPU boots).
        sched::kthread::cond_resched();
        if !busy_marked { busy_marked = true; sched::wedge::mark(sched::wedge::SLOT_NETD, 1, 0); }
        if drivers::netdev::take_kick() { procnet::NETD_KICK_PASSES.fetch_add(1, Ordering::Relaxed); }
        procnet::NETD_PASSES.fetch_add(1, Ordering::Relaxed);
        {
            // Per-pass histograms of frames sent / received since the last pass began.
            let (mut tx, mut rx) = (0u64, 0u64);
            for st in iface::STATS.iter() { tx += st.tx_pkts.load(Ordering::Relaxed); rx += st.rx_pkts.load(Ordering::Relaxed); }
            let hb = |d: u64| match d { 0 => 0, 1 => 1, 2..=7 => 2, 8..=63 => 3, _ => 4 };
            procnet::NETD_TX_HIST[hb(tx.wrapping_sub(prev_tx))].fetch_add(1, Ordering::Relaxed);
            procnet::NETD_RX_HIST[hb(rx.wrapping_sub(prev_rx))].fetch_add(1, Ordering::Relaxed);
            prev_tx = tx; prev_rx = rx;
        }
        let timestamp = stack_now();

        let mut readiness_changed = false;
        let mut wake_mask = 0u64;
        let mut wake_keys = sched::WakeKeys::new();
        let mut next_poll: Option<smoltcp::time::Duration> = None;
        let mut pass_rx = 0u64;
        for nif in drivers::netdev::iter() {
            let sid = iface::sid_of(nif.id);
            let tr = &mut track[nif.id as usize];
            let admin_down = track_admin(nif, tr);
            if !admin_down { track_carrier(nif, tr, sched::monotonic_ns()); }
            // A netif without a stack (Wi-Fi: the MAC comes from the firmware
            // after `init_nic` ran) gets one as soon as its MAC is known;
            // until then, and while admin down, its frames are dropped here
            // so `rx_rearm` below does not report them forever.
            let has_stack = stack_for(sid).is_some();
            if has_stack { ip6::sync(nif, admin_down); }
            match rxpolicy::nif_action(has_stack, admin_down, !has_stack && nif.mac().is_some()) {
                rxpolicy::NifAction::Poll => {}
                rxpolicy::NifAction::InitStack => {
                    init_nic(nif);
                    if stack_for(sid).is_none() { discard_rx(nif); continue; }
                }
                rxpolicy::NifAction::Discard => { discard_rx(nif); continue; }
            }
            for f in fwd::take_txq(nif.id) {
                if nif.tx_ready() && nif.send(&f) {
                    iface::STATS[nif.id as usize].tx(f.len());
                    LAST_TRAFFIC_NS.store(sched::monotonic_ns(), Ordering::Relaxed);
                } else {
                    iface::STATS[nif.id as usize].tx_drop.fetch_add(1, Ordering::Relaxed);
                }
            }
            let mut device = NicDevice::new(nif);
            let ev = {
                let mut stack = stack_for(sid);
                let Some(ref mut s) = *stack else { continue };
                let (changed, mask) = poll_stack(s, &mut device, timestamp, &mut sigs[sid as usize], sid, &mut wake_keys);
                readiness_changed |= changed;
                wake_mask |= mask;
                reap_orphans(s);
                if let Some(ns) = ip6::slaac_pass(nif, s, admin_down, sched::monotonic_ns()) {
                    let d = smoltcp::time::Duration::from_micros(ns / 1000);
                    next_poll = Some(next_poll.map_or(d, |n| n.min(d)));
                }

                let ev = match s.dhcp_handle {
                    Some(h) => {
                        let dhcp_socket =
                            s.socket_set.get_mut::<smoltcp::socket::dhcpv4::Socket>(h);
                        // A link that just came up (a real NIC; virtio-net is
                        // always up) gets a DISCOVER now rather than at the
                        // client's next retry, up to 10 s later.
                        let gen = nif.link_generation();
                        if gen != core::mem::replace(&mut tr.gen, gen) {
                            // The device's MAC address may have changed with it
                            // (Wi-Fi learns its own from the firmware).
                            if let Some(mac) = nif.mac() {
                                let hw = HardwareAddress::Ethernet(EthernetAddress(mac));
                                if s.interface.hardware_addr() != hw { s.interface.set_hardware_addr(hw); }
                            }
                            dhcp_socket.reset();
                            iface::note_dhcp_restart(nif.id);
                            // Keep the address while the new lease is
                            // sought: established flows survive a blip if
                            // the lease comes back the same.
                            if iface::rt(nif.id).cfg.is_some() {
                                tr.rebind_until = sched::monotonic_ns() + REBIND_NS;
                            }
                        }
                        match dhcp_socket.poll() {
                            Some(smoltcp::socket::dhcpv4::Event::Configured(config)) => {
                                Some(DhcpEv::Configured(config.address, config.router,
                                                        config.dns_servers.iter().map(|a| a.0).collect()))
                            }
                            Some(smoltcp::socket::dhcpv4::Event::Deconfigured) => Some(DhcpEv::Deconfigured),
                            None => None,
                        }
                    }
                    None => None,
                };
                if let Some(mut d) = s.interface.poll_delay(stack_now(), &s.socket_set) {
                    // smoltcp asks for another pass at once because it has
                    // segments to send, but the driver's queue is past its
                    // high-water mark: a pass now sends nothing. The driver
                    // kicks when the queue drains (iwlwifi TXQ_LOW, netshape
                    // half-empty), and that kick starts the next pass; the
                    // delay is only the fallback. Re-polling at once spun 8
                    // empty passes per 200 us (31 k passes/s on a 20 Mbit/s
                    // shaped upload).
                    if d.total_micros() == 0 && !nif.tx_ready() {
                        procnet::NETD_TX_HELD_SLEEPS.fetch_add(1, Ordering::Relaxed);
                        d = smoltcp::time::Duration::from_micros(TX_HELD_FALLBACK_NS / 1000);
                    }
                    next_poll = Some(next_poll.map_or(d, |n| n.min(d)));
                }
                ev
            };
            pass_rx += device.pass_rx;
            match ev {
                Some(DhcpEv::Configured(addr, router, servers)) => {
                    tr.rebind_until = 0;
                    // A different address: what used the old one is gone.
                    if iface::rt(nif.id).cfg.map_or(false, |c| c.addr != addr.address().0) {
                        deconfigure(nif, "new lease");
                    }
                    apply_lease(nif, addr, router, &servers);
                }
                Some(DhcpEv::Deconfigured) => {
                    if tr.rebind_until == 0 {
                        // The lease expired (or was refused on renewal): the
                        // address and route must go, or the interface keeps
                        // attracting flows that cannot work.
                        deconfigure(nif, "lease lost");
                    }
                }
                None => {}
            }
        }
        if pass_rx > 0 {
            LAST_TRAFFIC_NS.store(sched::monotonic_ns(), Ordering::Relaxed);
            procnet::NETD_MAX_RX.fetch_max(pass_rx, Ordering::Relaxed);
        }

        // The loopback stack is independent of the NICs and exists even when
        // no NIC does. One poll carries a whole 127.0.0.1 exchange — ARP, SYN,
        // SYN-ACK, ACK — because Interface::poll loops until neither ingress
        // nor egress made progress, and the Loopback phy feeds every frame it
        // transmits straight back into its own receive queue.
        {
            let mut lo = stack_for(LO);
            if let Some(ref mut s) = *lo {
                // Out of the stack for the poll (`poll_stack` takes the whole
                // stack); the stack lock stays held, so nobody sees it gone.
                if let Some(mut dev) = s.loopback_dev.take() {
                    let (changed, mask) = poll_stack(s, &mut dev, timestamp, &mut sigs[LO as usize], LO, &mut wake_keys);
                    s.loopback_dev = Some(dev);
                    readiness_changed |= changed;
                    wake_mask |= mask;
                    reap_orphans(s);
                    if let Some(d) = s.interface.poll_delay(stack_now(), &s.socket_set) {
                        next_poll = Some(next_poll.map_or(d, |n| n.min(d)));
                    }
                }
            }
        }

        fwd::drain_in();
        tunbe::poll_all(timestamp, &mut sigs, &mut wake_keys, &mut readiness_changed, &mut wake_mask, &mut next_poll);
        // Every other netns's loopback stack, polled the same way.
        for sid in vif::all_dyn_sids() {
            let mut st = stack_for(sid);
            if let Some(ref mut s) = *st {
                if let Some(mut dev) = s.loopback_dev.take() {
                    let (changed, mask) = poll_stack(s, &mut dev, timestamp, &mut sigs[sid as usize], sid, &mut wake_keys);
                    s.loopback_dev = Some(dev);
                    readiness_changed |= changed;
                    wake_mask |= mask;
                    reap_orphans(s);
                    if let Some(d) = s.interface.poll_delay(stack_now(), &s.socket_set) {
                        next_poll = Some(next_poll.map_or(d, |n| n.min(d)));
                    }
                }
            }
        }

        // Readiness edges, published with every stack lock released. Each
        // socket whose readiness moved during this pass wakes its own tag
        // (the one `handle_poll` reports), so a 1 kHz pass does not wake
        // every poller in the system. A broadcast still goes out at most
        // every 100 ms while smoltcp reports activity: the safety net for
        // any waiter whose readiness changed outside a pass. Off by default
        // (`net.bcastnet=1` restores it): every producer is keyed now
        // (3fdd0300, a8cc55d7, 2fac9086, 20673804, 3a23c777, ccf90b47,
        // 4864fa0d), so the broadcast only woke the whole system for nothing.
        let now = sched::monotonic_ns();
        if bcast_net() && readiness_changed && now.saturating_sub(last_broadcast) >= 100_000_000 {
            last_broadcast = now;
            procnet::NETD_BCAST_WAKES.fetch_add(1, Ordering::Relaxed);
            sched::wake_poll();
        } else if wake_mask != 0 {
            procnet::NETD_TAG_WAKES.fetch_add(1, Ordering::Relaxed);
            match wake_keys.exact() {
                Some(ks) => for &k in ks { sched::wake_poll_key(k); },
                None => sched::wake_poll_tagged(wake_mask),
            }
        }

        // Re-arm each NIC's receive interrupt now that its ring is drained;
        // a frame that arrived meanwhile means another pass right away.
        let mut again = false;
        for nif in drivers::netdev::iter() { again |= nif.rx_rearm(); }
        if rearm.again(again) { procnet::NETD_REARM_PASSES.fetch_add(1, Ordering::Relaxed); continue; }
        // Still pending after REARM_MAX passes in a row: no pass is making
        // the ring empty, so park briefly instead of spinning IRQ-off.
        if again && REARM_CAPS.fetch_add(1, Ordering::Relaxed) == 0 {
            iface::serial(&[b"[NET] rx rearm capped: a netif kept reporting frames no pass consumed\r\n"]);
        }

        // Sleep until smoltcp's next timer (delayed ACK, retransmit, keepalive,
        // DHCP renew/retry, ARP retry: all of them are in `poll_delay`), or
        // until a kick: a send, a socket request that changes what the stack
        // must do (connect, close, shutdown, listen, accept, setsockopt), an
        // RX or link interrupt, a reopened window, an admin change.
        //  - Frames flowing: 1 kHz, the backstop for a lost RX interrupt.
        //  - Idle, every NIC's receive path kicks: `IDLE_CAP_NS`. That cap only
        //    covers the daemon's own coarse upkeep (carrier grace, rebind
        //    deadline, orphan reaping, listener backlog) and a lost interrupt
        //    on real hardware.
        //  - Idle with a NIC that does not (yet) kick on receive (virtio-net
        //    before its first RX interrupt, a driver in polled mode): 100 Hz.
        //  A NIC whose every received frame kicks needs no 1 kHz backstop: the
        //  frame itself starts the next pass, and `rx_rearm` closes the race
        //  with one that lands while the ring is re-armed. Keeping the 1 kHz
        //  window after each frame meant ~200 passes per stray broadcast on
        //  an idle LAN (a chatty host network: 168 passes/s at idle).
        let irq_driven = drivers::netdev::iter().all(|n| n.rx_irq_driven());
        let active = !irq_driven
            && now.saturating_sub(LAST_TRAFFIC_NS.load(Ordering::Relaxed)) < ACTIVE_NS;
        let cadence_ns: u64 = if active { NET_CADENCE_NS.load(Ordering::Relaxed) }
            else if irq_driven { IDLE_CAP_NS }
            else { 10_000_000 };
        let mut sleep_ns = match next_poll {
            Some(d) => (d.total_micros() * 1000).min(cadence_ns),
            None => cadence_ns,
        };
        let timer_limited = sleep_ns < cadence_ns;
        if timer_limited {
            procnet::NETD_TIMER_SLEEPS.fetch_add(1, Ordering::Relaxed);
            let b = match sleep_ns { 0..=99_999 => 0, 100_000..=999_999 => 1, 1_000_000..=9_999_999 => 2, _ => 3 };
            procnet::NETD_SLEEP_HIST[b].fetch_add(1, Ordering::Relaxed);
        }
        if active { procnet::NETD_ACTIVE_SLEEPS.fetch_add(1, Ordering::Relaxed); }
        procnet::NETD_LAST_SLEEP_US.store(sleep_ns / 1000, Ordering::Relaxed);
        // The netshape link emulator (a test knob) releases its next frame then.
        if again { sleep_ns = sleep_ns.min(rxpolicy::REARM_PARK_NS); }
        if let Some(due) = drivers::netshape::next_due_ns() {
            sleep_ns = sleep_ns.min(due.saturating_sub(sched::monotonic_ns()).max(100_000));
        }
        if sleep_ns == 0 {
            // smoltcp wants another pass at once (more to send). Allow a few
            // back to back, then sleep a little so a socket that keeps asking
            // cannot spin this CPU.
            immediate_streak += 1;
            procnet::NETD_IMMEDIATE.fetch_add(1, Ordering::Relaxed);
            if immediate_streak < 8 { continue; }
            sleep_ns = 200_000;
        }
        immediate_streak = 0;
        // Keyed park: every kick is a keyed wake of NETD_KEY, so the daemon
        // refines its bit to that key's fingerprint and a keyed wake of some
        // other object whose key shares the bit no longer cuts the sleep
        // short. The deadline is the bounded fallback.
        rearm.parked();
        busy_marked = false;
        sched::wedge::mark(sched::wedge::SLOT_NETD, 0, 0);
        let deadline = sched::monotonic_ns() + sleep_ns;
        sched::block_on_poll_prepare_tag(deadline, &NETD_PARK);
        if drivers::netdev::kicked() {
            sched::block_on_poll_cancel();
            continue;
        }
        sched::block_on_poll_commit();
        let woke_kicked = drivers::netdev::kicked();
        if !woke_kicked && timer_limited { procnet::NETD_TIMER_EXPIRED.fetch_add(1, Ordering::Relaxed); }
        if !woke_kicked && sched::monotonic_ns() < deadline {
            procnet::NETD_EARLY_WAKES.fetch_add(1, Ordering::Relaxed);
            let site = sched::gdwake::take(sched::current_pid());
            if site != 0 { procnet::NETD_EARLY_SITE.store(site, Ordering::Relaxed); }
        }
    }
}

/// The daemon's park interest: NETD_KEY only (see the park in `net_daemon`).
static NETD_PARK: sched::PollTag = sched::PollTag {
    bits: sched::poll_key_bit(drivers::netdev::NETD_KEY), n: 1,
    keys: [drivers::netdev::NETD_KEY, 0, 0], nested: 0, foreign: false,
};

/// The stack's clock: smoltcp timers (delayed ACK, RTO, RTT estimates) at
/// microsecond resolution. It used to be `ticks() * 10 ms`.
fn stack_now() -> Instant {
    Instant::from_micros((sched::monotonic_ns() / 1000) as i64)
}

/// One byte per socket summarising what `handle_poll` derives from it.
fn readiness_sig(sock: &smoltcp::socket::Socket<'static>) -> u8 {
    use smoltcp::socket::Socket;
    match sock {
        Socket::Tcp(t) => {
            let st = t.state() as u8 & 0x0f;
            st | (((t.can_recv() || tcp_rx_eof(t)) as u8) << 4) | ((t.can_send() as u8) << 5)
               | ((t.is_active() as u8) << 6) | ((t.may_recv() as u8) << 7)
        }
        Socket::Udp(u) => (u.can_recv() as u8) | ((u.can_send() as u8) << 1) | ((((u.error() != 0) || u.errq_len() != 0) as u8) << 2),
        Socket::Icmp(i) => (i.can_recv() as u8) | ((i.can_send() as u8) << 1),
        #[allow(unreachable_patterns)]
        _ => 0,
    }
}

/// A socket's state as one daemon pass sees it: the readiness byte plus, for
/// TCP, the receive and send queue lengths. Nothing but the network changes a
/// socket during `Interface::poll` (the stack lock is held), so any difference
/// across a poll is an edge: data or a FIN arrived (recv queue grew, state
/// moved), an ACK freed send space (send queue shrank), a reset. The queue
/// lengths catch the edges the readiness byte cannot: more data arriving on a
/// socket that was already readable.
type Sig = (u8, u64);

fn sig_of(sock: &smoltcp::socket::Socket<'static>) -> Sig {
    let q = match sock {
        smoltcp::socket::Socket::Tcp(t) => (t.recv_queue() as u64 & 0xffff_ffff) | ((t.send_queue() as u64) << 32),
        #[allow(unreachable_patterns)]
        _ => 0,
    };
    (readiness_sig(sock), q)
}

// ── Datagram edges ────────────────────────────────────────────────────────────
//
// A UDP or ICMP socket has no queue length to compare across a poll (smoltcp
// 0.11 exposes none), and its readiness byte does not move when a datagram
// lands behind one that is still unread — the edge an EPOLLET reader is
// owed — nor when a send queue that had no room for a large datagram (but
// free slots) drains — the edge an EPOLLET writer that got EAGAIN waits for.
// What smoltcp does have (feature "async") are wakers: the receive waker
// fires on every datagram `process()` is handed, the send waker on every
// datagram `dispatch()` puts on the wire. Before each poll the daemon gives
// every UDP/ICMP socket both, naming the socket's position in the set (the
// order `readiness_sigs` snapshots in; a poll adds and removes no sockets);
// either sets that position's bit in `RX_FIRED`, and `readiness_moved` turns
// each set bit into a new edge-seq and a wake of the socket's tag. One edge
// per datagram sent is what Linux gives an EPOLLET UDP socket too
// (sock_wfree -> sk_write_space). Only the daemon polls, one stack at a time,
// so `RX_FIRED` is uncontended and is cleared for each stack before its poll.

/// Bit `i`: the `i`-th socket of the stack being polled was handed a
/// datagram or sent one.
static RX_FIRED: Mutex<alloc::vec::Vec<u64>> = Mutex::new(alloc::vec::Vec::new());

unsafe fn rxw_clone(p: *const ()) -> core::task::RawWaker { core::task::RawWaker::new(p, &RXW_VTABLE) }
unsafe fn rxw_wake(p: *const ()) {
    let i = (p as usize).wrapping_sub(1);
    if let Some(w) = RX_FIRED.lock().get_mut(i / 64) { *w |= 1u64 << (i % 64); }
}
unsafe fn rxw_drop(_: *const ()) {}
static RXW_VTABLE: core::task::RawWakerVTable =
    core::task::RawWakerVTable::new(rxw_clone, rxw_wake, rxw_wake, rxw_drop);

/// The waker of the socket at position `i` (data `i + 1`: never null).
fn rx_waker(i: usize) -> core::task::Waker {
    // SAFETY: the vtable's functions only read the data word as an integer.
    unsafe { core::task::Waker::from_raw(core::task::RawWaker::new((i + 1) as *const (), &RXW_VTABLE)) }
}

/// Snapshot every socket's readiness before a poll, and arm the
/// wakers of every datagram socket (see "Datagram edges"). Same stack
/// lock as the poll and the comparison after it, so a reader's recv between
/// passes can never hide a change; a poll adds and removes no sockets, so the
/// order matches.
fn readiness_sigs(set: &mut SocketSet<'static>, out: &mut alloc::vec::Vec<Sig>) {
    use smoltcp::socket::Socket;
    out.clear();
    for (i, (_, sk)) in set.iter_mut().enumerate() {
        out.push(sig_of(sk));
        // Re-registering the same waker is a compare, not a clone.
        match sk {
            Socket::Udp(u) => { u.register_recv_waker(&rx_waker(i)); u.register_send_waker(&rx_waker(i)); }
            Socket::Icmp(c) => { c.register_recv_waker(&rx_waker(i)); c.register_send_waker(&rx_waker(i)); }
            _ => {}
        }
    }
    let mut f = RX_FIRED.lock();
    f.clear();
    f.resize((out.len() + 63) / 64, 0);
}

/// The tags of the sockets whose readiness moved during the poll, or that
/// were handed or sent a datagram; each such TCP, UDP or ICMP socket also gets a new
/// EPOLLET edge-seq. A tag is woken on every edge, not only on a readiness
/// flip: an EPOLLET waiter whose socket is still readable from an older edge
/// is parked, and only the new seq frees it.
fn readiness_moved(set: &SocketSet<'static>, seqs: &mut alloc::collections::BTreeMap<SocketHandle, u64>,
                   before: &[Sig], sid: Sid, keys: &mut sched::WakeKeys) -> u64 {
    use smoltcp::socket::Socket;
    let fired = RX_FIRED.lock();
    let mut mask = 0u64;
    for (i, (h, sk)) in set.iter().enumerate() {
        let arrived = fired.get(i / 64).map_or(false, |w| w & (1u64 << (i % 64)) != 0);
        if arrived || before.get(i).copied() != Some(sig_of(sk)) {
            mask |= inet_tag(sid, h);
            keys.push(inet_key(sid, h));
            if matches!(sk, Socket::Tcp(_) | Socket::Udp(_) | Socket::Icmp(_)) {
                seqs.insert(h, INET_SEQ_GEN.fetch_add(1, Ordering::Relaxed));
            }
        }
    }
    mask
}

/// A new EPOLLET edge for the socket `h` of stack `s` that no daemon pass
/// will see: a local event that changes what poll reports (shutdown).
fn bump_seq(s: &mut NetStack, h: SocketHandle) {
    s.inet_seq.insert(h, INET_SEQ_GEN.fetch_add(1, Ordering::Relaxed));
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
            is_abstract: false, netns: 0,
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
    // Requests that give an inet stack something to do or a new timer (SYN,
    // FIN, RST, keepalive, a freed backlog slot) get a daemon pass now: the
    // daemon sleeps up to `IDLE_CAP_NS` when idle. Unix sockets never need it.
    let kick = match msg.tag {
        NET_CONNECT | NET_SHUTDOWN | NET_CLOSE | NET_LISTEN | NET_ACCEPT | NET_SETSOCKOPT | NET_BIND =>
            sock_entry(caller_pid, arg(msg,0) as usize).map_or(false, |e| is_inet(e.domain)),
        // Every exit sends CLOSE_ALL; only one that held an inet socket
        // needs a pass (decided in the arm below). Kicking on all of them
        // made each short-lived process (a shell pipeline, `cat`) a daemon
        // pass on an idle system.
        _ => false,
    };
    if matches!(msg.tag, NET_BIND | NET_LISTEN | NET_ACCEPT | NET_CONNECT | NET_SEND | NET_RECV
        | NET_SENDMSG | NET_RECVMSG | NET_SHUTDOWN | NET_GETSOCKNAME | NET_GETPEERNAME
        | NET_SETSOCKOPT | NET_GETSOCKOPT | NET_CLOSE | NET_POLL | NET_DUP | NET_QUEUE_LEN) {
        heal_stale(caller_pid, arg(msg,0) as usize);
    }
    let reply = match msg.tag {
        NET_SOCKET      => handle_socket(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize, arg(msg,2) as usize),
        NET_BIND        => with_state_wake(caller_pid, arg(msg,0) as usize, false, ||
                               handle_bind(caller_pid, arg(msg,0) as usize,
                                           arg(msg,1) as usize, arg(msg,2) as usize,
                                           host_path_arg(msg))),
        NET_LISTEN      => with_state_wake(caller_pid, arg(msg,0) as usize, false, ||
                               handle_listen(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize)),
        NET_ACCEPT      => handle_accept(caller_pid, arg(msg,0) as usize,
                                         arg(msg,1) as usize, arg(msg,2) as usize,
                                         arg(msg,3) as usize),
        NET_CONNECT     => with_state_wake(caller_pid, arg(msg,0) as usize, false, ||
                               handle_connect(caller_pid, arg(msg,0) as usize,
                                              arg(msg,1) as usize, arg(msg,2) as usize,
                                              host_path_arg(msg))),
        NET_SEND        => handle_send(caller_pid, arg(msg,0) as usize,
                                        arg(msg,1) as usize, arg(msg,2) as usize,
                                        arg(msg,4) as usize, arg(msg,5) as usize),
        NET_RECV        => handle_recv(caller_pid, arg(msg,0) as usize,
                                        arg(msg,1) as usize, arg(msg,2) as usize,
                                        arg(msg,4) as usize, arg(msg,5) as usize,
                                        arg(msg,3) as usize),
        // msghdr_ptr == 0 with arg3/arg4 = (iov_ptr, iovcnt): a writev/readv
        // on a socket, i.e. a msghdr with no name and no control.
        NET_SENDMSG     => handle_sendmsg(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize, arg(msg,2) as usize,
                                          if arg(msg,1) == 0 { Some((arg(msg,3) as usize, arg(msg,4) as usize)) } else { None }),
        NET_RECVMSG     => handle_recvmsg(caller_pid, arg(msg,0) as usize,
                                          arg(msg,1) as usize, arg(msg,2) as usize,
                                          if arg(msg,1) == 0 { Some((arg(msg,3) as usize, arg(msg,4) as usize)) } else { None }),
        NET_SHUTDOWN    => with_state_wake(caller_pid, arg(msg,0) as usize, true, ||
                               handle_shutdown(caller_pid, arg(msg,0) as usize, arg(msg,1) as usize)),
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
        NET_CLOSE_ALL   => { if handle_close_all(arg(msg,0) as u32) { drivers::netdev::kick(); } ok_reply() }
        NET_CLOSE       => with_state_wake(caller_pid, arg(msg,0) as usize, true, ||
                               handle_close(caller_pid, arg(msg,0) as usize)),
        NET_POLL        => handle_poll(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32, arg(msg,2) as u32),
        NET_DUP         => handle_dup(tk(caller_pid), arg(msg,0) as usize, arg(msg,1) != 0, arg(msg,2) != 0),
        NET_FORK_DUP    => handle_fork_dup(arg(msg,0) as u32, arg(msg,1) as u32),
        NET_EXEC_CLOEXEC => handle_exec_cloexec(arg(msg,0) as u32),
        NET_SETFL       => handle_setfl(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        NET_GETFL       => handle_getfl(caller_pid, arg(msg,0) as usize),
        NET_SETFD       => handle_setfd(caller_pid, arg(msg,0) as usize, arg(msg,1) as u32),
        NET_GETFD       => handle_getfd(caller_pid, arg(msg,0) as usize),
        NET_QUEUE_LEN   => handle_queue_len(caller_pid, arg(msg,0) as usize, arg(msg,1) != 0),
        _               => err_reply(-38),
    };
    if kick { drivers::netdev::kick(); }
    reply
}

// ── Handlers ─────────────────────────────────────────────────────────────────

fn handle_socket(pid: u32, domain: usize, sock_type: usize, protocol: usize) -> Message {
    match domain {
        AF_UNIX | AF_INET => {}
        // TCP and UDP only until ICMPv6/raw sockets land (L3).
        AF_INET6 => {
            let t = sock_type & 0xf;
            // ICMPv6: the unprivileged ping socket and the raw one.
            let icmp6 = protocol == IPPROTO_ICMPV6 && (t == SOCK_DGRAM || t == SOCK_RAW);
            if !icmp6 {
                if t != SOCK_STREAM && t != SOCK_DGRAM { return err_reply(-93); }
                if protocol != 0 && !(t == SOCK_STREAM && protocol == 6) && !(t == SOCK_DGRAM && protocol == 17) {
                    return err_reply(-93); // EPROTONOSUPPORT
                }
            }
        }
        // NETLINK_ROUTE (`rtnetlink`) and NETLINK_NETFILTER (`nfnl`): getifaddrs, if_nameindex, nICEr.
        AF_NETLINK => {
            if sock_type & 0xf != SOCK_RAW && sock_type & 0xf != SOCK_DGRAM { return err_reply(-94); } // ESOCKTNOSUPPORT
            if protocol != rtnetlink::NETLINK_ROUTE && protocol != nfnl::NETLINK_NETFILTER { return err_reply(-93); } // EPROTONOSUPPORT
        }
        _                 => return err_reply(-97),
    }
    // SOCK_RAW needs CAP_NET_RAW over the netns owner's userns (Linux ns_capable) — EPERM. An unprivileged ping uses
    // SOCK_DGRAM/IPPROTO_ICMP instead, which Linux allows for every group in
    // net.ipv4.ping_group_range (all of them on common distributions) and
    // which lands on the same ICMP socket below.
    if (domain == AF_INET || domain == AF_INET6) && sock_type & 0xf == SOCK_RAW
        && !sched::caps::ns_capable_in_of(pid, sched::ns::owner_userns(1, sched::ns::netns_of(pid)), sched::caps::CAP_NET_RAW) {
        return err_reply(-1);
    }
    // net.ipv4.ping_group_range of the caller's netns gates SOCK_DGRAM ICMP.
    if sock_type & 0xf == SOCK_DGRAM && ((domain == AF_INET && protocol == IPPROTO_ICMP) || (domain == AF_INET6 && protocol == IPPROTO_ICMPV6))
        && !sched::ns::ping_allowed(pid) {
        return err_reply(-13);
    }
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match get_or_create(pid, &mut *tbls) {
        Some(t) => t, None => return err_reply(-23), // ENFILE: table pool full
    };
    let slot = match tbl.alloc() { Some(s) => s, None => return err_reply(-24) };
    let state = if (domain == AF_INET && protocol == IPPROTO_ICMP) || (domain == AF_INET6 && protocol == IPPROTO_ICMPV6) {
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
        ofd:        vfs::ofd::alloc(),
        hidden:     false,
        connecting: false,
        ns:         sched::ns::NsRef::get(sched::ns::netns_of(pid)),
    };
    if domain == AF_INET6 && sched::ns::bindv6only(sched::ns::netns_of(pid)) {
        let ofd = tbl.socks[slot].ofd;
        let mut o = sock_opts(ofd);
        o.v6only = true;
        set_sock_opts(ofd, o);
    }
    if domain == AF_NETLINK { nlsock::create(tbl.socks[slot].ofd, sched::tgid_of(pid), sched::ns::netns_of(pid), protocol); }
    // A raw ICMPv6 socket receives every echo and error message without having sent first (traceroute6 reads
    // Time Exceeded from one it never wrote to): bind a wildcard socket on the stack of the default route.
    if domain == AF_INET6 && sock_type & 0xf == SOCK_RAW && protocol == IPPROTO_ICMPV6 {
        drop(tbls);
        let ns = sched::ns::netns_of(pid);
        let dflt = IpAddress::Ipv6(smoltcp::wire::Ipv6Address::new(0x2000, 0, 0, 0, 0, 0, 0, 1));
        if let Some(sid) = iface::route_select(ns, dflt) {
            let mut stack = stack_for(sid);
            let mut made = None;
            if let Some(s) = stack.as_mut() {
                let rx_buffer = icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; 16], alloc::vec![0; 16384]);
                let tx_buffer = icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; 4], alloc::vec![0; 2048]);
                let mut socket = icmp::Socket::new(rx_buffer, tx_buffer);
                if socket.bind(icmp::Endpoint::Raw).is_ok() { made = Some(s.socket_set.add(socket)); }
            }
            drop(stack);
            if let Some(socket_handle) = made {
                let mut tbls = SOCK_TABLES.lock();
                let mut ok = false;
                if let Some(tbl) = find_tbl(pid, &mut *tbls) {
                    if slot < tbl.socks.len() && tbl.socks[slot].in_use && matches!(tbl.socks[slot].state, SockState::IcmpUnbound) {
                        tbl.socks[slot].state = SockState::IcmpBound { socket_handle, sid };
                        ok = true;
                    }
                }
                drop(tbls);
                if !ok { let mut stack = stack_for(sid); if let Some(ref mut s) = *stack { release_inet_socket(s, socket_handle); } }
            }
        }
    }
    val_reply((slot + SOCK_FD_BASE) as u64)
}

fn handle_bind(pid: u32, fd: usize, addr_ptr: usize, addrlen: usize, host: Option<&[u8]>) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    if let Some(o) = nlsock::of(pid, fd) { return nlsock::bind(o, addr_ptr, addrlen); }
    if addrlen < 2 { return err_reply(-22); }

    let sa_family = match sched::uaccess::read_user::<u16>(addr_ptr) { Some(f) => f as usize, None => return err_reply(-14) };

    match sa_family {
        AF_INET | AF_INET6 => {
            // The sockaddr is read out of user memory here, before any lock is
            // taken: a demand-paging fault under SOCK_TABLES would re-enter the
            // scheduler with a server lock held.
            let sdom = sock_entry(pid, fd).map_or(AF_INET, |e| e.domain as usize);
            if !is_inet(sdom as u8) { return err_reply(-22); }
            let parsed = match sa::parse_for(sdom, addr_ptr, addrlen) { Ok(a) => a, Err(e) => return err_reply(e) };
            let port = parsed.port;
            let explicit_port = port != 0;
            // Ports below 1024 need CAP_NET_BIND_SERVICE (root): Linux's
            // net.ipv4.ip_unprivileged_port_start default, EACCES.
            if port != 0 && !sched::ns::bind_port_allowed(pid, sock_netns(pid, slot), port) { return err_reply(-13); }
            let ip = parsed.ip;
            // A V6ONLY socket cannot take a v4-mapped address (Linux inet6_bind: EINVAL).
            if sdom == AF_INET6 && matches!(ip, IpAddress::Ipv4(_)) {
                let ofd = sock_entry(pid, fd).map_or(0, |e| e.ofd);
                if sock_opts(ofd).v6only { return err_reply(-22); }
            }
            // A link-local address needs its interface named (Linux inet6_bind: EINVAL without a scope id).
            if ip6::needs_scope(ip) {
                if let Err(e) = ip6::scope_sid(sock_netns(pid, slot), parsed.scope) { return err_reply(e); }
            }
            // A fresh network namespace has only a DOWN loopback: no address
            // can be bound but the wildcard.
            if !ip.is_unspecified() && sock_netns(pid, slot) != 0 && vif::lo_up_sid(sock_netns(pid, slot)).is_none() { return err_reply(-99); }

            // An address no interface owns is EADDRNOTAVAIL on Linux. INADDR_ANY
            // and 127.0.0.0/8 always bind; anything else must be an address the
            // NIC currently holds. Without this a bind to a bogus address
            // "succeeded" and then silently never received anything.
            if !ip.is_unspecified() && !is_loopback_addr(ip) && stack_holding(sock_netns(pid, slot), ip).is_none() {
                return err_reply(-99); // EADDRNOTAVAIL
            }

            // TIME_WAIT is a leaf lock, so its snapshot is taken before
            // SOCK_TABLES and never underneath it.
            let mut resv = [0u16; MAX_TIME_WAIT];
            let nresv = time_wait_snapshot(&mut resv);

            let mut tbls = SOCK_TABLES.lock();
            // The entry (and its SO_REUSEADDR) is read through a shared
            // borrow, before the mutable one below.
            let me = match tbls.iter().find(|t| t.in_use && t.pid == tk(pid))
                .and_then(|t| t.socks.get(slot).cloned()).filter(|e| e.in_use)
            {
                Some(e) => e,
                None => return err_reply(-9),
            };
            // A UDP socket whose automatic port connect(AF_UNSPEC) gave up
            // is unbound again (Linux: inet_sport 0) and may bind; its old
            // smoltcp socket goes once the bind is certain.
            let mut released: Option<(Sid, SocketHandle)> = None;
            match me.state {
                SockState::Unbound { .. } if is_inet(me.domain) => {}
                SockState::InetConnected { socket_handle, sid, .. }
                    if me.sock_type == SOCK_DGRAM as u8 && me.bound_port == 0
                        && udpany::of(me.ofd).is_none() => released = Some((sid, socket_handle)),
                // A ping socket: Linux records a source address; this server
                // picks the stack at the first send, so nothing is kept. It
                // used to turn the socket into a UDP one.
                SockState::IcmpUnbound | SockState::IcmpBound { .. } => return ok_reply(),
                // Already bound (Linux inet_bind: EINVAL), or not AF_INET.
                // Going on replaced the state of a bound UDP socket and
                // leaked its smoltcp socket, port included.
                _ => return err_reply(-22),
            }
            let reuseaddr = me.reuseaddr;
            // A port another socket holds (Linux inet_csk_bind_conflict /
            // udp_lib_lport_inuse). bind() used to succeed regardless, and
            // the second socket then never received anything: smoltcp hands
            // a datagram or a SYN to the first socket that matches the port.
            if port != 0 && bind_conflict(&*tbls, pid, &me, ip, port) {
                return err_reply(-98); // EADDRINUSE
            }
            // An explicit port whose last connection closed less than 2*MSL ago
            // is EADDRINUSE unless the caller asked for SO_REUSEADDR — the
            // Linux answer, and the reason every server sets that option.
            // Live ports are `bind_conflict`'s, above.
            if port != 0 && !reuseaddr && me.netns() == 0 && resv[..nresv].contains(&port) {
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
            if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }

            let local_endpoint = IpEndpoint::new(ip, port);

            if let Some((sid, h)) = released {
                let mut stack = stack_for(sid);
                if let Some(s) = stack.as_mut() { s.socket_set.remove(h); }
            }
            tbl.socks[slot].state = SockState::InetBound {
                domain: me.domain,
                sock_type: tbl.socks[slot].sock_type,
                local_endpoint,
            };
            tbl.socks[slot].bound_port = port;
            // A bound UDP socket must be able to receive before it ever
            // sends (a server, or a client that polls first), so it gets its
            // smoltcp socket now.
            let dgram = tbl.socks[slot].sock_type == SOCK_DGRAM as u8;
            share_state(&mut *tbls, pid, slot);
            drop(tbls);
            // What this bind fixed, for connect(AF_UNSPEC) (Linux sets
            // SOCK_BINDADDR_LOCK / SOCK_BINDPORT_LOCK only for what the
            // caller named).
            {
                let mut o = sock_opts(me.ofd);
                o.bind_lock = (if ip.is_unspecified() { 0 } else { BIND_ADDR_LOCK })
                    | (if explicit_port { BIND_PORT_LOCK } else { 0 });
                set_sock_opts(me.ofd, o);
            }
            if dgram {
                // A wildcard bind receives on every interface (`udpany`), as
                // on Linux — also bind(0.0.0.0:0), which quinn-udp and other
                // clients make: on one stack, a datagram arriving through
                // another interface was refused with a port unreachable and
                // its sends left by the first stack's route (lane udp2).
                let server = ip.is_unspecified();
                // A non-initial netns has only a down loopback: its UDP bind
                // stays a pure table entry, no host-stack socket (unreachable).
                if me.netns() == 0 || vif::lo_up_sid(me.netns()).is_some() {
                    if let Err(e) = udp_materialize(pid, slot, None, 0, server) { return err_reply(e); }
                }
            }
            ok_reply()
        }
        AF_UNIX | _ => {
            if addrlen < 3 || addrlen > 2 + PATH_MAX { return err_reply(-22); }
            let path_len = addrlen - 2;
            let path_ptr = (addr_ptr + 2) as *const u8;

            // Copy sun_path out of user memory into a kernel buffer up front —
            // no user dereference happens under any lock or inside the VFS.
            let mut pbytes = [0u8; PATH_MAX];
            if !ucopy_in(&mut pbytes[..path_len], path_ptr as usize) { return err_reply(-14); }
            let is_abstract = pbytes[0] == 0;
            let sock_id = alloc_sock_id();
            let nsid_b = sock_netns(pid, slot);

            // ── Pathname socket: the VFS owns uniqueness via a real S_IFSOCK
            // node. Bind does not follow a symlink for the final component.
            if !is_abstract {
                // sun_path is a C string for pathname sockets; the address ends
                // at the first NUL (addrlen may or may not include it).
                let name_end = pbytes[..path_len].iter().position(|&b| b == 0).unwrap_or(path_len);
                let name = &pbytes[..name_end];
                if name.is_empty() { return err_reply(-22); }
                match vfs::unix_bind_node(pid, host.unwrap_or(name), sock_id) {
                    0   => {}
                    -17 => return err_reply(-98), // node exists → EADDRINUSE
                    e   => return make_reply(e as i64), // ENOENT/EACCES/EOPNOTSUPP/ENOSPC
                }
            } else {
                // Abstract namespace: byte-matched here, never a VFS node.
                let nsid = sock_netns(pid, slot);
                let bound = BOUND_PATHS.lock();
                for bp in bound.iter() {
                    if bp.in_use && bp.is_abstract && bp.netns == nsid && bp.path_len == path_len
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
                                     sock_id, is_abstract, netns: if is_abstract { nsid_b } else { 0 }, refs: 1,
                                     _owner_pid: pid, _owner_sock: slot };
            drop(bound);

            // Scope the SOCK_TABLES lock so it is released before any
            // free_bound_idx (which takes BOUND_PATHS) — the two are never held
            // nested, keeping BOUND_PATHS strictly a leaf.
            let ok = {
                let mut tbls = SOCK_TABLES.lock();
                match find_tbl(pid, &mut *tbls) {
                    Some(t) if slot < t.socks.len() && t.socks[slot].in_use => {
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
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }

    if is_inet(tbl.socks[slot].domain) {
        // bind() is what assigns the port (including the ephemeral one for a
        // bind to :0), so a socket that never bound has no InetBound state and
        // gets EINVAL — the same answer the old zero-bound_port check gave.
        let local = match tbl.socks[slot].state {
            SockState::InetBound { local_endpoint, .. } => local_endpoint,
            // A second listen() on an already-listening socket is legal on
            // Linux — `inet_listen` only updates sk_max_ack_backlog and
            // returns 0 — and matching `InetBound` alone answered EINVAL.
            // Answer success and change nothing: falling through to the
            // listener creation below would be actively wrong, a second
            // listener on the same port. The backlog is `listen::BACKLOG`
            // per stack whatever the caller asks for.
            SockState::InetListening { .. } => return ok_reply(),
            _ => return err_reply(-22),
        };
        if local.port == 0 { return err_reply(-22); }

        // INADDR_ANY listens on every stack, as it does on Linux; an explicit
        // address on every NIC stack, filtered by that address (see
        // `listen`). NIC stacks exist from boot whether or not they hold an
        // address, so a listener follows every later lease, address change
        // and roam without being touched.
        // Registered first: a stack built after this point (wifi0, once
        // its MAC is known) adopts the listener (`listen::adopt`).
        let lns = tbl.socks[slot].netns();
        let v6only = tbl.socks[slot].domain == AF_INET6 as u8 && sock_opts(tbl.socks[slot].ofd).v6only;
        let lid = listen::new_lid();
        if lns != 0 {
            // A non-initial netns: the listener lives on its own lo stack, if
            // that exists. With lo never brought up it is a table entry only
            // (port space, nothing reachable), as on Linux.
            for sid in listen::stacks_for(lns, local) { listen::create(sid, lid, local, v6only); }
            tbl.socks[slot].state = SockState::InetListening { lid, local };
            share_state(&mut *tbls, pid, slot);
            return ok_reply();
        }
        listen::register(lid, local, v6only);
        let mut any = false;
        for sid in listen::stacks_for(tbl.socks[slot].netns(), local) { any |= listen::create(sid, lid, local, v6only); }
        if !any { listen::destroy_all(lid); return err_reply(-100); } // ENETDOWN

        tbl.socks[slot].state = SockState::InetListening { lid, local };
        share_state(&mut *tbls, pid, slot);
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

    let (state, _bound_port, sock_type, listen_ofd, lnetns) = {
        let tbls = SOCK_TABLES.read();
        let tbl = match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
        (tbl.socks[slot].state, tbl.socks[slot].bound_port, tbl.socks[slot].sock_type, tbl.socks[slot].ofd, tbl.socks[slot].netns())
    };

    match state {
        SockState::InetListening { lid, .. } => {
            // The oldest completed connection on any stack: the NICs', then
            // loopback. `listen::accept` holds one stack lock at a time and
            // hands the connection over (its queue no longer names it).
            let order = iface::nic_sids().chain(core::iter::once(LO)).chain(vif::all_dyn_sids());
            let Some((from, established)) = order
                .filter_map(|sid| listen::accept(sid, lid).map(|h| (sid, h)))
                .next()
            else {
                return err_reply(-11); // EAGAIN: no completed handshake yet
            };
            // Undo the take-over when the connection cannot be installed.
            let drop_conn = |sid: Sid, h: SocketHandle| {
                let mut stack = stack_for(sid);
                if let Some(ref mut s) = *stack {
                    s.socket_set.get_mut::<tcp::Socket>(h).abort();
                    s.orphans.push((h, sched::ticks() + ORPHAN_TICKS));
                }
            };

            let new_slot = {
                let mut tbls = SOCK_TABLES.lock();
                // The listener may have been closed meanwhile (by another
                // holder): then the connection is reset, as its queue was.
                let still = find_tbl(pid, &mut *tbls)
                    .filter(|t| slot < t.socks.len() && t.socks[slot].in_use)
                    .map(|t| matches!(t.socks[slot].state, SockState::InetListening { lid: l, .. } if l == lid));
                if still != Some(true) {
                    drop(tbls);
                    drop_conn(from, established);
                    return err_reply(-9);
                }
                let tbl = match find_tbl(pid, &mut *tbls) {
                    Some(t) => t, None => return err_reply(-9),
                };
                let new_slot = match tbl.alloc() {
                    Some(sn) => sn,
                    None => { drop(tbls); drop_conn(from, established); return err_reply(-24); }
                };
                // Linux sk_clone: the child lives in the listener's netns.
                let lns = tbl.socks[slot].ns.clone();
                tbl.socks[new_slot] = SockEntry {
                    state: SockState::InetConnected {
                        socket_handle: established, remote_endpoint: None, sid: from },
                    in_use: true,
                    bound_port: 0,
                    domain: tbl.socks[slot].domain,
                    sock_type: SOCK_STREAM as u8,
                    cloexec: acc_cloexec,
                    nonblock: acc_nonblock,
                    // An accepted socket is never bind()ed, so the flag has no
                    // consumer on it.
                    reuseaddr: false,
                    ofd: vfs::ofd::alloc(),
                    hidden: false,
                    connecting: false,
                    ns: lns,
                };
                // Linux clones the accepted socket from the listener, so it
                // inherits TCP_NODELAY and the keepalive settings. The
                // smoltcp socket was the listener's own, which never had them
                // applied (they only matter once connected).
                let o = sock_opts(listen_ofd);
                if o != SockOpts::DEFAULT {
                    set_sock_opts(tbl.socks[new_slot].ofd, o);
                    let mut stack = stack_for(from);
                    if let Some(s) = stack.as_mut() {
                        apply_tcp_opts(s.socket_set.get_mut::<tcp::Socket>(established), o, true);
                    }
                }
                new_slot
            };

            if addr_ptr != 0 && addrlen_ptr != 0 {
                // Read the peer endpoint under the stack lock, write it into
                // user memory only after that lock is gone.
                let peer = {
                    let mut stack = stack_for(from);
                    stack.as_mut().and_then(|s|
                        s.socket_set.get_mut::<tcp::Socket>(established).remote_endpoint())
                };
                if let Some(endpoint) = peer {
                    unsafe { write_sockaddr_in(sock_dom(pid, fd), addr_ptr, addrlen_ptr, endpoint); }
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
            // The oldest connection on this listener's backlog, including one
            // whose connector has closed meanwhile (see `pending_sid`).
            let found = unix_backlog_head(&UNIX_CONNS.lock(), listen_sock_id);
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
                        if conn_idx >= c.len() || !c[conn_idx].in_use {
                            return err_reply(-11);
                        }
                        c[conn_idx].cred_b = cred_b;
                        c[conn_idx].pending_sid = 0;
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
                        ofd:        vfs::ofd::alloc(),
                        hidden:     false,
                        ns:         sched::ns::NsRef::get(lnetns),
                        connecting: false,
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
                    put_sockaddr_unnamed(addr_ptr, addrlen_ptr, AF_UNIX as u16);
                    dbg("[NET] accept pid="); dbg_u(pid as u64);
                    dbg(" sid="); dbg_u(listen_sock_id);
                    dbg(" conn="); dbg_u(conn_idx as u64);
                    dbg(" -> fd="); dbg_u((new_slot + SOCK_FD_BASE) as u64); dbg("\r\n");
                    // Wake the connector parked in poll/connect (K2).
                    sched::wake_poll_tagged(sched::poll_tag(sched::poll_class::UNIX, conn_idx as u32));
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

/// Give an AF_INET datagram socket its smoltcp UDP socket: bound to the
/// port `bind()` recorded or a fresh ephemeral one, on the loopback stack when
/// the bound address or the first destination is 127.0.0.0/8, else on the
/// stack holding the bound address, else on the stack the first destination
/// routes through (`iface::route_select`), else on the default route's
/// stack. The fd becomes `InetConnected` with no remote endpoint, which the
/// send/recv/poll paths already treat as an unconnected UDP socket. A socket
/// that already has one is left alone.
/// Move an unconnected-address UDP socket (`handle` on stack `from`) to stack
/// `to`, keeping its port and peer. False when the socket changed meanwhile,
/// is bound to a specific address, or the port cannot be bound on `to`.
/// SOCK_TABLES, then one stack at a time (the documented order).
fn udp_migrate(pid: u32, fd: usize, handle: SocketHandle, from: Sid, to: Sid) -> bool {
    let Some(slot) = fd_to_slot(fd) else { return false };
    let mut tbls = SOCK_TABLES.lock();
    let Some(tbl) = find_tbl(pid, &mut *tbls) else { return false };
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return false; }
    if udpany::of(tbl.socks[slot].ofd).is_some() { return false; }
    let remote = match tbl.socks[slot].state {
        SockState::InetConnected { socket_handle, remote_endpoint, sid }
            if socket_handle == handle && sid == from => remote_endpoint,
        _ => return false,
    };
    let port = {
        let mut old = stack_for(from);
        let Some(s) = old.as_mut() else { return false };
        let ep = s.socket_set.get_mut::<udp::Socket>(handle).endpoint();
        if ep.addr.is_some() { return false; }
        ep.port
    };
    let new_handle = {
        let mut new = stack_for(to);
        let Some(s) = new.as_mut() else { return false };
        let mut socket = new_udp_socket();
        if socket.bind(port).is_err() { return false; }
        socket.set_peer(remote);
        socket.set_reuseport(sock_opts(tbl.socks[slot].ofd).reuseport);
        { let so = sock_opts(tbl.socks[slot].ofd); socket.set_recverr(so.recv & RECV_ERR != 0 || so.recv6 & RECV6_ERR != 0); }
        s.socket_set.add(socket)
    };
    {
        let mut old = stack_for(from);
        if let Some(s) = old.as_mut() { s.socket_set.remove(handle); }
    }
    let old_keys = [inet_key(from, handle), sock_state_key(tbl.socks[slot].ofd)];
    tbl.socks[slot].state = SockState::InetConnected { socket_handle: new_handle, remote_endpoint: remote, sid: to };
    share_state(&mut *tbls, pid, slot);
    drop(tbls);
    // Its pollers parked on the old socket's key, which nothing wakes now.
    sock_state_moved(&old_keys);
    true
}

/// Set (or clear) the peer smoltcp filters a UDP socket's datagrams by
/// (`udp::Socket::set_peer`). Caller may hold SOCK_TABLES.
fn udp_set_peer(sid: Sid, h: SocketHandle, peer: Option<IpEndpoint>) {
    let mut stack = stack_for(sid);
    if let Some(s) = stack.as_mut() { s.socket_set.get_mut::<udp::Socket>(h).set_peer(peer); }
}

/// No route to a destination: ENETDOWN without any NIC, ENETUNREACH else.
fn no_route_err() -> i32 { if drivers::netdev::count() == 0 { -100 } else { -101 } }

/// The stack a UDP socket bound to `local` (if bound) and first sending to
/// `dest` (if known) lives on (see `udp_materialize`).
fn udp_stack(ns: u16, local: Option<IpAddress>, dest: Option<IpAddress>, scope: u32) -> Result<Sid, i32> {
    if ns != 0 {
        // Loopback stays on the namespace's own lo (down = no route); anything
        // else leaves over its own tun/tap interfaces only, never a host NIC.
        if local.map_or(false, is_loopback_addr) || dest.map_or(false, is_loopback_addr) {
            return vif::lo_up_sid(ns).ok_or(-101);
        }
        if let Some(IpAddress::Ipv6(d)) = dest {
            return tunbe::route6_in_ns(ns, d.0).ok_or(-101);
        }
        if let Some(a) = local.filter(|a| !a.is_unspecified()) {
            return stack_holding(ns, a).ok_or(-99);
        }
        let d = dest.and_then(|d| if let IpAddress::Ipv4(x) = d { Some(x.0) } else { None });
        if d.is_none() {
            // A wildcard bind with no peer yet: the namespace's first interface stack, else its lo.
            let lo = vif::lo_up_sid(ns);
            return vif::ns_sids_dyn(ns).into_iter().find(|&s| Some(s) != lo).or(lo).ok_or(-101);
        }
        return d.and_then(|d| vif::route_in_ns(ns, d)).ok_or(-101);
    }
    if local.map_or(false, is_loopback_addr) || dest.map_or(false, is_loopback_addr) { return Ok(LO); }
    if let Some(a) = local.filter(|a| !a.is_unspecified()) {
        if let Some(sid) = iface::holder_of(a) { return Ok(sid); }
    }
    if let Some(d) = dest.filter(|d| ip6::needs_scope(*d)) { return ip6::scope_sid(ns, scope); }
    match dest {
        Some(d) => iface::route_select(ns, d).ok_or_else(no_route_err),
        None => iface::default_sid().ok_or(-100),
    }
}

/// `server`: a wildcard bind to a chosen port, which gets a socket on every
/// stack (`udpany`) and so needs no route to exist yet: its primary is the
/// default route's stack, else loopback.
fn udp_materialize(pid: u32, slot: usize, dest: Option<IpAddress>, scope: u32, server: bool) -> Result<(), i32> {
    let mut resv = [0u16; MAX_TIME_WAIT];
    let nresv = time_wait_snapshot(&mut resv);
    let mut tbls = SOCK_TABLES.lock();
    let ephemeral = alloc_ephemeral_port(&*tbls, &resv[..nresv]);
    let tbl = find_tbl(pid, &mut *tbls).ok_or(-9i32)?;
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return Err(-9); }
    // Backstop: a socket of a non-initial netns only ever lands on its own
    // lo or tun/tap interfaces (`udp_stack`); with neither it fails here.
    if tbl.socks[slot].netns() != 0 && vif::lo_up_sid(tbl.socks[slot].netns()).is_none()
        && !tunbe::has_vifs(tbl.socks[slot].netns()) { return Err(-101); }
    let local_addr = match tbl.socks[slot].state {
        SockState::Unbound { .. } => None,
        SockState::InetBound { local_endpoint, .. } => Some(local_endpoint.addr),
        SockState::InetConnected { .. } => return Ok(()),
        _ => return Err(-22),
    };
    // A wildcard socket is held to its family: AF_INET (or a mapped 0.0.0.0)
    // never hears a v6 datagram; AF_INET6 `::` is dual unless V6ONLY.
    let v4only = tbl.socks[slot].domain == AF_INET as u8
        || matches!(local_addr, Some(IpAddress::Ipv4(_)));
    let v6only = tbl.socks[slot].domain == AF_INET6 as u8 && sock_opts(tbl.socks[slot].ofd).v6only;
    let fam = if v4only { Some(smoltcp::wire::IpVersion::Ipv4) }
        else if v6only { Some(smoltcp::wire::IpVersion::Ipv6) } else { None };
    // No object yet, so this is just its SOCKSTATE key (no `udpany::of` lock).
    let old_keys = [sock_state_key(tbl.socks[slot].ofd)];
    let port = if tbl.socks[slot].bound_port != 0 {
        tbl.socks[slot].bound_port
    } else {
        ephemeral.ok_or(-98i32)? // EADDRINUSE: range exhausted
    };
    let sid = match udp_stack(tbl.socks[slot].netns(), local_addr, dest, scope) {
        Ok(s) => s,
        Err(_) if server && tbl.socks[slot].netns() == 0 => LO,
        Err(e) => return Err(e),
    };
    // Multicast and limited broadcast leave through IP_MULTICAST_IF's
    // interface when one is set (Linux udp_sendmsg: oif = mc_index).
    let sid = match dest.and_then(|d| mcast_if_sid(tbl.socks[slot].ofd, d)) {
        Some(m) if local_addr.map_or(true, |a| a.is_unspecified()) => m,
        _ => sid,
    };
    let handle = {
        let mut stack = stack_for(sid);
        let s = stack.as_mut().ok_or(-100i32)?; // ENETDOWN
        let mut socket = new_udp_socket();
        let ep = smoltcp::wire::IpListenEndpoint {
            addr: local_addr.filter(|a| !a.is_unspecified()),
            port,
        };
        if socket.bind(ep).is_err() { return Err(-22); }
        if fam.is_some() { socket.set_family(fam); }
        socket.set_reuseport(sock_opts(tbl.socks[slot].ofd).reuseport);
        { let so = sock_opts(tbl.socks[slot].ofd); socket.set_recverr(so.recv & RECV_ERR != 0 || so.recv6 & RECV6_ERR != 0); }
        s.socket_set.add(socket)
    };
    tbl.socks[slot].bound_port = port;
    tbl.socks[slot].state = SockState::InetConnected { socket_handle: handle, remote_endpoint: None, sid };
    if server { udpany::create(tbl.socks[slot].netns(), tbl.socks[slot].ofd, sid, handle, port, fam); }
    share_state(&mut *tbls, pid, slot);
    drop(tbls);
    // A poller of the unbound socket (another thread, another holder) now
    // has a socket to wait on: re-probe under its key.
    sock_state_moved(&old_keys);
    Ok(())
}

/// connect(AF_UNSPEC) on an AF_INET socket; `None` when the slot is not one
/// (the caller goes on as before).
///
/// UDP (Linux `__udp_disconnect`): no default destination any more (send is
/// EDESTADDRREQ, getpeername ENOTCONN), datagrams from anyone again, and a
/// port nobody chose (no `BIND_PORT_LOCK`: picked by bind(port 0), connect or
/// the first send) is given up: getsockname says port 0 and the next send,
/// connect or bind binds anew. Datagrams already queued stay readable.
///
/// TCP (Linux `tcp_disconnect`): a connection is aborted (an RST once
/// synchronized), a listener stops listening, and the socket is unconnected
/// again: it may connect anew, keeping only the port and address its bind()
/// named. Always 0, as on Linux.
fn inet_disconnect(pid: u32, slot: usize) -> Option<Message> {
    let mut tbls = SOCK_TABLES.lock();
    let tbl = find_tbl(pid, &mut *tbls)?;
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return None; }
    let e = tbl.socks[slot].clone();
    if !is_inet(e.domain) { return None; }
    let lock = sock_opts(e.ofd).bind_lock;
    match e.state {
        SockState::Unbound { .. } | SockState::InetBound { .. }
        | SockState::IcmpUnbound | SockState::IcmpBound { .. } => Some(ok_reply()),
        SockState::InetConnected { socket_handle, sid, .. } if e.sock_type == SOCK_DGRAM as u8 => {
            let keep_port = lock & BIND_PORT_LOCK != 0 || udpany::of(e.ofd).is_some();
            {
                let mut stack = stack_for(sid);
                if let Some(s) = stack.as_mut() {
                    let u = s.socket_set.get_mut::<udp::Socket>(socket_handle);
                    u.set_peer(None);
                    if !keep_port { u.unbind_port(); }
                }
            }
            if !keep_port { tbl.socks[slot].bound_port = 0; }
            tbl.socks[slot].state = SockState::InetConnected { socket_handle, remote_endpoint: None, sid };
            share_state(&mut *tbls, pid, slot);
            Some(ok_reply())
        }
        SockState::InetConnected { socket_handle, sid, .. } if e.sock_type == SOCK_STREAM as u8 => {
            let mut local = None;
            {
                let mut stack = stack_for(sid);
                if let Some(s) = stack.as_mut() {
                    let t = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    local = t.local_endpoint();
                    if matches!(t.state(), tcp::State::Closed | tcp::State::TimeWait) {
                        s.icmp_peeked.remove(&socket_handle);
                        s.socket_set.remove(socket_handle);
                    } else {
                        // The RST leaves on the daemon's next pass; the
                        // orphan is reaped after it (`reap_orphans`).
                        t.abort();
                        s.orphans.push((socket_handle, sched::ticks() + ORPHAN_TICKS));
                    }
                }
            }
            let addr = local.map(|l| l.addr).filter(|_| lock & BIND_ADDR_LOCK != 0)
                .unwrap_or(IpAddress::v4(0, 0, 0, 0));
            tcp_unconnect(tbl, slot, lock, addr);
            share_state(&mut *tbls, pid, slot);
            drop(tbls);
            drivers::netdev::kick();
            Some(ok_reply())
        }
        SockState::InetListening { lid, local } => {
            tcp_unconnect(tbl, slot, lock, local.addr);
            share_state(&mut *tbls, pid, slot);
            drop(tbls);
            listen::destroy_all(lid);
            Some(ok_reply())
        }
        _ => None,
    }
}

/// The table side of a TCP disconnect: unbound, or bound to what bind()
/// named (`lock`), not connecting.
fn tcp_unconnect(tbl: &mut ProcSockTable, slot: usize, lock: u8, addr: IpAddress) {
    let e = &mut tbl.socks[slot];
    e.connecting = false;
    if lock & BIND_PORT_LOCK != 0 && e.bound_port != 0 {
        e.state = SockState::InetBound { domain: e.domain, sock_type: e.sock_type,
                                         local_endpoint: IpEndpoint::new(addr, e.bound_port) };
    } else {
        e.bound_port = 0;
        e.state = SockState::Unbound { domain: e.domain, sock_type: e.sock_type };
    }
}

/// A UDP socket whose automatic port connect(AF_UNSPEC) gave up binds a new
/// one before it sends or connects again (Linux autobinds the same way).
/// Nothing to do for any other socket.
fn udp_rebind_port(pid: u32, slot: usize) -> Result<(), i32> {
    let mut resv = [0u16; MAX_TIME_WAIT];
    let nresv = time_wait_snapshot(&mut resv);
    let mut tbls = SOCK_TABLES.lock();
    let ephemeral = alloc_ephemeral_port(&*tbls, &resv[..nresv]);
    let tbl = find_tbl(pid, &mut *tbls).ok_or(-9i32)?;
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return Err(-9); }
    let e = tbl.socks[slot].clone();
    let SockState::InetConnected { socket_handle, sid, .. } = e.state else { return Ok(()) };
    if e.sock_type != SOCK_DGRAM as u8 || e.bound_port != 0 { return Ok(()); }
    let port = ephemeral.ok_or(-98i32)?; // EADDRINUSE: range exhausted
    {
        let mut stack = stack_for(sid);
        let s = stack.as_mut().ok_or(-100i32)?;
        let u = s.socket_set.get_mut::<udp::Socket>(socket_handle);
        let addr = u.endpoint().addr;
        u.bind(smoltcp::wire::IpListenEndpoint { addr, port }).map_err(|_| -22i32)?;
    }
    tbl.socks[slot].bound_port = port;
    share_state(&mut *tbls, pid, slot);
    Ok(())
}

/// Connections the host made before an OUTPUT DNAT rewrote the peer: (pid, slot, original, actual).
static ORIG_PEER: Mutex<alloc::vec::Vec<(u32, usize, IpEndpoint, IpEndpoint)>> = Mutex::new(alloc::vec::Vec::new());

/// nftables OUTPUT chain for a host-local connect (podnat L2): `fib daddr type local` + DNAT redirects
/// 127.0.0.1:H / nicip:H to the container. getpeername keeps the original (see `ORIG_PEER`).
fn nft_output_dnat(pid: u32, fd: usize, slot: usize, ip: IpAddress, ep: IpEndpoint) -> (IpAddress, IpEndpoint) {
    if !nfnl::has_tables(0) || sock_netns(pid, slot) != 0 { return (ip, ep) }
    let l4 = match sock_entry(pid, fd).map(|e| e.sock_type) {
        Some(t) if t == SOCK_STREAM as u8 => 6u8,
        Some(t) if t == SOCK_DGRAM as u8 => 17u8,
        _ => return (ip, ep),
    };
    let (mut m, v6) = match ip {
        IpAddress::Ipv4(a) => {
            let d = a.0;
            if !fwd::host_local(d) { return (ip, ep) }
            let src = if d[0] == 127 { [127, 0, 0, 1] } else { d };
            (nfnl::Pkt { l4, src, dst: d, sport: 0, dport: ep.port, oif: [0; 16], mark: 0, local: true, v6: None }, false)
        }
        IpAddress::Ipv6(a) => {
            let d = a.0;
            let lo = d == [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1];
            if !lo && !ip6::is_host6(&d) { return (ip, ep) }
            (nfnl::Pkt { l4, src: [0; 4], dst: [0; 4], sport: 0, dport: ep.port, oif: [0; 16], mark: 0, local: true, v6: Some((d, d)) }, true)
        }
    };
    let (acc, nat) = nfnl::eval(0, nfnl::H_OUT, &mut m);
    let _ = acc;
    let real = match (nat, v6) {
        (nfnl::Nat::Dnat(c, cp), false) => IpEndpoint::new(IpAddress::Ipv4(smoltcp::wire::Ipv4Address::new(c[0], c[1], c[2], c[3])), if cp == 0 { ep.port } else { cp }),
        (nfnl::Nat::Dnat6(c, cp), true) => IpEndpoint::new(IpAddress::Ipv6(smoltcp::wire::Ipv6Address(c)), if cp == 0 { ep.port } else { cp }),
        _ => return (ip, ep),
    };
    let mut t = ORIG_PEER.lock();
    t.retain(|x| !(x.0 == pid && x.1 == slot));
    if t.len() >= 256 { t.remove(0); }
    t.push((pid, slot, ep, real));
    (real.addr, real)
}

/// A datagram's source as the application knows the peer: the container's address maps back to the
/// 127.0.0.1:H it sent to (see `ORIG_PEER`).
fn nft_unorig(pid: u32, fd: usize, ep: IpEndpoint) -> IpEndpoint {
    if !nfnl::has_tables(0) { return ep; }
    let Some(slot) = fd_to_slot(fd) else { return ep };
    ORIG_PEER.lock().iter().find(|x| x.0 == pid && x.1 == slot && x.3 == ep).map_or(ep, |x| x.2)
}

fn handle_connect(pid: u32, fd: usize, addr_ptr: usize, addrlen: usize, host: Option<&[u8]>) -> Message {
    if let Some(o) = nlsock::of(pid, fd) { return nlsock::connect(o, addr_ptr, addrlen); }
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    if addrlen < 2 { return err_reply(-22); }

    let sa_family = match sched::uaccess::read_user::<u16>(addr_ptr) { Some(f) => f as usize, None => return err_reply(-14) };
    // connect(AF_UNSPEC) on an AF_INET socket (it used to fall into the
    // AF_UNIX path below): see `inet_disconnect`.
    if sa_family == AF_UNSPEC {
        if let Some(r) = inet_disconnect(pid, slot) { return r; }
    }

    if sa_family == AF_INET || sa_family == AF_INET6 {
        let sdom = sock_entry(pid, fd).map_or(AF_INET, |e| e.domain as usize);
        let parsed = match sa::parse_for(sdom, addr_ptr, addrlen) { Ok(a) => a, Err(e) => return err_reply(e) };
        let port = parsed.port;
        let remote_ip = parsed.ip;
        // A V6ONLY socket cannot reach a v4-mapped peer (Linux: ENETUNREACH).
        if sdom == AF_INET6 && matches!(remote_ip, IpAddress::Ipv4(_)) {
            if sock_entry(pid, fd).map_or(false, |e| sock_opts(e.ofd).v6only) { return err_reply(-101); }
        }
        let remote_endpoint = IpEndpoint::new(remote_ip, port);
        let (remote_ip, remote_endpoint) = nft_output_dnat(pid, fd, slot, remote_ip, remote_endpoint);
        // 127.0.0.0/8 is served by the loopback stack, everything else by the
        // stack the route lookup picks (`iface::route_select`), or the one
        // holding a bound local address. Decided once, here, and recorded in
        // the socket state so every later send/recv/poll/close looks in the
        // same socket set: an established flow stays on its interface.
        let (bound_addr, dgram_ofd) = {
            let tbls = SOCK_TABLES.read();
            let e = tbls.iter().find(|t| t.in_use && t.pid == tk(pid))
                .and_then(|t| t.socks.get(slot).filter(|e| e.in_use).cloned());
            (e.clone().and_then(|e| match e.state {
                SockState::InetBound { local_endpoint, .. } => Some(local_endpoint.addr),
                _ => None,
            }),
             e.filter(|e| is_inet(e.domain) && e.sock_type == SOCK_DGRAM as u8).map(|e| e.ofd))
        };
        if let Some(ofd) = dgram_ofd {
            // A broadcast peer needs SO_BROADCAST (Linux ip4_datagram_connect).
            if iface::is_broadcast(remote_ip) && !sock_opts(ofd).broadcast { return err_reply(-13); }
            // A port connect(AF_UNSPEC) gave up is replaced first.
            if let Err(e) = udp_rebind_port(pid, slot) { return err_reply(e); }
        }
        let cns = sock_netns(pid, slot);
        // A link-local peer is reached through the interface its scope id names.
        let scoped = if ip6::needs_scope(remote_ip) {
            match ip6::scope_sid(cns, parsed.scope) { Ok(s) => Some(s), Err(e) => return err_reply(e) }
        } else { None };
        let sid = if let Some(s) = scoped { s } else { match bound_addr.filter(|a| cns == 0 && !a.is_unspecified() && !is_loopback_addr(*a))
            .and_then(iface::holder_of)
        {
            Some(sid) if !is_loopback_addr(remote_ip) => sid,
            _ => match iface::route_select(cns, remote_ip) {
                Some(sid) => sid,
                None => return err_reply(if cns != 0 { -101 } else { no_route_err() }),
            },
        } };

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
        if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }

        let sock_type = tbl.socks[slot].sock_type;
        // A repeat connect() on a TCP socket: EALREADY while the first is
        // still in flight, EISCONN after (Linux). Going on would add a second
        // smoltcp socket and orphan the first.
        if sock_type == SOCK_STREAM as u8 {
            if let SockState::InetConnected { .. } = tbl.socks[slot].state {
                return err_reply(if tbl.socks[slot].connecting { -114 } else { -106 });
            }
        }
        // A UDP socket that already sent or bound owns a smoltcp socket on
        // its port: connect() only (re)sets the peer. Creating a second one,
        // as the code below does for a fresh socket, would leak the first
        // and fail to bind the port it still holds.
        if sock_type == SOCK_DGRAM as u8 {
            if let SockState::InetConnected { socket_handle, sid: was, .. } = tbl.socks[slot].state {
                // A wildcard server (`udpany`) that connects talks to one
                // peer from then on, which reaches it through one interface:
                // it keeps its socket on that stack and the others go. Left
                // as it was, the server kept receiving on every stack and a
                // move of its primary left the mirror table naming a removed
                // socket (a panic at the next recv or poll).
                match udpany::collapse(tbl.socks[slot].ofd, sid) {
                    Some(Some(h)) => {
                        udp_set_peer(sid, h, Some(remote_endpoint));
                        tbl.socks[slot].state = SockState::InetConnected {
                            socket_handle: h, remote_endpoint: Some(remote_endpoint), sid };
                        share_state(&mut *tbls, pid, slot);
                        return ok_reply();
                    }
                    // None of its sockets was on that stack, and all are
                    // gone now: one is made below.
                    Some(None) => {}
                    None => {
                        if was == sid {
                            udp_set_peer(sid, socket_handle, Some(remote_endpoint));
                            tbl.socks[slot].state = SockState::InetConnected {
                                socket_handle, remote_endpoint: Some(remote_endpoint), sid };
                            share_state(&mut *tbls, pid, slot);
                            return ok_reply();
                        }
                        // Moving between stacks: drop the old socket and fall
                        // through to create one on the new stack.
                        let mut old = stack_for(was);
                        if let Some(ref mut s) = *old { s.socket_set.remove(socket_handle); }
                    }
                }
            }
        }
        let local_port = if tbl.socks[slot].bound_port != 0 {
            tbl.socks[slot].bound_port
        } else {
            match ephemeral {
                Some(p) => { tbl.socks[slot].bound_port = p; p }
                None    => return err_reply(-98), // EADDRINUSE: range exhausted
            }
        };

        let reply = {
        let mut stack = stack_for(sid);
        if let Some(ref mut s) = *stack {
            if sock_type == SOCK_STREAM as u8 {
                let mut socket = new_tcp_socket(&s.socket_set);

                // A caller-supplied address must never panic the kernel: an
                // unroutable or malformed endpoint is EINVAL, not unwrap().
                if socket.connect(s.interface.context(), remote_endpoint, local_port).is_err() {
                    return err_reply(-22);
                }
                // Options set before connect(); after it, because connect()
                // resets the socket's timers. The keepalive timeout waits for
                // the handshake (tcp_connect_status).
                apply_tcp_opts(&mut socket, sock_opts(tbl.socks[slot].ofd), false);
                let handle = s.socket_set.add(socket);
                tbl.socks[slot].state = SockState::InetConnected {
                    socket_handle: handle, remote_endpoint: Some(remote_endpoint), sid };
                tbl.socks[slot].connecting = true;
                ok_reply()
            } else {
                // UDP connect: smoltcp hands this socket only its peer's
                // datagrams from now on (`udp::Socket::set_peer`).
                let mut socket = new_udp_socket();
                if socket.bind(local_port).is_err() { return err_reply(-22); }
                socket.set_peer(Some(remote_endpoint));
                socket.set_reuseport(sock_opts(tbl.socks[slot].ofd).reuseport);
        { let so = sock_opts(tbl.socks[slot].ofd); socket.set_recverr(so.recv & RECV_ERR != 0 || so.recv6 & RECV6_ERR != 0); }
                let handle = s.socket_set.add(socket);
                tbl.socks[slot].state = SockState::InetConnected {
                    socket_handle: handle, remote_endpoint: Some(remote_endpoint), sid };
                ok_reply()
            }
        } else {
            err_reply(-100)
        }
        };
        share_state(&mut *tbls, pid, slot);
        // The SYN (or nothing, for UDP) leaves on the next daemon pass: kick
        // it now. No broadcast: the pollers of the old state are woken by
        // their keys (`with_state_wake`), a listener on the other end by its
        // own (`listen::maintain`, in the pass that queues the connection),
        // and this socket's by `readiness_moved` when the handshake moves it.
        drop(tbls);
        drivers::netdev::kick();
        reply
    } else {
        // AF_UNIX Connect
        if addrlen < 3 || addrlen > 2 + PATH_MAX { return err_reply(-22); }
        let path_len = addrlen - 2;
        let path_ptr = (addr_ptr + 2) as *const u8;
        let mut pbytes = [0u8; PATH_MAX];
        if !ucopy_in(&mut pbytes[..path_len], path_ptr as usize) { return err_reply(-14); }
        let is_abstract = pbytes[0] == 0;

        // Resolve the address to the sock_id of a live listener. Abstract
        // names byte-match here; pathnames resolve through the VFS (symlinks,
        // the /dev/shm and /run/user tmpfs mounts, S_IFSOCK check).
        let sock_id = if is_abstract {
            let nsid = sock_netns(pid, slot);
            let bound = BOUND_PATHS.lock();
            let mut found = None;
            for bp in bound.iter() {
                if bp.in_use && bp.is_abstract && bp.netns == nsid && bp.path_len == path_len
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
            let r = vfs::unix_resolve_node(pid, host.unwrap_or(name)); // sock_id (>=0) or -errno
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
        let my_pc = sock_entry(pid, fd).map_or(false, |e| sock_opts(e.ofd).passcred);
        let (conn_idx, orphans) = {
            let mut conns = UNIX_CONNS.lock();
            let idx = match unix_conn_slot(&mut conns) {
                Some(i) => i, None => { drop(conns); report_conns_full(); return err_reply(-12) }
            };
            let orphans = conns[idx].take_fds();
            conns[idx] = UnixConn::new();
            conns[idx].in_use = true;
            conns[idx].cred_a = cred;
            conns[idx].pc_a = my_pc;
            (idx, orphans)
        };
        for x in orphans { xfer_drop(x); }

        let mut tbls = SOCK_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
        tbl.socks[slot].state = SockState::UnixPendingAccept { conn_idx, sock_id };
        // On the listener's backlog only now, with SOCK_TABLES held: an
        // accept() (which scans under SOCK_TABLES too) that took it before
        // the entry above existed would leave the entry pending for ever.
        {
            let mut conns = UNIX_CONNS.lock();
            conns[conn_idx].pending_sid = sock_id;
            conns[conn_idx].pending_ord = PENDING_ORD.fetch_add(1, Ordering::Relaxed);
        }
        drop(tbls);
        dbg("[NET] connect pid="); dbg_u(pid as u64);
        dbg(" fd="); dbg_u(fd as u64);
        dbg(" conn="); dbg_u(conn_idx as u64);
        dbg(" sid="); dbg_u(sock_id);
        dbg(if is_abstract { " abstract\r\n" } else { " path\r\n" });
        // A listener parked in accept/poll now has a pending connect on its
        // address (K2 real blocking).
        sched::wake_poll_tagged(sched::poll_tag(sched::poll_class::UNIX, sock_id as u32));
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
        let idx = match unix_conn_slot(&mut conns) {
            Some(i) => i, None => { drop(conns); report_conns_full(); return err_reply(-12) }
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
        Some(t) => t, None => return err_reply(-23), // ENFILE: table pool full
    };
    const SOCK_CLOEXEC: usize = 0x80000;
    const SOCK_NONBLOCK: usize = 0x800;
    let cloexec  = sock_type & SOCK_CLOEXEC != 0;
    let nonblock = sock_type & SOCK_NONBLOCK != 0;
    let slot_a = match tbl.alloc() { Some(s) => s, None => return err_reply(-24) };
    tbl.socks[slot_a] = SockEntry {
        state: SockState::UnixConnected { conn_idx, is_a: true },
        in_use: true, bound_port: 0, domain: AF_UNIX as u8, sock_type: sock_type as u8,
        cloexec, nonblock, reuseaddr: false, ofd: 0, hidden: false, connecting: false, ns: sched::ns::NsRef::get(sched::ns::netns_of(pid)),
    };
    let slot_b = match tbl.alloc() { Some(s) => s, None => {
        tbl.socks[slot_a] = SockEntry::empty(); return err_reply(-24);
    }};
    tbl.socks[slot_b] = SockEntry {
        state: SockState::UnixConnected { conn_idx, is_a: false },
        in_use: true, bound_port: 0, domain: AF_UNIX as u8, sock_type: sock_type as u8,
        cloexec, nonblock, reuseaddr: false, ofd: vfs::ofd::alloc(), hidden: false, connecting: false, ns: sched::ns::NsRef::get(sched::ns::netns_of(pid)),
    };
    tbl.socks[slot_a].ofd = vfs::ofd::alloc();
    drop(tbls);
    // sv[] is written with no lock held (a fault under SOCK_TABLES would
    // stall every socket call behind it), and fault-tolerantly.
    let sv = [(slot_a + SOCK_FD_BASE) as u32, (slot_b + SOCK_FD_BASE) as u32];
    if !wr_user(sv_ptr, sv) {
        let _ = handle_close(pid, slot_a + SOCK_FD_BASE);
        let _ = handle_close(pid, slot_b + SOCK_FD_BASE);
        return err_reply(-14);
    }
    ok_reply()
}

fn handle_send(pid: u32, fd: usize, buf_ptr: usize, len: usize, addr_ptr: usize, addrlen: usize) -> Message {
    if let Some(o) = nlsock::of(pid, fd) { return nlsock::send_buf(o, buf_ptr, len); }
    // A UDP socket with UDP_SEGMENT set splits a long send (see `udp_send_gso`).
    if GSO_USED.load(Ordering::Relaxed) {
        let seg = fd_ofd(pid, fd).map_or(0, |o| sock_opts(o).gso) as usize;
        if seg != 0 && len > seg && inet_dgram(pid, fd) {
            if len > UDP_MAX_PAYLOAD { return err_reply(-90); } // EMSGSIZE
            let mut data = alloc::vec![0u8; len];
            if !ucopy_in(&mut data, buf_ptr) { return err_reply(-14); }
            return udp_send_gso(pid, fd, addr_ptr, addrlen, data, seg);
        }
    }
    handle_send_k(pid, fd, buf_ptr, len, addr_ptr, addrlen, None)
}

/// Some socket set UDP_SEGMENT once: only then does a send look it up.
static GSO_USED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Linux UDP_MAX_SEGMENTS: a GSO send of more segments is EINVAL.
const UDP_MAX_SEGMENTS: usize = 64;

/// UDP generic segmentation offload (UDP_SEGMENT, as a sendmsg cmsg or a
/// socket option): `data` leaves as consecutive datagrams of `seg` bytes, the
/// last one shorter, all to the same destination — what Linux does in
/// software when the device cannot. QUIC stacks built on quinn-udp (neqo,
/// i.e. Firefox's HTTP/3 once it stops using NSPR) probe for it with
/// setsockopt(UDP_SEGMENT), which this server always accepted, and then hand
/// one buffer of several packets to sendmsg: sent as one datagram, that was
/// a single oversized packet the peer could not parse. Answers the bytes
/// sent; a failure after the first segment ends the send short.
fn udp_send_gso(pid: u32, fd: usize, addr_ptr: usize, addrlen: usize, data: alloc::vec::Vec<u8>, seg: usize) -> Message {
    if seg == 0 || data.len() <= seg { return handle_send_k(pid, fd, 0, 0, addr_ptr, addrlen, Some(data)); }
    if data.len() > seg * UDP_MAX_SEGMENTS { return err_reply(-22); }
    let mut sent = 0usize;
    for chunk in data.chunks(seg) {
        let r = net_val(&handle_send_k(pid, fd, 0, 0, addr_ptr, addrlen, Some(chunk.to_vec())));
        if r < 0 { return if sent > 0 { val_reply(sent as u64) } else { make_reply(r as i64) }; }
        sent += chunk.len();
    }
    val_reply(sent as u64)
}

/// The UDP_SEGMENT size a sendmsg control buffer carries (cmsg SOL_UDP /
/// UDP_SEGMENT, a u16), if any. Other control messages (IP_TOS, IP_PKTINFO)
/// are ignored, as before.
fn cmsg_udp_segment(ctrl_ptr: usize, ctrl_len: usize) -> Option<usize> {
    const SOL_UDP: i32 = 17;
    const UDP_SEGMENT: i32 = 103;
    let mut pos = 0usize;
    while pos + CMSG_HDR_LEN <= ctrl_len {
        let clen  = rd_usize(ctrl_ptr + pos).ok()?;
        let level = sched::uaccess::read_user::<i32>(ctrl_ptr + pos + 8)?;
        let ty    = sched::uaccess::read_user::<i32>(ctrl_ptr + pos + 12)?;
        if clen < CMSG_HDR_LEN || pos + clen > ctrl_len { return None; }
        if level == SOL_UDP && ty == UDP_SEGMENT && clen >= CMSG_HDR_LEN + 2 {
            return sched::uaccess::read_user::<u16>(ctrl_ptr + pos + CMSG_HDR_LEN).map(|v| v as usize);
        }
        let step = cmsg_align(clen);
        if step == 0 { break; }
        pos += step;
    }
    None
}

/// `handle_send`, with the payload optionally already in kernel memory
/// (`kdata`, then `buf_ptr`/`len` are ignored). Only the AF_INET datagram path
/// takes it: a multi-iovec `sendmsg` on a UDP socket must leave as ONE
/// datagram, so `handle_sendmsg` gathers the iovecs first.
fn handle_send_k(pid: u32, fd: usize, buf_ptr: usize, len: usize, addr_ptr: usize, addrlen: usize,
                 kdata: Option<alloc::vec::Vec<u8>>) -> Message {
    let len = match kdata { Some(ref d) => d.len(), None => len };
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    // Shared: a send only reads the table (lane ipcfast: the write lock here
    // serialized every send in the system and stalled all readers).
    let tbls = SOCK_TABLES.read();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
    // Gate on the SOCKET's own domain and captured netns, never on the user
    // sa_family (which a sibling thread can flip, or which can simply lie).
    // Folded into this read: one table lock per send.
    let state = tbl.socks[slot].state;
    let sock_type = tbl.socks[slot].sock_type;
    let sock_domain = tbl.socks[slot].domain as usize;
    let sock_is_raw = tbl.socks[slot].sock_type & 0xf == SOCK_RAW as u8;
    let ofd = tbl.socks[slot].ofd;

    match state {
        SockState::UnixConnected { conn_idx, is_a } => {
            drop(tbls);
            // A gathered sendmsg datagram arrives as `kdata` (kernel memory;
            // the ring copy takes either).
            let buf_ptr = kdata.as_ref().map_or(buf_ptr, |d| d.as_ptr() as usize);
            // Fault the source in before UNIX_CONNS is taken (a file-backed
            // page is read with no lock held); the ring copy under the lock
            // is fault-tolerant for whatever this could not populate.
            sched::uaccess::prefault(buf_ptr, len.min(RING_MAX), true);
            let sndbuf = unix_sndbuf(ofd);
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
            // `queued`: something reached the peer (a zero-length datagram
            // is a record too, and must wake its reader).
            let (r, queued) = if sock_type == SOCK_STREAM as u8 {
                let r = unix_stream_write(&mut conns, conn_idx, is_a, buf_ptr, len);
                (r, matches!(r, Ok(n) if n > 0))
            } else {
                let ring = if is_a { &mut conn.ring_ab } else { &mut conn.ring_ba };
                match ring.write_dgram(buf_ptr as *const u8, len, sndbuf) {
                    Ok(Some(n)) => (Ok(n), true),
                    // No room: EAGAIN (a zero-length one too).
                    Ok(None) => { drop(conns); return err_reply(-11); }
                    Err(e) => (Err(e), false),
                }
            };
            let n = match r { Ok(n) => n, Err(e) => return err_reply(e) }; // EFAULT, EMSGSIZE
            let conn = &mut conns[conn_idx];
            // New readable edge for the peer end.
            if queued { conn.seq = conn.seq.wrapping_add(1); }
            drop(conns); // release UNIX_CONNS before waking pollers (K2 lock order)
            if queued { sched::wake_poll_key(unix_rd_key(conn_idx, !is_a)); }
            if queued && sock_type != SOCK_STREAM as u8 { return val_reply(n as u64); }
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
            sched::uaccess::prefault(buf_ptr, len.min(RING_MAX), true);
            let mut conns = UNIX_CONNS.lock();
            let conn = &mut conns[conn_idx];
            if !conn.in_use { return err_reply(-32); } // EPIPE — conn torn down
            if conn.closed_b { return err_reply(-32); } // peer (post-accept) gone
            // Pre-accept the connector is end A, so its own half-close counts
            // here too (accept preserves conn_idx, so the flag carries over).
            if conn.wr_shut(true) { return err_reply(-32); } // EPIPE
            // A SOCK_SEQPACKET connector (conmon's attach socket: podman writes
            // right after connect, before conmon accepts) queues a framed
            // record, as the connected branch does: raw stream bytes here made
            // the accepted end read "ping" as a record length and fail EFAULT.
            if sock_type != SOCK_STREAM as u8 {
                let sndbuf = unix_sndbuf(ofd);
                let r = conns[conn_idx].ring_ab.write_dgram(buf_ptr as *const u8, len, sndbuf);
                return match r {
                    Ok(Some(n)) => {
                        let conn = &mut conns[conn_idx];
                        conn.seq = conn.seq.wrapping_add(1);
                        drop(conns);
                        sched::wake_poll_key(unix_rd_key(conn_idx, false));
                        val_reply(n as u64)
                    }
                    Ok(None) => { drop(conns); err_reply(-11) }
                    Err(e) => err_reply(e),
                };
            }
            let n = match unix_stream_write(&mut conns, conn_idx, true, buf_ptr, len) {
                Ok(n) => n,
                Err(e) => return err_reply(e), // EFAULT, EPIPE
            };
            let conn = &mut conns[conn_idx];
            if n > 0 { conn.seq = conn.seq.wrapping_add(1); }
            drop(conns);
            if n > 0 { sched::wake_poll_tagged(sched::poll_tag(sched::poll_class::UNIX, conn_idx as u32)); }
            // Full pre-accept buffer on a non-empty send → EAGAIN, not a bogus 0
            // (same livelock as the UnixConnected branch above; peer_closed/
            // closed_b already handled above).
            if n == 0 && len > 0 { return err_reply(-11); }
            val_reply(n as u64)
        }
        SockState::InetConnected { socket_handle, remote_endpoint, sid } => {
            let unported = sock_type == SOCK_DGRAM as u8 && tbl.socks[slot].bound_port == 0;
            drop(tbls);
            // shutdown(SHUT_WR) of a UDP socket: EPIPE, as on Linux.
            if sock_type == SOCK_DGRAM as u8 && sock_opts(ofd).shut & SHUT_WR_BIT != 0 { return err_reply(-32); }
            // A port connect(AF_UNSPEC) gave up: bind a new one, then send.
            if unported {
                if let Err(e) = udp_rebind_port(pid, slot) { return err_reply(e); }
                return handle_send_k(pid, fd, buf_ptr, len, addr_ptr, addrlen, kdata);
            }
            // Touch user memory ONLY with every lock released. `buf_ptr`/`addr_ptr`
            // are demand-paged, so reading them can take a page fault, and a fault
            // taken under the stack spinlock re-enters the scheduler while holding
            // it — the hazard shape that once froze all four vCPUs (fixed 82d0cc3).
            // read_user_buf is deliberately NOT used here: it does not fault a page
            // in, and sys_sendto only validates the range, so a first-touch .rodata
            // send buffer would spuriously EFAULT. Same idiom as the ICMP arm below.
            let data = match kdata {
                Some(d) => d,
                None => {
                    let mut data = alloc::vec![0u8; len];
                    if len > 0 && !ucopy_in(&mut data, buf_ptr) { return err_reply(-14); }
                    data
                }
            };
            // Destination sockaddr, likewise read before locking. Datagram-only:
            // the stream path never dereferenced `addr_ptr` and sys_sendto does not
            // validate it, so it must stay untouched there. Kept as an Option so the
            // EDESTADDRREQ/ENETDOWN precedence stays exactly as it was.
            let dest = if sock_type == SOCK_STREAM as u8 {
                None
            } else if addr_ptr != 0 && addrlen >= 8 {
                let sa = match sa::parse(addr_ptr, addrlen) { Ok(a) => a, Err(e) => return err_reply(e) };
                let ep = IpEndpoint::new(sa.ip, sa.port);
                Some(if sock_type == SOCK_DGRAM as u8 { nft_output_dnat(pid, fd, slot, sa.ip, ep).1 } else { ep })
            } else {
                remote_endpoint
            };

            let opts = if sock_type == SOCK_DGRAM as u8 { sock_opts(ofd) } else { SockOpts::DEFAULT };
            if sock_type == SOCK_DGRAM as u8 {
                if let Some(ep) = dest {
                    // A broadcast destination needs SO_BROADCAST (Linux:
                    // EACCES from the route lookup).
                    if iface::is_broadcast(ep.addr) && !opts.broadcast { return err_reply(-13); }
                }
            }
            // IP_MULTICAST_IF's interface for multicast and limited broadcast.
            let mc_sid = if sock_type == SOCK_DGRAM as u8 { dest.and_then(|ep| mcast_if_sid(ofd, ep.addr)) } else { None };
            // A wildcard UDP server answers from the stack the destination
            // routes through (its source address is that interface's).
            let mirrors = if sock_type == SOCK_DGRAM as u8 { udpany::of(ofd) } else { None };
            let (sid, socket_handle) = match (&mirrors, dest, mc_sid) {
                (Some(hs), Some(_), Some(m)) if hs[m as usize].is_some() => (m, hs[m as usize].unwrap()),
                (Some(hs), Some(ep), _) => udpany::send_via(sock_netns(pid, slot), hs, (sid, socket_handle), ep.addr),
                _ => (sid, socket_handle),
            };
            // A UDP socket whose interface can no longer reach `dest` (it lost
            // its address or carrier) moves to the stack the route lookup
            // picks now, keeping its port — unless it is bound to an address.
            // So does one sending multicast through another IP_MULTICAST_IF.
            if sock_type == SOCK_DGRAM as u8 && mirrors.is_none() {
                if let Some(ep) = dest {
                    let to = match mc_sid {
                        Some(m) => Some(m).filter(|&m| m != sid),
                        None if sid != LO && !iface::can_route(sid, ep.addr) =>
                            iface::route_select(sock_netns(pid, slot), ep.addr).filter(|&t| t != sid),
                        None => None,
                    };
                    if let Some(to) = to {
                        if udp_migrate(pid, fd, socket_handle, sid, to) {
                            return handle_send_k(pid, fd, buf_ptr, len, addr_ptr, addrlen, Some(data));
                        }
                    }
                }
            }
            let mut stack = stack_for(sid);
            if let Some(ref mut s) = *stack {
                if sock_type == SOCK_STREAM as u8 {
                    let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    if !socket.can_send() {
                        // Still connecting, or the send queue is full: wait.
                        // Gone (refused, reset, or our side shut down): an
                        // error, never EAGAIN — a blocking writer used to wait
                        // forever on a connection that no longer existed.
                        if socket.may_send()
                            || matches!(socket.state(), tcp::State::SynSent | tcp::State::SynReceived)
                        {
                            return err_reply(-11);
                        }
                        drop(stack);
                        let e = tcp_connect_status(pid, fd);
                        return err_reply(if e < 0 && e != -11 { e as i32 } else { -32 }); // EPIPE
                    }
                    // The segment leaves on the daemon's next pass: kick it
                    // now (with the stack lock released) instead of waiting.
                    let r = socket.send_slice(&data);
                    drop(stack);
                    match r {
                        Ok(n) => { if n > 0 { drivers::netdev::kick(); } val_reply(n as u64) }
                        Err(_) => err_reply(-104),
                    }
                } else {
                    let endpoint = match dest {
                        Some(ep) => ep,
                        None => return err_reply(-89),
                    };
                    // A pending ICMP error (connected socket) is this send's
                    // answer, once (Linux sock_alloc_send_pskb -> sock_error).
                    let (err, bound, cap) = {
                        let u = s.socket_set.get_mut::<udp::Socket>(socket_handle);
                        (u.take_error(), u.endpoint(), u.payload_send_capacity())
                    };
                    if err != 0 { return err_reply(-(err as i32)); }
                    // A datagram the send buffer can never hold: EMSGSIZE, not
                    // an EAGAIN that never clears (a blocking sender hung).
                    if data.len() > cap || data.len() > UDP_MAX_PAYLOAD { return err_reply(-90); }
                    let mcast = endpoint.addr.is_multicast();
                    let mut meta = smoltcp::socket::udp::UdpMetadata::from(endpoint);
                    meta.hop_limit = if mcast { Some(opts.mc_ttl) }
                                     else if opts.ttl != 0 { Some(opts.ttl) } else { None };
                    meta.meta.tos = opts.tos;
                    // Path-MTU discovery (IP_MTU_DISCOVER): DONT sends without
                    // DF; DO / PROBE refuse a datagram larger than the path.
                    let pd = opts.pmtud_mode();
                    if pd <= 1 { meta.meta.nodf = true; } // DONT and WANT: DF clear (ip_sk_ignore_df)
                    if let (IpAddress::Ipv4(d), true) = (endpoint.addr, pd >= 2) {
                        if data.len() + 28 > path_mtu(sid, d) as usize {
                            if opts.recv & RECV_ERR != 0 { udp_local_emsgsize(s, socket_handle, endpoint, path_mtu(sid, d)); }
                            return err_reply(-90); // EMSGSIZE
                        }
                    }
                    // IPv6 never fragments in the path and this stack does not
                    // fragment at the source: DO / PROBE refuse a datagram
                    // larger than the path (RFC 8201).
                    if let (IpAddress::Ipv6(_), true) = (endpoint.addr, pd >= 2) {
                        if data.len() + 48 > path_mtu_ip(sid, endpoint.addr) as usize {
                            if opts.recv6 & RECV6_ERR != 0 { udp_local_emsgsize(s, socket_handle, endpoint, path_mtu_ip(sid, endpoint.addr)); }
                            return err_reply(-90);
                        }
                    }
                    // A copy for this host: multicast unless IP_MULTICAST_LOOP
                    // is off, broadcast always (Linux ip_mc_output). Built
                    // before the send, while the socket's port and the
                    // interface's addresses are at hand.
                    let looped = if sid != LO && ((mcast && !opts.mc_noloop) || iface::is_broadcast(endpoint.addr)) {
                        let src = bound.addr.or_else(|| stack_src_addr(s, sid));
                        let ttl = if mcast { opts.mc_ttl } else if opts.ttl != 0 { opts.ttl } else { 64 };
                        src.and_then(|src| loop_frame(s, src, bound.port, endpoint, ttl, opts.tos, &data))
                    } else { None };
                    // TTL 0 multicast never leaves the host.
                    let r = if mcast && opts.mc_ttl == 0 { Ok(()) } else { s.socket_set.get_mut::<udp::Socket>(socket_handle).send_slice(&data, meta) };
                    drop(stack);
                    if let (Ok(()), Some(f)) = (&r, looped) { inject_frame(sid, f); }
                    match r {
                        Ok(()) => { drivers::netdev::kick(); val_reply(len as u64) }
                        // A full send queue is EAGAIN (a blocking sender
                        // waits for the daemon to drain it; poll reports it
                        // not writable until then), as a full socket buffer
                        // is on Linux. It was ENOMEM, which QUIC stacks and
                        // tokio treat as fatal.
                        Err(udp::SendError::BufferFull) => { drivers::netdev::kick(); err_reply(-11) }
                        // Port 0 or no address to send to: EINVAL.
                        Err(udp::SendError::Unaddressable) => err_reply(-22),
                    }
                }
            } else {
                err_reply(-100)
            }
        }
        // An AF_INET datagram socket that was never connected: its first
        // sendto() creates the smoltcp socket (on the stack the destination
        // lives on) and binds it, then sends. Without this every unconnected
        // UDP socket answered sendto() with EPIPE — musl's DNS resolver among
        // them, so no name ever resolved.
        SockState::Unbound { .. } | SockState::InetBound { .. }
            if is_inet(tbl.socks[slot].domain) && sock_type == SOCK_DGRAM as u8 =>
        {
            drop(tbls);
            if addr_ptr == 0 || addrlen < 8 { return err_reply(-89); } // EDESTADDRREQ
            let (dest, dscope) = match sa::parse(addr_ptr, addrlen) {
                Ok(a) => (nft_output_dnat(pid, fd, slot, a.ip, IpEndpoint::new(a.ip, a.port)).0, a.scope),
                Err(e) => return err_reply(e) };
            if let Err(e) = udp_materialize(pid, slot, Some(dest), dscope, false) { return err_reply(e); }
            handle_send_k(pid, fd, buf_ptr, len, addr_ptr, addrlen, kdata)
        }
        SockState::IcmpUnbound => {
            if addr_ptr == 0 || addrlen < 8 { return err_reply(-89); }
            if len < 8 { return err_reply(-22); }
            let dest_ip = match icmp_dest(sock_domain, addr_ptr, addrlen) { Ok(a) => a, Err(e) => return err_reply(e) };
            // Bind smoltcp's filter to whatever ident the caller already put in its
            // own ICMP header (bytes 4..6), rather than picking one ourselves and
            // having no way to tell the caller — see plan for why.
            // sendmsg hands the gathered datagram in as `kdata`.
            let data = match kdata {
                Some(d) => d,
                None => {
                    let mut d = alloc::vec![0u8; len];
                    if !ucopy_in(&mut d, buf_ptr) { return err_reply(-14); }
                    d
                }
            };
            let ident = u16::from_be_bytes([data[4], data[5]]);
            drop(tbls);

            // The ping socket lives on the stack its first destination routes
            // through, and stays there.
            let dscope = if sock_domain == AF_INET6 { sa::parse(addr_ptr, addrlen).map_or(0, |a| a.scope) } else { 0 };
            if ip6::needs_scope(dest_ip) {
                if let Err(e) = ip6::scope_sid(sock_netns(pid, slot), dscope) { return err_reply(e); }
            }
            let scoped = if ip6::needs_scope(dest_ip) { ip6::scope_sid(sock_netns(pid, slot), dscope).ok() } else { None };
            let sid = match scoped.or_else(|| iface::route_select(sock_netns(pid, slot), dest_ip)) { Some(s) => s, None => return err_reply(if sock_netns(pid, slot) != 0 { -101 } else { no_route_err() }) };
            let mut stack = stack_for(sid);
            if stack.is_none() { return err_reply(-100); }
            let s = stack.as_mut().unwrap();

            let rx_buffer = icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; 4], alloc::vec![0; 2048]);
            let tx_buffer = icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; 4], alloc::vec![0; 2048]);
            let mut socket = icmp::Socket::new(rx_buffer, tx_buffer);
            let ttl = sock_opts(ofd).ttl;
            if ttl != 0 { socket.set_hop_limit(Some(ttl)); }
            let raw6 = sock_domain == AF_INET6 && sock_is_raw;
            if socket.bind(if raw6 { icmp::Endpoint::Raw } else { icmp::Endpoint::Ident(ident) }).is_err() { return err_reply(-22); }
            let socket_handle = s.socket_set.add(socket);
            let result = s.socket_set.get_mut::<icmp::Socket>(socket_handle).send_slice(&data, dest_ip);
            drop(stack);
            drivers::netdev::kick();

            // Install the socket unless another send on this socket — a
            // sibling thread or another holder of it — got there first, or it
            // was closed meanwhile; then this one goes (its datagram is out).
            let mut tbls2 = SOCK_TABLES.lock();
            let installed = match find_tbl(pid, &mut *tbls2) {
                Some(tbl2) if slot < tbl2.socks.len() && tbl2.socks[slot].in_use
                    && matches!(tbl2.socks[slot].state, SockState::IcmpUnbound) =>
                {
                    tbl2.socks[slot].state = SockState::IcmpBound { socket_handle, sid };
                    true
                }
                _ => false,
            };
            let mut ofd = 0;
            if installed {
                share_state(&mut *tbls2, pid, slot);
                ofd = find_tbl(pid, &mut *tbls2).map_or(0, |t| t.socks[slot].ofd);
            }
            drop(tbls2);
            // Pollers of the unbound ping socket move to its new key.
            if ofd != 0 { sock_state_moved(&[sock_state_key(ofd)]); }
            if !installed {
                let mut stack = stack_for(sid);
                if let Some(ref mut s) = *stack { release_inet_socket(s, socket_handle); }
            }

            match result {
                Ok(()) => val_reply(len as u64),
                Err(icmp::SendError::BufferFull)    => err_reply(-11),
                Err(icmp::SendError::Unaddressable) => err_reply(-89),
            }
        }
        SockState::IcmpBound { socket_handle, sid } => {
            if addr_ptr == 0 || addrlen < 8 { return err_reply(-89); }
            let dest_ip = match icmp_dest(sock_domain, addr_ptr, addrlen) { Ok(a) => a, Err(e) => return err_reply(e) };
            let data = match kdata {
                Some(d) => d,
                None => {
                    let mut d = alloc::vec![0u8; len];
                    if !ucopy_in(&mut d, buf_ptr) { return err_reply(-14); }
                    d
                }
            };
            drop(tbls);

            // A raw ICMPv6 socket was bound on the default route's stack at socket(); a destination on another
            // stack (::1, a scoped link-local) moves it there.
            let (mut sid, mut socket_handle) = (sid, socket_handle);
            if sock_domain == AF_INET6 && sock_is_raw {
                if let Some(sid2) = iface::route_select(sock_netns(pid, slot), dest_ip) {
                    if sid2 != sid {
                        let mut st2 = stack_for(sid2);
                        let mut made = None;
                        if let Some(s2) = st2.as_mut() {
                            let rx_buffer = icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; 16], alloc::vec![0; 16384]);
                            let tx_buffer = icmp::PacketBuffer::new(alloc::vec![icmp::PacketMetadata::EMPTY; 4], alloc::vec![0; 2048]);
                            let mut ns2 = icmp::Socket::new(rx_buffer, tx_buffer);
                            if ns2.bind(icmp::Endpoint::Raw).is_ok() { made = Some(s2.socket_set.add(ns2)); }
                        }
                        drop(st2);
                        if let Some(h2) = made {
                            let mut t3 = SOCK_TABLES.lock();
                            let mut ok = false;
                            if let Some(tb) = find_tbl(pid, &mut *t3) {
                                if slot < tb.socks.len() && tb.socks[slot].in_use && matches!(tb.socks[slot].state, SockState::IcmpBound { .. }) {
                                    tb.socks[slot].state = SockState::IcmpBound { socket_handle: h2, sid: sid2 };
                                    share_state(&mut *t3, pid, slot);
                                    ok = true;
                                }
                            }
                            drop(t3);
                            if ok {
                                sock_state_moved(&[sock_state_key(ofd)]);
                                let mut old = stack_for(sid);
                                if let Some(ref mut so) = *old { release_inet_socket(so, socket_handle); }
                                drop(old);
                                sid = sid2; socket_handle = h2;
                            } else {
                                let mut st4 = stack_for(sid2);
                                if let Some(ref mut s4) = *st4 { release_inet_socket(s4, h2); }
                            }
                        }
                    }
                }
            }

            let mut stack = stack_for(sid);
            if stack.is_none() { return err_reply(-100); }
            let s = stack.as_mut().unwrap();
            let socket = s.socket_set.get_mut::<icmp::Socket>(socket_handle);
            let ttl = sock_opts(ofd).ttl;
            socket.set_hop_limit(if ttl != 0 { Some(ttl) } else { None });
            let r = socket.send_slice(&data, dest_ip);
            drop(stack);
            drivers::netdev::kick();
            match r {
                Ok(()) => val_reply(len as u64),
                Err(icmp::SendError::BufferFull)    => err_reply(-11),
                Err(icmp::SendError::Unaddressable) => err_reply(-89),
            }
        }
        _ => err_reply(-32),
    }
}

const MSG_PEEK: usize = 0x02;
const MSG_ERRQUEUE: usize = 0x2000;

/// What one AF_UNIX stream read took from an end's inbound direction.
struct StreamRead {
    nread: usize,
    /// A fault stopped the copy. When `nread` is 0, nothing was consumed.
    fault: bool,
    /// The SCM_RIGHTS batch that rides with the bytes read, if the caller
    /// asked for fds. It is the dequeued batch, or a clone under MSG_PEEK.
    fds: Option<alloc::vec::Vec<XferFd>>,
    /// SCM_CREDENTIALS of the batch taken (host ids).
    cred: Option<Ucred>,
    /// Fds the read consumed and nobody will receive: a batch read by a call
    /// with no control buffer. Release them with `xfer_drop` once
    /// UNIX_CONNS is released.
    discard: alloc::vec::Vec<XferFd>,
}

/// Read end `is_a`'s inbound stream of connection `conn_idx` into `iovs`.
/// Shared by recv/read and recvmsg so that queued fds follow the same rule
/// everywhere. The rule is Linux's (unix_stream_read_generic, scm_recv):
///   - The fds of a batch go with the first of its bytes that is read. Read
///     past that and you have them, so a later recvmsg never sees them.
///   - A read that takes a batch stops at the batch's last byte (`end_byte`).
///     Bytes sent after it, with or without fds, are left for the next call.
///   - With `want_fds` false (read, recv, recvfrom), a consumed batch is
///     closed: it is returned in `discard`. Leaving it queued attached it to
///     whichever later recvmsg first ran, with the wrong bytes, and held the
///     fds open until then (forever if no recvmsg came).
///   - `peek` (MSG_PEEK) copies the bytes and consumes nothing. With
///     `want_fds` the fds of the batch come back as new references
///     (unix_peek_fds) and stay queued. Without `want_fds`, nothing is
///     touched.
/// The caller holds UNIX_CONNS and has checked that the connection is in use.
/// A non-peek read copies out with UNIX_CONNS released (lane ipcfast:
/// reserve_read / commit_read); `conns` is re-taken before returning.
fn unix_stream_read_locked(conns: &mut sched::lockwatch::TrackedGuard<'static, alloc::vec::Vec<UnixConn>>,
                           conn_idx: usize, is_a: bool,
                           iovs: &[(usize, usize)], peek: bool, want_fds: bool) -> StreamRead {
    let mut out = StreamRead { nread: 0, fault: false, fds: None, cred: None, discard: alloc::vec::Vec::new() };
    // Another reader of this direction is mid-copy: wait for its commit (it
    // moves rtotal and may consume the fd batch the limit below depends on).
    loop {
        let c = &conns[conn_idx];
        let busy = if is_a { c.ring_ba.rbusy } else { c.ring_ab.rbusy };
        if !busy { break; }
        conns.unlocked(|| sched::yield_now("unix-rd"));
        if !conns[conn_idx].in_use { return out; }
    }
    let c = &mut conns[conn_idx];
    let rstart = if is_a { c.ring_ba.rtotal } else { c.ring_ab.rtotal };
    // A batch whose first byte is already consumed has no reader. The rule
    // above means it cannot exist, so this is a guard only: a batch like that
    // must not cap the read at zero bytes for ever.
    {
        let q = c.rx_fdq(is_a);
        while q.first().map_or(false, |b| b.seq_byte < rstart) {
            out.discard.extend(q.remove(0).fds);
        }
    }
    let limit = match c.rx_fdq(is_a).first() {
        Some(b) => (b.end_byte - rstart) as usize,
        None => usize::MAX,
    };
    let ring = if is_a { &mut c.ring_ba } else { &mut c.ring_ab };
    if peek {
        for &(base, len) in iovs.iter() {
            if out.nread >= limit { break; }
            let want = len.min(limit - out.nread);
            if want == 0 { continue; }
            match ring.peek(out.nread, base as *mut u8, want) {
                Ok(n) => { out.nread += n; if n < want { break; } }
                Err(_) => { out.fault = true; break; }
            }
        }
    } else if iovs.iter().fold(0usize, |a, &(_, l)| a.saturating_add(l)).min(limit) < UNIX_UNLOCKED_COPY_MIN {
        // Small read: the copy is cheaper than releasing and re-taking the lock.
        for &(base, len) in iovs.iter() {
            if out.nread >= limit { break; }
            let want = len.min(limit - out.nread);
            if want == 0 { continue; }
            match ring.read(base as *mut u8, want) {
                Ok(n) => { out.nread += n; if n < want { break; } }
                Err(_) => { out.fault = true; break; }
            }
        }
    } else {
        let want = iovs.iter().fold(0usize, |a, &(_, l)| a.saturating_add(l)).min(limit);
        // rbusy was clear above and the lock is still held: this succeeds.
        let sp = match ring.reserve_read(want) { Some(sp) => sp, None => return out };
        if sp.n == 0 { return out; }
        let (done, fault) = conns.unlocked(|| sp.drain(iovs));
        let c = &mut conns[conn_idx];
        let ring = if is_a { &mut c.ring_ba } else { &mut c.ring_ab };
        ring.commit_read(&sp, done);
        out.nread = done;
        out.fault = fault;
    }
    if out.nread == 0 { return out; }
    let c = &mut conns[conn_idx];
    let touched = c.rx_fdq(is_a).first().map_or(false, |b| b.seq_byte < rstart + out.nread as u64);
    if !touched { return out; }
    out.cred = c.rx_fdq(is_a)[0].cred;
    if peek {
        if want_fds {
            let batch = c.rx_fdq(is_a)[0].fds.clone();
            let mut clones = alloc::vec::Vec::with_capacity(batch.len());
            for x in batch.iter() {
                if let Some(y) = xfer_clone_locked(&mut conns[..], x) { clones.push(y); }
            }
            out.fds = Some(clones);
        }
    } else {
        let fds = c.rx_fdq(is_a).remove(0).fds;
        if want_fds { out.fds = Some(fds); } else { out.discard.extend(fds); }
    }
    out
}

/// Copies at least this long move between a unix stream ring and the caller
/// with UNIX_CONNS released (reserve / copy / commit); shorter ones are
/// cheaper done under the lock than releasing and re-taking it.
const UNIX_UNLOCKED_COPY_MIN: usize = 1024;

/// Stream write of `len` bytes at `src` (user or kernel memory) into end
/// `is_a`'s outgoing ring. The caller holds UNIX_CONNS and has done the
/// EPIPE checks; a long copy runs with the lock released (lane ipcfast).
/// Returns the bytes queued (0: ring full), EPIPE if the connection went
/// away while waiting for another writer, EFAULT if nothing could be read.
fn unix_stream_write(conns: &mut sched::lockwatch::TrackedGuard<'static, alloc::vec::Vec<UnixConn>>,
                     conn_idx: usize, is_a: bool, src: usize, len: usize) -> Result<usize, i32> {
    loop {
        let c = &conns[conn_idx];
        let busy = if is_a { c.ring_ab.wbusy } else { c.ring_ba.wbusy };
        if !busy { break; }
        conns.unlocked(|| sched::yield_now("unix-wr"));
        let c = &conns[conn_idx];
        if !c.in_use || (if is_a { c.closed_b } else { c.closed_a })
            || c.wr_shut(is_a) || c.rd_shut(!is_a) {
            return Err(-32); // EPIPE
        }
    }
    let c = &mut conns[conn_idx];
    let ring = if is_a { &mut c.ring_ab } else { &mut c.ring_ba };
    if len < UNIX_UNLOCKED_COPY_MIN { return ring.write(src as *const u8, len); }
    let sp = match ring.reserve_write(len) { Some(sp) => sp, None => return Ok(0) };
    if sp.n == 0 { return Ok(0); }
    let (done, fault) = conns.unlocked(|| sp.fill(&[(src, len)]));
    let c = &mut conns[conn_idx];
    let ring = if is_a { &mut c.ring_ab } else { &mut c.ring_ba };
    ring.commit_write(&sp, done);
    if done == 0 && fault { return Err(-14); } // EFAULT
    Ok(done)
}

/// What one AF_UNIX datagram/seqpacket read took (see `unix_dgram_read_locked`).
struct DgramRead {
    /// `(copied, full length)` of the record, `None` when none is queued.
    res: Result<Option<(usize, usize)>, i32>,
    fds: Option<alloc::vec::Vec<XferFd>>,
    cred: Option<Ucred>,
    discard: alloc::vec::Vec<XferFd>,
}

/// Read the next record queued for end `is_a` into `buf` (kernel or user
/// memory, `len` bytes; the rest of a longer record is dropped unless
/// `peek`). The SCM_RIGHTS batch sent with that record (pinned to its first
/// byte by `unix_dgram_send_fds`) comes with it: in `fds` when `want_fds`,
/// else in `discard`; under `peek` as new references, left queued. Linux
/// keeps a datagram's fds with its skb the same way. The caller holds
/// UNIX_CONNS and has checked that the connection is in use.
fn unix_dgram_read_locked(conns: &mut [UnixConn], conn_idx: usize, is_a: bool,
                          buf: usize, len: usize, peek: bool, want_fds: bool) -> DgramRead {
    let mut out = DgramRead { res: Ok(None), fds: None, cred: None, discard: alloc::vec::Vec::new() };
    let c = &mut conns[conn_idx];
    let rstart = if is_a { c.ring_ba.rtotal } else { c.ring_ab.rtotal };
    {
        let q = c.rx_fdq(is_a);
        while q.first().map_or(false, |b| b.seq_byte < rstart) {
            out.discard.extend(q.remove(0).fds);
        }
    }
    let ring = if is_a { &mut c.ring_ba } else { &mut c.ring_ab };
    out.res = if peek { ring.peek_dgram(buf as *mut u8, len) } else { ring.read_dgram(buf as *mut u8, len) };
    let Ok(Some((_, full))) = out.res else { return out };
    let rend = rstart + 4 + full as u64;
    if !c.rx_fdq(is_a).first().map_or(false, |b| b.seq_byte < rend) { return out; }
    out.cred = c.rx_fdq(is_a)[0].cred;
    if peek {
        if want_fds {
            let batch = c.rx_fdq(is_a)[0].fds.clone();
            let mut clones = alloc::vec::Vec::with_capacity(batch.len());
            for x in batch.iter() {
                if let Some(y) = xfer_clone_locked(&mut conns[..], x) { clones.push(y); }
            }
            out.fds = Some(clones);
        }
    } else {
        let fds = c.rx_fdq(is_a).remove(0).fds;
        if want_fds { out.fds = Some(fds); } else { out.discard.extend(fds); }
    }
    out
}

fn handle_recv(pid: u32, fd: usize, buf_ptr: usize, len: usize, addr_ptr: usize, addrlen_ptr: usize,
               flags: usize) -> Message {
    if let Some(o) = nlsock::of(pid, fd) { return nlsock::recv(o, buf_ptr, len, addr_ptr, addrlen_ptr, flags); }
    let peek = flags & MSG_PEEK != 0;
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    let tbls = SOCK_TABLES.read();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }

    let state = tbl.socks[slot].state;
    let sock_type = tbl.socks[slot].sock_type;
    let ofd = tbl.socks[slot].ofd;
    let sock_domain = tbl.socks[slot].domain as usize;
    drop(tbls);

    match state {
        // A UDP socket: the next datagram (from its peer if connected; from
        // any stack for a wildcard server), see `udp_recv_k`.
        // A UDP socket bound in a non-initial netns has no stack socket: nothing
        // can ever arrive, so a read just reports no data (blocking waits on its key).
        SockState::InetBound { .. } if sock_type == SOCK_DGRAM as u8 => err_reply(-11),
        SockState::InetConnected { .. } if sock_type == SOCK_DGRAM as u8 => {
            match udp_recv_k(pid, fd, len, peek, false) {
                Ok((data, full, m)) => {
                    let endpoint = m.from;
                    let n = data.len();
                    if !ucopy_out(buf_ptr, &data[..n]) { return err_reply(-14); }
                    if addr_ptr != 0 && addrlen_ptr != 0 {
                        sa::write(sock_dom(pid, fd), addr_ptr, addrlen_ptr, endpoint);
                    }
                    // recv(MSG_TRUNC) answers the datagram's real length
                    // (Linux udp_recvmsg); the rest of it is gone either way.
                    val_reply((if flags & MSG_TRUNC as usize != 0 { full } else { n }) as u64)
                }
                // After shutdown(SHUT_RD) an empty queue is end of file: a
                // reader blocked in recv returns 0 (Linux), which is how a
                // program wakes the thread it parked there.
                Err(-11) if sock_opts(ofd).shut & SHUT_RD_BIT != 0 => val_reply(0),
                Err(e) => err_reply(e),
            }
        }
        SockState::UnixConnected { conn_idx, is_a } => {
            // Destination faulted in (and unshared from copy-on-write) before
            // UNIX_CONNS is taken; see handle_send.
            sched::uaccess::prefault(buf_ptr, len.min(RING_MAX), false);
            let mut conns = UNIX_CONNS.lock();
            if !conns[conn_idx].in_use { return val_reply(0); }
            // Our own read direction retired (shutdown(fd, SHUT_RD)): EOF at
            // once, queued bytes included — Linux discards them.
            if conns[conn_idx].rd_shut(is_a) { return val_reply(0); }
            let discard;
            // A whole record was read (datagram sockets): its length is the
            // answer even when 0, and recv(MSG_TRUNC) answers its full length.
            let mut record: Option<(usize, usize)> = None;
            let r = if sock_type == SOCK_STREAM as u8 {
                // No control buffer: fds riding with these bytes are closed.
                let sr = unix_stream_read_locked(&mut conns, conn_idx, is_a,
                                                 &[(buf_ptr, len)], peek, false);
                discard = sr.discard;
                if sr.nread == 0 && sr.fault { Err(-14) } else { Ok(sr.nread) }
            } else {
                let dr = unix_dgram_read_locked(&mut conns, conn_idx, is_a, buf_ptr, len, peek, false);
                discard = dr.discard;
                match dr.res {
                    Ok(Some((n, full))) => { record = Some((n, full)); Ok(n) }
                    Ok(None) => Ok(0),
                    Err(e) => Err(e),
                }
            };
            let conn = &mut conns[conn_idx];
            if let Some((n, full)) = record {
                if !peek { conn.seq = conn.seq.wrapping_add(1); }
                drop(conns);
                for x in discard { xfer_drop(x); }
                if !peek { sched::wake_poll_key(unix_wr_key(conn_idx, !is_a)); }
                return val_reply((if flags & MSG_TRUNC as usize != 0 { full } else { n }) as u64);
            }
            // POSIX stream semantics: 0 bytes means EOF, and EOF only exists
            // once the peer will never write again. An empty ring with a live
            // peer is EAGAIN — returning 0 here made tokio's signal driver see
            // "EOF on self-pipe" on its very first empty poll and panic. The
            // peer having retired just its write direction ends the stream the
            // same way a full close does, and must read as EOF, not an error.
            let reply = match r {
                Err(e) => err_reply(e), // EFAULT, nothing consumed
                // Reset before accept (`unix_listener_gone`): reported once.
                Ok(0) if len > 0 && is_a && conn.reset_a && !peek => {
                    conn.reset_a = false;
                    err_reply(-104) // ECONNRESET
                }
                Ok(0) if len > 0 && !(if is_a { conn.closed_b } else { conn.closed_a })
                                 && !conn.wr_shut(!is_a) => err_reply(-11), // EAGAIN
                Ok(n) => val_reply(n as u64),
            };
            // Draining bytes frees ring space → a POLLOUT edge for the peer.
            let consumed = matches!(r, Ok(n) if n > 0) && !peek;
            if consumed { conn.seq = conn.seq.wrapping_add(1); }
            drop(conns); // release UNIX_CONNS before waking pollers (K2 lock order)
            // Fds that rode with the consumed bytes, released with no net
            // lock held: one may be a socket.
            for x in discard { xfer_drop(x); }
            if consumed { sched::wake_poll_key(unix_wr_key(conn_idx, !is_a)); }
            reply
        }
        SockState::UnixPendingAccept { conn_idx, .. } => {
            // Connector (end A) before the peer accept()s: reads its inbound
            // direction (ring_ba), which stays empty until the peer accepts and
            // replies. Empty + peer-not-closed is EAGAIN, never a spurious EOF.
            sched::uaccess::prefault(buf_ptr, len.min(RING_MAX), false);
            let mut conns = UNIX_CONNS.lock();
            let conn = &mut conns[conn_idx];
            if !conn.in_use { return val_reply(0); }
            if conn.rd_shut(true) { return val_reply(0); } // own read half retired
            if sock_type != SOCK_STREAM as u8 {
                // Records, not bytes (see the pre-accept send above).
                let r = if peek { conn.ring_ba.peek_dgram(buf_ptr as *mut u8, len) }
                        else { conn.ring_ba.read_dgram(buf_ptr as *mut u8, len) };
                return match r {
                    Ok(Some((n, full))) => {
                        if !peek { conn.seq = conn.seq.wrapping_add(1); }
                        drop(conns);
                        if !peek { sched::wake_poll_key(unix_wr_key(conn_idx, false)); }
                        val_reply((if flags & MSG_TRUNC as usize != 0 { full } else { n }) as u64)
                    }
                    Ok(None) if !conn.closed_b => err_reply(-11),
                    Ok(None) => val_reply(0),
                    Err(e) => err_reply(e),
                };
            }
            let n = match conn.ring_ba.read(buf_ptr as *mut u8, len) {
                Ok(n) => n,
                Err(e) => return err_reply(e), // EFAULT
            };
            if n == 0 && len > 0 && !conn.closed_b { return err_reply(-11); } // EAGAIN
            if n > 0 { conn.seq = conn.seq.wrapping_add(1); }
            drop(conns);
            if n > 0 { sched::wake_poll_tagged(sched::poll_tag(sched::poll_class::UNIX, conn_idx as u32)); }
            val_reply(n as u64)
        }
        SockState::InetConnected { socket_handle, sid, .. } => {
            let mut stack = stack_for(sid);
            if let Some(ref mut s) = *stack {
                if sock_type == SOCK_STREAM as u8 {
                    let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    if !socket.can_recv() {
                        if tcp_rx_eof(socket) {
                            drop(stack);
                            let e = tcp_connect_status(pid, fd);
                            return if e < 0 && e != -11 { err_reply(e as i32) } else { val_reply(0) };
                        }
                        return err_reply(-11);
                    }
                    // Sized to what is queued: a 64 KiB read() of a socket
                    // holding one segment does not zero 64 KiB.
                    let mut data = alloc::vec![0u8; len.min(socket.recv_queue())];
                    // A reader draining a buffer that was more than half full
                    // reopens the window: have the daemon advertise it now.
                    let pressure = !peek && socket.recv_queue() * 2 >= socket.recv_capacity();
                    // MSG_PEEK copies without dequeuing. It used to be ignored,
                    // so a peek consumed the bytes it reported.
                    let r = if peek { socket.peek_slice(&mut data) } else { socket.recv_slice(&mut data) };
                    match r {
                        Ok(n) => {
                            // Out to the caller with the stack lock released.
                            drop(stack);
                            if pressure && n > 0 { drivers::netdev::kick(); }
                            if !ucopy_out(buf_ptr, &data[..n]) { return err_reply(-14); }
                            val_reply(n as u64)
                        }
                        Err(_) => err_reply(-104),
                    }
                } else {
                    // UDP is the arm above.
                    err_reply(-9)
                }
            } else {
                err_reply(-100)
            }
        }
        SockState::IcmpBound { socket_handle, sid } => {
            let r = match stack_for(sid).as_mut() {
                Some(s) => icmp_recv_filtered(s, socket_handle, len, peek, sock_type == SOCK_RAW as u8,
                                              sock_domain == AF_INET6, &sock_opts(ofd).icmp6f),
                None => return err_reply(-100),
            };
            match r {
                Ok((data, _, from_addr)) => {
                    let n = data.len();
                    // Out to the caller with NET_STACK released.
                    if !ucopy_out(buf_ptr, &data) { return err_reply(-14); }
                    if addr_ptr != 0 && addrlen_ptr != 0 {
                        // ICMP has no port.
                        let (sa, n) = sa::encode(sock_domain, from_addr, 0, 0);
                        let cap = sched::uaccess::read_user::<u32>(addrlen_ptr).unwrap_or(0) as usize;
                        ucopy_out(addr_ptr, &sa[..cap.min(n)]);
                        wr_user(addrlen_ptr, n as u32);
                    }
                    val_reply(n as u64)
                }
                Err(e) => err_reply(e),
            }
        }
        // A ping socket that has sent nothing has nothing to receive (it used
        // to answer EBADF).
        SockState::IcmpUnbound => err_reply(-11),
        // An AF_INET datagram socket nothing has been sent from or bound yet
        // has nothing to receive.
        SockState::Unbound { domain, sock_type: ty }
            if is_inet(domain) && ty == SOCK_DGRAM as u8 => err_reply(-11),
        _ => err_reply(-9),
    }
}

/// Resolve `fd` to a connected AF_UNIX **stream** end. SCM_RIGHTS fd passing is
/// scoped to stream sockets (Wayland/D-Bus): dgram framing and per-message fd
/// attachment don't compose cleanly, so a dgram/inet fd returns None and the
/// caller takes the plain-data path.
/// A connected AF_UNIX datagram or seqpacket socket of this process.
fn unix_dgram_connected(pid: u32, fd: usize) -> bool {
    let Some(slot) = fd_to_slot(fd) else { return false };
    let tbls = SOCK_TABLES.read();
    tbls.iter().find(|t| t.in_use && t.pid == tk(pid)).map_or(false, |t| {
        slot < t.socks.len() && t.socks[slot].in_use
            && t.socks[slot].sock_type != SOCK_STREAM as u8
            && matches!(t.socks[slot].state, SockState::UnixConnected { .. })
    })
}

/// The connection end of a connected AF_UNIX socket of any type.
fn unix_conn_end(pid: u32, fd: usize) -> Option<(usize, bool)> {
    match sock_entry(pid, fd)?.state {
        SockState::UnixConnected { conn_idx, is_a } => Some((conn_idx, is_a)),
        _ => None,
    }
}

fn unix_stream_end(pid: u32, fd: usize) -> Option<(usize, bool)> {
    let slot = fd_to_slot(fd)?;
    let tbls = SOCK_TABLES.read();
    let tbl = tbls.iter().find(|t| t.in_use && t.pid == tk(pid))?;
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return None; }
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
/// Read a user word (msghdr / iovec / cmsghdr field); EFAULT on a bad
/// pointer instead of a kernel fault that kills the caller.
fn rd_usize(p: usize) -> Result<usize, i32> {
    sched::uaccess::read_user::<usize>(p).ok_or(-14)
}

/// `rd_usize` for a handler returning a reply: `?`-style early EFAULT.
macro_rules! rd_or_efault {
    ($p:expr) => { match rd_usize($p) { Ok(v) => v, Err(e) => return err_reply(e) } };
}

/// Store to user memory that only reports failure (the result was already
/// produced; a bad pointer here leaves the caller's field unwritten).
fn wr_user<T: Copy>(p: usize, v: T) -> bool { sched::uaccess::write_user(p, v) }

/// Fault-tolerant copy of a caller buffer (user, or a kernel buffer from an
/// in-kernel caller) into `dst`; false if it could not all be read.
fn ucopy_in(dst: &mut [u8], src: usize) -> bool {
    unsafe { sched::uaccess::copy_raw(dst.as_mut_ptr(), src as *const u8, dst.len()) == 0 }
}

/// Fault-tolerant copy of `src` out to a caller buffer; false on a fault.
fn ucopy_out(dst: usize, src: &[u8]) -> bool {
    unsafe { sched::uaccess::copy_raw(dst as *mut u8, src.as_ptr(), src.len()) == 0 }
}

/// Store an unnamed address (family only, addrlen 2). Fault-tolerant.
fn put_sockaddr_unnamed(addr_ptr: usize, addrlen_ptr: usize, family: u16) -> bool {
    (addr_ptr == 0 || wr_user(addr_ptr, family)) && (addrlen_ptr == 0 || wr_user(addrlen_ptr, 2u32))
}

/// Parse SCM_RIGHTS control messages out of a user control buffer into
/// `out` (int fds). Returns the fd count, or Err(errno) on a malformed cmsg
/// (EINVAL). Non-SCM_RIGHTS cmsgs are skipped. Reads user memory directly
/// (caller's address space is active) — no lock is held here.
unsafe fn parse_scm_rights(ctrl_ptr: usize, ctrl_len: usize, out: &mut [i32; SCM_MAX_FD]) -> Result<usize, i32> {
    let mut pos = 0usize;
    let mut nfd = 0usize;
    while pos + CMSG_HDR_LEN <= ctrl_len {
        let cmsg_len   = rd_usize(ctrl_ptr + pos)?;
        let cmsg_level = sched::uaccess::read_user::<i32>(ctrl_ptr + pos + 8).ok_or(-14)?;
        let cmsg_type  = sched::uaccess::read_user::<i32>(ctrl_ptr + pos + 12).ok_or(-14)?;
        if cmsg_len < CMSG_HDR_LEN || pos + cmsg_len > ctrl_len { return Err(-22); } // EINVAL
        if cmsg_level == SOL_SOCKET && cmsg_type == SCM_RIGHTS {
            let data_len = cmsg_len - CMSG_HDR_LEN;
            let count = data_len / 4;
            for i in 0..count {
                if nfd >= SCM_MAX_FD { return Err(-22); } // > SCM_MAX_FD → EINVAL
                out[nfd] = sched::uaccess::read_user::<i32>(ctrl_ptr + pos + CMSG_HDR_LEN + i * 4).ok_or(-14)?;
                nfd += 1;
            }
        }
        let step = cmsg_align(cmsg_len);
        if step == 0 { break; }
        pos += step;
    }
    Ok(nfd)
}

/// Find an SCM_CREDENTIALS cmsg in the control buffer and check it as Linux's
/// scm_check_creds does. `Ok(None)` when there is none; Err(errno) for a
/// malformed one (EINVAL), an unmapped id (EINVAL) or one the sender may not
/// claim (EPERM): its own pid / ruid,euid,suid / rgid,egid,sgid, unless it
/// holds CAP_SYS_ADMIN / CAP_SETUID / CAP_SETGID in its own namespace.
fn parse_scm_creds(pid: u32, ctrl_ptr: usize, ctrl_len: usize) -> Result<Option<Ucred>, i32> {
    const CAP_SETGID: u32 = 6;
    const CAP_SETUID: u32 = 7;
    const CAP_SYS_ADMIN: u32 = 21;
    let mut pos = 0usize;
    while pos + CMSG_HDR_LEN <= ctrl_len {
        let cmsg_len   = rd_usize(ctrl_ptr + pos)?;
        let cmsg_level = sched::uaccess::read_user::<i32>(ctrl_ptr + pos + 8).ok_or(-14)?;
        let cmsg_type  = sched::uaccess::read_user::<i32>(ctrl_ptr + pos + 12).ok_or(-14)?;
        if cmsg_len < CMSG_HDR_LEN || pos + cmsg_len > ctrl_len { return Err(-22); }
        if cmsg_level == SOL_SOCKET && cmsg_type == SCM_CREDENTIALS {
            if cmsg_len < CMSG_HDR_LEN + 12 { return Err(-22); }
            let b = ctrl_ptr + pos + CMSG_HDR_LEN;
            let vpid = sched::uaccess::read_user::<u32>(b).ok_or(-14)?;
            let vuid = sched::uaccess::read_user::<u32>(b + 4).ok_or(-14)?;
            let vgid = sched::uaccess::read_user::<u32>(b + 8).ok_or(-14)?;
            // Ids are in the sender's user namespace: translate to kernel ids
            // (kuids) before any permission check; an unmapped one is invalid.
            let (Some(uid), Some(gid)) = (sched::ns::to_k(0, sched::ns::current_userns(), vuid), sched::ns::to_k(1, sched::ns::current_userns(), vgid))
                else { return Err(-22); };
            // Linux scm_check_creds: CAP_SYS_ADMIN over the user namespace that
            // owns the sender's PID namespace (not the sender's own userns).
            let pid_admin = sched::caps::ns_capable_in(sched::ns::owner_userns(1, sched::pidns::pidns_of(pid)), CAP_SYS_ADMIN);
            let kpid = match sched::pidns::from_cur(vpid) {
                Ok(k) => k,
                Err(_) => return Err(if pid_admin { -3 } else { -1 }),
            };
            if kpid != pid && !pid_admin { return Err(-1); }
            if uid != sched::ruid_of(pid) && uid != sched::euid_of(pid) && uid != sched::current_suid()
                && !sched::caps::ns_capable(CAP_SETUID) { return Err(-1); }
            if gid != sched::rgid_of(pid) && gid != sched::egid_of(pid) && gid != sched::current_sgid()
                && !sched::caps::ns_capable(CAP_SETGID) { return Err(-1); }
            return Ok(Some(Ucred { pid: kpid, uid, gid }));
        }
        let step = cmsg_align(cmsg_len);
        if step == 0 { break; }
        pos += step;
    }
    Ok(None)
}

/// The credentials a send on `(conn_idx, is_a)` queues: the explicit
/// SCM_CREDENTIALS, else the sender's own when the peer end wants them.
fn send_cred(pid: u32, conn_idx: usize, is_a: bool, explicit: Option<Ucred>) -> Option<Ucred> {
    if explicit.is_some() { return explicit; }
    let want = {
        let c = UNIX_CONNS.lock();
        conn_idx < c.len() && c[conn_idx].in_use && (if is_a { c[conn_idx].pc_b } else { c[conn_idx].pc_a })
    };
    if !want { return None; }
    Some(Ucred { pid, uid: sched::ruid_of(pid), gid: sched::rgid_of(pid) })
}

/// sendmsg of one record with SCM_RIGHTS fds on a connected AF_UNIX
/// datagram/seqpacket socket: the record is queued whole (or EAGAIN /
/// EMSGSIZE, as a plain send) and the fd batch is pinned to its first byte,
/// so `unix_dgram_read_locked` hands it to the recvmsg that takes the record.
fn unix_dgram_send_fds(pid: u32, fd: usize, conn_idx: usize, is_a: bool, data: &[u8], fd_nums: &[i32], cred: Option<Ucred>) -> Message {
    let mut batch: alloc::vec::Vec<XferFd> = alloc::vec::Vec::new();
    for &n in fd_nums {
        match xfer_export(pid, n as usize) {
            Some(tf) => batch.push(tf),
            None => { for tf in batch { xfer_drop(tf); } return err_reply(-9); } // EBADF
        }
    }
    let sndbuf = unix_sndbuf(fd_ofd(pid, fd).unwrap_or(0));
    let fail = |batch: alloc::vec::Vec<XferFd>, e: i32| { for tf in batch { xfer_drop(tf); } err_reply(e) };
    let mut conns = UNIX_CONNS.lock();
    let conn = &mut conns[conn_idx];
    if !conn.in_use || (if is_a { conn.closed_b } else { conn.closed_a })
        || conn.wr_shut(is_a) || conn.rd_shut(!is_a) {
        drop(conns);
        return fail(batch, -32); // EPIPE
    }
    let queued: usize = conn.rx_fdq(!is_a).iter().map(|b| b.fds.len()).sum();
    if queued + batch.len() > QUEUED_FD_CAP { drop(conns); return fail(batch, ETOOMANYREFS); }
    let ring = if is_a { &mut conn.ring_ab } else { &mut conn.ring_ba };
    let seq = ring.wtotal;
    match ring.write_dgram(data.as_ptr(), data.len(), sndbuf) {
        Ok(Some(_)) => {}
        Ok(None) => { drop(conns); return fail(batch, -11); }
        Err(e) => { drop(conns); return fail(batch, e); }
    }
    let end = ring.wtotal;
    conn.rx_fdq(!is_a).push(PendingFdBatch { seq_byte: seq, end_byte: end, fds: batch, cred });
    conn.seq = conn.seq.wrapping_add(1);
    drop(conns);
    sched::wake_poll_key(unix_rd_key(conn_idx, !is_a));
    val_reply(data.len() as u64)
}

fn handle_sendmsg(pid: u32, fd: usize, msghdr_ptr: usize, _flags: usize, syn: Option<(usize, usize)>) -> Message {
    if msghdr_ptr == 0 && syn.is_none() { return err_reply(-14); }
    if let Some(o) = nlsock::of(pid, fd) {
        // A netlink writev stays one send per iovec (the caller's fallback).
        if syn.is_some() { return err_reply(-38); }
        return nlsock::send_msg(o, msghdr_ptr);
    }
    let (iov_ptr, iovcnt, ctrl_ptr, ctrl_len) = match syn {
        Some((p, c)) => (p, c, 0, 0),
        None => (rd_or_efault!(msghdr_ptr + 16), rd_or_efault!(msghdr_ptr + 24),
                 rd_or_efault!(msghdr_ptr + 32), rd_or_efault!(msghdr_ptr + 40)),
    };
    if ctrl_ptr != 0 && ctrl_len > 0 && !sched::uaccess::user_range_ok(ctrl_ptr, ctrl_len) { return err_reply(-14); }

    let unix_end = unix_stream_end(pid, fd);

    // Parse SCM_RIGHTS fds only for a unix stream socket that actually carries
    // a control buffer. Inet/dgram control is ignored (can't carry fds).
    let mut fd_nums = [0i32; SCM_MAX_FD];
    let mut nfd = 0usize;
    // A connected datagram/seqpacket socket carries fds too: each batch
    // rides with its record (`unix_dgram_send_fds`). They used to be dropped
    // without a word.
    let dgram_end = if unix_end.is_none() && unix_dgram_connected(pid, fd) { unix_conn_end(pid, fd) } else { None };
    if (unix_end.is_some() || dgram_end.is_some()) && ctrl_ptr != 0 && ctrl_len >= CMSG_HDR_LEN {
        match unsafe { parse_scm_rights(ctrl_ptr, ctrl_len.min(MAX_CONTROL), &mut fd_nums) } {
            Ok(n)  => nfd = n,
            Err(e) => return err_reply(e),
        }
    }

    // SCM_CREDENTIALS: the explicit one (checked), else the sender's own
    // when the peer end has SO_PASSCRED.
    let mut cred: Option<Ucred> = None;
    if let Some(&(ci, ia)) = unix_end.as_ref().or(dgram_end.as_ref()) {
        let explicit = if ctrl_ptr != 0 && ctrl_len >= CMSG_HDR_LEN {
            match parse_scm_creds(pid, ctrl_ptr, ctrl_len.min(MAX_CONTROL)) { Ok(c) => c, Err(e) => return err_reply(e) }
        } else { None };
        cred = send_cred(pid, ci, ia, explicit);
    }

    // AF_INET datagram: the iovecs are ONE datagram, and msg_name is its
    // destination. Sending each iovec on its own (the stream path below) split
    // a datagram into several and dropped msg_name, so an unconnected socket
    // could not sendmsg() at all.
    if nfd == 0 && cred.is_none() && inet_dgram(pid, fd) {
        let name_ptr = if syn.is_some() { 0 } else { rd_or_efault!(msghdr_ptr) };
        let name_len = if syn.is_some() { 0 } else { match sched::uaccess::read_user::<u32>(msghdr_ptr + 8) {
            Some(v) => v as usize, None => return err_reply(-14),
        } };
        let mut data: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        for i in 0..iovcnt.min(UIO_MAXIOV) {
            let iov  = iov_ptr + i * 16;
            let base = rd_or_efault!(iov);
            let len  = rd_or_efault!(iov + 8);
            if len > 0 && !sched::uaccess::user_range_ok(base, len) { return err_reply(-14); } // EFAULT: kernel iov_base
            if data.len() + len > UDP_MAX_PAYLOAD { return err_reply(-90); } // EMSGSIZE
            let at = data.len();
            data.resize(at + len, 0);
            if len > 0 && !ucopy_in(&mut data[at..], base) { return err_reply(-14); }
        }
        let (a, l) = if name_ptr != 0 { (name_ptr, name_len) } else { (0, 0) };
        // UDP_SEGMENT: the cmsg, else the socket option.
        let seg = if ctrl_ptr != 0 && ctrl_len >= CMSG_HDR_LEN {
            cmsg_udp_segment(ctrl_ptr, ctrl_len.min(MAX_CONTROL))
        } else { None };
        let seg = match seg {
            Some(s) => s,
            None if GSO_USED.load(Ordering::Relaxed) => fd_ofd(pid, fd).map_or(0, |o| sock_opts(o).gso) as usize,
            None => 0,
        };
        return udp_send_gso(pid, fd, a, l, data, seg);
    }

    // AF_UNIX datagram / seqpacket: the iovecs are ONE record, as for UDP
    // above. Sent one iovec at a time, a two-iovec message reached the peer
    // as two datagrams.
    if dgram_end.is_some() || (nfd == 0 && cred.is_none() && unix_dgram_connected(pid, fd)) {
        let mut data: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        for i in 0..iovcnt.min(UIO_MAXIOV) {
            let iov  = iov_ptr + i * 16;
            let base = rd_or_efault!(iov);
            let len  = rd_or_efault!(iov + 8);
            if len > 0 && !sched::uaccess::user_range_ok(base, len) { return err_reply(-14); } // EFAULT: kernel iov_base
            if data.len() + len > UNIX_DGRAM_MAX { return err_reply(-90); } // EMSGSIZE
            let at = data.len();
            if data.try_reserve(len).is_err() { return err_reply(-105); } // ENOBUFS
            data.resize(at + len, 0);
            if len > 0 && !ucopy_in(&mut data[at..], base) { return err_reply(-14); }
        }
        if nfd == 0 && cred.is_none() { return handle_send_k(pid, fd, 0, 0, 0, 0, Some(data)); }
        let (conn_idx, is_a) = dgram_end.unwrap();
        return unix_dgram_send_fds(pid, fd, conn_idx, is_a, &data, &fd_nums[..nfd], cred);
    }

    // No ancillary fds or credentials → original plain-data fast path (also covers inet).
    if nfd == 0 && cred.is_none() {
        let mut total = 0isize;
        for i in 0..iovcnt.min(16) {
            let iov  = iov_ptr + i * 16;
            let base = rd_or_efault!(iov);
            let len  = rd_or_efault!(iov + 8);
            if len > 0 && !sched::uaccess::user_range_ok(base, len) { return err_reply(-14); } // EFAULT: kernel iov_base
            let n = net_val(&handle_send(pid, fd, base, len, 0, 0));
            if n < 0 { return if total > 0 { val_reply(total as u64) } else { make_reply(n as i64) }; }
            total += n;
            // A short write ends the call. Going on to the next iovec would
            // put its bytes in the stream right after a truncated one as soon
            // as a reader on another CPU made room — the caller resends from
            // the count it gets back, so the tail of this iovec would be lost
            // and the stream corrupted. Firefox's IPC messages failed to parse
            // at random that way ("File handle not found in message!") while
            // the ring was a fixed 4 KiB; it now grows to RING_MAX, but a full
            // ring still writes short.
            if (n as usize) < len { break; }
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
        iovs[i] = match (rd_usize(iov), rd_usize(iov + 8)) {
            (Ok(b), Ok(l)) if l == 0 || sched::uaccess::user_range_ok(b, l) => (b, l),
            _ => { for tf in batch { xfer_drop(tf); } return err_reply(-14); }
        };
        requested += iovs[i].1;
    }
    // Source bytes faulted in before UNIX_CONNS is taken (see handle_send).
    {
        let mut budget = RING_MAX;
        for &(base, len) in iovs[..n_iov].iter() {
            if budget == 0 { break; }
            sched::uaccess::prefault(base, len.min(budget), true);
            budget -= len.min(budget);
        }
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
    // Reserve ring space, copy from the caller with UNIX_CONNS released
    // (lane ipcfast: a 64 KiB copy held the lock every unix socket in the
    // system needs for ~20 µs), then commit. Another writer on this
    // direction mid-copy: wait for its commit with the lock released.
    let sp = loop {
        let conn = &mut conns[conn_idx];
        let ring = if is_a { &mut conn.ring_ab } else { &mut conn.ring_ba };
        if let Some(sp) = ring.reserve_write(requested) { break sp; }
        conns.unlocked(|| sched::yield_now("unix-wr"));
        let conn = &conns[conn_idx];
        if !conn.in_use || (if is_a { conn.closed_b } else { conn.closed_a })
            || conn.wr_shut(is_a) || conn.rd_shut(!is_a) {
            drop(conns);
            for tf in batch { xfer_drop(tf); }
            return err_reply(-32); // EPIPE
        }
    };
    let (total, fault) = if sp.n == 0 { (0, false) } else { conns.unlocked(|| sp.fill(&iovs[..n_iov])) };
    let conn = &mut conns[conn_idx];
    let seq = if is_a { conn.ring_ab.wtotal } else { conn.ring_ba.wtotal };
    if is_a { conn.ring_ab.commit_write(&sp, total) } else { conn.ring_ba.commit_write(&sp, total) }
    // The peer went away during the copy: the bytes are lost with it, and so
    // must the fds be (pushing them would pin them in a dead queue).
    if total > 0 && (!conn.in_use || (if is_a { conn.closed_b } else { conn.closed_a })) {
        drop(conns);
        for tf in batch { xfer_drop(tf); }
        return val_reply(total as u64);
    }
    if total == 0 && fault {
        drop(conns);
        for tf in batch { xfer_drop(tf); }
        return err_reply(-14); // EFAULT
    }
    if total == 0 {
        // Couldn't place even the first byte; the blocking wrapper will retry.
        // Release the batch so the retry re-exports a fresh copy.
        drop(conns);
        for tf in batch { xfer_drop(tf); }
        return err_reply(-11); // EAGAIN
    }
    let end = seq + total as u64;
    if is_a {
        conn.fdq_ab.push(PendingFdBatch { seq_byte: seq, end_byte: end, fds: batch, cred });
    } else {
        conn.fdq_ba.push(PendingFdBatch { seq_byte: seq, end_byte: end, fds: batch, cred });
    }
    // New readable edge for the peer (total > 0 guaranteed above).
    conn.seq = conn.seq.wrapping_add(1);
    drop(conns); // release UNIX_CONNS before waking pollers (K2 lock order)
    sched::wake_poll_key(unix_rd_key(conn_idx, !is_a));
    val_reply(total as u64)
}

/// Write msg_controllen (offset 40) and msg_flags (offset 48) into a user
/// msghdr. Always called on the recvmsg success path — msg_flags must be set
/// even when it is 0.
/// Upper bound on iovecs walked by sendmsg/recvmsg on an AF_INET datagram
/// socket (Linux UIO_MAXIOV).
const UIO_MAXIOV: usize = 1024;
/// Largest UDP payload over IPv4.
const UDP_MAX_PAYLOAD: usize = 65507;
const MSG_TRUNC: i32 = 0x20;

/// True for an AF_INET datagram socket of this process: UDP, or ICMP (the
/// SOCK_DGRAM ping socket and SOCK_RAW alike). sendmsg/recvmsg move ONE
/// datagram over all the iovecs, with its address in msg_name.
fn inet_dgram(pid: u32, fd: usize) -> bool {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return false };
    let tbls = SOCK_TABLES.read();
    match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) {
        Some(t) if slot < t.socks.len() && t.socks[slot].in_use =>
            matches!(t.socks[slot].state, SockState::IcmpUnbound | SockState::IcmpBound { .. })
                || (is_inet(t.socks[slot].domain) && t.socks[slot].sock_type == SOCK_DGRAM as u8),
        _ => false,
    }
}

/// What recvmsg can report about a received datagram besides its bytes.
#[derive(Clone, Copy)]
struct RxMeta {
    from: IpEndpoint,
    /// Its destination address (IP_PKTINFO's ipi_addr).
    dst: Option<IpAddress>,
    /// The local address a reply comes from (ipi_spec_dst): the destination,
    /// or for broadcast and multicast the interface's address.
    spec_dst: Option<IpAddress>,
    /// IPv4 TOS byte and TTL (IP_RECVTOS, IP_RECVTTL).
    tos: u8,
    ttl: u8,
    /// The stack it arrived on (ifindex = sid + 1).
    sid: Sid,
    /// UDP_GRO: the size of each coalesced datagram (0 = just one).
    seg: usize,
}

/// Dequeue one datagram from an AF_INET UDP (or ping) socket: its first `cap`
/// bytes, its full length and what recvmsg may report about it. EAGAIN when
/// none is queued (or the socket has no smoltcp socket yet); a pending ICMP
/// error comes first (see `udp_next_locked`). `gro`: see `udp_next_locked`.
fn udp_recv_k(pid: u32, fd: usize, cap: usize, peek: bool, gro: bool)
    -> Result<(alloc::vec::Vec<u8>, usize, RxMeta), i32>
{
    let mut r = udp_recv_k0(pid, fd, cap, peek, gro);
    if let Ok((_, _, m)) = &mut r { m.from = nft_unorig(pid, fd, m.from); }
    r
}

fn udp_recv_k0(pid: u32, fd: usize, cap: usize, peek: bool, gro: bool)
    -> Result<(alloc::vec::Vec<u8>, usize, RxMeta), i32>
{
    let (state, sock_type, _) = inet_sock_info(pid, fd).ok_or(-9i32)?;
    let (handle, sid) = match state {
        SockState::InetConnected { socket_handle, sid, .. } => {
            let ofd = fd_ofd(pid, fd).unwrap_or(0);
            if udpany::of(ofd).is_some() { return udp_any_recv(ofd, (sid, socket_handle), cap, peek, gro); }
            (socket_handle, sid)
        }
        SockState::IcmpBound { socket_handle, sid } => {
            let mut stack = stack_for(sid);
            let s = stack.as_mut().ok_or(-100i32)?;
            let ofd = fd_ofd(pid, fd).unwrap_or(0);
            let (d, full, from) = icmp_recv_filtered(s, socket_handle, cap, peek, sock_type == SOCK_RAW as u8,
                                                     sock_dom(pid, fd) == AF_INET6, &sock_opts(ofd).icmp6f)?;
            return Ok((d, full, RxMeta { from: IpEndpoint::new(from, 0), dst: None, spec_dst: None,
                                         tos: 0, ttl: 0, sid, seg: 0 }));
        }
        _ => return Err(-11),
    };
    let mut stack = stack_for(sid);
    let s = stack.as_mut().ok_or(-100i32)?;
    udp_next_locked(s, sid, handle, cap, peek, gro, true)
}

/// UDP_GRO's limit on datagrams returned together (Linux UDP_MAX_SEGMENTS).
const UDP_GRO_MAX: usize = 64;

/// The next datagram of UDP socket `h` on stack `s` (`sid`): its first `cap`
/// bytes, its full length and its details; EAGAIN when none is queued.
/// smoltcp already filtered what a connected socket may see (its peer's
/// datagrams only, `udp::Socket::set_peer`). MSG_PEEK (`peek`) leaves the
/// datagram queued. `recv`, not `recv_slice`: the latter dequeues a datagram
/// longer than the buffer and then fails, losing it; Linux delivers the head
/// and drops the rest.
///
/// `err`: a pending socket error (an ICMP error on a connected socket) is the
/// answer instead, and is cleared — Linux checks it before the queue
/// (`__skb_recv_udp`). FIONREAD passes false.
///
/// `gro` (UDP_GRO on, not a peek): the datagrams that follow from the same
/// source with the first one's size (the last may be shorter) come along in
/// the same buffer, up to `cap` bytes and `UDP_GRO_MAX` datagrams, and `seg`
/// says the size — what Linux's GRO hands such a socket, read back by
/// quinn-udp from the UDP_GRO cmsg. The length returned is then the total.
fn udp_next_locked(s: &mut NetStack, sid: Sid, h: SocketHandle, cap: usize, peek: bool, gro: bool, err: bool)
    -> Result<(alloc::vec::Vec<u8>, usize, RxMeta), i32>
{
    let if_addr = stack_src_addr(s, sid);
    let u = s.socket_set.get_mut::<udp::Socket>(h);
    if err {
        let e = u.take_error();
        if e != 0 { return Err(-(e as i32)); }
    }
    let r = if peek { u.peek().map(|(p, m)| (p, *m)) } else { u.recv() };
    let (p, m) = r.map_err(|_| -11i32)?;
    let full = p.len();
    let mut data = p[..full.min(cap)].to_vec();
    let mut seg = 0usize;
    if gro && !peek && full > 0 && full <= cap {
        let mut n = 1usize;
        loop {
            let next = match u.peek() {
                Ok((q, qm)) if qm.endpoint == m.endpoint && !q.is_empty() && q.len() <= full
                    && data.len() + q.len() <= cap && n < UDP_GRO_MAX => q.len(),
                _ => break,
            };
            if let Ok((q, _)) = u.recv() { data.extend_from_slice(q); }
            n += 1;
            if next < full { break; }
        }
        if n > 1 { seg = full; }
    }
    let dst = m.local_address;
    let spec_dst = match dst {
        Some(d) if !d.is_multicast() && !iface::is_broadcast(d) => Some(d),
        _ => if_addr,
    };
    let total = if seg != 0 { data.len() } else { full };
    Ok((data, total, RxMeta { from: m.endpoint, dst, spec_dst, tos: m.tos,
                              ttl: m.hop_limit.unwrap_or(0), sid, seg }))
}

/// The next datagram of a wildcard UDP server (`udpany`) from whichever of
/// its stacks has one, primary first (see `udp_next_locked`). One stack lock
/// at a time; EAGAIN when none is queued.
fn udp_any_recv(ofd: u32, primary: (Sid, SocketHandle), cap: usize, peek: bool, gro: bool)
    -> Result<(alloc::vec::Vec<u8>, usize, RxMeta), i32>
{
    let Some(hs) = udpany::of(ofd) else { return Err(-11) };
    for (sid, h) in udpany::order(&hs, primary) {
        let mut stack = stack_for(sid);
        let Some(s) = stack.as_mut() else { continue };
        if let Ok(r) = udp_next_locked(s, sid, h, cap, peek, gro, false) {
            return Ok(r);
        }
    }
    Err(-11)
}

/// The control messages recvmsg adds for a UDP datagram, as Linux lays them
/// out: UDP_GRO (udp_cmsg_recv) first, then IP_PKTINFO, IP_TTL, IP_TOS
/// (ip_cmsg_recv_offset's order).
fn udp_cmsgs(recv: u8, recv6: u8, m: &RxMeta) -> alloc::vec::Vec<u8> {
    const IPPROTO_IP: i32 = 0;
    const IP_TOS: i32 = 1;
    const IP_TTL: i32 = 2;
    const IP_PKTINFO: i32 = 8;
    const UDP_GRO: i32 = 104;
    let mut out = alloc::vec::Vec::new();
    let mut put = |level: i32, ty: i32, data: &[u8]| {
        let len = CMSG_HDR_LEN + data.len();
        out.extend_from_slice(&(len as u64).to_ne_bytes());
        out.extend_from_slice(&level.to_ne_bytes());
        out.extend_from_slice(&ty.to_ne_bytes());
        out.extend_from_slice(data);
        out.resize(out.len() + cmsg_align(len) - len, 0);
    };
    let v4 = |a: Option<IpAddress>| match a {
        Some(IpAddress::Ipv4(x)) => x.0,
        #[allow(unreachable_patterns)] _ => [0; 4],
    };
    if recv & RECV_GRO != 0 && m.seg != 0 {
        put(SOL_UDP as i32, UDP_GRO, &(m.seg as i32).to_ne_bytes());
    }
    if recv & RECV_PKTINFO != 0 && m.dst.is_some() {
        let mut d = [0u8; 12];
        d[0..4].copy_from_slice(&((m.sid as i32) + 1).to_ne_bytes());
        d[4..8].copy_from_slice(&v4(m.spec_dst));
        d[8..12].copy_from_slice(&v4(m.dst));
        put(IPPROTO_IP, IP_PKTINFO, &d);
    }
    if recv & RECV_TTL != 0 && m.dst.is_some() {
        put(IPPROTO_IP, IP_TTL, &(m.ttl as i32).to_ne_bytes());
    }
    if recv & RECV_TOS != 0 && m.dst.is_some() {
        put(IPPROTO_IP, IP_TOS, &[m.tos]);
    }
    // IPv6 (ip6_datagram_recv_specific_ctl's order): IPV6_PKTINFO (a v4
    // datagram shows its addresses v4-mapped), IPV6_HOPLIMIT, IPV6_TCLASS.
    if recv6 != 0 && m.dst.is_some() {
        if recv6 & RECV6_PKTINFO != 0 {
            let mut d = [0u8; 20];
            let a = match m.dst {
                Some(IpAddress::Ipv6(a)) => a.0,
                Some(IpAddress::Ipv4(a)) => { let mut b = [0u8; 16]; b[10] = 0xff; b[11] = 0xff; b[12..].copy_from_slice(&a.0); b }
                None => [0; 16],
            };
            d[0..16].copy_from_slice(&a);
            d[16..20].copy_from_slice(&((m.sid as u32) + 1).to_ne_bytes());
            put(IPPROTO_IPV6 as i32, IPV6_PKTINFO as i32, &d);
        }
        if recv6 & RECV6_HOPLIMIT != 0 {
            put(IPPROTO_IPV6 as i32, IPV6_HOPLIMIT as i32, &(m.ttl as i32).to_ne_bytes());
        }
        if recv6 & RECV6_TCLASS != 0 {
            put(IPPROTO_IPV6 as i32, IPV6_TCLASS as i32, &(m.tos as i32).to_ne_bytes());
        }
    }
    out
}

unsafe fn write_msg_tail(msghdr_ptr: usize, controllen: usize, msg_flags: i32) {
    if msghdr_ptr == 0 { return; } // readv: no msghdr to report into
    wr_user(msghdr_ptr + 40, controllen);
    wr_user(msghdr_ptr + 48, msg_flags);
}

fn handle_recvmsg(pid: u32, fd: usize, msghdr_ptr: usize, flags: usize, syn: Option<(usize, usize)>) -> Message {
    if msghdr_ptr == 0 && syn.is_none() { return err_reply(-14); }
    if let Some(o) = nlsock::of(pid, fd) {
        if syn.is_some() { return err_reply(-38); }
        return nlsock::recv_msg(o, msghdr_ptr, flags);
    }
    let (iov_ptr, iovcnt, ctrl_ptr, ctrl_cap) = match syn {
        Some((p, c)) => (p, c, 0, 0),
        None => (rd_or_efault!(msghdr_ptr + 16), rd_or_efault!(msghdr_ptr + 24),
                 rd_or_efault!(msghdr_ptr + 32), rd_or_efault!(msghdr_ptr + 40)),
    };
    if ctrl_ptr != 0 && ctrl_cap > 0 && !sched::uaccess::user_range_ok(ctrl_ptr, ctrl_cap) { return err_reply(-14); }

    let unix_end = unix_stream_end(pid, fd);

    // AF_INET datagram: one datagram scattered over the iovecs, its source
    // in msg_name, MSG_TRUNC when it did not fit.
    if unix_end.is_none() && inet_dgram(pid, fd) {
        let name_ptr = if syn.is_some() { 0 } else { rd_or_efault!(msghdr_ptr) };
        let mut iovs: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
        let mut cap = 0usize;
        for i in 0..iovcnt.min(UIO_MAXIOV) {
            let iov = iov_ptr + i * 16;
            let (b, l) = (rd_or_efault!(iov), rd_or_efault!(iov + 8));
            if l > 0 && !sched::uaccess::user_range_ok(b, l) { return err_reply(-14); } // EFAULT: kernel iov_base
            iovs.push((b, l));
            cap += l;
        }
        let (recv, recv6) = fd_ofd(pid, fd).map_or((0, 0), |o| { let so = sock_opts(o); (so.recv, so.recv6) });
        if flags & MSG_ERRQUEUE != 0 {
            // The socket error queue (Linux ip_recv_error / ipv6_recv_error):
            // the quoted payload, the failed destination in msg_name and an
            // IP_RECVERR cmsg: sock_extended_err + the ICMP sender.
            let e = match udp_recv_err(pid, fd, flags & MSG_PEEK == 0) { Ok(e) => e, Err(x) => return err_reply(x) };
            let mut off = 0usize;
            for &(b, l) in iovs.iter() {
                if off >= e.payload.len() { break; }
                let k = l.min(e.payload.len() - off);
                if !ucopy_out(b, &e.payload[off..off + k]) { return err_reply(-14); }
                off += k;
            }
            if name_ptr != 0 { sa::write(sock_dom(pid, fd), name_ptr, msghdr_ptr + 8, e.dst); }
            let v6 = sock_dom(pid, fd) == AF_INET6;
            let mut c = alloc::vec::Vec::new();
            c.extend_from_slice(&(e.errno as u32).to_ne_bytes());
            c.extend_from_slice(&[e.origin, e.ty, e.code, 0]);
            c.extend_from_slice(&e.info.to_ne_bytes());
            c.extend_from_slice(&0u32.to_ne_bytes());
            if v6 {
                let mut sin6 = [0u8; 28];
                sin6[0..2].copy_from_slice(&10u16.to_ne_bytes());
                if let IpAddress::Ipv6(a) = e.offender { sin6[8..24].copy_from_slice(&a.0); }
                else if let IpAddress::Ipv4(a) = e.offender { sin6[18] = 0xff; sin6[19] = 0xff; sin6[20..24].copy_from_slice(&a.0); }
                c.extend_from_slice(&sin6);
            } else {
                let mut sin = [0u8; 16];
                if let IpAddress::Ipv4(a) = e.offender { sin[0..2].copy_from_slice(&2u16.to_ne_bytes()); sin[4..8].copy_from_slice(&a.0); }
                c.extend_from_slice(&sin);
            }
            let clen = CMSG_HDR_LEN + c.len();
            let mut m = alloc::vec::Vec::new();
            m.extend_from_slice(&(clen as u64).to_ne_bytes());
            m.extend_from_slice(&(if v6 { 41i32 } else { 0i32 }).to_ne_bytes());
            m.extend_from_slice(&(if v6 { IPV6_RECVERR as i32 } else { IP_RECVERR as i32 }).to_ne_bytes());
            m.extend_from_slice(&c);
            let mut ctrunc = false;
            let mut written = 0usize;
            if ctrl_ptr == 0 || clen > ctrl_cap { ctrunc = true; }
            else if !ucopy_out(ctrl_ptr, &m) { return err_reply(-14); }
            else { written = clen; }
            let tr = e.payload.len() > off;
            unsafe { write_msg_tail(msghdr_ptr, written, MSG_ERRQUEUE as i32 | (if ctrunc { MSG_CTRUNC } else { 0 }) | (if tr { MSG_TRUNC } else { 0 })); }
            return val_reply(off as u64);
        }
        let (data, full, meta) = match udp_recv_k(pid, fd, cap, flags & MSG_PEEK != 0, recv & RECV_GRO != 0) {
            Ok(r) => r,
            // shutdown(SHUT_RD): end of file, as in handle_recv.
            Err(-11) if fd_ofd(pid, fd).map_or(false, |o| sock_opts(o).shut & SHUT_RD_BIT != 0) => {
                unsafe { write_msg_tail(msghdr_ptr, 0, 0); }
                return val_reply(0);
            }
            Err(e) => return err_reply(e),
        };
        let mut off = 0usize;
        for &(b, l) in iovs.iter() {
            if off >= data.len() { break; }
            let k = l.min(data.len() - off);
            if !ucopy_out(b, &data[off..off + k]) { return err_reply(-14); }
            off += k;
        }
        let from = meta.from;
        if name_ptr != 0 {
            sa::write(sock_dom(pid, fd), name_ptr, msghdr_ptr + 8, from);
        }
        let truncated = full > data.len();
        // IP_PKTINFO / IP_RECVTTL / IP_RECVTOS / UDP_GRO: whole control
        // messages that fit, MSG_CTRUNC for the rest (Linux put_cmsg).
        let mut ctrl_written = 0usize;
        let mut ctrunc = false;
        if recv & (RECV_PKTINFO | RECV_TOS | RECV_TTL | RECV_GRO) != 0 || recv6 != 0 {
            let c = udp_cmsgs(recv, recv6, &meta);
            let mut pos = 0usize;
            while pos < c.len() {
                let clen = u64::from_ne_bytes(c[pos..pos + 8].try_into().unwrap_or([0; 8])) as usize;
                let step = cmsg_align(clen).min(c.len() - pos);
                if ctrl_ptr == 0 || ctrl_written + clen > ctrl_cap { ctrunc = true; break; }
                let n = step.min(ctrl_cap - ctrl_written);
                if !ucopy_out(ctrl_ptr + ctrl_written, &c[pos..pos + n]) { return err_reply(-14); }
                ctrl_written += n;
                pos += step;
            }
        }
        let mflags = (if truncated { MSG_TRUNC } else { 0 }) | (if ctrunc { MSG_CTRUNC } else { 0 });
        unsafe { write_msg_tail(msghdr_ptr, ctrl_written, mflags); }
        // recvmsg(MSG_TRUNC) answers the datagram's real length, as on Linux.
        let n = if truncated && flags & (MSG_TRUNC as usize) != 0 { full } else { data.len() };
        return val_reply(n as u64);
    }

    // AF_UNIX datagram / seqpacket: one record, scattered over the iovecs.
    // Read one iovec at a time, a record that exactly filled the first iovec
    // was followed by the NEXT record in the second.
    if unix_end.is_none() && unix_dgram_connected(pid, fd) {
        let mut iovs: alloc::vec::Vec<(usize, usize)> = alloc::vec::Vec::new();
        let mut cap = 0usize;
        for i in 0..iovcnt.min(UIO_MAXIOV) {
            let iov = iov_ptr + i * 16;
            let (b, l) = (rd_or_efault!(iov), rd_or_efault!(iov + 8));
            if l > 0 && !sched::uaccess::user_range_ok(b, l) { return err_reply(-14); } // EFAULT: kernel iov_base
            iovs.push((b, l));
            cap = cap.saturating_add(l);
        }
        let cap = cap.min(UNIX_DGRAM_MAX);
        let mut kbuf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        if kbuf.try_reserve_exact(cap).is_err() { return err_reply(-105); } // ENOBUFS
        kbuf.resize(cap, 0);
        let Some((conn_idx, is_a)) = unix_conn_end(pid, fd) else { return err_reply(-9) };
        let peek = flags & MSG_PEEK != 0;
        let mut conns = UNIX_CONNS.lock();
        let c = &mut conns[conn_idx];
        if !c.in_use || c.rd_shut(is_a) {
            drop(conns);
            unsafe { write_msg_tail(msghdr_ptr, 0, 0); }
            return val_reply(0);
        }
        let dr = unix_dgram_read_locked(&mut conns, conn_idx, is_a, kbuf.as_mut_ptr() as usize, cap, peek, true);
        let c = &mut conns[conn_idx];
        let (n, full) = match dr.res {
            Ok(Some(r)) => r,
            Err(e) => { drop(conns); for x in dr.discard { xfer_drop(x); } return err_reply(e); }
            Ok(None) => {
                // Nothing queued: the same answers as handle_recv.
                let r = if is_a && c.reset_a && !peek { c.reset_a = false; -104 }
                    else if (if is_a { c.closed_b } else { c.closed_a }) || c.wr_shut(!is_a) { 0 }
                    else { -11 };
                drop(conns);
                for x in dr.discard { xfer_drop(x); }
                if r == 0 { unsafe { write_msg_tail(msghdr_ptr, 0, 0); } }
                return make_reply(r);
            }
        };
        if !peek { c.seq = c.seq.wrapping_add(1); }
        drop(conns);
        for x in dr.discard { xfer_drop(x); }
        if !peek { sched::wake_poll_key(unix_wr_key(conn_idx, !is_a)); }
        let mut off = 0usize;
        for &(b, l) in iovs.iter() {
            if off >= n { break; }
            let k = l.min(n - off);
            if k > 0 && !ucopy_out(b, &kbuf[off..off + k]) {
                if let Some(fds) = dr.fds { for x in fds { xfer_drop(x); } }
                return err_reply(-14);
            }
            off += k;
        }
        let (ctrl_written, ctrunc) = deliver_scm(pid, dr.fds, rcv_cred(pid, fd, dr.cred), ctrl_ptr, ctrl_cap, flags);
        let truncated = full > n;
        let mflags = (if truncated { MSG_TRUNC } else { 0 }) | (if ctrunc { MSG_CTRUNC } else { 0 });
        unsafe { write_msg_tail(msghdr_ptr, ctrl_written, mflags); }
        // recvmsg(MSG_TRUNC) answers the record's real length, as on Linux.
        return val_reply((if truncated && flags & MSG_TRUNC as usize != 0 { full } else { n }) as u64);
    }

    // Non-unix (inet/dgram): plain-data path; still writes msg_flags/controllen.
    let (conn_idx, is_a) = match unix_end {
        Some(e) => e,
        None => {
            let mut total = 0isize;
            for i in 0..iovcnt.min(16) {
                let iov  = iov_ptr + i * 16;
                let base = rd_or_efault!(iov);
                let len  = rd_or_efault!(iov + 8);
                if len > 0 && !sched::uaccess::user_range_ok(base, len) { return err_reply(-14); } // EFAULT: kernel iov_base
                let n = net_val(&handle_recv(pid, fd, base, len, 0, 0, flags));
                if n < 0 { return if total > 0 { val_reply(total as u64) } else { make_reply(n as i64) }; }
                total += n;
                // A peek would copy the same bytes into the next iovec again.
                if flags & MSG_PEEK != 0 { break; }
                // Same rule as a short write: the next iovec must not be filled
                // after a partly filled one.
                if (n as usize) < len { break; }
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
        iovs[i] = (rd_or_efault!(iov), rd_or_efault!(iov + 8));
        if iovs[i].1 > 0 && !sched::uaccess::user_range_ok(iovs[i].0, iovs[i].1) { return err_reply(-14); } // EFAULT: kernel iov_base
        requested += iovs[i].1;
    }
    // Destination bytes (and the control buffer) faulted in and unshared
    // before UNIX_CONNS is taken (see handle_send). Only the bytes queued now
    // are prefaulted (a 52-page walk per call otherwise); if more arrives and
    // the copy then faults with nothing delivered, the read is retried once
    // with the full window (below).
    let queued = {
        let conns = UNIX_CONNS.lock();
        if !conns[conn_idx].in_use { 0 } else if is_a { conns[conn_idx].ring_ba.count } else { conns[conn_idx].ring_ab.count }
    };
    let prefault_iovs = |cap: usize| {
        let mut budget = cap.min(RING_MAX);
        for &(base, len) in iovs[..n_iov].iter() {
            if budget == 0 { break; }
            sched::uaccess::prefault(base, len.min(budget), false);
            budget -= len.min(budget);
        }
    };
    prefault_iovs(queued);
    if ctrl_ptr != 0 { sched::uaccess::prefault(ctrl_ptr, ctrl_cap.min(MAX_CONTROL), false); }
    // A zero-length recv can't consume the fd-carrying byte and would spin the
    // blocking wrapper on EAGAIN forever — return 0 immediately (msg_flags set).
    if requested == 0 {
        unsafe { write_msg_tail(msghdr_ptr, 0, 0); }
        return val_reply(0);
    }

    // Read data and take at most one fd batch, under one lock.
    let peek = flags & MSG_PEEK != 0;
    let mut conns = UNIX_CONNS.lock();
    if !conns[conn_idx].in_use {
        drop(conns);
        unsafe { write_msg_tail(msghdr_ptr, 0, 0); }
        return val_reply(0);
    }
    let mut sr = unix_stream_read_locked(&mut conns, conn_idx, is_a, &iovs[..n_iov], peek, true);
    if sr.fault && sr.nread == 0 && queued < RING_MAX {
        // Data arrived after the queue length was sampled: fault in the full
        // window and read again.
        drop(conns);
        for x in sr.discard { xfer_drop(x); }
        prefault_iovs(RING_MAX);
        conns = UNIX_CONNS.lock();
        if !conns[conn_idx].in_use {
            drop(conns);
            unsafe { write_msg_tail(msghdr_ptr, 0, 0); }
            return val_reply(0);
        }
        sr = unix_stream_read_locked(&mut conns, conn_idx, is_a, &iovs[..n_iov], peek, true);
    }
    let nread = sr.nread;
    let conn = &mut conns[conn_idx];
    if nread == 0 {
        // Empty ring: EOF only if the peer end has closed or retired its write
        // direction, or we retired our own read direction — else EAGAIN.
        // Mirrors handle_recv — tokio's self-pipe must not see a spurious EOF.
        // A fault with nothing copied is EFAULT, with nothing consumed.
        let peer_closed = if is_a { conn.closed_b } else { conn.closed_a };
        let eagain = !peer_closed && !conn.wr_shut(!is_a) && !conn.rd_shut(is_a);
        // Reset before accept (`unix_listener_gone`): reported once.
        if !sr.fault && is_a && conn.reset_a && !peek {
            conn.reset_a = false;
            drop(conns);
            for x in sr.discard { xfer_drop(x); }
            return err_reply(-104); // ECONNRESET
        }
        if sr.fault || eagain {
            drop(conns);
            for x in sr.discard { xfer_drop(x); }
            return err_reply(if sr.fault { -14 } else { -11 });
        }
    }
    let deliver = sr.fds;
    // Draining bytes frees ring space → a POLLOUT edge for the peer.
    let freed = nread > 0 && !peek;
    if freed { conn.seq = conn.seq.wrapping_add(1); }
    drop(conns); // release before importing (locks FD_TABLES)
    for x in sr.discard { xfer_drop(x); }
    if freed { sched::wake_poll_key(unix_wr_key(conn_idx, !is_a)); }

    let (ctrl_written, ctrunc) = deliver_scm(pid, deliver, rcv_cred(pid, fd, sr.cred), ctrl_ptr, ctrl_cap, flags);
    unsafe { write_msg_tail(msghdr_ptr, ctrl_written, if ctrunc { MSG_CTRUNC } else { 0 }); }
    val_reply(nread as u64)
}

/// Install the fds a recvmsg took into the receiver and write their
/// SCM_RIGHTS cmsg into the control buffer: `(bytes written, MSG_CTRUNC)`.
/// No net lock held (importing takes FD_TABLES).
fn deliver_scm(pid: u32, deliver: Option<alloc::vec::Vec<XferFd>>, cred: Option<Ucred>,
               ctrl_ptr: usize, ctrl_cap: usize, flags: usize) -> (usize, bool) {
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
            let newfd = xfer_import(pid, fds[i].clone(), cloexec);
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
        for j in fit..nfds { xfer_drop(fds[j].clone()); }
        if fit > 0 {
            let clen = CMSG_HDR_LEN + fit * 4;
            let mut ok = wr_user(ctrl_ptr, clen)
                && wr_user(ctrl_ptr + 8, SOL_SOCKET)
                && wr_user(ctrl_ptr + 12, SCM_RIGHTS);
            for k in 0..fit {
                ok = ok && wr_user(ctrl_ptr + CMSG_HDR_LEN + k * 4, installed[k]);
            }
            // A control buffer that could not be written reports no cmsg
            // (the fds are installed either way, as on Linux's EFAULT path).
            if ok { ctrl_written = clen; } else { ctrunc = true; }
        }
    }
    if let Some(c) = cred {
        // SCM_CREDENTIALS, translated into the receiver's namespaces: a pid
        // its pid namespace cannot see reads 0, an unmapped id 65534.
        let clen = CMSG_HDR_LEN + 12;
        let at = cmsg_align(ctrl_written);
        if ctrl_ptr == 0 || ctrl_cap < at + clen {
            ctrunc = true;
        } else {
            let vn = sched::pidns::pidns_of(pid);
            let vpid = if vn != 0 && c.pid != 0 { sched::pidns::to_v(vn, c.pid) } else { c.pid };
            let ok = wr_user(ctrl_ptr + at, clen)
                && wr_user(ctrl_ptr + at + 8, SOL_SOCKET)
                && wr_user(ctrl_ptr + at + 12, SCM_CREDENTIALS)
                && wr_user(ctrl_ptr + at + 16, [vpid, sched::ns::view_id(0, c.uid), sched::ns::view_id(1, c.gid)]);
            if ok { ctrl_written = at + cmsg_align(clen).min(ctrl_cap - at); } else { ctrunc = true; }
        }
    }
    (ctrl_written, ctrunc)
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
    let (state, sock_type, ofd) = {
        let tbls = SOCK_TABLES.read();
        let tbl = match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
        (tbl.socks[slot].state, tbl.socks[slot].sock_type, tbl.socks[slot].ofd)
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
        sched::wake_poll_tagged(sched::poll_tag(sched::poll_class::UNIX, conn_idx as u32));
        return ok_reply();
    }

    if let SockState::InetConnected { socket_handle, sid, remote_endpoint } = state {
        // UDP (Linux inet_shutdown + datagram_poll): the directions are
        // recorded on the socket — SHUT_RD makes an empty queue read as end
        // of file and poll readable, SHUT_WR makes send EPIPE — and every
        // waiter is woken, which is what a program shutting a UDP socket down
        // wants: its thread blocked in recvfrom returns 0. Unconnected, the
        // same happens and the answer is ENOTCONN.
        if sock_type == SOCK_DGRAM as u8 {
            let bits = match how { SHUT_RD => SHUT_RD_BIT, SHUT_WR => SHUT_WR_BIT, _ => SHUT_RD_BIT | SHUT_WR_BIT };
            let mut o = sock_opts(ofd);
            o.shut |= bits;
            set_sock_opts(ofd, o);
            if let Some(s) = stack_for(sid).as_mut() { bump_seq(s, socket_handle); }
            // Its waiters are woken by its keys on the way out (the
            // NET_SHUTDOWN dispatch, `with_state_wake`).
            return if remote_endpoint.is_some() { ok_reply() } else { err_reply(-107) };
        }
        // Retiring the write direction on TCP is the FIN and nothing else: the
        // fd, the smoltcp handle and the port stay this socket's until
        // handle_close tears them down (and parks the port in TIME_WAIT).
        // SHUT_RD has no wire effect: accepted and nothing done, as on Linux.
        // Only a stream socket has a `tcp::Socket` behind the handle; `get_mut`
        // on the wrong socket type panics.
        if sock_type == SOCK_STREAM as u8 && (how == SHUT_WR || how == SHUT_RDWR) {
            let mut stack = stack_for(sid);
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
    if let Some(o) = nlsock::of(pid, fd) { return nlsock::getsockname(o, addr_ptr, addrlen_ptr); }
    // Endpoint resolved first; user memory is written with no lock held.
    let endpoint = inet_local_endpoint(pid, fd);
    match endpoint {
        Some(ep) => unsafe { write_sockaddr_in(sock_dom(pid, fd), addr_ptr, addrlen_ptr, ep); },
        None => match unix_bound_name(pid, fd) {
            Some((name, len)) => if !put_sockaddr_un(addr_ptr, addrlen_ptr, &name[..len]) {
                return err_reply(-14);
            },
            None => if !put_sockaddr_unnamed(addr_ptr, addrlen_ptr, 0) { return err_reply(-14); },
        },
    }
    ok_reply()
}

/// The address a bound AF_UNIX socket was bound to, as the caller wrote it
/// (sun_path bytes: a pathname without its NUL, or `\0`-led abstract name).
/// Read out with no lock held across the two tables (BOUND_PATHS is a leaf).
fn unix_bound_name(pid: u32, fd: usize) -> Option<([u8; PATH_MAX], usize)> {
    let slot = fd_to_slot(fd)?;
    let idx = {
        let mut tbls = SOCK_TABLES.lock();
        let tbl = find_tbl(pid, &mut *tbls)?;
        if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return None; }
        match tbl.socks[slot].state { SockState::UnixListening { bound_idx } => bound_idx, _ => return None }
    };
    let bound = BOUND_PATHS.lock();
    let bp = bound.get(idx).filter(|b| b.in_use)?;
    let len = if bp.is_abstract { bp.path_len }
              else { bp.path[..bp.path_len].iter().position(|&b| b == 0).unwrap_or(bp.path_len) };
    Some((bp.path, len))
}

/// Write `sockaddr_un { AF_UNIX, name }` for getsockname: a pathname gets its
/// NUL, and *addrlen reports the full length even when the caller's buffer
/// truncated it, as on Linux.
fn put_sockaddr_un(addr_ptr: usize, addrlen_ptr: usize, name: &[u8]) -> bool {
    let cap = match sched::uaccess::read_user::<u32>(addrlen_ptr) { Some(v) => v as usize, None => return false };
    let mut buf = [0u8; 2 + PATH_MAX + 1];
    buf[..2].copy_from_slice(&(AF_UNIX as u16).to_ne_bytes());
    buf[2..2 + name.len()].copy_from_slice(name);
    let full = 2 + name.len() + if name.first() == Some(&0) { 0 } else { 1 };
    let n = full.min(cap);
    ucopy_out(addr_ptr, &buf[..n]) && wr_user(addrlen_ptr, full as u32)
}

fn handle_getpeername(pid: u32, fd: usize, addr_ptr: usize, addrlen_ptr: usize) -> Message {
    if addr_ptr == 0 || addrlen_ptr == 0 { return err_reply(-14); }
    if let Some(o) = nlsock::of(pid, fd) { return nlsock::getpeername(o, addr_ptr, addrlen_ptr); }
    let endpoint = inet_remote_endpoint(pid, fd).map(|ep| {
        let slot = fd_to_slot(fd).unwrap_or(usize::MAX);
        ORIG_PEER.lock().iter().find(|x| x.0 == pid && x.1 == slot && x.3 == ep).map_or(ep, |x| x.2)
    });
    match endpoint {
        Some(ep) => unsafe { write_sockaddr_in(sock_dom(pid, fd), addr_ptr, addrlen_ptr, ep); },
        // An AF_INET socket with no peer (unconnected UDP, a listener, a
        // TCP socket never connected): ENOTCONN, as on Linux. It answered
        // success with an AF_UNSPEC address, which a caller asking "am I
        // connected?" (the UDP connect/disconnect dance) took for yes.
        None if inet_sock_info(pid, fd).map_or(false, |(st, _, _)| match st {
            SockState::Unbound { domain, .. } => is_inet(domain),
            st => is_inet_state(&st),
        }) => return err_reply(-107),
        None => if !put_sockaddr_unnamed(addr_ptr, addrlen_ptr, 0) { return err_reply(-14); },
    }
    ok_reply()
}

/// setsockopt was a bare `ok_reply()` for every option, and stays that way for
/// every option but these: SO_KEEPALIVE and TCP_NODELAY/KEEPIDLE/KEEPINTVL/
/// KEEPCNT are stored per description and applied to the smoltcp socket
/// (`SockOpts`), and SO_REUSEADDR is recorded, because bind()
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
    // Netlink: buffer sizes and the like are accepted and ignored.
    if nlsock::of(pid, fd).is_some() { return ok_reply(); }
    const SO_REUSEADDR: usize = 2;
    if level == SOL_SOCKET as usize && optname == SO_REUSEADDR {
        // The int is read out of user memory before any server lock is taken:
        // a demand-paging fault under SOCK_TABLES would re-enter the scheduler
        // with a server lock held.
        let on = optval_ptr != 0 && optlen >= 4
            && sched::uaccess::read_user::<u32>(optval_ptr).unwrap_or(0) != 0;
        let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
        let mut tbls = SOCK_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
        tbl.socks[slot].reuseaddr = on;
        share_state(&mut *tbls, pid, slot);
        return ok_reply();
    }
    if level == IPPROTO_IP { return ip_setsockopt(pid, fd, optname, optval_ptr, optlen); }
    if level == IPPROTO_IPV6 { return ipv6_setsockopt(pid, fd, optname, optval_ptr, optlen); }
    if level == IPPROTO_ICMPV6 { return icmp6_setsockopt(pid, fd, optname, optval_ptr, optlen); }
    // UDP_GRO: recvmsg returns runs of equal-sized datagrams together.
    if level == SOL_UDP && optname == UDP_GRO {
        let Some(v) = (if optlen < 4 { None } else { sched::uaccess::read_user::<i32>(optval_ptr) })
            else { return err_reply(if optlen < 4 { -22 } else { -14 }) };
        let Some(e) = sock_entry(pid, fd) else { return err_reply(-9) };
        if !is_udp_entry(&e) { return err_reply(-ENOPROTOOPT); }
        let mut o = sock_opts(e.ofd);
        if v != 0 { o.recv |= RECV_GRO; } else { o.recv &= !RECV_GRO; }
        set_sock_opts(e.ofd, o);
        return ok_reply();
    }
    // SO_BROADCAST / SO_REUSEPORT (read back by getsockopt; SO_REUSEPORT is
    // what bind() lets share a port) and UDP_SEGMENT (UDP GSO, see
    // `udp_send_gso`): ints, stored per description like the TCP options.
    let flag_opt = level == SOL_SOCKET as usize && (optname == SO_BROADCAST || optname == SO_REUSEPORT);
    let gso_opt = level == SOL_UDP && optname == UDP_SEGMENT;
    if flag_opt || gso_opt {
        let val: Option<i32> = if optlen < 4 { None } else { sched::uaccess::read_user::<i32>(optval_ptr) };
        let Some(v) = val else { return err_reply(if optlen < 4 { -22 } else { -14 }) };
        let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
        let tbls = SOCK_TABLES.read();
        let Some(e) = tbls.iter().find(|t| t.in_use && t.pid == tk(pid))
            .and_then(|t| t.socks.get(slot).cloned()).filter(|e| e.in_use) else { return err_reply(-9) };
        drop(tbls);
        let mut o = sock_opts(e.ofd);
        if gso_opt {
            if !is_udp_entry(&e) { return err_reply(-ENOPROTOOPT); }
            if !(0..=65535).contains(&v) { return err_reply(-22); }
            o.gso = v as u16;
            if v > 0 { GSO_USED.store(true, Ordering::Relaxed); }
        } else if optname == SO_BROADCAST {
            o.broadcast = v != 0;
        } else {
            o.reuseport = v != 0;
        }
        set_sock_opts(e.ofd, o);
        return ok_reply();
    }
    // SO_SNDBUF: stored as Linux's sock_setsockopt stores it (capped at
    // wmem_max, doubled, at least SOCK_MIN_SNDBUF) and read back that way.
    // An AF_UNIX datagram socket's sends are bounded by it (`write_dgram`).
    if level == SOL_SOCKET as usize && optname == SO_SNDBUF {
        let val: Option<i32> = if optlen < 4 { None } else { sched::uaccess::read_user::<i32>(optval_ptr) };
        let Some(v) = val else { return err_reply(if optlen < 4 { -22 } else { -14 }) };
        let Some(e) = sock_entry(pid, fd) else { return err_reply(-9) };
        let v = (v as u32).min(SNDBUF_MAX / 2);
        let mut o = sock_opts(e.ofd);
        o.sndbuf = (v * 2).max(SNDBUF_MIN);
        set_sock_opts(e.ofd, o);
        return ok_reply();
    }
    // SO_PASSCRED: recvmsg reports the sender's SCM_CREDENTIALS. The sender
    // consults the connection's per-end flag, so a connected end updates it.
    if level == SOL_SOCKET as usize && optname == SO_PASSCRED {
        let val: Option<i32> = if optlen < 4 { None } else { sched::uaccess::read_user::<i32>(optval_ptr) };
        let Some(v) = val else { return err_reply(if optlen < 4 { -22 } else { -14 }) };
        let Some(e) = sock_entry(pid, fd) else { return err_reply(-9) };
        if e.domain as usize != AF_UNIX { return err_reply(-ENOPROTOOPT); }
        let mut o = sock_opts(e.ofd);
        o.passcred = v != 0;
        set_sock_opts(e.ofd, o);
        let end = match e.state {
            SockState::UnixConnected { conn_idx, is_a } => Some((conn_idx, is_a)),
            SockState::UnixPendingAccept { conn_idx, .. } => Some((conn_idx, true)),
            _ => None,
        };
        if let Some((ci, ia)) = end {
            let mut c = UNIX_CONNS.lock();
            if ci < c.len() && c[ci].in_use { if ia { c[ci].pc_a = v != 0; } else { c[ci].pc_b = v != 0; } }
        }
        return ok_reply();
    }
    // SO_RCVTIMEO / SO_SNDTIMEO: Linux's sock_set_timeout. EINVAL for a
    // short struct, EDOM for tv_usec outside [0, 1e6), "do not wait" for a
    // negative tv_sec, none for zero.
    if level == SOL_SOCKET as usize
        && matches!(optname, SO_RCVTIMEO_OLD | SO_SNDTIMEO_OLD | SO_RCVTIMEO_NEW | SO_SNDTIMEO_NEW) {
        if optlen < 16 { return err_reply(-22); }
        let (Some(sec), Some(usec)) = (sched::uaccess::read_user::<i64>(optval_ptr),
                                       sched::uaccess::read_user::<i64>(optval_ptr + 8))
            else { return err_reply(-14) };
        if !(0..1_000_000).contains(&usec) { return err_reply(-33); } // EDOM
        let ns = if sec < 0 { 1 }
            else { (sec as u64).saturating_mul(1_000_000_000).saturating_add(usec as u64 * 1000) };
        let Some(e) = sock_entry(pid, fd) else { return err_reply(-9) };
        let mut o = sock_opts(e.ofd);
        if matches!(optname, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) { o.rcvtimeo = ns; } else { o.sndtimeo = ns; }
        set_sock_opts(e.ofd, o);
        return ok_reply();
    }
    let keepalive = level == SOL_SOCKET as usize && optname == SO_KEEPALIVE;
    if keepalive || level == IPPROTO_TCP {
        // Read before any lock, as above. Linux checks the length first.
        let val: Option<i32> = if optlen < 4 { None }
            else { sched::uaccess::read_user::<i32>(optval_ptr) };
        let bad_val = if optlen < 4 { -22 } else { -14 }; // EINVAL / EFAULT
        let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
        let mut tbls = SOCK_TABLES.lock();
        let tbl = match find_tbl(pid, &mut *tbls) {
            Some(t) => t, None => return err_reply(-9),
        };
        if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
        let e = tbl.socks[slot].clone();
        let is_tcp = is_tcp_entry(&e);
        // SOL_TCP on anything else: an AF_UNIX socket has no protocol-level
        // setsockopt (EOPNOTSUPP); UDP, ping and raw sockets hand the level
        // to ip_setsockopt, which does not know it (ENOPROTOOPT).
        if !keepalive && !is_tcp {
            return err_reply(if e.domain as usize == AF_UNIX { -EOPNOTSUPP } else { -ENOPROTOOPT });
        }
        let mut o = sock_opts(e.ofd);
        match optname {
            _ if keepalive => o.keepalive = match val {
                Some(v) => v != 0, None => return err_reply(bad_val) },
            TCP_NODELAY | TCP_KEEPIDLE | TCP_KEEPINTVL | TCP_KEEPCNT => {
                let v = match val { Some(v) => v, None => return err_reply(bad_val) };
                match optname {
                    TCP_NODELAY => o.nodelay = v != 0,
                    TCP_KEEPIDLE if (1..=32767).contains(&v) => o.keepidle = v as u16,
                    TCP_KEEPINTVL if (1..=32767).contains(&v) => o.keepintvl = v as u16,
                    TCP_KEEPCNT if (1..=127).contains(&v) => o.keepcnt = v as u8,
                    _ => return err_reply(-22),
                }
            }
            // Any other TCP option on a TCP socket stays an accepted no-op.
            _ => return ok_reply(),
        }
        set_sock_opts(e.ofd, o);
        // A connected TCP socket takes the change now. SOCK_TABLES is held
        // across the stack lock (the documented order), so the handle cannot
        // be released underneath us.
        if is_tcp {
            if let SockState::InetConnected { socket_handle, sid, .. } = e.state {
                let mut stack = stack_for(sid);
                if let Some(s) = stack.as_mut() {
                    apply_tcp_opts(s.socket_set.get_mut::<tcp::Socket>(socket_handle), o, !e.connecting);
                }
                drop(stack);
                drop(tbls);
                // Setting TCP_NODELAY pushes what Nagle was holding back
                // (Linux: tcp_push_pending_frames), and a keepalive change
                // re-arms the timer: run the stack now rather than at its
                // next timer or packet.
                drivers::netdev::kick();
            }
        }
        return ok_reply();
    }
    ok_reply()
}

/// SO_SNDTIMEO (`send`) or SO_RCVTIMEO of socket `fd` in nanoseconds, 0 for
/// none: the bound a blocking send/recv/accept/connect waits for.
pub fn sock_timeo(pid: u32, fd: usize, send: bool) -> u64 {
    let Some(e) = sock_entry(sched::tgid_of(pid), fd) else { return 0 };
    let o = sock_opts(e.ofd);
    if send { o.sndtimeo } else { o.rcvtimeo }
}

/// A copy of the table entry of `fd` (in use), if any.
fn sock_entry(pid: u32, fd: usize) -> Option<SockEntry> {
    let slot = fd_to_slot(fd)?;
    let tbls = SOCK_TABLES.read();
    tbls.iter().find(|t| t.in_use && t.pid == tk(pid))
        .and_then(|t| t.socks.get(slot).cloned()).filter(|e| e.in_use)
}

/// setsockopt(IPPROTO_IP, ...): Linux's do_ip_setsockopt for the options a
/// UDP program uses. Values are read before any lock is taken. Unknown IP
/// options (and any on an AF_UNIX socket) stay accepted no-ops, as before.
/// setsockopt(IPPROTO_IPV6, ...): V6ONLY and the options the ICMPv6 and UDP
/// paths honour. IPV6_UNICAST_HOPS and IPV6_TCLASS share the `ttl` / `tos`
/// fields with their IPv4 twins: an AF_INET6 socket uses one set.
fn ipv6_setsockopt(pid: u32, fd: usize, optname: usize, optval_ptr: usize, optlen: usize) -> Message {
    let Some(e) = sock_entry(pid, fd) else { return err_reply(-9) };
    if e.domain as usize != AF_INET6 { return if optname == IPV6_V6ONLY { err_reply(-ENOPROTOOPT) } else { ok_reply() }; }
    let v = match (if optlen < 4 { None } else { sched::uaccess::read_user::<i32>(optval_ptr) }) {
        Some(v) => v,
        None => return err_reply(if optlen < 4 { -22 } else { -14 }),
    };
    let mut o = sock_opts(e.ofd);
    let flag = |o: &mut SockOpts, bit: u8| if v != 0 { o.recv6 |= bit } else { o.recv6 &= !bit };
    match optname {
        IPV6_V6ONLY => {
            // Linux: only before the socket has a port (a bound or listening socket is EINVAL).
            if !matches!(e.state, SockState::Unbound { .. }) || e.bound_port != 0 { return err_reply(-22); }
            o.v6only = v != 0;
        }
        IPV6_UNICAST_HOPS => match v {
            -1 => o.ttl = 0,
            0..=255 => o.ttl = v as u8, // 0 reads as the default
            _ => return err_reply(-22),
        },
        IPV6_MULTICAST_HOPS => match v {
            -1 => o.mc_ttl = 1,
            0..=255 => o.mc_ttl = v as u8,
            _ => return err_reply(-22),
        },
        IPV6_TCLASS => match v {
            -1 => o.tos = 0,
            0..=255 => o.tos = v as u8,
            _ => return err_reply(-22),
        },
        IPV6_RECVPKTINFO => flag(&mut o, RECV6_PKTINFO),
        IPV6_RECVHOPLIMIT => flag(&mut o, RECV6_HOPLIMIT),
        IPV6_RECVTCLASS => flag(&mut o, RECV6_TCLASS),
        IPV6_RECVERR => { flag(&mut o, RECV6_ERR); udp_set_recverr(pid, fd, o.recv6 & RECV6_ERR != 0); }
        IPV6_MTU_DISCOVER => match v {
            0..=3 => o.pmtud1 = v as u8 + 1,
            _ => return err_reply(-22),
        },
        // Sticky pktinfo (source address / interface choice) is accepted, not used.
        IPV6_PKTINFO => if optlen < 20 { return err_reply(-22) },
        _ => return ok_reply(),
    }
    set_sock_opts(e.ofd, o);
    ok_reply()
}

/// setsockopt(IPPROTO_ICMPV6, ICMP6_FILTER): a raw ICMPv6 socket only.
fn icmp6_setsockopt(pid: u32, fd: usize, optname: usize, optval_ptr: usize, optlen: usize) -> Message {
    let Some(e) = sock_entry(pid, fd) else { return err_reply(-9) };
    if e.domain as usize != AF_INET6 || e.sock_type as usize != SOCK_RAW || optname != ICMP6_FILTER {
        return err_reply(-ENOPROTOOPT);
    }
    let mut b = [0u8; 32];
    let mut o = sock_opts(e.ofd);
    for (i, w) in o.icmp6f.iter().enumerate() { b[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes()); }
    let n = optlen.min(32);
    if n > 0 && !ucopy_in(&mut b[..n], optval_ptr) { return err_reply(-14); }
    for i in 0..8 { o.icmp6f[i] = u32::from_ne_bytes([b[i * 4], b[i * 4 + 1], b[i * 4 + 2], b[i * 4 + 3]]); }
    set_sock_opts(e.ofd, o);
    // Datagrams a new filter lets through may already be waiting.
    sock_state_moved(&[sock_state_key(e.ofd)]);
    ok_reply()
}

fn ip_setsockopt(pid: u32, fd: usize, optname: usize, optval_ptr: usize, optlen: usize) -> Message {
    let Some(e) = sock_entry(pid, fd) else { return err_reply(-9) };
    if e.domain as usize != AF_INET { return ok_reply(); }
    // Most IP options take an int, or a single byte (Linux: optlen 1..3).
    let int_val = || -> Result<i32, i32> {
        if optlen >= 4 { sched::uaccess::read_user::<i32>(optval_ptr).ok_or(-14) }
        else if optlen >= 1 { sched::uaccess::read_user::<u8>(optval_ptr).map(|b| b as i32).ok_or(-14) }
        else { Err(-22) }
    };
    let mut o = sock_opts(e.ofd);
    let flag = |o: &mut SockOpts, bit: u8, on: bool| if on { o.recv |= bit } else { o.recv &= !bit };
    match optname {
        IP_TOS => match int_val() { Ok(v) => o.tos = v as u8, Err(x) => return err_reply(x) },
        IP_TTL => match int_val() {
            Ok(-1) => o.ttl = 0,
            Ok(v) if (1..=255).contains(&v) => o.ttl = v as u8,
            Ok(_) => return err_reply(-22),
            Err(x) => return err_reply(x),
        },
        IP_MTU_DISCOVER => match int_val() {
            Ok(v) if (0..=3).contains(&v) => o.pmtud1 = v as u8 + 1,
            Ok(_) => return err_reply(-22),
            Err(x) => return err_reply(x),
        },
        IP_PKTINFO | IP_RECVTTL | IP_RECVTOS | IP_RECVERR => match int_val() {
            Ok(v) => {
                flag(&mut o, match optname {
                    IP_PKTINFO => RECV_PKTINFO, IP_RECVTTL => RECV_TTL, IP_RECVTOS => RECV_TOS, _ => RECV_ERR,
                }, v != 0);
                if optname == IP_RECVERR { udp_set_recverr(pid, fd, v != 0); }
            }
            Err(x) => return err_reply(x),
        },
        IP_MULTICAST_TTL => match int_val() {
            Ok(-1) => o.mc_ttl = 1,
            Ok(v) if (0..=255).contains(&v) => o.mc_ttl = v as u8,
            Ok(_) => return err_reply(-22),
            Err(x) => return err_reply(x),
        },
        IP_MULTICAST_LOOP => match int_val() { Ok(v) => o.mc_noloop = v == 0, Err(x) => return err_reply(x) },
        // Always on here: a socket hears every group joined on the host
        // for its port (Linux's default).
        IP_MULTICAST_ALL => if let Err(x) = int_val() { return err_reply(x) },
        IP_MULTICAST_IF => {
            // struct ip_mreqn, struct ip_mreq or struct in_addr (Linux).
            let (addr, ifindex) = if optlen >= 12 {
                match (sched::uaccess::read_user::<[u8; 4]>(optval_ptr + 4), sched::uaccess::read_user::<i32>(optval_ptr + 8)) {
                    (Some(a), Some(i)) => (a, i), _ => return err_reply(-14),
                }
            } else if optlen >= 8 {
                match sched::uaccess::read_user::<[u8; 4]>(optval_ptr + 4) { Some(a) => (a, 0), None => return err_reply(-14) }
            } else if optlen >= 4 {
                match sched::uaccess::read_user::<[u8; 4]>(optval_ptr) { Some(a) => (a, 0), None => return err_reply(-14) }
            } else {
                return err_reply(-22);
            };
            if ifindex == 0 && addr == [0; 4] {
                o.mc_if = 0;
                o.mc_addr = [0; 4];
            } else {
                let sid = if ifindex > 0 {
                    let s = (ifindex - 1) as usize;
                    if s >= MAX_STACKS || stack_for(s as Sid).is_none() { return err_reply(-19); } // ENODEV
                    s as Sid
                } else {
                    let a = IpAddress::Ipv4(smoltcp::wire::Ipv4Address(addr));
                    match if is_loopback_addr(a) { Some(LO) } else { iface::holder_of(a) } {
                        Some(s) => s,
                        None => return err_reply(-99), // EADDRNOTAVAIL
                    }
                };
                o.mc_if = sid + 1;
                o.mc_addr = addr;
            }
        }
        IP_ADD_MEMBERSHIP | IP_DROP_MEMBERSHIP => {
            // struct ip_mreqn when long enough, else struct ip_mreq.
            if optlen < 8 { return err_reply(-22); }
            let Some(group) = sched::uaccess::read_user::<[u8; 4]>(optval_ptr) else { return err_reply(-14) };
            let Some(addr) = sched::uaccess::read_user::<[u8; 4]>(optval_ptr + 4) else { return err_reply(-14) };
            let ifindex = if optlen >= 12 {
                match sched::uaccess::read_user::<i32>(optval_ptr + 8) { Some(i) => i, None => return err_reply(-14) }
            } else { 0 };
            if is_tcp_entry(&e) { return err_reply(-71); } // EPROTO
            if group[0] & 0xf0 != 0xe0 { return err_reply(-22); }
            let r = if optname == IP_ADD_MEMBERSHIP {
                mcast::join(e.ofd, group, addr, ifindex)
            } else {
                mcast::leave(e.ofd, group, addr, ifindex)
            };
            return match r { Ok(()) => ok_reply(), Err(x) => err_reply(x) };
        }
        _ => return ok_reply(),
    }
    set_sock_opts(e.ofd, o);
    ok_reply()
}

/// getsockopt(IPPROTO_IP, ...) for what `ip_setsockopt` keeps; `None` for
/// any other option.
fn ip_getsockopt(e: &SockEntry, optname: usize) -> Option<u32> {
    let o = sock_opts(e.ofd);
    Some(match optname {
        IP_TOS => o.tos as u32,
        IP_TTL => if o.ttl == 0 { 64 } else { o.ttl as u32 },
        IP_MTU_DISCOVER => o.pmtud_mode() as u32,
        // Connected sockets only (Linux: ENOTCONN, which this reports as ENOPROTOOPT).
        IP_MTU => match e.state {
            SockState::InetConnected { socket_handle, sid, .. } if is_udp_entry(e) => {
                let peer = stack_for(sid).as_mut().and_then(|s| s.socket_set.get_mut::<udp::Socket>(socket_handle).peer());
                match peer {
                    Some(IpEndpoint { addr: IpAddress::Ipv4(d), .. }) => path_mtu(sid, d),
                    _ => return None,
                }
            }
            _ => return None,
        },
        IP_PKTINFO => (o.recv & RECV_PKTINFO != 0) as u32,
        IP_RECVTTL => (o.recv & RECV_TTL != 0) as u32,
        IP_RECVTOS => (o.recv & RECV_TOS != 0) as u32,
        IP_RECVERR => (o.recv & RECV_ERR != 0) as u32,
        IP_MULTICAST_TTL => o.mc_ttl as u32,
        IP_MULTICAST_LOOP => (!o.mc_noloop) as u32,
        IP_MULTICAST_ALL => 1,
        // struct in_addr: the address it was set by.
        IP_MULTICAST_IF => u32::from_ne_bytes(o.mc_addr),
        _ => return None,
    })
}

/// Linux ip_local_error: a DF datagram larger than the path with IP_RECVERR
/// fails with EMSGSIZE and leaves an SO_EE_ORIGIN_LOCAL entry (ee_info = MTU).
fn udp_local_emsgsize(s: &mut NetStack, h: SocketHandle, dst: IpEndpoint, mtu: u32) {
    let unspec = if matches!(dst.addr, IpAddress::Ipv6(_)) { IpAddress::Ipv6(smoltcp::wire::Ipv6Address::UNSPECIFIED) }
                 else { IpAddress::Ipv4(smoltcp::wire::Ipv4Address::UNSPECIFIED) };
    s.socket_set.get_mut::<udp::Socket>(h).push_err(udp::ErrEntry {
        errno: 90, origin: 1, ty: 0, code: 0, info: mtu, offender: unspec, dst, payload: alloc::vec::Vec::new(),
    });
}

/// IP_RECVERR / IPV6_RECVERR on a UDP socket: its smoltcp socket queues
/// ICMP errors for the error queue from now on.
fn udp_set_recverr(pid: u32, fd: usize, on: bool) {
    let Some(e) = sock_entry(pid, fd) else { return };
    if !is_udp_entry(&e) { return; }
    let SockState::InetConnected { socket_handle, sid, .. } = e.state else { return };
    if udpany::of(e.ofd).is_some() { return; }
    let mut stack = stack_for(sid);
    if let Some(s) = stack.as_mut() { s.socket_set.get_mut::<udp::Socket>(socket_handle).set_recverr(on); }
}

/// MSG_ERRQUEUE: the oldest entry of the socket error queue of a UDP socket
/// (EAGAIN when empty); `take` dequeues it (not MSG_PEEK).
fn udp_recv_err(pid: u32, fd: usize, take: bool) -> Result<udp::ErrEntry, i32> {
    let e = sock_entry(pid, fd).ok_or(-9i32)?;
    if !is_udp_entry(&e) { return Err(-11); }
    let SockState::InetConnected { socket_handle, sid, .. } = e.state else { return Err(-11) };
    let mut stack = stack_for(sid);
    let s = stack.as_mut().ok_or(-100i32)?;
    s.socket_set.get_mut::<udp::Socket>(socket_handle).recv_err(take).ok_or(-11)
}

/// Read and clear the pending ICMP error of a connected UDP socket (SO_ERROR);
/// `None` when `fd` is not a UDP socket with a smoltcp socket.
fn udp_take_error(pid: u32, fd: usize) -> Option<u8> {
    let e = sock_entry(pid, fd)?;
    if !is_udp_entry(&e) { return None; }
    let SockState::InetConnected { socket_handle, sid, .. } = e.state else { return Some(0) };
    if udpany::of(e.ofd).is_some() { return Some(0); }
    let mut stack = stack_for(sid);
    Some(stack.as_mut().map_or(0, |s| s.socket_set.get_mut::<udp::Socket>(socket_handle).take_error()))
}

/// An AF_INET UDP socket in any state (not a ping socket): the sockets the
/// SOL_UDP level applies to.
fn is_udp_entry(e: &SockEntry) -> bool {
    is_inet(e.domain) && e.sock_type as usize == SOCK_DGRAM
        && !matches!(e.state, SockState::IcmpUnbound | SockState::IcmpBound { .. })
}

/// An AF_INET SOCK_STREAM socket in any state — the sockets the IPPROTO_TCP
/// level applies to. Ping and raw sockets are SOCK_DGRAM/SOCK_RAW.
fn is_tcp_entry(e: &SockEntry) -> bool {
    is_inet(e.domain) && e.sock_type as usize == SOCK_STREAM
}

fn handle_getsockopt(pid: u32, fd: usize, level: usize, optname: usize,
                     optval_ptr: usize, optlen_ptr: usize) -> Message {
    // SO_PEERCRED: report the peer end's captured {pid,uid,gid} (struct ucred).
    // D-Bus EXTERNAL auth reads the uid from here.
    if level == SOL_SOCKET as usize && optname == SO_PEERCRED {
        if optval_ptr == 0 { return err_reply(-14); }
        let mut cred = unix_peer_cred(pid, fd);
        // The peer's pid as the caller's pid namespace sees it (0 when invisible).
        let vn = sched::pidns::pidns_of(pid);
        if vn != 0 && cred.pid != 0 { cred.pid = sched::pidns::to_v(vn, cred.pid); }
        if !wr_user(optval_ptr, [cred.pid, sched::ns::view_id(0, cred.uid), sched::ns::view_id(1, cred.gid)]) { return err_reply(-14); }
        if optlen_ptr != 0 && !wr_user(optlen_ptr, 12u32) { return err_reply(-14); }
        return ok_reply();
    }
    // SO_ERROR: the pending socket error, read once. The only one this
    // server tracks is a refused TCP connect, which a non-blocking connect's
    // caller (NSPR's PR_ConnectContinue, mio) reads here when poll reports the
    // socket; anything else, including a connect still in flight, is 0.
    if level == SOL_SOCKET as usize && optname == SO_ERROR {
        // A connected UDP socket's pending ICMP error, read once.
        if let Some(err) = udp_take_error(pid, fd) {
            if optval_ptr != 0 && !wr_user(optval_ptr, err as u32) { return err_reply(-14); }
            if optlen_ptr != 0 && !wr_user(optlen_ptr, 4u32) { return err_reply(-14); }
            return ok_reply();
        }
        // An AF_UNIX connector whose listener closed before accepting it:
        // Linux's `sk_err` = ECONNRESET, read (and cleared) here.
        if let Some(SockState::UnixConnected { conn_idx, is_a: true }) = sock_entry(pid, fd).map(|e| e.state) {
            let reset = {
                let mut conns = UNIX_CONNS.lock();
                let c = &mut conns[conn_idx];
                let r = c.in_use && c.reset_a;
                if r { c.reset_a = false; }
                r
            };
            let v: u32 = if reset { 104 } else { 0 };
            if optval_ptr != 0 && !wr_user(optval_ptr, v) { return err_reply(-14); }
            if optlen_ptr != 0 && !wr_user(optlen_ptr, 4u32) { return err_reply(-14); }
            return ok_reply();
        }
        let e = tcp_connect_status(pid, fd);
        let v: u32 = if e < 0 && e != -11 { (-e) as u32 } else { 0 };
        if optval_ptr != 0 && !wr_user(optval_ptr, v) { return err_reply(-14); }
        if optlen_ptr != 0 && !wr_user(optlen_ptr, 4u32) { return err_reply(-14); }
        return ok_reply();
    }
    if level == IPPROTO_ICMPV6 {
        let Some(e) = sock_entry(pid, fd) else { return err_reply(-9) };
        if e.domain as usize != AF_INET6 || e.sock_type as usize != SOCK_RAW || optname != ICMP6_FILTER {
            return err_reply(-ENOPROTOOPT);
        }
        let o = sock_opts(e.ofd);
        let mut b = [0u8; 32];
        for (i, w) in o.icmp6f.iter().enumerate() { b[i * 4..i * 4 + 4].copy_from_slice(&w.to_ne_bytes()); }
        let Some(cap) = sched::uaccess::read_user::<u32>(optlen_ptr) else { return err_reply(-14) };
        let n = (cap as usize).min(32);
        if !ucopy_out(optval_ptr, &b[..n]) || !wr_user(optlen_ptr, n as u32) { return err_reply(-14); }
        return ok_reply();
    }
    // Plain int options a socket library reads back (NSPR's
    // PR_GetSocketOption, glib, Python's socket module). The buffer sizes are
    // the smoltcp buffers a TCP socket gets; SO_KEEPALIVE and the TCP options
    // read back what setsockopt stored (`SockOpts`).
    {
        const SO_TYPE: usize = 3;
        const SO_RCVBUF: usize = 8;
        const SO_PROTOCOL: usize = 38;
        const SO_DOMAIN: usize = 39;
        const SO_REUSEADDR: usize = 2;
        let entry = fd_to_slot(fd).and_then(|slot| {
            let tbls = SOCK_TABLES.read();
            let t = tbls.iter().find(|t| t.in_use && t.pid == tk(pid))?;
            if slot < t.socks.len() && t.socks[slot].in_use { Some(t.socks[slot].clone()) } else { None }
        });
        let Some(e) = entry else { return err_reply(-9) };
        let v: Option<u32> = match (level, optname) {
            (l, SO_TYPE) if l == SOL_SOCKET as usize => Some(e.sock_type as u32),
            (l, SO_DOMAIN) if l == SOL_SOCKET as usize => Some(e.domain as u32),
            // A ping socket (SOCK_DGRAM or SOCK_RAW) is IPPROTO_ICMP, not UDP.
            (l, SO_PROTOCOL) if l == SOL_SOCKET as usize
                && matches!(e.state, SockState::IcmpUnbound | SockState::IcmpBound { .. }) =>
                Some(if e.domain as usize == AF_INET6 { IPPROTO_ICMPV6 } else { IPPROTO_ICMP } as u32),
            (l, SO_PROTOCOL) if l == SOL_SOCKET as usize => Some(match (e.domain as usize, e.sock_type as usize) {
                (AF_INET | AF_INET6, SOCK_STREAM) => 6, (AF_INET | AF_INET6, SOCK_DGRAM) => 17, _ => 0 }),
            (l, SO_SNDBUF) if l == SOL_SOCKET as usize && sock_opts(e.ofd).sndbuf != 0 =>
                Some(sock_opts(e.ofd).sndbuf),
            (l, SO_SNDBUF) | (l, SO_RCVBUF) if l == SOL_SOCKET as usize && e.domain as usize == AF_UNIX =>
                Some(UNIX_SNDBUF_DEFAULT),
            (l, SO_SNDBUF) | (l, SO_RCVBUF) if l == SOL_SOCKET as usize && is_udp_entry(&e) =>
                Some(if optname == SO_SNDBUF { UDP_TX_BUF as u32 } else { UDP_RX_BUF as u32 }),
            (l, SO_SNDBUF) | (l, SO_RCVBUF) if l == SOL_SOCKET as usize => Some(if optname == SO_SNDBUF { tcp_tx_buf() as u32 } else { TCP_RX_BUF as u32 }),
            (l, SO_BROADCAST) if l == SOL_SOCKET as usize => Some(sock_opts(e.ofd).broadcast as u32),
            (l, SO_REUSEPORT) if l == SOL_SOCKET as usize => Some(sock_opts(e.ofd).reuseport as u32),
            (SOL_UDP, UDP_SEGMENT) if is_udp_entry(&e) => Some(sock_opts(e.ofd).gso as u32),
            (SOL_UDP, UDP_GRO) if is_udp_entry(&e) => Some((sock_opts(e.ofd).recv & RECV_GRO != 0) as u32),
            (IPPROTO_IP, n) if is_inet(e.domain) && n != IP_MULTICAST_IF => ip_getsockopt(&e, n),
            (IPPROTO_IPV6, n) if e.domain as usize == AF_INET6 => {
                let o = sock_opts(e.ofd);
                match n {
                    IPV6_V6ONLY => Some(o.v6only as u32),
                    IPV6_UNICAST_HOPS => Some(if o.ttl == 0 { 64 } else { o.ttl as u32 }),
                    IPV6_MULTICAST_HOPS => Some(o.mc_ttl as u32),
                    IPV6_TCLASS => Some(o.tos as u32),
                    IPV6_RECVPKTINFO => Some((o.recv6 & RECV6_PKTINFO != 0) as u32),
                    IPV6_RECVHOPLIMIT => Some((o.recv6 & RECV6_HOPLIMIT != 0) as u32),
                    IPV6_RECVTCLASS => Some((o.recv6 & RECV6_TCLASS != 0) as u32),
                    IPV6_RECVERR => Some((o.recv6 & RECV6_ERR != 0) as u32),
                    IPV6_MTU_DISCOVER => Some(o.pmtud_mode() as u32),
                    IPV6_MTU => match e.state {
                        SockState::InetConnected { socket_handle, sid, .. } if is_udp_entry(&e) => {
                            let peer = stack_for(sid).as_mut().and_then(|s| s.socket_set.get_mut::<udp::Socket>(socket_handle).peer());
                            match peer {
                                Some(IpEndpoint { addr: a @ IpAddress::Ipv6(_), .. }) => Some(path_mtu_ip(sid, a)),
                                _ => None,
                            }
                        }
                        _ => return err_reply(-107), // ENOTCONN
                    },
                    _ => None,
                }
            }
            (l, SO_PASSCRED) if l == SOL_SOCKET as usize && e.domain as usize == AF_UNIX => Some(sock_opts(e.ofd).passcred as u32),
            (l, SO_KEEPALIVE) if l == SOL_SOCKET as usize => Some(sock_opts(e.ofd).keepalive as u32),
            (l, SO_REUSEADDR) if l == SOL_SOCKET as usize => Some(e.reuseaddr as u32),
            // As setsockopt: the TCP level exists only on a TCP socket.
            (IPPROTO_TCP, _) if !is_tcp_entry(&e) =>
                return err_reply(if e.domain as usize == AF_UNIX { -EOPNOTSUPP } else { -ENOPROTOOPT }),
            (IPPROTO_TCP, TCP_NODELAY)   => Some(sock_opts(e.ofd).nodelay as u32),
            (IPPROTO_TCP, TCP_KEEPIDLE)  => Some(sock_opts(e.ofd).keepidle as u32),
            (IPPROTO_TCP, TCP_KEEPINTVL) => Some(sock_opts(e.ofd).keepintvl as u32),
            (IPPROTO_TCP, TCP_KEEPCNT)   => Some(sock_opts(e.ofd).keepcnt as u32),
            _ => None,
        };
        if let Some(v) = v {
            if optval_ptr != 0 && !wr_user(optval_ptr, v) { return err_reply(-14); }
            if optlen_ptr != 0 && !wr_user(optlen_ptr, 4u32) { return err_reply(-14); }
            return ok_reply();
        }
        // SO_RCVTIMEO / SO_SNDTIMEO: a struct timeval, {0, 0} for none
        // (Linux's sock_get_timeout); a shorter buffer gets its prefix.
        if level == SOL_SOCKET as usize
            && matches!(optname, SO_RCVTIMEO_OLD | SO_SNDTIMEO_OLD | SO_RCVTIMEO_NEW | SO_SNDTIMEO_NEW) {
            let o = sock_opts(e.ofd);
            let ns = if matches!(optname, SO_RCVTIMEO_OLD | SO_RCVTIMEO_NEW) { o.rcvtimeo } else { o.sndtimeo };
            let tv = [ns / 1_000_000_000, (ns % 1_000_000_000) / 1000];
            let mut b = [0u8; 16];
            b[..8].copy_from_slice(&tv[0].to_ne_bytes());
            b[8..].copy_from_slice(&tv[1].to_ne_bytes());
            if optlen_ptr == 0 { return err_reply(-14); }
            let Some(len) = sched::uaccess::read_user::<i32>(optlen_ptr) else { return err_reply(-14) };
            if len < 0 { return err_reply(-22); }
            let n = (len as usize).min(16);
            if n > 0 && (optval_ptr == 0 || !b[..n].iter().enumerate().all(|(i, &x)| wr_user(optval_ptr + i, x))) {
                return err_reply(-14);
            }
            if !wr_user(optlen_ptr, n as u32) { return err_reply(-14); }
            return ok_reply();
        }
        // IP_MTU on a socket with no peer: ENOTCONN, as Linux.
        if level == IPPROTO_IP && optname == IP_MTU && is_inet(e.domain)
            && !matches!(e.state, SockState::InetConnected { .. }) {
            return err_reply(-107);
        }
        // IP_MULTICAST_IF answers a struct in_addr.
        if level == IPPROTO_IP && optname == IP_MULTICAST_IF && is_inet(e.domain) {
            let a = sock_opts(e.ofd).mc_addr;
            if optval_ptr != 0 && !wr_user(optval_ptr, a) { return err_reply(-14); }
            if optlen_ptr != 0 && !wr_user(optlen_ptr, 4u32) { return err_reply(-14); }
            return ok_reply();
        }
        // TCP_INFO: Linux's `struct tcp_info` prefix (104 bytes: through
        // total_retrans). Only the state and a few plausible constants are
        // modelled; passt's pasta reads it on every connection and sizes its
        // feature probes from the length we return.
        if level == IPPROTO_TCP && optname == TCP_INFO && is_tcp_entry(&e) {
            let st: u8 = match e.state {
                SockState::InetListening { .. } => 10,
                SockState::InetConnected { socket_handle, sid, .. } => {
                    let mut stack = stack_for(sid);
                    match stack.as_mut() {
                        Some(s) => match s.socket_set.get_mut::<tcp::Socket>(socket_handle).state() {
                            tcp::State::Established => 1, tcp::State::SynSent => 2,
                            tcp::State::SynReceived => 3, tcp::State::FinWait1 => 4,
                            tcp::State::FinWait2 => 5, tcp::State::TimeWait => 6,
                            tcp::State::Closed => 7, tcp::State::CloseWait => 8,
                            tcp::State::LastAck => 9, tcp::State::Listen => 10,
                            tcp::State::Closing => 11,
                        },
                        None => 7,
                    }
                }
                _ => 7,
            };
            let mut b = [0u8; TCP_INFO_LEN];
            b[0] = st;
            let put = |b: &mut [u8; TCP_INFO_LEN], off: usize, v: u32| b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
            put(&mut b, 8, 200_000);   // rto (us)
            // The path MTU the cache knows (ICMP frag-needed / packet-too-big)
            // clamps the sender's segment size, as tcp_sync_mss does.
            let pmtu = match e.state {
                SockState::InetConnected { sid, remote_endpoint: Some(r), .. } =>
                    path_mtu_ip(sid, r.addr).min(1500),
                _ => 1500,
            };
            let hdr = if matches!(e.state, SockState::InetConnected { remote_endpoint: Some(IpEndpoint { addr: IpAddress::Ipv6(_), .. }), .. }) { 72 } else { 52 };
            put(&mut b, 16, (pmtu - hdr).min(1448)); // snd_mss
            put(&mut b, 20, 1448);     // rcv_mss
            put(&mut b, 60, pmtu);     // pmtu
            put(&mut b, 68, 1000);     // rtt (us)
            put(&mut b, 80, 10);       // snd_cwnd
            put(&mut b, 84, 1460);     // advmss
            if optlen_ptr == 0 { return err_reply(-14); }
            let Some(len) = sched::uaccess::read_user::<i32>(optlen_ptr) else { return err_reply(-14) };
            if len < 0 { return err_reply(-22); }
            let n = (len as usize).min(TCP_INFO_LEN);
            if n > 0 && (optval_ptr == 0 || !b[..n].iter().enumerate().all(|(i, &x)| wr_user(optval_ptr + i, x))) {
                return err_reply(-14);
            }
            if !wr_user(optlen_ptr, n as u32) { return err_reply(-14); }
            return ok_reply();
        }
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
/// The credentials recvmsg on `fd` reports: those of the batch it took, when
/// the receiving description has SO_PASSCRED.
fn rcv_cred(pid: u32, fd: usize, c: Option<Ucred>) -> Option<Ucred> {
    let c = c?;
    let e = sock_entry(pid, fd)?;
    if sock_opts(e.ofd).passcred { Some(c) } else { None }
}

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
    match tbls.iter().find(|t| t.in_use && t.pid == tk(parent)) {
        None => return ok_reply(), // parent has no sockets — nothing to do
        // A table whose sockets are all closed: nothing to inherit, so do not
        // claim (and possibly heap-allocate, 32 KiB) a table for the child.
        // A shell that once opened a socket would otherwise give every
        // process it forks a table it never uses.
        Some(t) if !t.socks.iter().any(|e| e.in_use) => return ok_reply(),
        Some(_) => {}
    }
    if get_or_create(child, &mut *tbls).is_none() {
        return err_reply(-12);
    }
    let parent_pos = tbls.iter().position(|t| t.in_use && t.pid == tk(parent)).unwrap();
    let child_pos  = tbls.iter().position(|t| t.in_use && t.pid == tk(child)).unwrap();

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
    let n = tbls[parent_pos].socks.len();
    if tbls[child_pos].socks.len() < n {
        let have = tbls[child_pos].socks.len();
        if tbls[child_pos].socks.try_reserve(n - have).is_err() { return err_reply(-12); }
        tbls[child_pos].socks.resize(n, SockEntry::empty());
    }
    let mut conns = UNIX_CONNS.lock();
    for i in 0..n {
        let e = tbls[parent_pos].socks[i].clone();
        if !e.in_use { continue; }
        match e.state {
            SockState::UnixConnected { conn_idx, is_a } => {
                if conn_idx >= conns.len() || !conns[conn_idx].in_use { continue; }
                if is_a { conns[conn_idx].refs_a += 1; } else { conns[conn_idx].refs_b += 1; }
                tbls[child_pos].socks[i] = e.clone();
                vfs::ofd::get(e.ofd);
            }
            // An unaccepted connect() is end A of a real connection — see the
            // doc comment for why dropping it here broke COSMIC's privileged
            // applet sockets.
            SockState::UnixPendingAccept { conn_idx, .. } => {
                if conn_idx >= conns.len() || !conns[conn_idx].in_use { continue; }
                conns[conn_idx].refs_a += 1; // the connector is always end A
                tbls[child_pos].socks[i] = e.clone();
                vfs::ofd::get(e.ofd);
                if NET_DEBUG { traced.push((i, conn_idx)); }
            }
            SockState::Unbound { .. } => { tbls[child_pos].socks[i] = e.clone(); vfs::ofd::get(e.ofd); }
            // A listener: one more reference on its bound address, as
            // `handle_dup` takes. BOUND_PATHS is a leaf (nothing is locked
            // under it), so taking it here, under both locks, is safe.
            SockState::UnixListening { bound_idx } => {
                bound_ref_inc(bound_idx);
                tbls[child_pos].socks[i] = e.clone();
                vfs::ofd::get(e.ofd);
            }
            // An inet socket: the child is one more holder of the description,
            // which is the socket's refcount (see "Shared inet sockets").
            ref s if is_inet_state(s) && inet_shareable(&e) => {
                tbls[child_pos].socks[i] = e.clone();
                vfs::ofd::get(e.ofd);
            }
            _ => {} // no object, or an inet socket with no description
        }
    }
    // Socket aliases are inherited: the entries copied above keep `hidden`.
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
    let mut slot = 0usize;
    loop {
        let is_cloexec = {
            let tbls = SOCK_TABLES.read();
            match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) {
                Some(t) if slot < t.socks.len() => t.socks[slot].in_use && t.socks[slot].cloexec,
                _ => return ok_reply(),
            }
        };
        slot += 1;
        let slot = slot - 1;
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
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
    tbl.socks[slot].nonblock = flags & 0x800 != 0;
    ok_reply()
}

/// fcntl(F_GETFL) for sockets: report O_NONBLOCK.
/// ioctl(FIONREAD) (`outq == false`) / ioctl(TIOCOUTQ, alias SIOCOUTQ) on a
/// socket: bytes queued for the next read, or written but not yet consumed
/// by the peer. Linux answers both for AF_UNIX and TCP sockets; FIONREAD on
/// a UDP socket is the size of the next datagram. A listener or an unbound
/// socket has nothing queued (0).
///
/// Firefox's in-process Wayland proxy (widget/gtk/wayland-proxy) sizes every
/// relay read with FIONREAD and treats a failing ioctl as a dead connection.
/// Socket fds used to fall through to the VFS, which knows only its own fds
/// and answered EBADF, so the proxy dropped GTK's connection on the first
/// message ("ProxiedConnection::TransferOrQueue() broken source socket: Bad
/// file descriptor") and Firefox exited with "we don't have any display".
fn handle_queue_len(pid: u32, sockfd: usize, outq: bool) -> Message {
    let slot = match fd_to_slot(sockfd) { Some(s) => s, None => return err_reply(-9) };
    let tbls = SOCK_TABLES.read();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) { Some(t) => t, None => return err_reply(-9) };
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
    let state = tbl.socks[slot].state;
    let sock_type = tbl.socks[slot].sock_type;
    let ofd = tbl.socks[slot].ofd;
    let sock_domain = tbl.socks[slot].domain as usize;
    drop(tbls);
    let n = match state {
        // A wildcard UDP server: the datagram a read would return next.
        SockState::InetConnected { socket_handle, sid, .. }
            if sock_type == SOCK_DGRAM as u8 && udpany::of(ofd).is_some() =>
        {
            if outq { 0 } else { udp_any_recv(ofd, (sid, socket_handle), 0, true, false).map(|(_, full, _)| full).unwrap_or(0) }
        }
        SockState::UnixConnected { conn_idx, is_a } => {
            let conns = UNIX_CONNS.lock();
            let conn = &conns[conn_idx];
            // End A reads ring_ba and writes ring_ab; end B the opposite.
            match (is_a, outq) {
                (true, false) | (false, true) => conn.ring_ba.count,
                (true, true) | (false, false) => conn.ring_ab.count,
            }
        }
        SockState::UnixPendingAccept { conn_idx, .. } => {
            // The connector is end A until the accept.
            let conns = UNIX_CONNS.lock();
            let conn = &conns[conn_idx];
            if outq { conn.ring_ab.count } else { conn.ring_ba.count }
        }
        SockState::InetConnected { socket_handle, sid, .. } => {
            let mut stack = stack_for(sid);
            match *stack {
                Some(ref mut s) if sock_type == SOCK_STREAM as u8 => {
                    let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    if outq { socket.send_queue() } else { socket.recv_queue() }
                }
                // The next datagram's length.
                Some(ref mut s) => {
                    if outq { 0 } else { udp_next_locked(s, sid, socket_handle, 0, true, false, false).map(|(_, n, _)| n).unwrap_or(0) }
                }
                None => 0,
            }
        }
        // The next datagram's length, as for UDP. smoltcp cannot peek one, so
        // this parks it in `icmp_peeked`, where the next read finds it.
        SockState::IcmpBound { socket_handle, sid } if !outq => match stack_for(sid).as_mut() {
            Some(s) => icmp_recv_filtered(s, socket_handle, 0, true, sock_type == SOCK_RAW as u8,
                                          sock_domain == AF_INET6, &sock_opts(ofd).icmp6f).map(|(_, full, _)| full).unwrap_or(0),
            None => 0,
        },
        _ => 0,
    };
    val_reply(n as u64)
}

fn handle_getfl(pid: u32, sockfd: usize) -> Message {
    let slot = match fd_to_slot(sockfd) { Some(s) => s, None => return err_reply(-9) };
    let tbls = SOCK_TABLES.read();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) { Some(t) => t, None => return err_reply(-9) };
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
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
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
    tbl.socks[slot].cloexec = arg & 1 != 0; // FD_CLOEXEC
    ok_reply()
}

/// fcntl(F_GETFD) for sockets: report FD_CLOEXEC.
fn handle_getfd(pid: u32, sockfd: usize) -> Message {
    let slot = match fd_to_slot(sockfd) { Some(s) => s, None => return err_reply(-9) };
    let cloexec = {
        let tbls = SOCK_TABLES.read();
        match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) {
            Some(t) if slot < t.socks.len() && t.socks[slot].in_use => Some(t.socks[slot].cloexec),
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
fn handle_dup(key: u32, sockfd: usize, cloexec: bool, hidden: bool) -> Message {
    let slot = match fd_to_slot(sockfd) { Some(s) => s, None => return err_reply(-9) };
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match find_tbl_key(key, &mut *tbls) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
    let entry = tbl.socks[slot].clone();
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
        let placed = match find_tbl_key(key, &mut *tbls) {
            Some(t) => match t.alloc() {
                Some(s) => { let mut e = entry.clone(); e.cloexec = cloexec; t.socks[s] = e; t.set_hidden(s, hidden); Some(s) }
                None => None,
            },
            None => None,
        };
        drop(tbls);
        if placed.is_some() { vfs::ofd::get(entry.ofd); }
        return match placed {
            Some(s) => val_reply((s + SOCK_FD_BASE) as u64),
            // Nothing was installed, so hand the reference straight back
            // rather than leaking the address for the life of the system.
            None => { free_bound_idx(bound_idx); err_reply(-24) } // EMFILE
        };
    }
    // An inet socket: the dup is one more holder of the description, which is
    // the socket's refcount (see "Shared inet sockets"); `vfs::ofd::get` below
    // takes it in this same SOCK_TABLES hold.
    else if is_inet_state(&entry.state) {
        if !inet_shareable(&entry) { return err_reply(-23); } // ENFILE: no description
    }
    else if !matches!(entry.state, SockState::Unbound { .. }) {
        return err_reply(-22); // EINVAL — dup of this socket kind unsupported
    }
    let mut new_entry = entry.clone();
    new_entry.cloexec = cloexec;
    tbl.socks[new_slot] = new_entry;
    tbl.set_hidden(new_slot, hidden);
    vfs::ofd::get(entry.ofd); // leaf lock: fine under SOCK_TABLES
    val_reply((new_slot + SOCK_FD_BASE) as u64)
}

/// Whether `fd` is a socket slot that backs a VFS-range alias (see
/// `SockEntry::hidden`). The kernel refuses a user `close()` of one.
pub fn sock_is_hidden(pid: u32, fd: usize) -> bool {
    let pid = sched::tgid_of(pid);
    let Some(slot) = fd_to_slot(fd) else { return false };
    let tbls = SOCK_TABLES.read();
    tbls.iter().find(|t| t.in_use && t.pid == tk(pid))
        .map_or(false, |t| slot < t.socks.len() && t.socks[slot].in_use && t.is_hidden(slot))
}

/// Whether `fd` is an open socket of `pid` (used to drop VFS aliases whose
/// socket the close-on-exec sweep retired).
/// Descriptor numbers of every open socket of `pid`'s process.
pub fn open_sock_fds(pid: u32) -> alloc::vec::Vec<usize> {
    let pid = sched::tgid_of(pid);
    let tbls = SOCK_TABLES.read();
    match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) {
        Some(t) => t.socks.iter().enumerate().filter(|(_, e)| e.in_use).map(|(i, _)| SOCK_FD_BASE + i).collect(),
        None => alloc::vec::Vec::new(),
    }
}

pub fn sock_is_open(pid: u32, fd: usize) -> bool {
    let pid = sched::tgid_of(pid);
    let Some(slot) = fd_to_slot(fd) else { return false };
    let tbls = SOCK_TABLES.read();
    tbls.iter().find(|t| t.in_use && t.pid == tk(pid))
        .map_or(false, |t| slot < t.socks.len() && t.socks[slot].in_use)
}

/// Drop one reference to end `is_a` of connection `conn_idx` (a closed fd, or
/// an in-flight SCM_RIGHTS copy that was never received). Only the last alias
/// of an end actually closes it (dup'd fds share it — see refs_a/refs_b).
/// Closing one end marks it closed so the peer observes EOF/EPIPE; the
/// connection object itself lives until both ends are gone.
/// Caller must hold neither SOCK_TABLES nor UNIX_CONNS.
fn unix_end_release(conn_idx: usize, is_a: bool) {
    let mut conns = UNIX_CONNS.lock();
    if conn_idx >= conns.len() { return; }
    let c = &mut conns[conn_idx];
    if !c.in_use { return; }
    let (end_closed, orphans) = c.end_put(is_a);
    drop(conns);
    for x in orphans { xfer_drop(x); }
    // Peer sees POLLHUP/POLLIN (EOF) once this end really closed.
    // Keyed: every connected end's probe names this key (`unix_end_keys`),
    // so a bit-only wake here only added pollers that collide on the bit.
    if end_closed { sched::wake_poll_key(sched::poll_key(sched::poll_class::UNIX, conn_idx as u32)); }
    // A reference just went away, so a cycle of in-flight ends may now be
    // unreachable.
    unix_gc();
}

/// close(2) on a socket fd: the object's own teardown (`close_entry`), then
/// the fd's reference on its open file description — the last one releases
/// the description's epoll registrations (`vfs::ofd`), and for an inet socket
/// its smoltcp handles (see "Shared inet sockets").
fn handle_close(pid: u32, sockfd: usize) -> Message {
    let (r, removed) = close_entry(pid, sockfd);
    match removed {
        Some(Closed::Plain(ofd)) => vfs::ofd::put(ofd),
        Some(Closed::Inet(p)) => inet_put_finish(p, true),
        None => {}
    }
    r
}

/// What `close_entry` leaves its caller: the description reference of an
/// ordinary entry to put, or an inet socket's put, already counted under
/// SOCK_TABLES, to finish.
enum Closed { Plain(u32), Inet(InetPut) }

/// Remove `sockfd` from `pid`'s table and drop this holder's reference on
/// the object (None: EBADF).
fn close_entry(pid: u32, sockfd: usize) -> (Message, Option<Closed>) {
    if sockfd < SOCK_FD_BASE { return (err_reply(-9), None); }
    let slot = sockfd - SOCK_FD_BASE;
    let mut tbls = SOCK_TABLES.lock();
    let tbl = match tbls.iter_mut().find(|t| t.in_use && t.pid == tk(pid)) {
        Some(t) => t,
        None    => return (err_reply(-9), None),
    };
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return (err_reply(-9), None); }
    let entry = tbl.socks[slot].clone();
    let state = entry.state;
    // There is no port to release here, which is why `bound_port` is not read:
    // `alloc_ephemeral_port` derives "free" from the live tables (any socket
    // with `in_use && bound_port == p`), and every arm below stores
    // `SockEntry::empty()`, which zeroes `bound_port`. Clearing the slot *is*
    // the release; there is no separate pool to hand the port back to.
    match state {
        SockState::UnixConnected { conn_idx, is_a } => {
            drop(tbls);
            unix_end_release(conn_idx, is_a);
            let mut tbls2 = SOCK_TABLES.lock();
            if let Some(t2) = tbls2.iter_mut().find(|t| t.in_use && t.pid == tk(pid)) {
                t2.socks[slot] = SockEntry::empty();
            }
            drop(tbls2);
        }
        // Inet sockets: this holder's copy goes; the handles go with the
        // description's last reference (finished in `handle_close`).
        _ if inet_family(&entry) => {
            tbl.socks[slot] = SockEntry::empty();
            let p = inet_put(&mut *tbls, &entry);
            drop(tbls);
            return (ok_reply(), Some(Closed::Inet(p)));
        }
        SockState::UnixListening { bound_idx } => {
            tbl.socks[slot] = SockEntry::empty();
            drop(tbls);
            // Reclaim the address (its VFS node, if any, lingers per Linux).
            free_bound_idx(bound_idx);
            // A listener whose other references are all in flight may now
            // be part of a cycle through its own backlog.
            unix_gc();
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
                if conn_idx < conns.len() && conns[conn_idx].in_use {
                    let c = &mut conns[conn_idx];
                    c.refs_a = c.refs_a.saturating_sub(1);
                    refs = c.refs_a;
                    if c.refs_a == 0 && c.pending_sid != 0 {
                        // Still on the listener's backlog: Linux keeps the
                        // embryo there, so accept() returns it and the
                        // acceptor reads what was sent, then EOF. Only end A
                        // goes; the connection lives on as a closed peer.
                        c.closed_a = true;
                        orphans = c.take_rx_fds(true);
                        c.seq = c.seq.wrapping_add(1);
                    } else if c.refs_a == 0 {
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
            if died { sched::wake_poll_tagged(sched::poll_tag(sched::poll_class::UNIX, conn_idx as u32)); }
        }
        _ => { tbl.socks[slot] = SockEntry::empty(); }
    }
    (ok_reply(), Some(Closed::Plain(entry.ofd)))
}

/// Exact key of "end `is_a` of `conn` became readable" (`poll_class::UNIX_RD`).
/// The data producers wake it with `sched::wake_poll_key` and `unix_end_keys`
/// reports it to pollers: both MUST come from here (see sched::PollSet).
fn unix_rd_key(conn_idx: usize, is_a: bool) -> u64 {
    sched::poll_key(sched::poll_class::UNIX_RD, (conn_idx as u32) * 2 + (!is_a) as u32)
}
/// Exact key of "end `is_a` of `conn` gained write space" (`poll_class::UNIX_WR`).
fn unix_wr_key(conn_idx: usize, is_a: bool) -> u64 {
    sched::poll_key(sched::poll_class::UNIX_WR, (conn_idx as u32) * 2 + (!is_a) as u32)
}
/// What a poller on end `is_a` of a connected socket must be woken by for
/// `requested` (POLLIN/POLLOUT bits; 0 = both): the connection's state tag
/// (close, shutdown, death, accept) plus the data-edge tag of each direction
/// it waits on.
fn unix_end_keys(conn_idx: usize, is_a: bool, requested: u32) -> sched::PollTag {
    let want_in  = requested == 0 || requested & POLLIN as u32 != 0;
    let want_out = requested == 0 || requested & POLLOUT as u32 != 0;
    let mut keys = [0u64; 3];
    let mut n = 0;
    keys[n] = sched::poll_key(sched::poll_class::UNIX, conn_idx as u32); n += 1;
    if want_in  { keys[n] = unix_rd_key(conn_idx, is_a); n += 1; }
    if want_out { keys[n] = unix_wr_key(conn_idx, is_a); n += 1; }
    sched::PollTag::keyed(&keys[..n], 0)
}

/// `want_ofd` != 0: answer only if `fd` still names that open file
/// description, else ESTALE (an epoll registration whose fd number was closed
/// or reused; see `vfs::ofd`).
fn handle_poll(pid: u32, fd: usize, requested: u32, want_ofd: u32) -> Message {
    let slot = match fd_to_slot(fd) { Some(s) => s, None => return err_reply(-9) };
    let tbls = SOCK_TABLES.read();
    let tbl = match tbls.iter().find(|t| t.in_use && t.pid == tk(pid)) {
        Some(t) => t, None => return err_reply(-9),
    };
    if slot >= tbl.socks.len() || !tbl.socks[slot].in_use { return err_reply(-9); }
    if want_ofd != 0 && tbl.socks[slot].ofd != want_ofd { return err_reply(-116); } // ESTALE
    let state = tbl.socks[slot].state;
    let sock_type = tbl.socks[slot].sock_type;
    let connecting = tbl.socks[slot].connecting;
    let ofd = tbl.socks[slot].ofd;
    // The tag of every state but the AF_UNIX objects comes from `sock_tag`,
    // the helper `fd_wake_tag` and the state-change wake share.
    let ent = tbl.socks[slot].clone();

    let (revents, seq, tag): (u64, Option<u64>, Option<sched::PollTag>) = match state {
        SockState::UnixConnected { conn_idx, is_a } => {
            drop(tbls);
            let conns = UNIX_CONNS.lock();
            let conn = &conns[conn_idx];
            let readable   = if is_a { conn.ring_ba.count } else { conn.ring_ab.count };
            let write_free = if is_a { conn.ring_ab.space() } else { conn.ring_ba.space() };
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
            if is_a && conn.reset_a { ev |= POLLERR; }
            // Connected sockets carry the edge-seq so EPOLLET works.
            (ev, Some(conn.seq), Some(unix_end_keys(conn_idx, is_a, requested)))
        }
        SockState::UnixPendingAccept { conn_idx, .. } => {
            // Connector (end A) awaiting accept: established + writable now (Linux),
            // so a client that waits for POLLOUT before its first write proceeds.
            // Readable once the peer accepts and replies (ring_ba) or the conn dies.
            drop(tbls);
            let conns = UNIX_CONNS.lock();
            let conn = &conns[conn_idx];
            let readable   = conn.ring_ba.count;
            let write_free = conn.ring_ab.space();
            let mut ev = 0;
            // Pre-accept the connector is end A, so its own half-close applies
            // here the same way it does once the connection is established.
            if readable > 0 || !conn.in_use || conn.rd_shut(true) { ev |= POLLIN; }
            if conn.in_use && !conn.closed_b && !conn.wr_shut(true) && write_free > 0 { ev |= POLLOUT; }
            if !conn.in_use || conn.closed_b { ev |= POLLHUP; }
            // End A's full tag, not just the connection's: accept() turns this
            // socket into UnixConnected { is_a: true } and then wakes the
            // connection tag, but a poller that probed before the accept and
            // parks after that wake keeps this mask, and the acceptor's data
            // then wakes only UNIX_RD(conn, A).
            (ev, Some(conn.seq), Some(unix_end_keys(conn_idx, true, requested)))
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
            let pending = listen_sock_id != 0
                && unix_backlog_head(&UNIX_CONNS.lock(), listen_sock_id).is_some();
            // A connect() to this listener wakes with poll_tag(UNIX, sock_id)
            // (see handle_connect), keyed on the SAME sock_id this arm matches.
            let tag = if listen_sock_id != 0 {
                // Foreign: connect() wakes it bit-only (handle_connect).
                Some(sched::PollTag::foreign(sched::poll_tag(sched::poll_class::UNIX, listen_sock_id as u32)))
            } else { None };
            (if pending { POLLIN } else { 0 }, None, tag)
        }
        // A wildcard UDP server: readable when any of its stacks holds a
        // datagram; its edge-seq is the sum of its sockets' (any one moving
        // moves the sum). Its tag names the key of every per-stack socket:
        // the daemon wakes each mirror's own key when its readiness moves
        // (`readiness_moved`), so a datagram on any stack reaches the poller
        // without a broadcast mask (which woke Firefox's QUIC socket thread
        // on every AF_UNIX message in the system).
        SockState::InetConnected { socket_handle, sid, .. }
            if sock_type == SOCK_DGRAM as u8 && udpany::of(ofd).is_some() =>
        {
            drop(tbls);
            let shut = sock_opts(ofd).shut;
            let hs = udpany::of(ofd).unwrap_or([None; MAX_STACKS]);
            let (mut readable, mut writable, mut seq) = (false, false, 0u64);
            let socks = udpany::order(&hs, (sid, socket_handle));
            for &(sd, h) in socks.iter() {
                let mut stack = stack_for(sd);
                let Some(s) = stack.as_mut() else { continue };
                seq = seq.wrapping_add(s.inet_seq.get(&h).copied().unwrap_or(0));
                let u = s.socket_set.get_mut::<udp::Socket>(h);
                readable |= u.can_recv();
                if sd == sid { writable = u.can_send(); }
            }
            let mut ev = (if readable { POLLIN } else { 0 }) | (if writable { POLLOUT } else { 0 });
            ev |= udp_shut_events(shut);
            (ev, Some(seq), sock_tag(&ent))
        }
        SockState::InetConnected { socket_handle, sid, .. } => {
            drop(tbls);
            let shut = if sock_type == SOCK_DGRAM as u8 { sock_opts(ofd).shut } else { 0 };
            let mut stack = stack_for(sid);
            // TCP and UDP carry the edge-seq (see NetStack::inet_seq) so
            // EPOLLET works: tokio registers every TcpStream and UdpSocket
            // EPOLLIN|EPOLLOUT|EPOLLET, and a level-reported, permanently
            // writable socket made its reactor spin (sshd at 100% CPU per
            // open session).
            let mut seq = None;
            let ev = if let Some(ref mut s) = *stack {
                if sock_type == SOCK_STREAM as u8 {
                    seq = Some(s.inet_seq.get(&socket_handle).copied().unwrap_or(0));
                    let socket = s.socket_set.get_mut::<tcp::Socket>(socket_handle);
                    let mut ev = 0;
                    if socket.can_recv() || tcp_rx_eof(socket) { ev |= POLLIN; }
                    // Peer FIN (read side retired): Linux tcp_poll adds
                    // POLLRDHUP; with the send side retired as well, POLLHUP.
                    // State-based: smoltcp's may_recv() stays true in CLOSE-WAIT
                    // while unread data remains, hiding the FIN.
                    if tcp_peer_fin(socket) {
                        ev |= POLLRDHUP;
                        if !socket.may_send() { ev |= POLLHUP; }
                    }
                    if socket.can_send() && socket.is_active() { ev |= POLLOUT; }
                    if !socket.is_active() {
                        // A dead connection is writable as on Linux, so a
                        // writer wakes and learns the error from send(); a
                        // connect that never completed also raises POLLERR,
                        // which is what makes a non-blocking connect's caller
                        // (NSPR's PR_ConnectContinue) read SO_ERROR.
                        ev |= POLLHUP | POLLOUT;
                        if connecting { ev |= POLLERR; }
                    }
                    // Settle a connect that has completed, so a later reset
                    // is not mistaken for a refusal.
                    if connecting && socket.state() == tcp::State::Established {
                        drop(stack);
                        tcp_connect_status(pid, fd);
                        return net_poll_reply(ev, None,
                            sock_tag(&ent));
                    }
                    ev
                } else {
                    seq = Some(s.inet_seq.get(&socket_handle).copied().unwrap_or(0));
                    let socket = s.socket_set.get_mut::<udp::Socket>(socket_handle);
                    let mut ev = 0;
                    if socket.can_recv() { ev |= POLLIN; }
                    // A pending ICMP error (datagram_poll: sk_err).
                    if socket.error() != 0 || socket.errq_len() != 0 { ev |= POLLERR; }
                    // Writable while the send queue has room: a full one
                    // (EAGAIN) becomes writable again when the daemon drains
                    // it, which is the edge an EPOLLET writer waits for.
                    if socket.can_send() { ev |= POLLOUT; }
                    ev | udp_shut_events(shut)
                }
            } else {
                0
            };
            // The daemon wakes this tag whenever the socket's readiness moves
            // during one of its passes (see `net_daemon`).
            drop(stack);
            (ev, seq, sock_tag(&ent))
        }
        SockState::InetListening { lid, .. } => {
            drop(tbls);
            // Readable as soon as any stack holds a completed connection for
            // it (ESTABLISHED, or CLOSE-WAIT: the client already sent its
            // request and FIN — accept() takes both). One stack lock at a time.
            // The edge-seq is the sum of the listener's per-stack ones, each
            // bumped by a connection becoming ready (see `listen::Listen::seq`):
            // an EPOLLET acceptor (tokio's TcpListener) is woken once per new
            // connection, not on every epoll_wait while one waits.
            let (mut ready, mut seq) = (false, 0u64);
            for sid in listen::all_sids() {
                let (r, q) = listen::ready_seq(sid, lid);
                ready |= r;
                seq = seq.wrapping_add(q);
            }
            // The listener's own key (`listen::wake_key`): the daemon wakes
            // it in the same pass that moves the edge-seq above, and its
            // removal wakes it too (`listen::destroy_all`). With no tag here
            // every poll set holding a listener parked broadcast.
            (if ready { POLLIN } else { 0 }, Some(seq),
             sock_tag(&ent))
        }
        SockState::IcmpBound { socket_handle, sid } => {
            drop(tbls);
            let mut stack = stack_for(sid);
            let mut seq = None;
            let ev = if let Some(ref mut s) = *stack {
                seq = Some(s.inet_seq.get(&socket_handle).copied().unwrap_or(0));
                let mut ev = 0;
                if s.socket_set.get_mut::<icmp::Socket>(socket_handle).can_send() { ev |= POLLOUT; }
                let readable = if ent.domain as usize == AF_INET6 && sock_opts(ent.ofd).icmp6f != [0u32; 8] {
                    icmp_recv_filtered(s, socket_handle, 0, true, false, true, &sock_opts(ent.ofd).icmp6f).is_ok()
                } else { icmp_readable(s, socket_handle) };
                if readable { ev |= POLLIN; }
                ev
            } else {
                0
            };
            // The daemon wakes it like any UDP socket (`readiness_moved`
            // covers ICMP); a MSG_PEEK recv changes no readiness.
            drop(stack);
            (ev, seq, sock_tag(&ent))
        }
        // A UDP or ping socket with no smoltcp socket yet can always be
        // written to: its first sendto() creates one. Edge-seq 0: one
        // EPOLLOUT edge, and nothing new until a socket exists to move.
        // Tag: the description's SOCKSTATE key (`sock_keys`), woken by the
        // bind/connect/first sendto that gives it a socket.
        SockState::Unbound { domain, sock_type: ty }
            if is_inet(domain) && ty == SOCK_DGRAM as u8 =>
        {
            drop(tbls);
            (POLLOUT, Some(0), sock_tag(&ent))
        }
        SockState::IcmpUnbound => { drop(tbls); (POLLOUT, Some(0), sock_tag(&ent)) }
        SockState::Unbound { domain, .. } if domain == AF_NETLINK as u8 => {
            drop(tbls);
            let (readable, gen) = nlsock::poll_state(ofd);
            (POLLOUT | if readable { POLLIN } else { 0 }, Some(gen), sock_tag(&ent))
        }
        // Not bound yet, TCP bound but not listening, AF_UNIX not bound or
        // connected: nothing to report until bind/listen/connect, which wake
        // the SOCKSTATE key.
        _ => { drop(tbls); (0, None, sock_tag(&ent)) }
    };
    net_poll_reply(revents, seq, tag)
}

/// True when the process held an inet socket (its teardown gives a stack
/// something to do: FIN/RST, a freed port), i.e. the daemon wants a pass.
fn handle_close_all(pid: u32) -> bool {
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
        // Inet sockets: this process's reference on each, counted under
        // SOCK_TABLES (`inet_put`), and the handles of those it held last.
        let mut inet_puts: alloc::vec::Vec<InetPut> = alloc::vec::Vec::new();
        let mut inet_held: alloc::vec::Vec<SockEntry> = alloc::vec::Vec::new();
        let mut bound_to_free: alloc::vec::Vec<usize> = alloc::vec::Vec::new();

        let mut ofds: alloc::vec::Vec<u32> = alloc::vec::Vec::new();
        for s in tbl.socks.iter() {
            if !s.in_use { continue; }
            if inet_family(s) { inet_held.push(s.clone()); continue; }
            if s.ofd != 0 { ofds.push(s.ofd); }
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
                _ => {}
            }
        }
        // Each inet entry leaves the table and puts its reference in one
        // SOCK_TABLES hold, so a last put sees what the other holders
        // published (see "Shared inet sockets").
        for e in tbl.socks.iter_mut() {
            if e.in_use && inet_family(e) { *e = SockEntry::empty(); }
        }
        for e in inet_held.iter() { inet_puts.push(inet_put(&mut *tbls, e)); }
        let had_inet = !inet_held.is_empty();
        drop(tbls);
        for d in ofds { vfs::ofd::put(d); }
        // Process exit parks no TIME_WAIT port (as before).
        for p in inet_puts { inet_put_finish(p, false); }

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
            if ci < conns.len() && conns[ci].in_use {
                let (closed, mut lifted) = conns[ci].end_put(is_a);
                peer_hup |= closed;
                orphans.append(&mut lifted);
            }
        }
        // Pending-accept half-open connections: release this holder's
        // reference, and only tear the connection down once the last one is
        // gone. Force-freeing here undid the fork-inheritance fix above — a
        // forked child that exits (an exec failure, say) would have destroyed
        // the connector its parent still holds, and vice versa.
        for ci in unix_pending_close {
            if ci < conns.len() && conns[ci].in_use {
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

        let mut tbls = SOCK_TABLES.lock();
        if let Some(tbl) = tbls.iter_mut().find(|t| t.in_use && t.pid == pid) {
            tbl.reset();
        }
        drop(tbls);
        // Peers of the torn-down connections see POLLHUP/EOF (K2).
        if peer_hup { sched::wake_poll(); }
        // The exiting process held roots; what only they kept reachable is
        // garbage now.
        unix_gc();
        return had_inet;
    }
    false
}

fn net_val(m: &Message) -> isize {
    let bytes: [u8; 8] = m.data[0..8].try_into().unwrap_or([0u8; 8]);
    i64::from_le_bytes(bytes) as isize
}

/// Diagnostic, appended to the Ctrl-T task dump (`sched::dump_tasks`): every
/// process socket table entry, every live AF_UNIX connection with its per-end
/// refcounts, closed/shutdown flags, peer credentials and ring fill, and every
/// bound AF_UNIX address. IRQ context: try_lock only, prints on the raw UART.
pub fn dump_sockets() {
    extern "C" { fn arch_serial_putc(c: u8); fn print_number(n: u32); }
    fn ps(s: &str) { for &b in s.as_bytes() { unsafe { arch_serial_putc(b); } } }
    fn pn(n: u64) { unsafe { print_number(n as u32) } }
    fn pb(b: bool) { ps(if b { "1" } else { "0" }) }
    {
        let tbls = match SOCK_TABLES.try_lock() {
            Some(t) => t,
            None => { ps("[SOCK] tables busy\n"); return; }
        };
        for t in tbls.iter() {
            if !t.in_use { continue; }
            ps("[SOCK] pid="); pn(t.pid as u64); ps(":");
            for (i, e) in t.socks.iter().enumerate() {
                if !e.in_use { continue; }
                ps(" fd"); pn((i + SOCK_FD_BASE) as u64);
                if e.cloexec { ps("x"); }
                if e.nonblock { ps("n"); }
                ps("=");
                match e.state {
                    SockState::None => ps("none"),
                    SockState::Unbound { .. } => ps("unbound"),
                    SockState::UnixListening { bound_idx } => { ps("listen@"); pn(bound_idx as u64); }
                    SockState::UnixConnected { conn_idx, is_a } => { ps("conn"); pn(conn_idx as u64); ps(if is_a { "a" } else { "b" }); }
                    SockState::UnixPendingAccept { conn_idx, .. } => { ps("pend"); pn(conn_idx as u64); }
                    SockState::InetBound { .. } => ps("inetbound"),
                    SockState::InetListening { lid, .. } => {
                        let (syn, est) = listen::backlog(lid);
                        ps("inetlisten#"); pn(lid as u64); ps("("); pn(syn as u64); ps("+"); pn(est as u64); ps(")");
                    }
                    SockState::InetConnected { .. } => ps("inetconn"),
                    SockState::IcmpUnbound | SockState::IcmpBound { .. } => ps("icmp"),
                }
            }
            ps("\n");
        }
    }
    {
        let conns = match UNIX_CONNS.try_lock() {
            Some(c) => c,
            None => { ps("[SOCK] conns busy\n"); return; }
        };
        for (i, c) in conns.iter().enumerate() {
            if !c.in_use { continue; }
            ps("[CONN] "); pn(i as u64);
            ps(" a{pid="); pn(c.cred_a.pid as u64); ps(" uid="); pn(c.cred_a.uid as u64);
            ps(" refs="); pn(c.refs_a as u64); ps(" closed="); pb(c.closed_a);
            ps(" shut="); pb(c.shut_rd_a); pb(c.shut_wr_a); ps("}");
            ps(" b{pid="); pn(c.cred_b.pid as u64); ps(" uid="); pn(c.cred_b.uid as u64);
            ps(" refs="); pn(c.refs_b as u64); ps(" closed="); pb(c.closed_b);
            ps(" shut="); pb(c.shut_rd_b); pb(c.shut_wr_b); ps("}");
            ps(" ab="); pn(c.ring_ab.count as u64); ps("/"); pn(c.ring_ab.wtotal);
            ps(" ba="); pn(c.ring_ba.count as u64); ps("/"); pn(c.ring_ba.wtotal);
            ps(" fdq="); pn(c.fdq_ab.len() as u64); ps("/"); pn(c.fdq_ba.len() as u64);
            ps(" seq="); pn(c.seq);
            ps("\n");
        }
    }
    {
        let bound = match BOUND_PATHS.try_lock() {
            Some(b) => b,
            None => { ps("[SOCK] bound busy\n"); return; }
        };
        for (i, b) in bound.iter().enumerate() {
            if !b.in_use { continue; }
            ps("[BOUND] "); pn(i as u64); ps(" refs="); pn(b.refs as u64);
            ps(" id="); pn(b.sock_id); ps(" ");
            if b.is_abstract { ps("@"); }
            let n = b.path_len.min(PATH_MAX);
            for &ch in &b.path[..n] { if ch >= 0x20 && ch < 0x7f { unsafe { arch_serial_putc(ch); } } }
            ps("\n");
        }
    }
}
