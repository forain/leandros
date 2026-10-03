//! scmtest — spike S2 of the Wayland/COSMIC plan (see repo-root
//! `wayland_cosmic_plan.md`, "S2: SCM_RIGHTS + shared-memfd two-process pixel
//! test"): the acceptance test for kernel work item K1 (blocker #1,
//! SCM_RIGHTS fd-passing) and blocker #2 (MAP_SHARED-degrades-to-private).
//!
//! **This test is EXPECTED TO FAIL today.** It is the spec for upcoming
//! kernel/net-server work, not a regression test for something already
//! built. Every Wayland buffer handoff (shm pools, keymaps, dmabufs) and
//! D-Bus itself depend on real SCM_RIGHTS + a real shared-VMO mmap, neither
//! of which exist yet:
//!   - `servers/net/src/lib.rs`'s `handle_sendmsg`/`handle_recvmsg` only ever
//!     walk `msg_iov`/`msg_iovlen`; `msg_control`/`msg_controllen` are never
//!     read or written, so `SCM_RIGHTS` cmsgs are silently dropped end to end.
//!   - `kernel/src/syscall.rs`'s `sys_mmap` file-backed path has a comment
//!     admitting exactly this: "MAP_SHARED is not supported (no VMO page
//!     cache yet); silently treat as MAP_PRIVATE — data is copied on map,
//!     modifications are local only." That breaks pixel-buffer sharing even
//!     within a single process's two mappings of the same fd, let alone
//!     across processes.
//!   - `servers/vfs/src/lib.rs`'s `handle_fcntl` has a catch-all
//!     `_ => ok_reply()` for any command it doesn't recognise, which silently
//!     "succeeds" for `F_ADD_SEALS`/`F_GET_SEALS` without storing or
//!     enforcing anything.
//!
//! Each subtest below is written to run to completion and print a clear
//! PASS/FAIL line no matter which of the above is missing — extraction of a
//! cmsg-carried fd is gated on the cmsg actually being found, so a dropped
//! SCM_RIGHTS never causes an attempt to use a bogus/aliased fd number.
//! Extra `printf` diagnostics before each verdict record the exact
//! return values/flags observed, so a serial-log capture shows precisely
//! which assumption broke.
//!
//! Follows vfstest's conventions: plain `leandros-libc` (no relibc/TLS
//! needed — every primitive here is a raw syscall), `fork()`/`wait4()` for
//! two-process tests, and syscalls not yet wrapped by leandros-libc
//! (sendmsg/recvmsg/socketpair/memfd_create/ftruncate) are made directly via
//! `syscall2`..`syscall6`, following the `raw_chroot`/`raw_setxattr` pattern
//! in `userland/vfstest/src/main.rs`. Syscall numbers are taken from
//! `kernel/src/syscall.rs`'s own per-arch `nr` tables (`SENDMSG`/`RECVMSG`/
//! `SOCKETPAIR`/`MEMFD_CREATE`/`FTRUNCATE`), which the kernel already
//! dispatches (just without cmsg/seal/MAP_SHARED semantics — see above).
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); `main` returns the number of failures as the exit code.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

extern crate leandros_libc;
use leandros_libc::*;
use leandros_libc::syscall::{nr, syscall1, syscall2, syscall3, syscall4, syscall6};

// Syscall numbers not yet wrapped by leandros-libc. Values match
// `kernel/src/syscall.rs`'s `mod nr` tables exactly (AArch64 first,
// x86_64 second, in the same order the kernel defines them).
#[cfg(target_arch = "aarch64")] const SYS_SENDMSG:      usize = 211;
#[cfg(target_arch = "x86_64")]  const SYS_SENDMSG:      usize = 46;
#[cfg(target_arch = "aarch64")] const SYS_RECVMSG:      usize = 212;
#[cfg(target_arch = "x86_64")]  const SYS_RECVMSG:      usize = 47;
#[cfg(target_arch = "aarch64")] const SYS_SOCKETPAIR:   usize = 199;
#[cfg(target_arch = "x86_64")]  const SYS_SOCKETPAIR:   usize = 53;
#[cfg(target_arch = "aarch64")] const SYS_MEMFD_CREATE: usize = 279;
#[cfg(target_arch = "x86_64")]  const SYS_MEMFD_CREATE: usize = 319;
#[cfg(target_arch = "aarch64")] const SYS_FTRUNCATE:    usize = 46;
#[cfg(target_arch = "x86_64")]  const SYS_FTRUNCATE:    usize = 77;
// AF_UNIX socket syscalls (K1-C) + newfstatat, for the socket-node tests.
#[cfg(target_arch = "aarch64")] const SYS_SOCKET:       usize = 198;
#[cfg(target_arch = "x86_64")]  const SYS_SOCKET:       usize = 41;
#[cfg(target_arch = "aarch64")] const SYS_BIND:         usize = 200;
#[cfg(target_arch = "x86_64")]  const SYS_BIND:         usize = 49;
#[cfg(target_arch = "aarch64")] const SYS_LISTEN:       usize = 201;
#[cfg(target_arch = "x86_64")]  const SYS_LISTEN:       usize = 50;
#[cfg(target_arch = "aarch64")] const SYS_ACCEPT:       usize = 202;
#[cfg(target_arch = "x86_64")]  const SYS_ACCEPT:       usize = 43;
#[cfg(target_arch = "aarch64")] const SYS_CONNECT:      usize = 203;
#[cfg(target_arch = "x86_64")]  const SYS_CONNECT:      usize = 42;
#[cfg(target_arch = "aarch64")] const SYS_NEWFSTATAT:   usize = 79;
#[cfg(target_arch = "x86_64")]  const SYS_NEWFSTATAT:   usize = 262;
// M7q TASK 1 (fork+exec+fd-inherit decider): execve + epoll. Numbers match
// kernel/src/syscall.rs's `mod nr`. AArch64 has no bare EPOLL_WAIT — the kernel
// routes EPOLL_PWAIT (nr 22) to the same 4-arg sys_epoll_wait, so we use that.
#[cfg(target_arch = "aarch64")] const SYS_EXECVE:        usize = 221;
#[cfg(target_arch = "x86_64")]  const SYS_EXECVE:        usize = 59;
#[cfg(target_arch = "aarch64")] const SYS_EPOLL_CREATE1: usize = 20;
#[cfg(target_arch = "x86_64")]  const SYS_EPOLL_CREATE1: usize = 291;
#[cfg(target_arch = "aarch64")] const SYS_EPOLL_CTL:     usize = 21;
#[cfg(target_arch = "x86_64")]  const SYS_EPOLL_CTL:     usize = 233;
#[cfg(target_arch = "aarch64")] const SYS_EPOLL_WAIT:    usize = 22;  // EPOLL_PWAIT
#[cfg(target_arch = "x86_64")]  const SYS_EPOLL_WAIT:    usize = 232;
// mincore(2): POSIX residency probe. Numbers match kernel/src/syscall.rs `mod nr`
// (aarch64 nr module at :279, x86_64 at :489).
#[cfg(target_arch = "aarch64")] const SYS_MINCORE:       usize = 232;
#[cfg(target_arch = "x86_64")]  const SYS_MINCORE:       usize = 27;
// getsockname(2): the only way to learn the port a bind-to-zero was handed.
// Numbers match kernel/src/syscall.rs `mod nr`.
#[cfg(target_arch = "aarch64")] const SYS_GETSOCKNAME:   usize = 204;
#[cfg(target_arch = "x86_64")]  const SYS_GETSOCKNAME:   usize = 51;
// setsockopt(2): SO_REUSEADDR is what lets a bind take a port still in
// TIME_WAIT. Numbers match kernel/src/syscall.rs `mod nr`.
#[cfg(target_arch = "aarch64")] const SYS_SETSOCKOPT:    usize = 208;
#[cfg(target_arch = "x86_64")]  const SYS_SETSOCKOPT:    usize = 54;
// shutdown(2): the one syscall tokio adds that the existing fork+exec+inherit
// decider omits — `OwnedWriteHalf::drop` half-closes the write direction of the
// half it is not keeping. Numbers match kernel/src/syscall.rs `mod nr`.
#[cfg(target_arch = "aarch64")] const SYS_SHUTDOWN:      usize = 210;
#[cfg(target_arch = "x86_64")]  const SYS_SHUTDOWN:      usize = 48;

// epoll_event wire layout must match the kernel's per-arch struct exactly
// (kernel/src/syscall.rs EPOLL_EVENT_SIZE/EPOLL_EVENT_DATA_OFF): x86_64 uses the
// packed 12-byte form (data at +4); aarch64 the natural 16-byte form (data at +8).
#[cfg(target_arch = "x86_64")]  const EPOLL_EVENT_SIZE:     usize = 12;
#[cfg(target_arch = "x86_64")]  const EPOLL_EVENT_DATA_OFF: usize = 4;
#[cfg(target_arch = "aarch64")] const EPOLL_EVENT_SIZE:     usize = 16;
#[cfg(target_arch = "aarch64")] const EPOLL_EVENT_DATA_OFF: usize = 8;
const EPOLLIN: u32 = 0x0001;
const EPOLLOUT: u32 = 0x0004;
/// POLLNVAL. kernel/src/syscall.rs reports it for any socket fd the net server
/// answers EBADF for, with no edge-seq attached; mio decodes it as an empty
/// readiness set, so an epoll waiter sleeps on it forever instead of erroring.
/// No live socket may ever report it.
const POLLNVAL: u32 = 0x0020;
const EPOLL_CTL_ADD: usize = 1;
const F_SETFD: i32 = 2;
// shutdown(2) `how`.
const SHUT_WR: i32 = 1;

// ── Wire-format structs (Linux/glibc ABI, 64-bit) ───────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
struct iovec {
    iov_base: *mut u8,
    iov_len: usize,
}

/// `repr(C)` gives this the standard 56-byte Linux layout: msg_iov/msg_iovlen
/// land at offsets 16/24, which is exactly what
/// `servers/net/src/lib.rs`'s `handle_sendmsg`/`handle_recvmsg` read via raw
/// pointer arithmetic — confirming this struct's shape matches what the
/// kernel already expects on the wire, even though it never looks past
/// `msg_iovlen` today.
#[repr(C)]
#[derive(Clone, Copy)]
struct msghdr {
    msg_name: *mut u8,
    msg_namelen: u32,
    msg_iov: *mut iovec,
    msg_iovlen: usize,
    msg_control: *mut u8,
    msg_controllen: usize,
    msg_flags: i32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct cmsghdr {
    cmsg_len: usize,
    cmsg_level: i32,
    cmsg_type: i32,
}

/// 8-byte-aligned control-message backing storage (`cmsghdr` requires 8-byte
/// alignment for its `size_t` field); a plain `[u8; 32]` local has no such
/// guarantee.
#[repr(C, align(8))]
struct CmsgBuf {
    b: [u8; 32],
}

const SOL_SOCKET: i32 = 1;
const SCM_RIGHTS: i32 = 1;
const MSG_CTRUNC: i32 = 0x08;
const MSG_DONTWAIT: i32 = 0x40;
const MSG_CMSG_CLOEXEC: i32 = 0x40000000;

const AF_UNIX: i32 = 1;
const AF_INET: i32 = 2;
const SOCK_STREAM: i32 = 1;
const SOCK_DGRAM: i32 = 2;
/// `unix_listen` answers EOPNOTSUPP — not EINVAL — for a socket type that
/// cannot accept, and it checks the type before the address.
const EOPNOTSUPP: i32 = 95;

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 0x01;

const F_GETFD: i32 = 1;
const FD_CLOEXEC: isize = 1;
// Linux fcntl seal commands/flags (not yet in leandros-libc's io.rs).
const F_ADD_SEALS: i32 = 1033;
const F_GET_SEALS: i32 = 1034;
const F_SEAL_SHRINK: i32 = 0x0002;

fn cmsg_align(len: usize) -> usize { (len + 7) & !7 }
fn cmsg_len(len: usize) -> usize { cmsg_align(core::mem::size_of::<cmsghdr>()) + len }
fn cmsg_space(len: usize) -> usize { cmsg_align(core::mem::size_of::<cmsghdr>()) + cmsg_align(len) }
fn cmsg_data_off() -> usize { cmsg_align(core::mem::size_of::<cmsghdr>()) }

// ── Raw syscall wrappers for the pieces leandros-libc doesn't have yet ─────

fn xret(r: isize) -> isize {
    if r < 0 { set_errno(-r as i32); -1 } else { r }
}

unsafe fn raw_socketpair(domain: i32, kind: i32, protocol: i32, sv: *mut i32) -> i32 {
    xret(syscall4(SYS_SOCKETPAIR, domain as usize, kind as usize, protocol as usize, sv as usize)) as i32
}
unsafe fn raw_sendmsg(fd: i32, msg: *const msghdr, flags: i32) -> isize {
    xret(syscall3(SYS_SENDMSG, fd as usize, msg as usize, flags as usize))
}
unsafe fn raw_recvmsg(fd: i32, msg: *mut msghdr, flags: i32) -> isize {
    xret(syscall3(SYS_RECVMSG, fd as usize, msg as usize, flags as usize))
}
unsafe fn raw_memfd_create(name: *const u8, flags: u32) -> i32 {
    xret(syscall2(SYS_MEMFD_CREATE, name as usize, flags as usize)) as i32
}
unsafe fn raw_ftruncate(fd: i32, len: i64) -> i32 {
    xret(syscall2(SYS_FTRUNCATE, fd as usize, len as usize)) as i32
}
unsafe fn raw_fcntl(fd: i32, cmd: i32, arg: usize) -> isize {
    xret(syscall3(nr::FCNTL, fd as usize, cmd as usize, arg))
}
unsafe fn raw_send(fd: i32, buf: *const u8, len: usize, flags: i32) -> isize {
    xret(syscall6(nr::SENDTO, fd as usize, buf as usize, len, flags as usize, 0, 0))
}
unsafe fn raw_recv(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize {
    xret(syscall6(nr::RECVFROM, fd as usize, buf as usize, len, flags as usize, 0, 0))
}

// ── AF_UNIX socket wrappers (K1-C) ──────────────────────────────────────────

/// `sockaddr_un`: sun_family(2) + sun_path(108). A pathname address ends at a
/// NUL and is passed with addrlen = 2 + strlen(path) + 1 (as musl does).
#[repr(C)]
struct sockaddr_un { sun_family: u16, sun_path: [u8; 108] }

impl sockaddr_un {
    /// Build from a NUL-terminated path (the NUL is not part of `name`).
    unsafe fn from_path(name: &[u8]) -> (sockaddr_un, usize) {
        let mut a = sockaddr_un { sun_family: AF_UNIX as u16, sun_path: [0u8; 108] };
        let n = name.len().min(107);
        a.sun_path[..n].copy_from_slice(&name[..n]);
        (a, 2 + n + 1)
    }

    /// Build a Linux abstract-namespace address: `sun_path[0]` is NUL and the
    /// name is the following `addrlen - 3` bytes, matched by bytes with no VFS
    /// node behind it. Nothing is left on disk, so an abstract test needs no
    /// unlink and cannot be poisoned by a dirty image. This is the form
    /// `wp_security_context_v1.create_listener` uses.
    unsafe fn from_abstract(name: &[u8]) -> (sockaddr_un, usize) {
        let mut a = sockaddr_un { sun_family: AF_UNIX as u16, sun_path: [0u8; 108] };
        let n = name.len().min(106);
        a.sun_path[1..1 + n].copy_from_slice(&name[..n]);
        (a, 2 + 1 + n)
    }
}

unsafe fn raw_socket(domain: i32, kind: i32, proto: i32) -> i32 {
    xret(syscall3(SYS_SOCKET, domain as usize, kind as usize, proto as usize)) as i32
}
unsafe fn raw_bind(fd: i32, addr: *const sockaddr_un, addrlen: usize) -> isize {
    xret(syscall3(SYS_BIND, fd as usize, addr as usize, addrlen))
}
unsafe fn raw_listen(fd: i32, backlog: i32) -> isize {
    xret(syscall2(SYS_LISTEN, fd as usize, backlog as usize))
}
unsafe fn raw_connect(fd: i32, addr: *const sockaddr_un, addrlen: usize) -> isize {
    xret(syscall3(SYS_CONNECT, fd as usize, addr as usize, addrlen))
}
/// accept() with a small bounded retry: accept is non-blocking at the syscall
/// level and returns EAGAIN until a connect is pending. In these single-process
/// tests the connect always precedes the accept, so one attempt normally
/// suffices; the retry only guards against scheduler jitter.
unsafe fn raw_accept(fd: i32) -> i32 {
    let mut tries = 0;
    loop {
        let r = syscall3(SYS_ACCEPT, fd as usize, 0, 0);
        if r != -11 { return xret(r) as i32; }
        tries += 1;
        if tries > 10000 { return xret(r) as i32; }
    }
}

// ── AF_INET socket wrappers ───────────────────────────────────

/// `sockaddr_in`, Linux ABI: sin_family(2) + sin_port(2, network order) +
/// sin_addr(4, network order) + 8 bytes of padding = 16 bytes, which is the
/// addrlen every call below passes.
#[repr(C)]
#[derive(Clone, Copy)]
struct sockaddr_in { sin_family: u16, sin_port: u16, sin_addr: [u8; 4], sin_zero: [u8; 8] }

impl sockaddr_in {
    /// `addr` in dotted-quad order, `port` in host order.
    fn new(addr: [u8; 4], port: u16) -> sockaddr_in {
        sockaddr_in { sin_family: AF_INET as u16, sin_port: port.to_be(),
                      sin_addr: addr, sin_zero: [0u8; 8] }
    }
}

unsafe fn raw_bind_in(fd: i32, addr: *const sockaddr_in) -> isize {
    xret(syscall3(SYS_BIND, fd as usize, addr as usize, 16))
}
unsafe fn raw_connect_in(fd: i32, addr: *const sockaddr_in) -> isize {
    xret(syscall3(SYS_CONNECT, fd as usize, addr as usize, 16))
}
unsafe fn raw_getsockname(fd: i32, addr: *mut sockaddr_in, len: *mut u32) -> isize {
    xret(syscall3(SYS_GETSOCKNAME, fd as usize, addr as usize, len as usize))
}
unsafe fn raw_shutdown(fd: i32, how: i32) -> isize {
    xret(syscall2(SYS_SHUTDOWN, fd as usize, how as usize))
}

/// Sleep `ms` milliseconds. TCP over 127.0.0.1 is still real TCP, and its
/// packets only move when the kernel's net daemon runs its 100 Hz smoltcp poll,
/// so the inet test waits in real time rather than spinning on syscalls.
unsafe fn sleep_ms(ms: i64) {
    let ts: [i64; 2] = [ms / 1000, (ms % 1000) * 1_000_000]; // struct timespec
    let _ = syscall2(nr::NANOSLEEP, ts.as_ptr() as usize, 0);
}

const S_IFMT:  u32 = 0o170000;
const S_IFSOCK: u32 = 0o140000;
const S_IFDIR: u32 = 0o040000;
const ETOOMANYREFS: i32 = 109;
const EMFILE: i32 = 24;
const EADDRINUSE: i32 = 98;
/// setsockopt level/option for SO_REUSEADDR (Linux, both arches).
const SOL_SOCKET_OPT: i32 = 1;
const SO_REUSEADDR: i32 = 2;

/// setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &1, 4).
unsafe fn set_reuseaddr(fd: i32) -> isize {
    let on: u32 = 1;
    xret(syscall6(SYS_SETSOCKOPT, fd as usize, SOL_SOCKET_OPT as usize,
                  SO_REUSEADDR as usize, &on as *const u32 as usize, 4, 0))
}

#[repr(C, align(8))]
struct StatBuf { b: [u8; 144] }

/// stat(path) → (ret, st_mode). Uses newfstatat(AT_FDCWD, path, &st, 0) so the
/// same call works on both arches (only the st_mode offset differs).
unsafe fn raw_stat_mode(path: *const u8) -> (isize, u32) {
    const AT_FDCWD: usize = (-100isize) as usize;
    let mut sb = StatBuf { b: [0u8; 144] };
    let r = syscall4(SYS_NEWFSTATAT, AT_FDCWD, path as usize, sb.b.as_mut_ptr() as usize, 0);
    #[cfg(target_arch = "x86_64")] let off = 24usize;
    #[cfg(target_arch = "aarch64")] let off = 16usize;
    let mode = u32::from_ne_bytes(sb.b[off..off + 4].try_into().unwrap());
    (r, mode)
}

// ── cmsg build/parse helpers, shared by every subtest ───────────────────────

/// Build a single-fd `SCM_RIGHTS` cmsg into `buf` (must be >= `cmsg_space(4)`
/// bytes). Returns the control length to pass as `msg_controllen`.
unsafe fn build_fd_cmsg(buf: &mut [u8], fd: i32) -> usize {
    let hdr = cmsghdr { cmsg_len: cmsg_len(4), cmsg_level: SOL_SOCKET, cmsg_type: SCM_RIGHTS };
    core::ptr::write(buf.as_mut_ptr() as *mut cmsghdr, hdr);
    let off = cmsg_data_off();
    buf[off..off + 4].copy_from_slice(&fd.to_ne_bytes());
    cmsg_space(4)
}

unsafe fn send_fd_and_byte(sockfd: i32, fd: i32, byte: u8) -> isize {
    let mut cbuf = CmsgBuf { b: [0u8; 32] };
    let clen = build_fd_cmsg(&mut cbuf.b, fd);
    let mut db = [byte];
    let mut iov = iovec { iov_base: db.as_mut_ptr(), iov_len: 1 };
    let mut mh: msghdr = core::mem::zeroed();
    mh.msg_iov = &mut iov;
    mh.msg_iovlen = 1;
    mh.msg_control = cbuf.b.as_mut_ptr();
    mh.msg_controllen = clen;
    raw_sendmsg(sockfd, &mh, 0)
}

/// Receives one byte + (attempted) one fd. Returns
/// (recvmsg's return value, msg_flags after the call, extracted fd or -1 if
/// no valid `SOL_SOCKET`/`SCM_RIGHTS` cmsg was found, msg_controllen after
/// the call). `control_cap` lets a caller request a too-small control buffer
/// to probe `MSG_CTRUNC` — the backing storage is always the full 32 bytes,
/// only the *declared* capacity shrinks, matching how a real caller would
/// probe this (never handing the kernel a buffer smaller than what it might
/// actually write into).
unsafe fn recv_fd_and_byte(sockfd: i32, control_cap: usize, flags: i32) -> (isize, i32, i32, usize) {
    let mut cbuf = CmsgBuf { b: [0u8; 32] };
    let mut db = [0u8; 1];
    let mut iov = iovec { iov_base: db.as_mut_ptr(), iov_len: 1 };
    let mut mh: msghdr = core::mem::zeroed();
    mh.msg_iov = &mut iov;
    mh.msg_iovlen = 1;
    mh.msg_control = cbuf.b.as_mut_ptr();
    mh.msg_controllen = control_cap.min(32);

    let n = raw_recvmsg(sockfd, &mut mh, flags);

    let ch: cmsghdr = core::ptr::read(cbuf.b.as_ptr() as *const cmsghdr);
    let off = cmsg_data_off();
    let found = mh.msg_controllen >= core::mem::size_of::<cmsghdr>()
        && ch.cmsg_level == SOL_SOCKET && ch.cmsg_type == SCM_RIGHTS;
    let fd = if found { i32::from_ne_bytes(cbuf.b[off..off + 4].try_into().unwrap()) } else { -1 };

    (n, mh.msg_flags, fd, mh.msg_controllen)
}

// ── printf-based diagnostics (both target arches have real `printf`, see
// userland/libc/src/stdio.rs) ────────────────────────────────────────────────

extern "C" {
    fn printf(fmt: *const u8, a0: u64, a1: u64, a2: u64, a3: u64) -> i32;
}
unsafe fn dbg0(fmt: &[u8]) { printf(fmt.as_ptr(), 0, 0, 0, 0); }
unsafe fn dbg1(fmt: &[u8], a: i64) { printf(fmt.as_ptr(), a as u64, 0, 0, 0); }
unsafe fn dbg2(fmt: &[u8], a: i64, b: i64) { printf(fmt.as_ptr(), a as u64, b as u64, 0, 0); }

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(STDOUT_FILENO, name.as_ptr(), name.len() - 1); // drop the NUL terminator
    if passed {
        write(STDOUT_FILENO, b": PASS\n".as_ptr(), 7);
    } else {
        write(STDOUT_FILENO, b": FAIL\n".as_ptr(), 7);
    }
    passed
}

// ── M7q TASK 1: the fork+exec+fd-inherit "decider" ──────────────────────────
//
// This mirrors the EXACT mechanism COSMIC's cosmic-session↔cosmic-comp
// readiness handshake uses, MINUS tokio, to settle empirically whether the
// stuck handshake is a kernel bug (fork/execve fd-inheritance or write→read
// wake) or purely a tokio async-read integration gap in userspace:
//
//   1. parent socketpair(AF_UNIX, SOCK_STREAM)  -> (A = child end, B = parent end)
//   2. clear FD_CLOEXEC on A so it survives execve  (cosmic-session does the same
//      before handing the fd NUMBER to comp via the COSMIC_SESSION_SOCK env var)
//   3. fork()+execve(self) — the child inherits A by its raw fd number, passed in
//      the env var SCMTEST_INHERIT_FD=<A> (private name; the kernel mechanism is
//      identical whatever the string says — this just avoids any real-session
//      COSMIC_SESSION_SOCK collision)
//   4. the re-exec'd child, detecting the env var, write()s a length-prefixed
//      message to the inherited fd and exits  (== comp writing SetEnv{WAYLAND_
//      DISPLAY} to fd 261)
//   5. parent epoll_wait's on B, then read()s and asserts byte-exact delivery
//      (== cosmic-session's tokio reactor waking on its end of the pair)
//
// PASS => the kernel fork/execve/fd-inherit + AF_UNIX write→epoll-wake path is
//         sound; a stuck COSMIC handshake is a userspace/tokio issue.
// FAIL => a real kernel bug in exactly that path (the handshake root cause).

/// The framed message the helper writes and the parent expects, byte for byte.
const SCM_INHERIT_MSG: &[u8] = b"WAYLAND_DISPLAY=wayland-1";

/// Scan a NULL-terminated `envp` for `key=`; return the integer that follows, or
/// None. No allocation, no libc — pure pointer walk (helper mode runs this
/// before any suite state exists).
unsafe fn env_int(envp: *const *const u8, key: &[u8]) -> Option<i32> {
    if envp.is_null() { return None; }
    let mut pp = envp;
    while !(*pp).is_null() {
        let s = *pp;
        let mut i = 0usize;
        let mut matched = true;
        while i < key.len() {
            if *s.add(i) != key[i] { matched = false; break; }
            i += 1;
        }
        if matched && *s.add(key.len()) == b'=' {
            let mut j = key.len() + 1;
            let mut val: i32 = 0;
            let mut any = false;
            loop {
                let c = *s.add(j);
                if !(b'0'..=b'9').contains(&c) { break; }
                val = val * 10 + (c - b'0') as i32;
                any = true;
                j += 1;
            }
            if any { return Some(val); }
        }
        pp = pp.add(1);
    }
    None
}

/// Helper mode: write the framed message (4-byte LE length + body) to the
/// inherited fd, then exit. Reached only when SCMTEST_INHERIT_FD is present in
/// the environment, i.e. only via this test's own self-execve — never on a
/// plain `scmtest` invocation.
unsafe fn scm_inherit_helper(fd: i32) -> ! {
    let body = SCM_INHERIT_MSG;
    let mut wire = [0u8; 4 + 64];
    wire[..4].copy_from_slice(&(body.len() as u32).to_le_bytes());
    wire[4..4 + body.len()].copy_from_slice(body);
    let total = 4 + body.len();
    let w = write(fd, wire.as_ptr(), total);
    dbg2(b"[fei:helper] inherited fd=%d wrote=%d\n\0", fd as i64, w as i64);
    exit(if w == total as isize { 0 } else { 7 });
}

/// The decider test (parent side). See the block comment above.
unsafe fn test_fork_exec_inherit() -> bool {
    let name = b"fork_exec_inherit\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"[fei] socketpair failed\n\0");
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);

    // Clear FD_CLOEXEC on A so the child keeps it across execve.
    raw_fcntl(a, F_SETFD, 0);

    // Build "SCMTEST_INHERIT_FD=<a>\0" before fork so the child's COW copy has it.
    let mut envbuf = [0u8; 48];
    build_name(&mut envbuf, b"SCMTEST_INHERIT_FD=", a as usize);

    let pid = fork();
    if pid == 0 {
        // Child: re-exec self; the env var flips it into helper mode.
        close(b);
        let path = b"/bin/scmtest\0";
        let av: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
        let ev: [*const u8; 2] = [envbuf.as_ptr(), core::ptr::null()];
        syscall3(SYS_EXECVE, path.as_ptr() as usize, av.as_ptr() as usize, ev.as_ptr() as usize);
        dbg0(b"[fei:child] execve(/bin/scmtest) failed\n\0");
        exit(9);
    }

    // Parent = the cosmic-session role. Drop our copy of A so only the child
    // holds the write end (clean EOF semantics), then epoll_wait on B.
    close(a);
    report(name, parent_epoll_read_ok(b, pid))
}

/// The same decider, plus the one syscall tokio adds — and that is the whole
/// point. `test_fork_exec_inherit` above passes, and that PASS was used to
/// exonerate the kernel and blame tokio for the stuck cosmic-session handshake.
/// It omits `shutdown(2)`: `cosmic-session` does
/// `let (mut session_rx, _session_tx) = session.into_split();` and lets the
/// write half drop at the end of a non-async function, and tokio's
/// `OwnedWriteHalf::drop` is `shutdown(fd, Shutdown::Write)`. Everything else
/// here is byte-for-byte the test above, so a FAIL here beside a PASS there
/// localises the defect to shutdown(2) and to nothing else.
///
/// The parent never writes on this socket, so retiring its write direction must
/// cost it nothing: the child's message still has to arrive and the epoll
/// registration on `b` still has to fire.
unsafe fn test_fork_exec_inherit_after_shutdown_wr() -> bool {
    let name = b"fork_exec_inherit_after_shutdown_wr\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"[feis] socketpair failed\n\0");
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);

    // Clear FD_CLOEXEC on A so the child keeps it across execve.
    raw_fcntl(a, F_SETFD, 0);

    // Build "SCMTEST_INHERIT_FD=<a>\0" before fork so the child's COW copy has it.
    let mut envbuf = [0u8; 48];
    build_name(&mut envbuf, b"SCMTEST_INHERIT_FD=", a as usize);

    let pid = fork();
    if pid == 0 {
        // Child: re-exec self; the env var flips it into helper mode.
        close(b);
        let path = b"/bin/scmtest\0";
        let av: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
        let ev: [*const u8; 2] = [envbuf.as_ptr(), core::ptr::null()];
        syscall3(SYS_EXECVE, path.as_ptr() as usize, av.as_ptr() as usize, ev.as_ptr() as usize);
        dbg0(b"[feis:child] execve(/bin/scmtest) failed\n\0");
        exit(9);
    }

    close(a);
    let sh = raw_shutdown(b, SHUT_WR);
    dbg2(b"[feis] shutdown(b, SHUT_WR) -> %d errno=%d\n\0", sh as i64, get_errno() as i64);
    // Run the read half unconditionally even if the shutdown reported an
    // error, so the helper is always reaped and the verdict is never a hang.
    let read_ok = parent_epoll_read_ok(b, pid);
    report(name, sh == 0 && read_ok)
}

/// Parent side shared by the fork+exec+inherit deciders: epoll_wait(5s) on `b`,
/// read the framed message, assert byte-exact, reap the helper. Returns true iff
/// the message arrived intact AND the helper exited 0. The 5s bounded wait makes
/// a broken inherit FAIL loudly instead of hanging the suite.
unsafe fn parent_epoll_read_ok(b: i32, pid: i32) -> bool {
    let ep = xret(syscall1(SYS_EPOLL_CREATE1, 0)) as i32;
    if ep < 0 {
        dbg1(b"[fei] epoll_create1 failed errno=%d\n\0", get_errno() as i64);
        close(b); reap(pid); return false;
    }
    let mut evbuf = [0u8; EPOLL_EVENT_SIZE];
    evbuf[..4].copy_from_slice(&EPOLLIN.to_le_bytes());
    evbuf[EPOLL_EVENT_DATA_OFF..EPOLL_EVENT_DATA_OFF + 8]
        .copy_from_slice(&(b as u64).to_le_bytes());
    if xret(syscall4(SYS_EPOLL_CTL, ep as usize, EPOLL_CTL_ADD, b as usize, evbuf.as_mut_ptr() as usize)) != 0 {
        dbg1(b"[fei] epoll_ctl(ADD) failed errno=%d\n\0", get_errno() as i64);
        close(ep); close(b); reap(pid); return false;
    }

    let mut out = [0u8; EPOLL_EVENT_SIZE];
    let nready = xret(syscall4(SYS_EPOLL_WAIT, ep as usize, out.as_mut_ptr() as usize, 1, 5000));
    dbg1(b"[fei] epoll_wait -> %d (want >=1; 0 == INHERIT BROKEN)\n\0", nready as i64);

    let mut ok = false;
    if nready >= 1 {
        let mut lenb = [0u8; 4];
        let r1 = read(b, lenb.as_mut_ptr(), 4);
        let mlen = u32::from_le_bytes(lenb) as usize;
        let mut body = [0u8; 64];
        let r2 = if mlen <= 64 { read(b, body.as_mut_ptr(), mlen) } else { -1 };
        ok = r1 == 4 && mlen == SCM_INHERIT_MSG.len() && r2 == mlen as isize
            && &body[..mlen] == SCM_INHERIT_MSG;
        dbg2(b"[fei] read len=%d body=%d\n\0", r1 as i64, r2 as i64);
    }

    close(ep); close(b);
    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    dbg1(b"[fei] helper exit status=%d\n\0", status as i64);
    ok && status == 0
}

/// Variant of the decider that mirrors launch_pad's `with_fds` handoff — the
/// exact path cosmic-session uses to hand cosmic-panel / cosmic-notifications
/// their notification socket (PANEL_/DAEMON_NOTIFICATIONS_FD). Unlike
/// test_fork_exec_inherit (which clears CLOEXEC in the PARENT, like
/// COSMIC_SESSION_SOCK), here the inherited end is created SOCK_CLOEXEC (as tokio/
/// std UnixStream::pair does) and FD_CLOEXEC is cleared in the CHILD's post-fork,
/// pre-execve window. This exercises the kernel's execve cloexec-sweep against a
/// CHILD-cleared net-socket fd. PASS => the path is sound; FAIL => the child-
/// cleared fd is wrongly closed at execve (child sees EBADF), which is exactly
/// the "Bad file descriptor" crash cosmic-notifications/-panel hit.
unsafe fn test_fork_exec_child_clears_cloexec() -> bool {
    let name = b"fork_exec_child_clears_cloexec\0";
    const SOCK_CLOEXEC: i32 = 0x80000;
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"[feic] socketpair(SOCK_CLOEXEC) failed\n\0");
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);

    // Sanity: A must START close-on-exec, else this isn't testing the clear path.
    let pre = raw_fcntl(a, F_GETFD, 0);
    if pre & FD_CLOEXEC == 0 {
        dbg1(b"[feic] warn: A not cloexec at start (fdflags=%d); SOCK_CLOEXEC ignored\n\0", pre as i64);
    }

    let mut envbuf = [0u8; 48];
    build_name(&mut envbuf, b"SCMTEST_INHERIT_FD=", a as usize);

    let pid = fork();
    if pid == 0 {
        close(b);
        // The launch_pad pre_exec step: clear FD_CLOEXEC on the inherited fd in
        // the CHILD, after fork and before execve.
        raw_fcntl(a, F_SETFD, 0);
        let path = b"/bin/scmtest\0";
        let av: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
        let ev: [*const u8; 2] = [envbuf.as_ptr(), core::ptr::null()];
        syscall3(SYS_EXECVE, path.as_ptr() as usize, av.as_ptr() as usize, ev.as_ptr() as usize);
        dbg0(b"[feic:child] execve failed\n\0");
        exit(9);
    }

    close(a);
    report(name, parent_epoll_read_ok(b, pid))
}

unsafe fn reap(pid: i32) {
    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
}

/// recv with a bounded retry, so a broken half of this handshake FAILs loudly
/// instead of hanging the suite (the same reason `parent_epoll_read_ok` uses a
/// 5 s epoll_wait rather than a blocking read).
unsafe fn recv_retry(fd: i32, buf: *mut u8, len: usize) -> isize {
    let mut tries = 0;
    while tries < 250 {
        let r = raw_recv(fd, buf, len, MSG_DONTWAIT);
        if r >= 0 || get_errno() != EAGAIN { return r; }
        sleep_ms(20);
        tries += 1;
    }
    -1
}

// ── fork() inherits an UNACCEPTED connector ────────────────────────────────
//
// The regression pinned here is what stopped every `X-HostWaylandDisplay=true`
// COSMIC applet from ever running. cosmic-panel binds an abstract listener,
// marshals it to cosmic-comp over `wp_security_context_v1.create_listener`,
// and connects to it ITSELF — all synchronously, before the Wayland request
// has even been flushed, so the connector is still `UnixPendingAccept`. It
// then hands launch-pad that raw fd number, which forks, clears FD_CLOEXEC in
// the child's pre_exec window, execs the applet, and drops the parent's copy.
//
// Two gaps sat on exactly that sequence:
//
//   * `handle_fork_dup` copied only `UnixConnected`/`Unbound`, so the child's
//     fd number did not exist. The pre_exec `fcntl(F_GETFD)` answered EBADF,
//     `Command::spawn` failed BEFORE `execve`, and cosmic-panel's
//     `if let Ok(key)` swallowed the error — the applet looked like a live
//     process that drew nothing when in fact it never ran.
//   * `handle_close`'s `UnixPendingAccept` arm force-freed the whole
//     `UnixConn` with no refcount, so the parent dropping its copy right after
//     the fork destroyed the child's connection too. cosmic-comp gets exactly
//     one chance to accept (smithay's listener source removes itself as soon
//     as the paired close_fd pipe reports POLLERR), so that is unrecoverable.
//
// Two phases, because the reference can be released from either side and the
// two releases live in different functions:
//
//   A. PARENT closes first (handle_close's UnixPendingAccept arm) — the
//      cosmic-panel shape, where the child goes on to use the connection.
//   B. CHILD exits first without closing (handle_close_all's pending loop) —
//      the exec-failure shape, where the parent must keep a usable connector.
//
// PASS => a pending connector survives fork and outlives whichever holder goes
// away first. FAIL at fcntl => the fork_dup gap. FAIL at accept or on the data
// round-trip => a refcount gap in the matching teardown path.
unsafe fn test_fork_inherits_pending_connector() -> bool {
    let name = b"fork_inherits_pending_connector\0";
    let ok_a = pending_connector_parent_closes_first();
    let ok_b = pending_connector_child_exits_first();
    report(name, ok_a && ok_b)
}

/// Phase A: the parent drops its copy immediately after the fork and the child
/// must still have a working connection.
unsafe fn pending_connector_parent_closes_first() -> bool {
    let (addr, alen) = sockaddr_un::from_abstract(b"scmtest-pending-connector-a");

    let ls = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if ls < 0 { dbg0(b"[fpc-a] listener socket failed\n\0"); return false; }
    if raw_bind(ls, &addr, alen) != 0 {
        dbg1(b"[fpc-a] bind failed errno=%d\n\0", get_errno() as i64);
        close(ls); return false;
    }
    if raw_listen(ls, 8) != 0 {
        dbg1(b"[fpc-a] listen failed errno=%d\n\0", get_errno() as i64);
        close(ls); return false;
    }

    // Connect and deliberately DO NOT accept: the connector stays
    // `UnixPendingAccept` across the whole fork, which is the state the old
    // handle_fork_dup dropped on the floor.
    let cs = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if cs < 0 { close(ls); return false; }
    if raw_connect(cs, &addr, alen) != 0 {
        dbg1(b"[fpc-a] connect failed errno=%d\n\0", get_errno() as i64);
        close(cs); close(ls); return false;
    }

    let pid = fork();
    if pid == 0 {
        close(ls);
        // (a) The inherited fd must exist. This is byte-for-byte the syscall
        // launch-pad's `mark_as_not_cloexec` issues first, and the one that
        // used to answer EBADF.
        if raw_fcntl(cs, F_GETFD, 0) < 0 { exit(11); }
        // (b) …and the clear must land too — the other half of that helper.
        if raw_fcntl(cs, F_SETFD, 0) != 0 { exit(12); }
        // (c) A write before the peer accepts must queue, as on Linux.
        if write(cs, b"CHLD".as_ptr(), 4) != 4 { exit(13); }
        // (d) …and the server's reply must arrive on the inherited fd even
        //     though the parent closed its own copy in the meantime.
        let mut rb = [0u8; 4];
        if recv_retry(cs, rb.as_mut_ptr(), 4) != 4 { exit(14); }
        if &rb != b"SRVR" { exit(15); }
        close(cs);
        exit(0);
    }

    // The parent drops its copy immediately, exactly as cosmic-panel does the
    // moment launch-pad has forked. The half-open connection must survive on
    // the child's reference alone.
    close(cs);

    // Note what a failure looks like, because the two gaps present different
    // symptoms here and the accept return code alone does not name either.
    // With BOTH gaps the child has no entry and the parent's close destroys
    // the connection, so accept finds nothing and answers -1. With only the
    // close gap the child's entry survives, accept succeeds, and the damage
    // shows up one line later as a 0-byte (EOF) read off a dead connection.
    let asf = raw_accept(ls);
    dbg1(b"[fpc-a] accept after parent close -> %d (want >=0)\n\0", asf as i64);

    let mut ok = asf >= 0;
    if ok {
        let mut rb = [0u8; 4];
        let got = recv_retry(asf, rb.as_mut_ptr(), 4);
        let sent = if got == 4 && &rb == b"CHLD" { write(asf, b"SRVR".as_ptr(), 4) } else { -1 };
        dbg2(b"[fpc-a] server read=%d wrote=%d (want 4 4; read=0 == conn destroyed)\n\0",
             got as i64, sent as i64);
        ok = got == 4 && &rb == b"CHLD" && sent == 4;
    }

    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    // 11/12 = the fd never reached the child (fork_dup gap); 13/14/15 = it
    // reached the child but the connection was torn down under it (close gap).
    dbg1(b"[fpc-a] child exit status=%d (want 0)\n\0", status as i64);
    ok = ok && status == 0;

    if asf >= 0 { close(asf); }
    close(ls);
    ok
}

/// Phase B: the mirror image, and the only coverage `handle_close_all`'s
/// pending arm has. The child exits WITHOUT closing its inherited connector —
/// the exec-failure shape, and what every forked child that never reaches
/// `execve` does — so process teardown, not `close(2)`, releases the
/// reference. Force-freeing there (the old behaviour, and what undoes the
/// `handle_close` fix if only one of the two is repaired) destroys the
/// connector the PARENT still holds.
unsafe fn pending_connector_child_exits_first() -> bool {
    let (addr, alen) = sockaddr_un::from_abstract(b"scmtest-pending-connector-b");

    let ls = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if ls < 0 { dbg0(b"[fpc-b] listener socket failed\n\0"); return false; }
    if raw_bind(ls, &addr, alen) != 0 {
        dbg1(b"[fpc-b] bind failed errno=%d\n\0", get_errno() as i64);
        close(ls); return false;
    }
    if raw_listen(ls, 8) != 0 {
        dbg1(b"[fpc-b] listen failed errno=%d\n\0", get_errno() as i64);
        close(ls); return false;
    }

    let cs = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if cs < 0 { close(ls); return false; }
    if raw_connect(cs, &addr, alen) != 0 {
        dbg1(b"[fpc-b] connect failed errno=%d\n\0", get_errno() as i64);
        close(cs); close(ls); return false;
    }

    let pid = fork();
    if pid == 0 {
        close(ls);
        // Same two fcntls, so a fork_dup regression still FAILs here and not
        // only in phase A — then exit holding the fd, leaving the release to
        // handle_close_all.
        if raw_fcntl(cs, F_GETFD, 0) < 0 { exit(11); }
        if raw_fcntl(cs, F_SETFD, 0) != 0 { exit(12); }
        exit(0);
    }

    // Reap first: the reference must be gone before the parent proves its own
    // still works, or the test would pass on timing rather than on refcounts.
    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    dbg1(b"[fpc-b] child exit status=%d (want 0)\n\0", status as i64);

    // The parent's connector must still be acceptable and must still carry
    // data. A force-free at child teardown shows up either as accept -1 (the
    // pending entry adopted a dead conn and was refused) or as a 0-byte read.
    let asf = raw_accept(ls);
    dbg1(b"[fpc-b] accept after child exit -> %d (want >=0)\n\0", asf as i64);

    let mut ok = asf >= 0 && status == 0;
    if asf >= 0 {
        let wrote = write(cs, b"PARN".as_ptr(), 4);
        let mut rb = [0u8; 4];
        let got = recv_retry(asf, rb.as_mut_ptr(), 4);
        dbg2(b"[fpc-b] parent wrote=%d server read=%d (want 4 4; read=0 == conn destroyed)\n\0",
             wrote as i64, got as i64);
        ok = ok && wrote == 4 && got == 4 && &rb == b"PARN";
        close(asf);
    }

    close(cs);
    close(ls);
    ok
}

// ── mincore: POSIX residency probe (the Mesa _eglPointerIsDereferenceable signal) ──
//
// The kernel's mincore used to be a bare `=> 0` stub that reported success for
// ANY address, including the unmapped null page. That made Mesa's
// `_eglPointerIsDereferenceable((void*)3)` return TRUE, so `get_wayland_surface`
// misread `wl_egl_window.version==3` as a `wl_surface*` and cosmic-panel's EGL
// window-surface create faulted (FAR=0x1B). POSIX-correct mincore must:
//   - ENOMEM (-> raw -12) when the range covers an unmapped page (e.g. page 0),
//   - 0 with the residency vector filled for a fully-mapped range,
//   - EINVAL (-> raw -22) for a non-page-aligned addr.
// Raw syscall returns are inspected directly (no errno wrapper) so the exact
// error codes are asserted.
// ── FIONREAD / TIOCOUTQ on a socket ─────────────────────────────────────────
//
// Firefox's in-process Wayland proxy sizes every relay read with
// ioctl(FIONREAD) and drops the connection when it fails. Socket fds used to
// reach the VFS ioctl path, which answered EBADF for them. Check the counts
// on both ends of a socketpair across a write, a partial read and a drain.
unsafe fn test_socket_fionread() -> bool {
    let name = b"socket_fionread\0";
    const FIONREAD: usize = 0x541B;
    const TIOCOUTQ: usize = 0x5411;
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 { return report(name, false); }
    let (a, b) = (sv[0], sv[1]);
    let q = |fd: i32, cmd: usize| -> (isize, i32) {
        let mut n: i32 = -1;
        let r = syscall3(nr::IOCTL, fd as usize, cmd, &mut n as *mut i32 as usize);
        (r, n)
    };
    let empty = q(b, FIONREAD);
    let w = write(a, b"hello, proxy".as_ptr(), 12);
    let after_write_b = q(b, FIONREAD);
    let after_write_a_out = q(a, TIOCOUTQ);
    let after_write_a_in = q(a, FIONREAD);
    let mut buf = [0u8; 5];
    let r1 = read(b, buf.as_mut_ptr(), 5);
    let after_part = q(b, FIONREAD);
    let mut rest = [0u8; 16];
    let r2 = read(b, rest.as_mut_ptr(), 16);
    let drained = q(b, FIONREAD);
    let drained_out = q(a, TIOCOUTQ);
    close(a); close(b);
    let ok = empty == (0, 0) && w == 12 && after_write_b == (0, 12) && after_write_a_out == (0, 12)
        && after_write_a_in == (0, 0) && r1 == 5 && after_part == (0, 7) && r2 == 7
        && drained == (0, 0) && drained_out == (0, 0);
    if !ok {
        for (tag, (r, n)) in [(&b"[fionread] empty rc=%ld n=%ld\n\0"[..], empty),
                              (b"[fionread] b-after-write rc=%ld n=%ld\n\0", after_write_b),
                              (b"[fionread] a-outq rc=%ld n=%ld\n\0", after_write_a_out),
                              (b"[fionread] a-in rc=%ld n=%ld\n\0", after_write_a_in),
                              (b"[fionread] b-after-partial rc=%ld n=%ld\n\0", after_part),
                              (b"[fionread] b-drained rc=%ld n=%ld\n\0", drained),
                              (b"[fionread] a-outq-drained rc=%ld n=%ld\n\0", drained_out)] {
            printf(tag.as_ptr(), r as u64, n as i64 as u64, 0, 0);
        }
        dbg1(b"[fionread] write=%ld\n\0", w as i64);
    }
    report(name, ok)
}

// ── dup2 of a socket onto a low (VFS-range) descriptor ──────────────────────
//
// Socket fds live at and above the net server's SOCK_FD_BASE, so dup2(sock, 100)
// used to reach the VFS, which answered EBADF. Firefox launches every child
// with exactly that — dup2(ipc_socketpair_end, 3), a sweep closing every other
// fd, then execve — and `_exit(127)`s when the dup2 fails, which killed every
// content process. The kernel now installs an alias at the low number, backed
// by a hidden duplicate of the socket.
const ALIAS_FD: i32 = 100;
/// One past every VFS and socket descriptor: VFS fds are [0, SOCK_FD_BASE =
/// 0x200), sockets run from 0x200 up to net's SOCK_FD_END, which stays below
/// epoll's EPOLL_FD_BASE = 0x400. A brute-force sweep below it therefore also
/// hits the hidden socket behind an alias.
const FD_SWEEP_END: i32 = 0x400;
const O_CLOEXEC_FL: i32 = 0x80000;

unsafe fn test_socket_dup2_low_fd() -> bool {
    let name = b"socket_dup2_low_fd\0";
    const FIONREAD: usize = 0x541B;
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM | O_CLOEXEC_FL, 0, sv.as_mut_ptr()) != 0 {
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);
    let mut ok = true;
    let mut step = 0i64;
    let mut check = |c: bool, s: &mut i64| { *s += 1; if !c && ok { dbg1(b"[dup2low] failed at step %ld\n\0", *s); ok = false; } };

    check(dup3(a, ALIAS_FD, 0) == ALIAS_FD, &mut step);                          // 1
    check(raw_fcntl(ALIAS_FD, F_GETFD, 0) == 0, &mut step);                     // 2 dup2 clears cloexec
    check(dup3(a, ALIAS_FD + 1, O_CLOEXEC_FL) == ALIAS_FD + 1, &mut step);       // 3
    check(raw_fcntl(ALIAS_FD + 1, F_GETFD, 0) == FD_CLOEXEC, &mut step);        // 4
    check(write(ALIAS_FD, b"ping".as_ptr(), 4) == 4, &mut step);                 // 5 alias writes the socket
    let mut buf = [0u8; 8];
    check(read(b, buf.as_mut_ptr(), 8) == 4 && &buf[..4] == b"ping", &mut step); // 6
    check(write(b, b"pong".as_ptr(), 4) == 4, &mut step);                        // 7
    let mut n: i32 = -1;
    let q = syscall3(nr::IOCTL, ALIAS_FD as usize, FIONREAD, &mut n as *mut i32 as usize);
    check(q == 0 && n == 4, &mut step);                                          // 8 ioctl reaches the socket
    let mut rd = [0u8; 8];
    let mut iov = iovec { iov_base: rd.as_mut_ptr(), iov_len: 8 };
    let mut mh: msghdr = core::mem::zeroed();
    mh.msg_iov = &mut iov; mh.msg_iovlen = 1;
    check(raw_recvmsg(ALIAS_FD, &mut mh, 0) == 4 && &rd[..4] == b"pong", &mut step); // 9 recvmsg
    let d = dup(ALIAS_FD);
    check(d >= 0 && write(d, b"x".as_ptr(), 1) == 1, &mut step);                // 10 dup of an alias
    check(read(b, buf.as_mut_ptr(), 8) == 1, &mut step);                         // 11
    if d >= 0 { close(d); }
    check(close(ALIAS_FD) == 0, &mut step);                                       // 12
    check(raw_fcntl(ALIAS_FD, F_GETFD, 0) < 0, &mut step);                      // 13 closed for real
    check(write(a, b"y".as_ptr(), 1) == 1, &mut step);                           // 14 original unaffected
    check(read(b, buf.as_mut_ptr(), 8) == 1, &mut step);                         // 15
    // An ordinary fd dup2'd over an alias replaces it.
    let nul = open(b"/dev/null\0".as_ptr(), O_RDWR, 0);
    check(nul >= 0 && dup3(nul, ALIAS_FD + 1, 0) == ALIAS_FD + 1, &mut step);    // 16
    check(write(ALIAS_FD + 1, b"zz".as_ptr(), 2) == 2, &mut step);              // 17 now /dev/null
    let mut n2: i32 = -1;
    let q2 = syscall3(nr::IOCTL, a as usize, FIONREAD, &mut n2 as *mut i32 as usize);
    check(q2 == 0 && n2 == 0, &mut step);                                        // 18 nothing reached the peer
    close(ALIAS_FD + 1); if nul >= 0 { close(nul); }
    close(a); close(b);
    report(name, ok)
}

/// Firefox's child launch, step for step: dup2 the SOCK_CLOEXEC socket onto a
/// low fd, close every other descriptor by brute force (as its
/// CloseSuperfluousFds does when it cannot list /proc/self/fd), then execve.
/// The re-exec'd helper writes the framed message through the low fd.
unsafe fn test_fork_dup2_low_exec() -> bool {
    let name = b"fork_dup2_low_exec\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM | O_CLOEXEC_FL, 0, sv.as_mut_ptr()) != 0 {
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);
    let mut envbuf = [0u8; 48];
    build_name(&mut envbuf, b"SCMTEST_INHERIT_FD=", ALIAS_FD as usize);
    let pid = fork();
    if pid == 0 {
        if dup3(a, ALIAS_FD, 0) != ALIAS_FD {
            dbg0(b"[fdle:child] dup2(sock, low) failed\n\0");
            exit(127);
        }
        let mut fd = 3;
        while fd < FD_SWEEP_END { if fd != ALIAS_FD { close(fd); } fd += 1; }
        let path = b"/bin/scmtest\0";
        let av: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
        let ev: [*const u8; 2] = [envbuf.as_ptr(), core::ptr::null()];
        syscall3(SYS_EXECVE, path.as_ptr() as usize, av.as_ptr() as usize, ev.as_ptr() as usize);
        exit(127);
    }
    close(a);
    report(name, parent_epoll_read_ok(b, pid))
}

/// SCM_RIGHTS of a *connected* AF_UNIX end — how Firefox hands every new IPC
/// channel to the process that will use it. The net server used to refuse
/// connected ends (EBADF for the whole sendmsg). The received end must talk to
/// the original peer, keep the connection open after the sender closes its
/// copy, and its close must be the EOF the peer sees.
unsafe fn test_pass_connected_socket() -> bool {
    let name = b"pass_connected_socket\0";
    let mut carrier = [0i32; 2];
    let mut chan = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, carrier.as_mut_ptr()) != 0
        || raw_socketpair(AF_UNIX, SOCK_STREAM, 0, chan.as_mut_ptr()) != 0 {
        return report(name, false);
    }
    let mut ok = true;
    let mut step = 0i64;
    let mut check = |c: bool, s: &mut i64| { *s += 1; if !c && ok { dbg1(b"[passconn] failed at step %ld\n\0", *s); ok = false; } };
    let pid = fork();
    if pid == 0 {
        close(carrier[0]); close(chan[0]); close(chan[1]);
        let (n, _f, fd, _c) = recv_fd_and_byte(carrier[1], 32, 0);
        if n != 1 || fd < 0 { exit(2); }
        let mut b = [0u8; 4];
        if read(fd, b.as_mut_ptr(), 4) != 4 || &b != b"ping" { exit(3); }
        if write(fd, b"pong".as_ptr(), 4) != 4 { exit(4); }
        close(fd);
        exit(0);
    }
    close(carrier[1]);
    check(send_fd_and_byte(carrier[0], chan[1], b'x') == 1, &mut step);          // 1 sendmsg accepted
    close(chan[1]);                                                              // only the child holds it now
    check(write(chan[0], b"ping".as_ptr(), 4) == 4, &mut step);                  // 2 still connected
    let mut b = [0u8; 4];
    check(read(chan[0], b.as_mut_ptr(), 4) == 4 && &b == b"pong", &mut step);    // 3 the child answered
    check(read(chan[0], b.as_mut_ptr(), 4) == 0, &mut step);                     // 4 EOF once the child closed it
    let mut status = 0i32;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    check(status == 0, &mut step);                                               // 5
    if status != 0 { dbg1(b"[passconn] child status %ld\n\0", status as i64); }
    close(carrier[0]); close(chan[0]);
    report(name, ok)
}

/// open("/proc/self/fd/N", O_RDONLY) on a memfd: a new open of the same file
/// with its own offset and a read-only access mode — how Firefox makes the
/// read-only half of every shared-memory region. It used to be ENOENT.
unsafe fn test_memfd_reopen_readonly() -> bool {
    let name = b"memfd_reopen_readonly\0";
    let fd = raw_memfd_create(b"reopen\0".as_ptr(), 0);
    if fd < 0 { return report(name, false); }
    let mut ok = write(fd, b"abcdef".as_ptr(), 6) == 6;
    let mut path = [0u8; 32];
    build_name(&mut path, b"/proc/self/fd/", fd as usize);
    let ro = open(path.as_ptr(), O_RDONLY, 0);
    if ro < 0 {
        dbg1(b"[reopen] open /proc/self/fd/N failed errno=%ld\n\0", get_errno() as i64);
        close(fd);
        return report(name, false);
    }
    let mut b = [0u8; 8];
    ok &= read(ro, b.as_mut_ptr(), 8) == 6 && &b[..6] == b"abcdef";   // own offset, from 0
    ok &= write(ro, b"x".as_ptr(), 1) < 0;                             // read-only
    let p = mmap(core::ptr::null_mut(), 4096, PROT_READ, MAP_SHARED, ro, 0);
    ok &= p as isize != -1;
    if p as isize != -1 {
        ok &= core::ptr::read_volatile(p.add(2)) == b'c';
        // The writer's later stores show through the read-only mapping.
        ok &= lseek(fd, 2, 0) == 2 && write(fd, b"Z".as_ptr(), 1) == 1;
        ok &= core::ptr::read_volatile(p.add(2)) == b'Z';
        munmap(p, 4096);
    }
    close(ro); close(fd);
    report(name, ok)
}

/// send/recv/getsockopt on an open descriptor that is not a socket (a pipe)
/// fail with ENOTSOCK, on a closed one with EBADF — the net server answered
/// EBADF for both. libpulse's `pa_write` tries `send(fd, MSG_NOSIGNAL)` first
/// and falls back to `write` only on ENOTSOCK, so its threaded mainloop could
/// not wake itself through its wakeup pipe and Firefox played no audio.
unsafe fn test_send_on_pipe_enotsock() -> bool {
    let name = b"send_on_pipe_enotsock\0";
    let mut p = [0i32; 2];
    if pipe2(p.as_mut_ptr(), 0) != 0 { return report(name, false); }
    let x = 1u8;
    let mut b = [0u8; 4];
    let s = syscall6(nr::SENDTO, p[1] as usize, &x as *const u8 as usize, 1, 0x4000 /* MSG_NOSIGNAL */, 0, 0);
    let r = syscall6(nr::RECVFROM, p[0] as usize, b.as_mut_ptr() as usize, 4, 0, 0, 0);
    let w = write(p[1], &x, 1);                        // the fallback still works
    let rd = read(p[0], b.as_mut_ptr(), 4);
    close(p[0]); close(p[1]);
    let bad = syscall6(nr::SENDTO, p[1] as usize, &x as *const u8 as usize, 1, 0, 0, 0);
    let ok = s == -88 && r == -88 && w == 1 && rd == 1 && bad == -9;
    if !ok {
        dbg1(b"[enotsock] send on pipe=%ld (want -88)\n\0", s as i64);
        dbg1(b"[enotsock] recv on pipe=%ld (want -88)\n\0", r as i64);
        dbg1(b"[enotsock] send on closed fd=%ld (want -9)\n\0", bad as i64);
    }
    report(name, ok)
}

/// Socket calls on an epoll fd: ENOTSOCK while it is open, EBADF once closed
/// (the epoll range sits above the socket range, so it used to reach the net
/// server, which answered EBADF).
unsafe fn test_send_on_epoll_enotsock() -> bool {
    let name = b"send_on_epoll_enotsock\0";
    let ep = xret(syscall1(SYS_EPOLL_CREATE1, 0)) as i32;
    if ep < 0 { return report(name, false); }
    let x = 1u8;
    let mut b = [0u8; 4];
    let mut v = 0i32;
    let mut vl = 4u32;
    let s = syscall6(nr::SENDTO, ep as usize, &x as *const u8 as usize, 1, 0x4000, 0, 0);
    let r = syscall6(nr::RECVFROM, ep as usize, b.as_mut_ptr() as usize, 4, 0, 0, 0);
    let g = syscall6(SYS_GETSOCKOPT, ep as usize, 1, 4, &mut v as *mut i32 as usize, &mut vl as *mut u32 as usize, 0);
    close(ep);
    let bad = syscall6(nr::SENDTO, ep as usize, &x as *const u8 as usize, 1, 0, 0, 0);
    let ok = s == -88 && r == -88 && g == -88 && bad == -9;
    if !ok {
        dbg1(b"[enotsock] send on epoll=%ld (want -88)\n\0", s as i64);
        dbg1(b"[enotsock] recv on epoll=%ld (want -88)\n\0", r as i64);
        dbg1(b"[enotsock] getsockopt on epoll=%ld (want -88)\n\0", g as i64);
        dbg1(b"[enotsock] send on closed epoll=%ld (want -9)\n\0", bad as i64);
    }
    report(name, ok)
}

/// A short write of a multi-iovec sendmsg must end the call. The plain
/// (no-fd) path went on to the next iovec after a partial one, so with a
/// reader draining a full ring on another CPU the next iovec's bytes landed
/// right after the truncated one; the caller resends from the returned count,
/// and the stream lost the truncated tail. Firefox's IPC messages, mostly
/// larger than the old fixed 4 KiB ring, failed to parse at random.
///
/// The ring now grows to 208 KiB, so the stream is several times that to keep
/// the writer running into a full ring. The parent streams 2 MiB as 2-iovec sendmsgs of a position-keyed pattern,
/// resending exactly as Firefox does after a short write; a forked reader
/// checks every byte.
unsafe fn test_sendmsg_short_write_keeps_stream() -> bool {
    let name = b"sendmsg_short_write_keeps_stream\0";
    const TOTAL: usize = 2 * 1024 * 1024;
    const HALF: usize = 3000;
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 { return report(name, false); }
    let pid = fork();
    if pid == 0 {
        close(sv[0]);
        let mut buf = [0u8; 1500];
        let mut pos = 0usize;
        while pos < TOTAL {
            let n = read(sv[1], buf.as_mut_ptr(), buf.len());
            if n <= 0 { exit(2); }
            for k in 0..n as usize {
                if buf[k] != ((pos + k) % 251) as u8 {
                    dbg1(b"[shortwr] corrupt at byte %ld\n\0", (pos + k) as i64);
                    exit(3);
                }
            }
            pos += n as usize;
        }
        exit(0);
    }
    close(sv[1]);
    static mut SRC: [u8; 2 * 3000] = [0; 2 * 3000];
    let mut sent = 0usize;
    let mut ok = true;
    while sent < TOTAL {
        // One "message": two iovecs over the next 6000 bytes of the pattern.
        let msg_len = (2 * HALF).min(TOTAL - sent);
        for k in 0..msg_len { SRC[k] = ((sent + k) % 251) as u8; }
        let mut done = 0usize;
        while done < msg_len {
            let base = core::ptr::addr_of_mut!(SRC) as *mut u8;
            let (a_off, a_len, b_off, b_len) = if done < HALF.min(msg_len) {
                (done, HALF.min(msg_len) - done, HALF.min(msg_len), msg_len - HALF.min(msg_len))
            } else {
                (done, msg_len - done, 0, 0)
            };
            let mut iov = [iovec { iov_base: base.add(a_off), iov_len: a_len },
                           iovec { iov_base: base.add(b_off), iov_len: b_len }];
            let mut mh: msghdr = core::mem::zeroed();
            mh.msg_iov = iov.as_mut_ptr();
            mh.msg_iovlen = if b_len > 0 { 2 } else { 1 };
            let n = raw_sendmsg(sv[0], &mh, 0);
            if n <= 0 { ok = false; break; }
            done += n as usize;
        }
        if !ok { break; }
        sent += msg_len;
    }
    let mut status = -1i32;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    close(sv[0]);
    if status != 0 { dbg1(b"[shortwr] reader status %ld\n\0", status as i64); }
    report(name, ok && status == 0)
}

/// epoll_ctl on a socket alias registers the socket it names (the hidden slot
/// behind the alias, and its open file description). With the alias the only
/// reference left, closing it must end the registration: the next sockets,
/// which reuse the freed hidden slot number, must not fire under it.
unsafe fn test_socket_alias_epoll_close() -> bool {
    let name = b"socket_alias_epoll_close\0";
    let fd = ALIAS_FD + 2;
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 { return report(name, false); }
    let (a, b) = (sv[0], sv[1]);
    let ep = xret(syscall1(SYS_EPOLL_CREATE1, 0)) as i32;
    let mut ok = ep >= 0 && dup3(a, fd, 0) == fd;
    // The alias now holds end `a` alone, as in a launched Firefox child.
    close(a);
    let mut ev = [0u8; EPOLL_EVENT_SIZE];
    ev[..4].copy_from_slice(&EPOLLIN.to_le_bytes());
    ev[EPOLL_EVENT_DATA_OFF..EPOLL_EVENT_DATA_OFF + 8].copy_from_slice(&0x5151u64.to_le_bytes());
    if ok {
        ok = xret(syscall4(SYS_EPOLL_CTL, ep as usize, EPOLL_CTL_ADD, fd as usize, ev.as_mut_ptr() as usize)) == 0;
    }
    let mut out = [0u8; EPOLL_EVENT_SIZE * 4];
    // The alias's registration works: a byte from the peer makes it ready.
    ok &= write(b, b"q".as_ptr(), 1) == 1;
    let n1 = xret(syscall4(SYS_EPOLL_WAIT, ep as usize, out.as_mut_ptr() as usize, 4, 1000));
    if n1 != 1 { dbg1(b"[aliasep] ready alias: epoll_wait=%d (want 1)\n\0", n1 as i64); ok = false; }
    // Close the alias (the hidden socket and the end go with it), then open
    // sockets that reuse the freed slot and make every one of them readable.
    ok &= close(fd) == 0;
    let mut fresh = [[-1i32; 2]; 4];
    for p in fresh.iter_mut() {
        if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, p.as_mut_ptr()) == 0 {
            ok &= write(p[1], b"r".as_ptr(), 1) == 1;
        } else { ok = false; }
    }
    let n2 = xret(syscall4(SYS_EPOLL_WAIT, ep as usize, out.as_mut_ptr() as usize, 4, 0));
    if n2 != 0 { dbg1(b"[aliasep] after close: epoll_wait=%d (want 0)\n\0", n2 as i64); ok = false; }
    for p in fresh.iter() { if p[0] >= 0 { close(p[0]); close(p[1]); } }
    if ep >= 0 { close(ep); }
    close(b);
    report(name, ok)
}

unsafe fn test_mincore() -> bool {
    let name = b"mincore";
    let page = 4096usize;

    // (a) A definitely-mapped range: the page holding this stack local.
    let probe: u64 = 0xA5A5_A5A5;
    let sp = &probe as *const u64 as usize;
    let _ = core::ptr::read_volatile(&probe); // ensure the stack page is faulted in
    let mapped = sp & !(page - 1);
    let mut vec = [0u8; 1];
    let r_mapped = syscall3(SYS_MINCORE, mapped, page, vec.as_mut_ptr() as usize);
    let mapped_ok = r_mapped == 0 && (vec[0] & 1) == 1; // resident, no swap

    // (b) The unmapped null page — exactly Mesa's `(void*)3 & ~0xfff` probe.
    let r_null = syscall3(SYS_MINCORE, 0usize, page, vec.as_mut_ptr() as usize);
    let null_ok = r_null == -12; // -ENOMEM

    // (c) A misaligned addr → EINVAL.
    let r_unalign = syscall3(SYS_MINCORE, mapped + 1, page, vec.as_mut_ptr() as usize);
    let einval_ok = r_unalign == -22; // -EINVAL

    if !mapped_ok { dbg0(b"[mincore] mapped-range probe wrong\n\0"); }
    if !null_ok   { dbg0(b"[mincore] null page not ENOMEM\n\0"); }
    if !einval_ok { dbg0(b"[mincore] unaligned addr not EINVAL\n\0"); }

    report(name, mapped_ok && null_ok && einval_ok)
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, envp: *const *const u8) -> i32 {
    // M7q TASK 1: helper mode is entered ONLY via this test's own self-execve,
    // which sets SCMTEST_INHERIT_FD. A plain `scmtest` invocation never has it,
    // so the full suite runs as before.
    if let Some(fd) = env_int(envp, b"SCMTEST_INHERIT_FD") {
        scm_inherit_helper(fd);
    }
    if let Some(n) = env_int(envp, b"SCMTEST_ALIAS_PRUNE") {
        alias_prune_helper(n);
    }

    let mut failures = 0;

    if !test_fd_pass() { failures += 1; }
    if !test_fork_child_exit_keeps_socket() { failures += 1; }
    if !test_fork_exec_inherit() { failures += 1; }
    if !test_fork_exec_inherit_after_shutdown_wr() { failures += 1; }
    if !test_fork_exec_child_clears_cloexec() { failures += 1; }
    if !test_fork_inherits_pending_connector() { failures += 1; }
    if !test_cmsg_flags() { failures += 1; }
    if !test_shared_memfd_pixels() { failures += 1; }
    if !test_seals() { failures += 1; }
    if !test_memfd_same_name_distinct() { failures += 1; }

    // ── K1-B VMO tests (single-process + fork; no SCM_RIGHTS needed) ─────────
    if !test_double_mmap_alias() { failures += 1; }
    if !test_read_mmap_coherence() { failures += 1; }
    if !test_big_memfd() { failures += 1; }
    if !test_fork_visibility() { failures += 1; }
    if !test_partial_munmap() { failures += 1; }
    if !test_close_while_mapped() { failures += 1; }
    if !test_ftruncate_grow_shrink() { failures += 1; }
    if !test_teardown_loop() { failures += 1; }
    if !test_memfd_anonymous_reclaim() { failures += 1; }
    if !test_memfd_inflight_close() { failures += 1; }

    // ── K1-C: AF_UNIX VFS socket nodes, tmpfs mounts, cap raise, fd-cap ──────
    if !test_socket_node_roundtrip() { failures += 1; }
    if !test_socket_node_devshm() { failures += 1; }
    if !test_unlink_rebind() { failures += 1; }
    if !test_unix_listen_strict() { failures += 1; }
    if !test_many_socketpairs_and_listeners() { failures += 1; }
    if !test_tmpfs_mounts_exist() { failures += 1; }
    if !test_devshm_shared_mmap() { failures += 1; }
    if !test_queued_fd_cap() { failures += 1; }
    if !test_scm_import_emfile_single_release() { failures += 1; }
    if !test_full_ring_eagain() { failures += 1; }

    // ── shutdown(2) is a per-direction half-close, not a close ──────────────
    if !test_socketpair_shutdown_wr_half_close() { failures += 1; }
    if !test_shutdown_wr_keeps_fd_pollable() { failures += 1; }

    // ── M7u: mincore residency probe (Mesa EGL pointer-dereferenceable signal) ──
    if !test_mincore() { failures += 1; }

    // ── FIONREAD / TIOCOUTQ on AF_UNIX sockets (Firefox's Wayland proxy) ──
    if !test_socket_fionread() { failures += 1; }

    // ── dup2 of a socket onto a low fd (Firefox's child launch) ──
    if !test_socket_dup2_low_fd() { failures += 1; }
    if !test_fork_dup2_low_exec() { failures += 1; }
    if !test_socket_alias_epoll_close() { failures += 1; }
    if !test_pass_connected_socket() { failures += 1; }
    if !test_memfd_reopen_readonly() { failures += 1; }
    if !test_sendmsg_short_write_keeps_stream() { failures += 1; }
    if !test_send_on_pipe_enotsock() { failures += 1; }
    if !test_send_on_epoll_enotsock() { failures += 1; }

    // ── In-flight fd lifetime: unix GC, read() with queued fds, exec aliases ──
    if !test_unix_gc_self_cycle() { failures += 1; }
    if !test_unix_gc_two_conn_cycle() { failures += 1; }
    if !test_unix_gc_keeps_reachable() { failures += 1; }
    if !test_unix_gc_listener_backlog() { failures += 1; }
    if !test_read_discards_fds() { failures += 1; }
    if !test_exec_prunes_many_aliases() { failures += 1; }

    // ── AF_INET TCP over the loopback interface ────────────────
    if !test_inet_loopback_tcp() { failures += 1; }
    if !test_inet_listen_twice() { failures += 1; }
    if !test_tcp_time_wait() { failures += 1; }
    if !test_udp_unconnected() { failures += 1; }
    if !test_udp_msghdr() { failures += 1; }
    if !test_tcp_peer_close_eof() { failures += 1; }
    if !test_tcp_connect_refused() { failures += 1; }
    if !test_inet_msg_peek() { failures += 1; }

    puts(b"--- scmtest done ---\0".as_ptr());
    failures
}

// ── 1. fd-pass: SCM_RIGHTS across a real fork()'d parent/child ─────────────
//
// Parent opens a regular tmpfs file with known contents, sends it to the
// child over an AF_UNIX SOCK_STREAM socketpair with a single-fd SCM_RIGHTS
// cmsg (plus 1 byte of ordinary data, per sendmsg/recvmsg convention). The
// child must recvmsg a cmsg with cmsg_level=SOL_SOCKET/cmsg_type=SCM_RIGHTS
// and the extracted fd must be independently readable and see the same
// bytes the parent wrote.
unsafe fn test_fd_pass() -> bool {
    let name = b"fd_pass\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"[fd_pass] socketpair failed\n\0");
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);

    let path = b"/tmp/scmtest_fdpass\0";
    let wfd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if wfd < 0 {
        dbg0(b"[fd_pass] open(/tmp/scmtest_fdpass) failed\n\0");
        close(a); close(b);
        return report(name, false);
    }
    write(wfd, b"SCMOK".as_ptr(), 5);
    lseek(wfd, 0, SEEK_SET);

    let pid = fork();
    if pid == 0 {
        close(a);
        let (n, _flags, recv_fd, controllen) = recv_fd_and_byte(b, 32, 0);
        if recv_fd < 0 {
            dbg2(b"[fd_pass:child] recvmsg n=%d controllen=%d -- no SCM_RIGHTS cmsg found\n\0",
                 n as i64, controllen as i64);
            exit(2);
        }
        dbg1(b"[fd_pass:child] extracted fd=%d from cmsg\n\0", recv_fd as i64);
        let mut rbuf = [0u8; 5];
        let got = read(recv_fd, rbuf.as_mut_ptr(), 5);
        let data_ok = got == 5 && &rbuf == b"SCMOK";
        dbg1(b"[fd_pass:child] read via received fd -> %d bytes\n\0", got as i64);
        exit(if data_ok { 0 } else { 1 });
    }

    let sret = send_fd_and_byte(a, wfd, b'X');
    dbg1(b"[fd_pass:parent] sendmsg returned %d\n\0", sret as i64);

    close(wfd);
    close(a);
    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    close(b);

    report(name, sret == 1 && status == 0)
}

// ── 1b. fork-child-exit must NOT tear down the parent's connected socket ────
//
// Regression for the W1 root cause: the net server's process-teardown path
// (handle_close_all) force-freed a connected AF_UNIX connection instead of
// decrementing its per-end refcount, so a forked child that INHERITED a live
// socket fd tore the connection down when it exited. cosmic-comp hit this via
// its failed kiosk-child fork (a copy of comp's session-bus socket was closed
// on the child's exec-error _exit), giving comp a spurious EOF and killing its
// zbus socket reader ("Socket reader task has errored out").
//
// Repro: parent makes a socketpair, forks a child that inherits both ends and
// exits without touching them, waits, then must STILL be able to send a→b. On
// the buggy kernel the parent's write/read fails (EPIPE / spurious EOF).
unsafe fn test_fork_child_exit_keeps_socket() -> bool {
    let name = b"fork_child_exit_keeps_socket\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"[fork_keep] socketpair failed\n\0");
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);

    let pid = fork();
    if pid == 0 {
        // Inherited a+b (handle_fork_dup bumped refs_a/refs_b). Exit WITHOUT
        // closing them — process teardown (handle_close_all) must only decrement
        // the per-end refcount, never force-free a still-parent-held connection.
        exit(0);
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());

    // Parent still holds both ends: a→b must carry data, not a spurious EOF.
    let wrote = write(a, b"PING".as_ptr(), 4);
    let mut rbuf = [0u8; 4];
    let got = read(b, rbuf.as_mut_ptr(), 4);
    dbg2(b"[fork_keep] after child exit: wrote=%d read=%d\n\0", wrote as i64, got as i64);
    let ok = wrote == 4 && got == 4 && &rbuf == b"PING";
    close(a);
    close(b);
    report(name, ok)
}

// ── 2. cmsg-flags: MSG_CTRUNC on a too-small buffer, MSG_CMSG_CLOEXEC ───────
//
// Round A: child recvmsg's with a control buffer smaller than
// cmsg_space(4) -- msg_flags must come back with MSG_CTRUNC set.
// Round B: child recvmsg's with MSG_CMSG_CLOEXEC -- the fd it extracts must
// carry FD_CLOEXEC per fcntl(F_GETFD).
unsafe fn test_cmsg_flags() -> bool {
    let name = b"cmsg_flags\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"[cmsg_flags] socketpair failed\n\0");
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);

    let path = b"/tmp/scmtest_cmsgflags\0";
    let wfd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if wfd < 0 {
        dbg0(b"[cmsg_flags] open failed\n\0");
        close(a); close(b);
        return report(name, false);
    }
    write(wfd, b"Z".as_ptr(), 1);

    let pid = fork();
    if pid == 0 {
        close(a);

        // Round A: a control buffer smaller than cmsg_space(4)=24 bytes.
        let (n1, flags1, _fd1, controllen1) = recv_fd_and_byte(b, 4, 0);
        let ctrunc_ok = flags1 & MSG_CTRUNC != 0;
        dbg2(b"[cmsg_flags:child] roundA(small buf) n=%d controllen=%d\n\0", n1 as i64, controllen1 as i64);
        dbg1(b"[cmsg_flags:child] roundA: msg_flags=0x%x (want MSG_CTRUNC=0x8 set)\n\0", flags1 as i64);

        // Round B: MSG_CMSG_CLOEXEC should mark the received fd close-on-exec.
        let (n2, _flags2, fd2, controllen2) = recv_fd_and_byte(b, 32, MSG_CMSG_CLOEXEC);
        dbg2(b"[cmsg_flags:child] roundB(CMSG_CLOEXEC) n=%d controllen=%d\n\0", n2 as i64, controllen2 as i64);
        let cloexec_ok = if fd2 >= 0 {
            let fdflags = raw_fcntl(fd2, F_GETFD, 0);
            dbg2(b"[cmsg_flags:child] roundB: fd=%d fcntl(F_GETFD) -> %d\n\0", fd2 as i64, fdflags as i64);
            fdflags & FD_CLOEXEC != 0
        } else {
            dbg0(b"[cmsg_flags:child] roundB: no SCM_RIGHTS cmsg found -- cannot check CLOEXEC\n\0");
            false
        };

        let mut code = 0;
        if !ctrunc_ok { code |= 1; }
        if !cloexec_ok { code |= 2; }
        exit(code);
    }

    let r1 = send_fd_and_byte(a, wfd, b'A');
    let r2 = send_fd_and_byte(a, wfd, b'B');
    dbg2(b"[cmsg_flags:parent] sendmsg roundA=%d roundB=%d\n\0", r1 as i64, r2 as i64);

    close(wfd);
    close(a);
    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    close(b);

    if status != 0 {
        dbg1(b"[cmsg_flags] child exit status=%d (bit0=MSG_CTRUNC missing, bit1=CLOEXEC missing)\n\0", status as i64);
    }
    report(name, status == 0)
}

// ── 3. shared-memfd-pixels: MAP_SHARED must alias real physical pages ──────
//
// Parent memfd_create+ftruncate(4096)+mmap(MAP_SHARED), writes pattern A,
// passes the fd via SCM_RIGHTS. Child mmaps MAP_SHARED on the received fd,
// checks it sees pattern A, then writes pattern B. After an ack byte over
// the socket, the parent checks its own (already-existing) mapping for
// pattern B -- proof both processes alias the same physical pages, not
// copy-on-map private pages.
unsafe fn test_shared_memfd_pixels() -> bool {
    let name = b"shared_memfd_pixels\0";
    let pattern_a = |i: usize| -> u8 { (0xA0usize ^ (i & 0xFF)) as u8 };
    let pattern_b = |i: usize| -> u8 { (0x5Cusize ^ (i & 0xFF)) as u8 };

    let mfd = raw_memfd_create(b"scmtest-shm\0".as_ptr(), 0);
    if mfd < 0 {
        dbg0(b"[shared_memfd] memfd_create failed\n\0");
        return report(name, false);
    }
    if raw_ftruncate(mfd, 4096) != 0 {
        dbg0(b"[shared_memfd] ftruncate(4096) failed\n\0");
        close(mfd);
        return report(name, false);
    }

    let parent_map = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if parent_map as usize == usize::MAX {
        dbg0(b"[shared_memfd] parent mmap(MAP_SHARED) failed\n\0");
        close(mfd);
        return report(name, false);
    }
    for i in 0..4096usize { *parent_map.add(i) = pattern_a(i); }

    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"[shared_memfd] socketpair failed\n\0");
        munmap(parent_map, 4096);
        close(mfd);
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);

    let pid = fork();
    if pid == 0 {
        close(a);
        let (n, _flags, recv_fd, controllen) = recv_fd_and_byte(b, 32, 0);
        if recv_fd < 0 {
            dbg2(b"[shared_memfd:child] no SCM_RIGHTS fd received (n=%d controllen=%d)\n\0",
                 n as i64, controllen as i64);
            exit(2); // distinct code: SCM_RIGHTS itself is the blocker here
        }
        dbg1(b"[shared_memfd:child] received fd=%d, mapping MAP_SHARED\n\0", recv_fd as i64);
        let child_map = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, recv_fd, 0);
        if child_map as usize == usize::MAX {
            dbg0(b"[shared_memfd:child] mmap of received fd failed\n\0");
            exit(3);
        }
        let mut a_matches = true;
        for i in 0..4096usize {
            if *child_map.add(i) != pattern_a(i) { a_matches = false; break; }
        }
        dbg1(b"[shared_memfd:child] pattern A visible through child mapping: %d\n\0", a_matches as i64);
        if !a_matches {
            raw_send(b, b"K".as_ptr(), 1, 0); // still ack so the parent doesn't need to guess EOF timing
            exit(4);
        }
        for i in 0..4096usize { *child_map.add(i) = pattern_b(i); }
        raw_send(b, b"K".as_ptr(), 1, 0);
        exit(0);
    }

    let sret = send_fd_and_byte(a, mfd, b'P');
    dbg1(b"[shared_memfd:parent] sendmsg(memfd) returned %d\n\0", sret as i64);

    // Block for the child's ack (or 0/EOF if it exited without sending one) --
    // never hangs regardless of how badly fd-passing/MAP_SHARED are broken.
    let mut ack = [0u8; 1];
    let ackn = raw_recv(a, ack.as_mut_ptr(), 1, 0);
    dbg1(b"[shared_memfd:parent] ack recv returned %d\n\0", ackn as i64);

    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());

    let mut b_matches = true;
    for i in 0..4096usize {
        if *parent_map.add(i) != pattern_b(i) { b_matches = false; break; }
    }
    dbg2(b"[shared_memfd:parent] child exit status=%d, pattern B visible through parent's own (pre-existing) mapping: %d\n\0",
         status as i64, b_matches as i64);

    munmap(parent_map, 4096);
    close(mfd);
    close(a);
    close(b);

    report(name, status == 0 && b_matches)
}

// ── 4. seals: F_ADD_SEALS(F_SEAL_SHRINK) must actually be enforced ─────────
unsafe fn test_seals() -> bool {
    let name = b"seals\0";
    let mfd = raw_memfd_create(b"scmtest-seals\0".as_ptr(), 0);
    if mfd < 0 {
        dbg0(b"[seals] memfd_create failed\n\0");
        return report(name, false);
    }
    if raw_ftruncate(mfd, 4096) != 0 {
        dbg0(b"[seals] ftruncate(4096) failed\n\0");
        close(mfd);
        return report(name, false);
    }

    let add = raw_fcntl(mfd, F_ADD_SEALS, F_SEAL_SHRINK as usize);
    dbg1(b"[seals] F_ADD_SEALS(F_SEAL_SHRINK) returned %d\n\0", add as i64);

    let got = raw_fcntl(mfd, F_GET_SEALS, 0);
    dbg1(b"[seals] F_GET_SEALS returned 0x%x\n\0", got as i64);
    let seal_reported = got >= 0 && (got as i32 & F_SEAL_SHRINK) != 0;

    let shrink = raw_ftruncate(mfd, 10);
    let shrink_errno = get_errno();
    dbg2(b"[seals] ftruncate(10) after F_SEAL_SHRINK returned %d, errno=%d (want -1/EPERM)\n\0",
         shrink as i64, shrink_errno as i64);
    let shrink_blocked = shrink != 0 && shrink_errno == EPERM;

    close(mfd);
    report(name, add == 0 && seal_reported && shrink_blocked)
}

/// M7q: two memfd_create calls with the SAME name must yield DISTINCT anonymous
/// inodes (Linux semantics). Mirrors smithay-client-toolkit, which creates every
/// wl_shm SlotPool with one fixed name "smithay-client-toolkit" and seals it —
/// on the buggy kernel the 2nd same-name memfd reopened the 1st's sealed inode
/// and its implicit O_TRUNC hit F_SEAL_SHRINK → EPERM, panicking every
/// libcosmic/winit client (cosmic-panel, cosmic-notifications). The fix makes
/// each memfd a unique inode; here the 2nd create must SUCCEED and carry no seal.
unsafe fn test_memfd_same_name_distinct() -> bool {
    let name = b"memfd_same_name_distinct\0";
    let m1 = raw_memfd_create(b"scm-dupname\0".as_ptr(), 0);
    if m1 < 0 { dbg0(b"[mfdn] first memfd_create failed\n\0"); return report(name, false); }
    if raw_ftruncate(m1, 4096) != 0 { close(m1); return report(name, false); }
    // Seal it exactly as smithay does (SHRINK is what made the reopen EPERM).
    let add = raw_fcntl(m1, F_ADD_SEALS, F_SEAL_SHRINK as usize);
    // Second memfd with the IDENTICAL name must be a fresh, unsealed inode.
    let m2 = raw_memfd_create(b"scm-dupname\0".as_ptr(), 0);
    if m2 < 0 {
        dbg1(b"[mfdn] 2nd same-name memfd_create FAILED errno=%d (buggy: reopened sealed inode)\n\0",
             get_errno() as i64);
        close(m1); return report(name, false);
    }
    // The decisive checks: m2 truncates freely (smithay's set_len) and carries
    // no seal — proving it is a distinct inode, not the sealed m1.
    let grow = raw_ftruncate(m2, 2);
    let seals = raw_fcntl(m2, F_GET_SEALS, 0);
    dbg2(b"[mfdn] add=%d 2nd ftrunc(2)=%d\n\0", add as i64, grow as i64);
    dbg1(b"[mfdn] 2nd inode seals=0x%x (want 0)\n\0", seals as i64);
    let ok = grow == 0 && seals == 0;
    close(m1); close(m2);
    report(name, ok)
}

// ── K1-B: shared-VMO tests that don't require SCM_RIGHTS ─────────────────────
//
// These isolate the shared file-backed mmap machinery (blocker #2) from
// fd-passing (blocker #1): every alias here is either two mappings in one
// process, a read()/write() on the same fd, or a mapping inherited across
// fork() — so a green result proves the VMO half regardless of SCM_RIGHTS.

const MAP_FAILED: usize = usize::MAX;

/// Format "<prefix><n>\0" into `buf`; returns length excluding the NUL.
unsafe fn build_name(buf: &mut [u8], prefix: &[u8], n: usize) -> usize {
    let mut p = 0usize;
    for &b in prefix { buf[p] = b; p += 1; }
    if n == 0 { buf[p] = b'0'; p += 1; }
    else {
        let mut digits = [0u8; 10]; let mut d = 0; let mut v = n;
        while v > 0 { digits[d] = b'0' + (v % 10) as u8; d += 1; v /= 10; }
        for i in (0..d).rev() { buf[p] = digits[i]; p += 1; }
    }
    buf[p] = 0;
    p
}

/// (a) Two `MAP_SHARED` mappings of one memfd must alias the same pages.
unsafe fn test_double_mmap_alias() -> bool {
    let name = b"double_mmap_alias\0";
    let mfd = raw_memfd_create(b"scm-dbl\0".as_ptr(), 0);
    if mfd < 0 || raw_ftruncate(mfd, 4096) != 0 {
        dbg0(b"[dbl] memfd/ftruncate failed\n\0");
        if mfd >= 0 { close(mfd); }
        return report(name, false);
    }
    let m1 = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    let m2 = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m1 as usize == MAP_FAILED || m2 as usize == MAP_FAILED {
        dbg0(b"[dbl] mmap failed\n\0"); close(mfd); return report(name, false);
    }
    let mut ok = true;
    for i in 0..4096usize { *m1.add(i) = (i & 0xFF) as u8; }
    for i in 0..4096usize { if *m2.add(i) != (i & 0xFF) as u8 { ok = false; break; } }
    for i in 0..4096usize { *m2.add(i) = (0xFF ^ (i & 0xFF)) as u8; }
    for i in 0..4096usize { if *m1.add(i) != (0xFF ^ (i & 0xFF)) as u8 { ok = false; break; } }
    dbg1(b"[dbl] alias coherent: %d\n\0", ok as i64);
    munmap(m1, 4096); munmap(m2, 4096); close(mfd);
    report(name, ok)
}

/// (b) read()↔mmap coherence, both directions — the wl_shm requirement.
unsafe fn test_read_mmap_coherence() -> bool {
    let name = b"read_mmap_coherence\0";
    let mfd = raw_memfd_create(b"scm-rw\0".as_ptr(), 0);
    if mfd < 0 || raw_ftruncate(mfd, 4096) != 0 {
        if mfd >= 0 { close(mfd); } return report(name, false);
    }
    let m = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m as usize == MAP_FAILED { close(mfd); return report(name, false); }
    let mut ok = true;
    // Direction 1: store via mmap → read(fd) observes it.
    for i in 0..256usize { *m.add(i) = (0xAB ^ i) as u8; }
    lseek(mfd, 0, SEEK_SET);
    let mut rb = [0u8; 256];
    let got = read(mfd, rb.as_mut_ptr(), 256);
    if got != 256 { ok = false; }
    else { for i in 0..256usize { if rb[i] != (0xAB ^ i) as u8 { ok = false; break; } } }
    dbg1(b"[rwcoh] read-after-mmap-store got=%d\n\0", got as i64);
    // Direction 2: write(fd) → load via mmap observes it.
    lseek(mfd, 512, SEEK_SET);
    let mut wb = [0u8; 128];
    for i in 0..128usize { wb[i] = (0x3C ^ i) as u8; }
    let wr = write(mfd, wb.as_ptr(), 128);
    if wr != 128 { ok = false; }
    for i in 0..128usize { if *m.add(512 + i) != (0x3C ^ i) as u8 { ok = false; break; } }
    dbg1(b"[rwcoh] write(fd) wrote=%d\n\0", wr as i64);
    munmap(m, 4096); close(mfd);
    report(name, ok)
}

/// (c) >32768-byte memfd write+mmap proves the inline 32 KiB cap is lifted.
unsafe fn test_big_memfd() -> bool {
    let name = b"big_memfd\0";
    const SZ: usize = 65536; // 64 KiB, twice the old inline cap
    let pat = |i: usize| -> u8 { (0x9E ^ (i & 0xFF) ^ ((i >> 8) & 0xFF)) as u8 };
    let mfd = raw_memfd_create(b"scm-big\0".as_ptr(), 0);
    if mfd < 0 || raw_ftruncate(mfd, SZ as i64) != 0 {
        dbg0(b"[big] memfd/ftruncate(64K) failed\n\0");
        if mfd >= 0 { close(mfd); } return report(name, false);
    }
    let m = mmap(core::ptr::null_mut(), SZ, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m as usize == MAP_FAILED { dbg0(b"[big] mmap(64K) failed\n\0"); close(mfd); return report(name, false); }
    for i in 0..SZ { *m.add(i) = pat(i); }
    let mut ok = true;
    for &i in &[0usize, 4095, 32768, 40000, SZ - 1] {
        if *m.add(i) != pat(i) { ok = false; }
    }
    // read(fd) past the old 32 KiB inline cap must see the data too.
    lseek(mfd, 40000, SEEK_SET);
    let mut rb = [0u8; 16];
    let got = read(mfd, rb.as_mut_ptr(), 16);
    if got != 16 { ok = false; }
    else { for i in 0..16usize { if rb[i] != pat(40000 + i) { ok = false; break; } } }
    dbg1(b"[big] read@40000 got=%d\n\0", got as i64);
    munmap(m, SZ); close(mfd);
    report(name, ok)
}

/// (d1) fork + write-visibility both directions across an inherited
/// `MAP_SHARED` mapping of a VMO-backed memfd (exercises clone_as's shared
/// branch on VMO frames — no SCM_RIGHTS needed).
unsafe fn test_fork_visibility() -> bool {
    let name = b"fork_visibility\0";
    let mfd = raw_memfd_create(b"scm-fork\0".as_ptr(), 0);
    if mfd < 0 || raw_ftruncate(mfd, 4096) != 0 {
        if mfd >= 0 { close(mfd); } return report(name, false);
    }
    let m = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m as usize == MAP_FAILED { close(mfd); return report(name, false); }
    for i in 0..4096usize { *m.add(i) = (0x11 ^ i) as u8; } // parent writes A
    let pid = fork();
    if pid == 0 {
        let mut a_ok = true;
        for i in 0..4096usize { if *m.add(i) != (0x11 ^ i) as u8 { a_ok = false; break; } }
        if !a_ok { exit(1); }
        for i in 0..4096usize { *m.add(i) = (0x22 ^ i) as u8; } // child writes B
        exit(0);
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    let mut b_ok = true;
    for i in 0..4096usize { if *m.add(i) != (0x22 ^ i) as u8 { b_ok = false; break; } }
    dbg1(b"[fork] child status=%d\n\0", status as i64);
    munmap(m, 4096); close(mfd);
    report(name, status == 0 && b_ok)
}

/// (d2) partial munmap: unmapping the middle pages leaves the ends mapped and
/// coherent with read(fd).
unsafe fn test_partial_munmap() -> bool {
    let name = b"partial_munmap\0";
    const SZ: usize = 16384; // 4 pages
    let pat = |i: usize| -> u8 { (0x40 ^ (i & 0xFF)) as u8 };
    let mfd = raw_memfd_create(b"scm-pm\0".as_ptr(), 0);
    if mfd < 0 || raw_ftruncate(mfd, SZ as i64) != 0 {
        if mfd >= 0 { close(mfd); } return report(name, false);
    }
    let m = mmap(core::ptr::null_mut(), SZ, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m as usize == MAP_FAILED { close(mfd); return report(name, false); }
    for i in 0..SZ { *m.add(i) = pat(i); }
    munmap(m.add(4096), 8192); // drop the middle two pages
    let mut ok = true;
    for i in 0..4096usize { *m.add(i) = pat(i) ^ 0xFF; }
    for i in 12288..SZ { *m.add(i) = pat(i) ^ 0xFF; }
    for i in 0..4096usize { if *m.add(i) != pat(i) ^ 0xFF { ok = false; break; } }
    for i in 12288..SZ { if *m.add(i) != pat(i) ^ 0xFF { ok = false; break; } }
    lseek(mfd, 0, SEEK_SET);
    let mut rb = [0u8; 16];
    let got = read(mfd, rb.as_mut_ptr(), 16);
    if got != 16 { ok = false; }
    else { for i in 0..16usize { if rb[i] != pat(i) ^ 0xFF { ok = false; break; } } }
    dbg1(b"[pm] read got=%d\n\0", got as i64);
    munmap(m, 4096); munmap(m.add(12288), 4096); close(mfd);
    report(name, ok)
}

/// (d3) close(fd) while mapped — the mapping stays readable/writable (frames
/// kept alive by pageref), then unmaps cleanly.
unsafe fn test_close_while_mapped() -> bool {
    let name = b"close_while_mapped\0";
    let mfd = raw_memfd_create(b"scm-cwm\0".as_ptr(), 0);
    if mfd < 0 || raw_ftruncate(mfd, 4096) != 0 {
        if mfd >= 0 { close(mfd); } return report(name, false);
    }
    let m = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m as usize == MAP_FAILED { close(mfd); return report(name, false); }
    for i in 0..4096usize { *m.add(i) = (0x55 ^ i) as u8; }
    close(mfd); // close while still mapped
    let mut ok = true;
    for i in 0..4096usize { if *m.add(i) != (0x55 ^ i) as u8 { ok = false; break; } }
    for i in 0..4096usize { *m.add(i) = (0xAA ^ i) as u8; }
    for i in 0..4096usize { if *m.add(i) != (0xAA ^ i) as u8 { ok = false; break; } }
    dbg1(b"[cwm] mapping usable after close: %d\n\0", ok as i64);
    munmap(m, 4096);
    report(name, ok)
}

/// (d4) ftruncate grow/shrink under a live mapping. Grow preserves old bytes
/// and zero-fills the new region; unsealed shrink succeeds.
unsafe fn test_ftruncate_grow_shrink() -> bool {
    let name = b"ftruncate_grow_shrink\0";
    let mfd = raw_memfd_create(b"scm-ft\0".as_ptr(), 0);
    if mfd < 0 || raw_ftruncate(mfd, 4096) != 0 {
        if mfd >= 0 { close(mfd); } return report(name, false);
    }
    let m1 = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m1 as usize == MAP_FAILED { close(mfd); return report(name, false); }
    for i in 0..4096usize { *m1.add(i) = (0x77 ^ i) as u8; }
    let g = raw_ftruncate(mfd, 8192); // grow
    let mut ok = g == 0;
    for i in 0..4096usize { if *m1.add(i) != (0x77 ^ i) as u8 { ok = false; break; } }
    let m2 = mmap(core::ptr::null_mut(), 8192, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if m2 as usize == MAP_FAILED { ok = false; }
    else {
        for i in 0..4096usize { if *m2.add(i) != (0x77 ^ i) as u8 { ok = false; break; } }
        for i in 4096..8192usize { if *m2.add(i) != 0 { ok = false; break; } } // new region zeroed
        munmap(m2, 8192);
    }
    let s = raw_ftruncate(mfd, 4096); // unsealed shrink succeeds
    if s != 0 { ok = false; }
    dbg2(b"[ft] grow=%d shrink=%d\n\0", g as i64, s as i64);
    munmap(m1, 4096); close(mfd);
    report(name, ok)
}

/// (e) teardown-order stress loop. 150 iterations (> the 128-slot pool) of
/// create → ftruncate → mmap → write/verify → munmap, alternating close/unlink
/// order so both `vmo_free_slot` call sites (tmp_drop_name and
/// tmp_release_ephemeral) are exercised. A frame leak surfaces as slot/buddy
/// exhaustion (a later create/ftruncate/mmap fails); a double-free surfaces as
/// buddy corruption caught by the per-iteration content check.
unsafe fn test_teardown_loop() -> bool {
    let name = b"teardown_loop\0";
    let mut ok = true;
    let mut i = 0usize;
    while i < 150 {
        let mut nm = [0u8; 32];
        build_name(&mut nm, b"td", i);
        let mfd = raw_memfd_create(nm.as_ptr(), 0);
        if mfd < 0 { dbg1(b"[td] memfd_create failed at i=%d\n\0", i as i64); ok = false; break; }
        if raw_ftruncate(mfd, 8192) != 0 {
            dbg1(b"[td] ftruncate failed at i=%d\n\0", i as i64); close(mfd); ok = false; break;
        }
        let m = mmap(core::ptr::null_mut(), 8192, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
        if m as usize == MAP_FAILED { dbg1(b"[td] mmap failed at i=%d\n\0", i as i64); close(mfd); ok = false; break; }
        let tag = (i & 0xFF) as u8;
        for j in 0..8192usize { *m.add(j) = tag ^ (j & 0xFF) as u8; }
        for j in (0..8192usize).step_by(97) {
            if *m.add(j) != tag ^ (j & 0xFF) as u8 { ok = false; break; }
        }
        if !ok { dbg1(b"[td] content mismatch at i=%d\n\0", i as i64); munmap(m, 8192); close(mfd); break; }
        munmap(m, 8192);
        let mut path = [0u8; 40];
        build_name(&mut path, b"/tmp/memfd:td", i);
        if i & 1 == 0 {
            close(mfd);
            unlink(path.as_ptr());
        } else {
            unlink(path.as_ptr()); // marks the inode ephemeral (fd still open)
            close(mfd);            // → tmp_release_ephemeral frees slot + VMO
        }
        i += 1;
    }
    dbg1(b"[td] completed %d iterations\n\0", i as i64);
    report(name, ok && i == 150)
}

/// A memfd must be an ANONYMOUS inode. Three claims, one loop.
///
/// (a) The "/tmp/memfd:<name>" node is unlinked at creation, so it is invisible
///     to stat/lookup — Linux semantics, and what `sys_memfd_create`'s old
///     comment claimed would break everything.
/// (b) ftruncate and MAP_SHARED still work on the now-nameless fd. That is the
///     exact failure that comment predicted, so this is the assertion retiring
///     it.
/// (c) The pool slot is RECLAIMED on the final close. Before the fix each call
///     burnt one of the 128 MAX_TMP_FILES slots forever and this loop died with
///     ENOSPC well short of 300. `teardown_loop` above cannot catch that: it
///     unlinks every node by hand, which is exactly what userspace does not do
///     and cannot do for an fd it received over SCM_RIGHTS.
unsafe fn test_memfd_anonymous_reclaim() -> bool {
    let name = b"memfd_anonymous_reclaim\0";
    const ROUNDS: usize = 300; // > 2 * MAX_TMP_FILES

    // (a) the backing name must not resolve while the fd is open.
    let probe = raw_memfd_create(b"anonprobe\0".as_ptr(), 0);
    if probe < 0 { dbg0(b"[anon] probe memfd_create failed\n\0"); return report(name, false); }
    let (st, _mode) = raw_stat_mode(b"/tmp/memfd:anonprobe\0".as_ptr());
    let nameless = st < 0;
    dbg1(b"[anon] stat(/tmp/memfd:anonprobe) = %d (want < 0)\n\0", st as i64);
    close(probe);

    let mut ok = nameless;
    let mut i = 0usize;
    while ok && i < ROUNDS {
        let mut nm = [0u8; 32];
        build_name(&mut nm, b"anon", i);
        let mfd = raw_memfd_create(nm.as_ptr(), 0);
        if mfd < 0 {
            dbg2(b"[anon] memfd_create FAILED at i=%d errno=%d (slot leak)\n\0",
                 i as i64, get_errno() as i64);
            ok = false;
            break;
        }
        // (b) both of these resolve the inode through VnodeKind::TmpFile { idx },
        // never by name — a nameless inode must serve them unchanged.
        if raw_ftruncate(mfd, 8192) != 0 {
            dbg2(b"[anon] ftruncate FAILED at i=%d errno=%d\n\0", i as i64, get_errno() as i64);
            close(mfd);
            ok = false;
            break;
        }
        let m = mmap(core::ptr::null_mut(), 8192, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
        if m as usize == MAP_FAILED {
            dbg2(b"[anon] mmap FAILED at i=%d errno=%d\n\0", i as i64, get_errno() as i64);
            close(mfd);
            ok = false;
            break;
        }
        let tag = (i & 0xFF) as u8;
        *m = tag;
        *m.add(8191) = !tag;
        if *m != tag || *m.add(8191) != !tag {
            dbg1(b"[anon] content mismatch at i=%d\n\0", i as i64);
            munmap(m, 8192);
            close(mfd);
            ok = false;
            break;
        }
        munmap(m, 8192);
        close(mfd); // (c) no unlink — the kernel already dropped the name
        i += 1;
    }
    dbg1(b"[anon] completed %d iterations\n\0", i as i64);
    report(name, ok && i == ROUNDS)
}

/// The sender may close its memfd the instant it has been handed to SCM_RIGHTS:
/// Mesa's swrast path does exactly `wl_shm_create_pool(fd); close(fd);`. Once
/// the inode is nameless, the ONLY thing keeping the pool slot alive between
/// `sendmsg` and the peer's `recvmsg` is the in-flight reference `export_fd`
/// takes — a queued TmpFile is in no fd table, so the table scan in
/// `tmp_release_ephemeral` cannot see it.
///
/// The second socketpair makes the ordering deterministic instead of hoping the
/// scheduler produces it: the child does not enter `recvmsg` until the parent
/// has already closed its last descriptor.
unsafe fn test_memfd_inflight_close() -> bool {
    let name = b"memfd_inflight_close\0";
    let mut sp = [0i32; 2];
    let mut sync = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sp.as_mut_ptr()) != 0 {
        dbg0(b"[inflight] socketpair failed\n\0");
        return report(name, false);
    }
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sync.as_mut_ptr()) != 0 {
        dbg0(b"[inflight] sync socketpair failed\n\0");
        close(sp[0]); close(sp[1]);
        return report(name, false);
    }
    let (a, b) = (sp[0], sp[1]);
    let (sa, sb) = (sync[0], sync[1]);

    let mfd = raw_memfd_create(b"scm-inflight\0".as_ptr(), 0);
    if mfd < 0 || raw_ftruncate(mfd, 4096) != 0 {
        dbg0(b"[inflight] memfd setup failed\n\0");
        close(a); close(b); close(sa); close(sb);
        return report(name, false);
    }
    // Stamp the pattern and drop the mapping: after this only the fd refers to
    // the inode, so the close below really is the last reference.
    let pm = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if pm as usize == MAP_FAILED {
        dbg0(b"[inflight] parent mmap failed\n\0");
        close(mfd); close(a); close(b); close(sa); close(sb);
        return report(name, false);
    }
    for j in 0..4096usize { *pm.add(j) = 0xA5u8 ^ (j & 0xFF) as u8; }
    munmap(pm, 4096);

    let pid = fork();
    if pid == 0 {
        close(a); close(sa);
        // Drop the copy of `mfd` this child inherited across fork, BEFORE the
        // parent is allowed to proceed. Without it the parent's close is not
        // the last fd-table reference: `tmp_release_ephemeral`'s table scan
        // still sees this entry, refuses to collect the slot, and the hazard
        // window never opens — the subtest then passes even with the in-flight
        // refcount removed (measured: it did).
        close(mfd);
        let mut go = [0u8; 1];
        if read(sb, go.as_mut_ptr(), 1) != 1 { exit(3); } // parent has closed mfd
        let (_n, _f, rfd, _cl) = recv_fd_and_byte(b, 32, 0);
        if rfd < 0 { dbg0(b"[inflight:child] no SCM_RIGHTS fd\n\0"); exit(2); }
        let cm = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, rfd, 0);
        if cm as usize == MAP_FAILED { dbg0(b"[inflight:child] mmap failed\n\0"); exit(4); }
        let mut good = true;
        for j in (0..4096usize).step_by(97) {
            if *cm.add(j) != 0xA5u8 ^ (j & 0xFF) as u8 { good = false; break; }
        }
        munmap(cm, 4096);
        close(rfd);
        exit(if good { 0 } else { 1 });
    }

    close(b); close(sb);
    let sret = send_fd_and_byte(a, mfd, b'I');
    close(mfd);                  // last reference gone; only in-flight left
    write(sa, b"g".as_ptr(), 1); // only now may the child recv
    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    dbg2(b"[inflight] sendmsg=%d child status=%d (0 = pattern survived)\n\0",
         sret as i64, status as i64);
    close(a); close(sa);
    report(name, sret > 0 && status == 0)
}

// ── K1-C: AF_UNIX VFS socket nodes, tmpfs mounts, cap raise, queued-fd cap ───

/// Full pathname-socket roundtrip at `name` (NUL-terminated `cpath` names the
/// same path): bind → stat must report S_IFSOCK → connect → accept →
/// bidirectional data. Cleans up (close + unlink). Returns true on success.
unsafe fn socket_roundtrip_at(name: &[u8], cpath: *const u8) -> bool {
    let ls = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if ls < 0 { dbg1(b"[node] socket(listen) failed errno=%d\n\0", get_errno() as i64); return false; }
    let (addr, alen) = sockaddr_un::from_path(name);
    if raw_bind(ls, &addr, alen) != 0 {
        dbg1(b"[node] bind failed errno=%d\n\0", get_errno() as i64); close(ls); return false;
    }
    if raw_listen(ls, 8) != 0 { dbg0(b"[node] listen failed\n\0"); close(ls); return false; }

    let (sr, mode) = raw_stat_mode(cpath);
    let sock_ok = sr == 0 && (mode & S_IFMT) == S_IFSOCK;
    dbg2(b"[node] stat ret=%d mode&IFMT=0%o (want 0140000 S_IFSOCK)\n\0", sr as i64, (mode & S_IFMT) as i64);

    let cs = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if cs < 0 { close(ls); return false; }
    if raw_connect(cs, &addr, alen) != 0 {
        dbg1(b"[node] connect failed errno=%d\n\0", get_errno() as i64); close(ls); close(cs); return false;
    }
    let asf = raw_accept(ls);
    if asf < 0 { dbg1(b"[node] accept failed errno=%d\n\0", get_errno() as i64); close(ls); close(cs); return false; }

    let w1 = raw_send(cs, b"ping".as_ptr(), 4, 0);
    let mut rb = [0u8; 4];
    let r1 = raw_recv(asf, rb.as_mut_ptr(), 4, 0);
    let fwd_ok = w1 == 4 && r1 == 4 && &rb == b"ping";
    let w2 = raw_send(asf, b"pong".as_ptr(), 4, 0);
    let mut rb2 = [0u8; 4];
    let r2 = raw_recv(cs, rb2.as_mut_ptr(), 4, 0);
    let rev_ok = w2 == 4 && r2 == 4 && &rb2 == b"pong";

    close(cs); close(asf); close(ls);
    unlink(cpath);
    sock_ok && fwd_ok && rev_ok
}

/// bind at a /tmp path → S_IFSOCK node → connect roundtrip.
unsafe fn test_socket_node_roundtrip() -> bool {
    report(b"socket_node_roundtrip\0",
           socket_roundtrip_at(b"/tmp/scmtest_sock", b"/tmp/scmtest_sock\0".as_ptr()))
}

/// The same roundtrip on a socket bound under the new /dev/shm tmpfs mount.
unsafe fn test_socket_node_devshm() -> bool {
    report(b"socket_node_devshm\0",
           socket_roundtrip_at(b"/dev/shm/scmtest_sock", b"/dev/shm/scmtest_sock\0".as_ptr()))
}

/// unlink removes the node and makes the address rebindable, while an
/// already-established connection lives on.
unsafe fn test_unlink_rebind() -> bool {
    let name = b"unlink_rebind\0";
    let path = b"/tmp/scmtest_rebind";
    let cpath = b"/tmp/scmtest_rebind\0";
    let (addr, alen) = sockaddr_un::from_path(path);

    let ls = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if ls < 0 || raw_bind(ls, &addr, alen) != 0 || raw_listen(ls, 8) != 0 {
        dbg0(b"[rebind] initial bind/listen failed\n\0");
        if ls >= 0 { close(ls); }
        return report(name, false);
    }
    let cs = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if raw_connect(cs, &addr, alen) != 0 { dbg0(b"[rebind] connect failed\n\0"); close(ls); close(cs); return report(name, false); }
    let asf = raw_accept(ls);
    if asf < 0 { dbg0(b"[rebind] accept failed\n\0"); close(ls); close(cs); return report(name, false); }

    let ur = unlink(cpath.as_ptr());
    // Connecting to the now-unlinked path must fail.
    let cs2 = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    let gone_ok = raw_connect(cs2, &addr, alen) != 0;
    close(cs2);

    // The pre-existing connection still passes data.
    let w = raw_send(cs, b"live".as_ptr(), 4, 0);
    let mut rb = [0u8; 4];
    let r = raw_recv(asf, rb.as_mut_ptr(), 4, 0);
    let live_ok = w == 4 && r == 4 && &rb == b"live";

    // Rebind a fresh listener to the same path, then a new connect resolves to
    // it (not the old, still-open listener).
    let ls2 = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    let br = raw_bind(ls2, &addr, alen);
    let _ = raw_listen(ls2, 8);
    let cs3 = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    let cr3 = raw_connect(cs3, &addr, alen);
    let as3 = if cr3 == 0 { raw_accept(ls2) } else { -1 };
    let rebind_ok = br == 0 && cr3 == 0 && as3 >= 0;

    dbg2(b"[rebind] unlink=%d rebind=%d\n\0", ur as i64, br as i64);
    if as3 >= 0 { close(as3); }
    close(cs3); close(ls2); close(asf); close(cs); close(ls);
    unlink(cpath.as_ptr());
    report(name, gone_ok && live_ok && rebind_ok)
}

/// `listen()` on AF_UNIX must refuse exactly the states Linux refuses.
///
/// The AF_UNIX arm of `handle_listen` used to be an unconditional
/// `ok_reply()`, which made the *repeat* listen right by accident and
/// everything else wrong. `unix_listen` (net/unix/af_unix.c) gates in a fixed
/// order: a type that cannot accept is EOPNOTSUPP *before* the address is even
/// looked at, a socket that never bound is EINVAL, and a socket whose
/// `sk_state` is neither TCP_CLOSE nor TCP_LISTEN — i.e. any connected socket —
/// is EINVAL.
///
/// Eight assertions. Five of them (a, d, e, f, g) return 0 against an
/// unpatched kernel: with the AF_UNIX arm back to a bare `ok_reply()` every one
/// of them reads rc=0 where it wants rc=-1. Three (b, c, h) pass both before
/// and after — they are here so that "make AF_UNIX listen() always fail" or
/// "re-arm the address on every listen()" cannot pass this test, and they are
/// deliberately NOT counted as evidence the fix is live.
///
/// (g) is also what pins the gate *order*: an unbound SOCK_DGRAM socket is
/// unbound as well as untyped, so a fix that ran the state match before the
/// type check would answer 22 there instead of 95.
unsafe fn test_unix_listen_strict() -> bool {
    let name = b"unix_listen_strict\0";
    let path = b"/tmp/scmtest_listen_strict";
    let cpath = b"/tmp/scmtest_listen_strict\0";
    // A node left by an earlier run would make bind() EADDRINUSE on a dirty
    // image; the socket file outlives its listener on Linux and here.
    unlink(cpath.as_ptr());
    let (addr, alen) = sockaddr_un::from_path(path);

    // (a) Never bound. Linux: `!u->addr` → EINVAL.
    let ub = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if ub < 0 { return report(name, false); }
    let a_rc = raw_listen(ub, 8);
    let a_errno = get_errno();
    close(ub);
    dbg2(b"[uls] (a) unbound listen rc=%d errno=%d (want -1 22)\n\0",
         a_rc as i64, a_errno as i64);

    // (g) Unbound SOCK_DGRAM. Linux checks the type first, so this is
    // EOPNOTSUPP and not the EINVAL an unbound STREAM socket gets.
    let dg = raw_socket(AF_UNIX, SOCK_DGRAM, 0);
    if dg < 0 { return report(name, false); }
    let g_rc = raw_listen(dg, 8);
    let g_errno = get_errno();
    close(dg);
    dbg2(b"[uls] (g) dgram listen rc=%d errno=%d (want -1 95)\n\0",
         g_rc as i64, g_errno as i64);

    // (b) The ordinary server sequence still works.
    let ls = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if ls < 0 { unlink(cpath.as_ptr()); return report(name, false); }
    if raw_bind(ls, &addr, alen) != 0 {
        dbg1(b"[uls] bind failed errno=%d\n\0", get_errno() as i64);
        close(ls); unlink(cpath.as_ptr());
        return report(name, false);
    }
    let b_rc = raw_listen(ls, 8);
    // (c) …and is still idempotent, with a different backlog.
    let c_rc = raw_listen(ls, 16);
    dbg2(b"[uls] (b,c) listen rc=%d repeat rc=%d (want 0 0)\n\0",
         b_rc as i64, c_rc as i64);

    // (d) A socketpair end is connected and unbound: EINVAL either way on
    // Linux, and the state arm is what produces it here.
    let mut sv = [0i32; 2];
    let d_ok = if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) == 0 {
        let rc = raw_listen(sv[0], 8);
        let e = get_errno();
        dbg2(b"[uls] (d) socketpair listen rc=%d errno=%d (want -1 22)\n\0",
             rc as i64, e as i64);
        close(sv[0]); close(sv[1]);
        rc == -1 && e == EINVAL
    } else {
        dbg0(b"[uls] socketpair failed\n\0");
        false
    };

    // (e) A connector whose connect() has returned but which the listener has
    // not accepted yet — `UnixPendingAccept`. `unix_stream_connect` sets
    // TCP_ESTABLISHED before returning 0, so Linux is EINVAL here too.
    let cs = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    if cs < 0 { close(ls); unlink(cpath.as_ptr()); return report(name, false); }
    if raw_connect(cs, &addr, alen) != 0 {
        dbg1(b"[uls] connect failed errno=%d\n\0", get_errno() as i64);
        close(cs); close(ls); unlink(cpath.as_ptr());
        return report(name, false);
    }
    let e_rc = raw_listen(cs, 8);
    let e_errno = get_errno();
    dbg2(b"[uls] (e) pending-accept listen rc=%d errno=%d (want -1 22)\n\0",
         e_rc as i64, e_errno as i64);

    // (f) The accepted socket — `UnixConnected`, TCP_ESTABLISHED.
    let asf = raw_accept(ls);
    if asf < 0 {
        dbg1(b"[uls] accept failed errno=%d\n\0", get_errno() as i64);
        close(cs); close(ls); unlink(cpath.as_ptr());
        return report(name, false);
    }
    let f_rc = raw_listen(asf, 8);
    let f_errno = get_errno();
    dbg2(b"[uls] (f) accepted-socket listen rc=%d errno=%d (want -1 22)\n\0",
         f_rc as i64, f_errno as i64);

    // (h) The listener is untouched by all of the above and still accepts.
    let cs2 = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    let h_conn = if cs2 >= 0 { raw_connect(cs2, &addr, alen) } else { -1 };
    let as2 = if h_conn == 0 { raw_accept(ls) } else { -1 };
    dbg2(b"[uls] (h) second connect=%d accept=%d (want 0 and >=0)\n\0",
         h_conn as i64, as2 as i64);

    let ok = a_rc == -1 && a_errno == EINVAL
        && g_rc == -1 && g_errno == EOPNOTSUPP
        && b_rc == 0 && c_rc == 0
        && d_ok
        && e_rc == -1 && e_errno == EINVAL
        && f_rc == -1 && f_errno == EINVAL
        && h_conn == 0 && as2 >= 0;

    if as2 >= 0 { close(as2); }
    if cs2 >= 0 { close(cs2); }
    close(asf); close(cs); close(ls);
    unlink(cpath.as_ptr());
    report(name, ok)
}

/// 64 concurrent socketpairs + 32 concurrent bound listeners, each passing its
/// own byte — proves the 16→512 socket / 16→512 bound-path / 32→256 conn caps.
unsafe fn test_many_socketpairs_and_listeners() -> bool {
    let name = b"many_socketpairs_and_listeners\0";
    let mut ok = true;

    const NP: usize = 64;
    let mut sp = [[0i32; 2]; NP];
    for k in 0..NP {
        if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sp[k].as_mut_ptr()) != 0 {
            dbg1(b"[many] socketpair %d failed\n\0", k as i64); ok = false; break;
        }
    }
    if ok {
        for k in 0..NP {
            let byte = (k & 0xFF) as u8;
            let w = raw_send(sp[k][0], &byte, 1, 0);
            let mut rb = [0u8; 1];
            let r = raw_recv(sp[k][1], rb.as_mut_ptr(), 1, 0);
            if w != 1 || r != 1 || rb[0] != byte { dbg1(b"[many] pair %d data mismatch\n\0", k as i64); ok = false; break; }
        }
    }
    // NOTE: the 64 socketpairs stay open across the listener phase below, so at
    // peak this process holds 64*2 + 32*3 = 224 socket fds and 64+32 = 96 live
    // connections at once — past the old 16-socket / 32-conn caps in both.

    const NL: usize = 32;
    let mut ls = [0i32; NL];
    let mut cs = [0i32; NL];
    let mut asf = [0i32; NL];
    let mut path = [[0u8; 32]; NL];
    if ok {
        for k in 0..NL {
            let n = build_name(&mut path[k], b"/tmp/scmL", k);
            let (addr, alen) = sockaddr_un::from_path(&path[k][..n]);
            ls[k] = raw_socket(AF_UNIX, SOCK_STREAM, 0);
            if ls[k] < 0 || raw_bind(ls[k], &addr, alen) != 0 || raw_listen(ls[k], 8) != 0 {
                dbg1(b"[many] listener %d setup failed\n\0", k as i64); ok = false; break;
            }
            cs[k] = raw_socket(AF_UNIX, SOCK_STREAM, 0);
            if raw_connect(cs[k], &addr, alen) != 0 { dbg1(b"[many] connect %d failed\n\0", k as i64); ok = false; break; }
            asf[k] = raw_accept(ls[k]);
            if asf[k] < 0 { dbg1(b"[many] accept %d failed\n\0", k as i64); ok = false; break; }
        }
    }
    if ok {
        for k in 0..NL {
            let byte = (0x80 | (k & 0x3F)) as u8;
            let w = raw_send(cs[k], &byte, 1, 0);
            let mut rb = [0u8; 1];
            let r = raw_recv(asf[k], rb.as_mut_ptr(), 1, 0);
            if w != 1 || r != 1 || rb[0] != byte { dbg1(b"[many] listener %d data mismatch\n\0", k as i64); ok = false; break; }
        }
    }
    for k in 0..NL {
        if asf[k] > 2 { close(asf[k]); }
        if cs[k] > 2 { close(cs[k]); }
        if ls[k] > 2 { close(ls[k]); }
        let mut cp = [0u8; 32];
        let _ = build_name(&mut cp, b"/tmp/scmL", k);
        unlink(cp.as_ptr());
    }
    // Now tear down the socketpairs held open across the whole listener phase.
    for k in 0..NP { if sp[k][0] > 2 { close(sp[k][0]); } if sp[k][1] > 2 { close(sp[k][1]); } }
    report(name, ok)
}

/// The K1 tmpfs mounts exist at boot with the right type + modes.
unsafe fn test_tmpfs_mounts_exist() -> bool {
    let name = b"tmpfs_mounts_exist\0";
    let (r1, m1) = raw_stat_mode(b"/dev/shm\0".as_ptr());
    let shm_ok = r1 == 0 && (m1 & S_IFMT) == S_IFDIR && (m1 & 0o7777) == 0o1777;
    let (r2, m2) = raw_stat_mode(b"/run/user/0\0".as_ptr());
    let run_ok = r2 == 0 && (m2 & S_IFMT) == S_IFDIR && (m2 & 0o7777) == 0o700;
    dbg2(b"[mounts] /dev/shm perms=0%o /run/user/0 perms=0%o\n\0", (m1 & 0o7777) as i64, (m2 & 0o7777) as i64);
    report(name, shm_ok && run_ok)
}

/// A MAP_SHARED file under /dev/shm, opened by NAME in two processes, aliases
/// the same physical pages (the K1-B VMO freebie under the new mount).
unsafe fn test_devshm_shared_mmap() -> bool {
    let name = b"devshm_shared_mmap\0";
    let path = b"/dev/shm/scmtest_shared\0";
    let pa = |i: usize| -> u8 { (0xA0usize ^ (i & 0xFF)) as u8 };
    let pb = |i: usize| -> u8 { (0x5Cusize ^ (i & 0xFF)) as u8 };

    let fd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { dbg1(b"[devshm] open failed errno=%d\n\0", get_errno() as i64); return report(name, false); }
    if raw_ftruncate(fd, 4096) != 0 { dbg0(b"[devshm] ftruncate failed\n\0"); close(fd); unlink(path.as_ptr()); return report(name, false); }
    let m = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if m as usize == MAP_FAILED { dbg0(b"[devshm] mmap failed\n\0"); close(fd); unlink(path.as_ptr()); return report(name, false); }
    for i in 0..4096usize { *m.add(i) = pa(i); }

    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
        munmap(m, 4096); close(fd); unlink(path.as_ptr()); return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);

    let pid = fork();
    if pid == 0 {
        close(a);
        let cfd = open(path.as_ptr(), O_RDWR, 0);
        if cfd < 0 { raw_send(b, b"K".as_ptr(), 1, 0); exit(2); }
        let cm = mmap(core::ptr::null_mut(), 4096, PROT_READ | PROT_WRITE, MAP_SHARED, cfd, 0);
        if cm as usize == MAP_FAILED { raw_send(b, b"K".as_ptr(), 1, 0); exit(3); }
        let mut a_ok = true;
        for i in 0..4096usize { if *cm.add(i) != pa(i) { a_ok = false; break; } }
        if !a_ok { raw_send(b, b"K".as_ptr(), 1, 0); exit(4); }
        for i in 0..4096usize { *cm.add(i) = pb(i); }
        raw_send(b, b"K".as_ptr(), 1, 0);
        exit(0);
    }

    let mut ack = [0u8; 1];
    let _ = raw_recv(a, ack.as_mut_ptr(), 1, 0);
    let mut status: i32 = -1;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    let mut b_ok = true;
    for i in 0..4096usize { if *m.add(i) != pb(i) { b_ok = false; break; } }
    dbg1(b"[devshm] child status=%d\n\0", status as i64);

    munmap(m, 4096); close(fd); close(a); close(b);
    unlink(path.as_ptr());
    report(name, status == 0 && b_ok)
}

/// The per-connection in-flight SCM_RIGHTS fd cap: repeatedly send an fd
/// without receiving; the send that would exceed the cap fails with
/// ETOOMANYREFS rather than growing the queue (or OOMing) without bound.
unsafe fn test_queued_fd_cap() -> bool {
    let name = b"queued_fd_cap\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 { return report(name, false); }
    let (a, b) = (sv[0], sv[1]);
    let path = b"/tmp/scmtest_capfd\0";
    let fd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { close(a); close(b); return report(name, false); }
    write(fd, b"x".as_ptr(), 1);

    let mut sent = 0i64;
    let mut hit_errno = 0i32;
    let mut i = 0;
    while i < 2000 {
        let r = send_fd_and_byte(a, fd, b'.');
        if r < 0 { hit_errno = get_errno(); break; }
        sent += 1;
        i += 1;
    }
    dbg2(b"[cap] sent=%d then errno=%d (want ETOOMANYREFS=109)\n\0", sent, hit_errno as i64);
    let ok = hit_errno == ETOOMANYREFS && sent >= 512;

    close(fd); close(a); close(b);
    unlink(path.as_ptr());
    report(name, ok)
}

/// H3 regression: a saturated stream send ring must answer EAGAIN, not a bogus
/// "sent 0 bytes". The plain (no-SCM) UnixConnected/UnixPendingAccept send path
/// used to `val_reply(0)` when the 4096-byte UnixRing was full. `net_blocking_op`
/// only retries on -11, so a 0 return reached libwayland, which treats it as
/// "flushed 0, tail unadvanced" and busy-loops in wl_connection_flush — the M4
/// "slow-vs-stuck" livelock, and a plausible perturbation of the panel's
/// first-frame window. Fill the ring with MSG_DONTWAIT writes (never draining the
/// peer) and assert the writer eventually gets -1/EAGAIN and NEVER a 0 for a
/// len>0 send.
unsafe fn test_full_ring_eagain() -> bool {
    let name = b"full_ring_eagain\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 { return report(name, false); }
    let (a, b) = (sv[0], sv[1]);
    // Never drain `b`; keep writing to `a` until ring_ab saturates. 256-byte
    // chunks so the final short write exercises the partial path (len.min(free)).
    // The direction grows on demand to net's RING_MAX (212992, Linux's
    // net.core.wmem_default) and must then report EAGAIN, never 0.
    const RING_MAX: isize = 212992;
    let buf = [b'Z'; 256];
    let mut total: isize = 0;
    let mut got_eagain = false;
    let mut bogus_zero = false;
    // RING_MAX / 256 = 832 full chunks; loop well past that.
    for _ in 0..2048 {
        let r = raw_send(a, buf.as_ptr(), buf.len(), MSG_DONTWAIT);
        if r < 0 {
            if get_errno() == EAGAIN { got_eagain = true; }
            break;
        } else if r == 0 {
            // len>0 send returned 0 → the H3 bug (livelocks a blocking caller).
            bogus_zero = true;
            break;
        } else {
            total += r;
        }
    }
    dbg2(b"[fre] total=%d eagain=%d (want 212992 then EAGAIN, never 0)\n\0",
         total as i64, got_eagain as i64);
    let ok = got_eagain && !bogus_zero && total == RING_MAX;
    close(a); close(b);
    report(name, ok)
}

/// The pure decider for the half-close defect, with no fork, no exec and no
/// epoll in the way.
///
/// `handle_shutdown` in servers/net/src/lib.rs took `how` and never read it: for
/// a connected AF_UNIX socket it set the end's "this end is fully gone" flag —
/// the one close(2) only sets once the last dup'd alias is released — and then
/// stored an empty SockEntry over the caller's fd slot. That made
/// `shutdown(fd, SHUT_WR)` strictly more destructive than `close(fd)`. Linux
/// half-closes one *direction* of the connection: the fd stays a valid socket,
/// the peer's two directions are untouched, and the peer reads EOF rather than
/// an error.
unsafe fn test_socketpair_shutdown_wr_half_close() -> bool {
    let name = b"socketpair_shutdown_wr_half_close\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"[shwr] socketpair failed\n\0");
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);

    // Exactly what tokio's OwnedWriteHalf::drop does.
    let sh = raw_shutdown(a, SHUT_WR);
    dbg2(b"[shwr] shutdown(a, SHUT_WR)=%d errno=%d (want 0)\n\0", sh as i64, get_errno() as i64);

    // The peer's write direction is untouched.
    let wb = write(b, b"hi".as_ptr(), 2);
    dbg2(b"[shwr] write(b)=%d errno=%d (want 2)\n\0", wb as i64, get_errno() as i64);

    // ...and so is our read direction: the peer's bytes still arrive.
    let mut buf = [0u8; 8];
    let ra = read(a, buf.as_mut_ptr(), 2);
    dbg2(b"[shwr] read(a)=%d errno=%d (want 2)\n\0", ra as i64, get_errno() as i64);

    // Our write direction is the one that is gone — EPIPE, never EBADF.
    let wa = write(a, b"x".as_ptr(), 1);
    let wa_errno = get_errno();
    dbg2(b"[shwr] write(a)=%d errno=%d (want -1 / EPIPE 32)\n\0", wa as i64, wa_errno as i64);

    // The peer sees end-of-stream, not an error.
    let rb = read(b, buf.as_mut_ptr().add(4), 1);
    dbg2(b"[shwr] read(b)=%d errno=%d (want 0, EOF)\n\0", rb as i64, get_errno() as i64);

    let ok = sh == 0
        && wb == 2
        && ra == 2 && &buf[..2] == &b"hi"[..]
        && wa == -1 && wa_errno == EPIPE
        && rb == 0;
    close(a); close(b);
    report(name, ok)
}

/// A half-closed fd is still a socket, and epoll has to keep saying so.
///
/// When shutdown emptied the caller's fd slot, NET_POLL answered EBADF,
/// kernel/src/syscall.rs turned that into POLLNVAL (0x20) with no edge-seq, and
/// mio decodes POLLNVAL as an empty readiness set — so no waker ever fires and
/// the awaiting task hangs instead of erroring. A hang is exactly the symptom
/// the cosmic-session handshake shows, which is why POLLNVAL is asserted
/// against explicitly here rather than only asserting that POLLIN arrives.
unsafe fn test_shutdown_wr_keeps_fd_pollable() -> bool {
    let name = b"shutdown_wr_keeps_fd_pollable\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"[shpo] socketpair failed\n\0");
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);
    let sh = raw_shutdown(a, SHUT_WR);

    let ep = xret(syscall1(SYS_EPOLL_CREATE1, 0)) as i32;
    if ep < 0 {
        dbg1(b"[shpo] epoll_create1 failed errno=%d\n\0", get_errno() as i64);
        close(a); close(b);
        return report(name, false);
    }
    let mut evbuf = [0u8; EPOLL_EVENT_SIZE];
    evbuf[..4].copy_from_slice(&(EPOLLIN | EPOLLOUT).to_le_bytes());
    evbuf[EPOLL_EVENT_DATA_OFF..EPOLL_EVENT_DATA_OFF + 8]
        .copy_from_slice(&(a as u64).to_le_bytes());
    let ctl = xret(syscall4(SYS_EPOLL_CTL, ep as usize, EPOLL_CTL_ADD, a as usize,
                            evbuf.as_mut_ptr() as usize));

    // The peer writes, so the half-closed end must come back readable.
    let wb = write(b, b"hi".as_ptr(), 2);

    let mut out = [0u8; EPOLL_EVENT_SIZE];
    let nready = xret(syscall4(SYS_EPOLL_WAIT, ep as usize, out.as_mut_ptr() as usize, 1, 2000));
    let revents = u32::from_le_bytes([out[0], out[1], out[2], out[3]]);

    dbg2(b"[shpo] shutdown=%d epoll_ctl=%d\n\0", sh as i64, ctl as i64);
    dbg2(b"[shpo] write(b)=%d epoll_wait=%d (want 2 and 1)\n\0", wb as i64, nready as i64);
    dbg1(b"[shpo] revents=0x%x (POLLIN 0x1 wanted; POLLNVAL 0x20 is the defect)\n\0",
         revents as i64);

    let ok = sh == 0 && ctl == 0 && wb == 2 && nready == 1
        && (revents & EPOLLIN) != 0 && (revents & POLLNVAL) == 0;
    close(ep); close(a); close(b);
    report(name, ok)
}

/// A full AF_INET TCP round-trip over 127.0.0.1.
///
/// `bind("127.0.0.1:0")` was the whole reported bug. bind() stored a zero port,
/// and listen() rejected a zero `bound_port` with EINVAL — which mio/tokio
/// report as "bind failed: Invalid argument", because `TcpListener::bind` is
/// socket + setsockopt + bind + listen in one call. Underneath that, the
/// smoltcp integration had no loopback interface at all: the only interface was
/// the virtio NIC on 10.0.2.15/24, so nothing could ever carry a 127.0.0.1
/// packet even once the ports were right.
///
/// This walks the sequence by hand so a failure says which half broke: bind to
/// the ephemeral port, read it back with getsockname (the only way to learn
/// it), connect a second socket to 127.0.0.1:<that port>, accept, then pass a
/// payload in each direction.
unsafe fn test_inet_loopback_tcp() -> bool {
    let name = b"inet_loopback_tcp\0";
    let srv = raw_socket(AF_INET, SOCK_STREAM, 0);
    if srv < 0 {
        dbg1(b"[inet] socket(AF_INET) failed errno=%d\n\0", get_errno() as i64);
        return report(name, false);
    }

    let ba = sockaddr_in::new([127, 0, 0, 1], 0);
    if raw_bind_in(srv, &ba) != 0 {
        dbg1(b"[inet] bind 127.0.0.1:0 failed errno=%d (want 0)\n\0", get_errno() as i64);
        close(srv);
        return report(name, false);
    }

    // getsockname must report AF_INET, 127.0.0.1 and the assigned, non-zero port.
    let mut sa = sockaddr_in::new([0, 0, 0, 0], 0);
    let mut slen: u32 = 16;
    if raw_getsockname(srv, &mut sa, &mut slen) != 0 {
        dbg1(b"[inet] getsockname failed errno=%d\n\0", get_errno() as i64);
        close(srv);
        return report(name, false);
    }
    let port = u16::from_be(sa.sin_port);
    let addr_ok = sa.sin_family == AF_INET as u16 && sa.sin_addr == [127, 0, 0, 1];
    dbg2(b"[inet] getsockname port=%d addr_ok=%d (want port!=0 and 1)\n\0",
         port as i64, addr_ok as i64);
    if port == 0 || !addr_ok { close(srv); return report(name, false); }

    if raw_listen(srv, 8) != 0 {
        dbg1(b"[inet] listen failed errno=%d (this was the EINVAL)\n\0", get_errno() as i64);
        close(srv);
        return report(name, false);
    }

    let cli = raw_socket(AF_INET, SOCK_STREAM, 0);
    if cli < 0 { close(srv); return report(name, false); }
    let ca = sockaddr_in::new([127, 0, 0, 1], port);
    if raw_connect_in(cli, &ca) != 0 {
        dbg1(b"[inet] connect failed errno=%d\n\0", get_errno() as i64);
        close(srv); close(cli);
        return report(name, false);
    }

    // connect() only queues a SYN; the handshake completes on the net daemon's
    // next poll, so accept answers EAGAIN until then.
    let mut acc = -1;
    let mut tries = 0;
    while tries < 100 {
        sleep_ms(20);
        acc = xret(syscall3(SYS_ACCEPT, srv as usize, 0, 0)) as i32;
        if acc >= 0 || get_errno() != EAGAIN { break; }
        tries += 1;
    }
    if acc < 0 {
        dbg2(b"[inet] accept failed after %d tries errno=%d\n\0", tries as i64, get_errno() as i64);
        close(srv); close(cli);
        return report(name, false);
    }

    // client → server
    let msg = b"hello-inet";
    let sn = raw_send(cli, msg.as_ptr(), msg.len(), 0);
    let mut rbuf = [0u8; 32];
    let rn = inet_recv_retry(acc, rbuf.as_mut_ptr(), rbuf.len());
    let c2s_ok = sn == msg.len() as isize && rn == msg.len() as isize
                 && &rbuf[..msg.len()] == &msg[..];

    // server → client, so the reverse direction is proven too
    let reply = b"ack-inet";
    let sn2 = raw_send(acc, reply.as_ptr(), reply.len(), 0);
    let mut rbuf2 = [0u8; 32];
    let rn2 = inet_recv_retry(cli, rbuf2.as_mut_ptr(), rbuf2.len());
    let s2c_ok = sn2 == reply.len() as isize && rn2 == reply.len() as isize
                 && &rbuf2[..reply.len()] == &reply[..];

    dbg2(b"[inet] c2s=%d s2c=%d (want 1 1)\n\0", c2s_ok as i64, s2c_ok as i64);
    close(srv); close(cli); close(acc);
    report(name, c2s_ok && s2c_ok)
}

/// `listen()` on an already-listening socket must succeed, as on Linux.
///
/// Linux's `inet_listen` accepts a repeat listen and only updates the backlog.
/// Ours matched `SockState::InetBound` alone, so the second call fell through
/// to `_ => err_reply(-22)` — measured `second_errno=22` — which any framework
/// that re-arms its listener trips on.
///
/// Three assertions, ordered so that a wrong fix fails a specific one:
///   (a) listen() before bind() is *still* EINVAL, so the fix cannot be "make
///       listen() always succeed".
///   (b) the repeat listen() returns 0.
///   (c) the listener still accepts afterwards. This is the assertion that
///       catches the careless fix: re-running the listen path on the repeat
///       call would add a second pair of smoltcp sockets on the same port and
///       orphan the handles the first listen() stored — which returns 0 and
///       then never completes a handshake.
unsafe fn test_inet_listen_twice() -> bool {
    let name = b"inet_listen_twice\0";

    // (a) A socket that never bound has no port, so listen() is EINVAL.
    let nb = raw_socket(AF_INET, SOCK_STREAM, 0);
    if nb < 0 { return report(name, false); }
    let nb_rc = raw_listen(nb, 8);
    let nb_errno = get_errno();
    close(nb);
    dbg2(b"[l2] listen-before-bind rc=%d errno=%d (want -1 22)\n\0",
         nb_rc as i64, nb_errno as i64);
    if nb_rc != -1 || nb_errno != EINVAL { return report(name, false); }

    let srv = raw_socket(AF_INET, SOCK_STREAM, 0);
    if srv < 0 { return report(name, false); }
    let ba = sockaddr_in::new([127, 0, 0, 1], 0);
    if raw_bind_in(srv, &ba) != 0 {
        dbg1(b"[l2] bind failed errno=%d\n\0", get_errno() as i64);
        close(srv);
        return report(name, false);
    }
    // The ephemeral port is discoverable only through getsockname.
    let mut sa = sockaddr_in::new([0, 0, 0, 0], 0);
    let mut slen: u32 = 16;
    if raw_getsockname(srv, &mut sa, &mut slen) != 0 { close(srv); return report(name, false); }
    let port = u16::from_be(sa.sin_port);
    if port == 0 { close(srv); return report(name, false); }

    if raw_listen(srv, 8) != 0 {
        dbg1(b"[l2] first listen failed errno=%d\n\0", get_errno() as i64);
        close(srv);
        return report(name, false);
    }

    // (b) The repeat listen, with a different backlog — this was the EINVAL.
    let second = raw_listen(srv, 16);
    let second_errno = get_errno();
    dbg2(b"[l2] second listen rc=%d errno=%d (want 0)\n\0",
         second as i64, second_errno as i64);

    // (c) The socket the first listen() armed must still be the live listener.
    let cli = raw_socket(AF_INET, SOCK_STREAM, 0);
    if cli < 0 { close(srv); return report(name, false); }
    let ca = sockaddr_in::new([127, 0, 0, 1], port);
    let conn = raw_connect_in(cli, &ca);
    let mut acc = -1;
    let mut tries = 0;
    while tries < 100 {
        sleep_ms(20);
        acc = xret(syscall3(SYS_ACCEPT, srv as usize, 0, 0)) as i32;
        if acc >= 0 || get_errno() != EAGAIN { break; }
        tries += 1;
    }
    dbg2(b"[l2] connect=%d accept=%d (want 0 and >=0)\n\0", conn as i64, acc as i64);

    let ok = second == 0 && conn == 0 && acc >= 0;
    close(srv); close(cli);
    if acc >= 0 { close(acc); }
    report(name, ok)
}

/// recv with the same poll-cadence-aware retry the accept loop uses: a segment
/// sent into a smoltcp socket only leaves on the daemon's next poll.
unsafe fn inet_recv_retry(fd: i32, buf: *mut u8, len: usize) -> isize {
    let mut tries = 0;
    while tries < 100 {
        sleep_ms(20);
        let r = raw_recv(fd, buf, len, MSG_DONTWAIT);
        if r >= 0 || get_errno() != EAGAIN { return r; }
        tries += 1;
    }
    -1
}

/// An SCM_RIGHTS import that fails with EMFILE must
/// release the descriptor EXACTLY once.
///
/// `import_fd` used to release the transfer's reference itself before returning
/// -EMFILE, while `handle_recvmsg` set `fit = i` and its overflow loop then
/// `drop_transfer`ed `fds[i]` a second time — two `release_vnode` calls for one
/// `export_fd`. The extra one is taken out of a *live* holder's reference.
///
/// A pipe makes that observable with no second process. The read end of a pipe
/// whose write end is still open must answer EAGAIN on an empty ring, never 0
/// (`handle_read`: `if r.writers > 0 { -11 } else { 0 }`). The double release
/// drives `writers` 2 -> 1 -> 0 while this process still holds the write fd, so
/// the read end reports a phantom EOF. Steps:
///   1. pipe2(O_NONBLOCK); read the empty ring — EAGAIN. This control proves
///      the final assertion can distinguish the two outcomes at all.
///   2. Send the write end over a socketpair: writers = 2 (export_fd's ref).
///   3. dup() until the fd table is full, so the import must fail with EMFILE.
///      Socket fds live above SOCK_FD_BASE and are not in that table, so the
///      socketpair survives the exhaustion.
///   4. recvmsg. The fd is dropped and MSG_CTRUNC is set either way — that is
///      correct behaviour, not the thing under test.
///   5. Free the table and re-read the still-empty pipe. MUST be EAGAIN.
///
/// One fd is enough: `fit` is 1, the import fails at i = 0, and `for j in 0..1`
/// re-drops that single descriptor. A batch larger than the free capacity is
/// not needed to reach the bug.
unsafe fn test_scm_import_emfile_single_release() -> bool {
    let name = b"scm_import_emfile_single_release\0";
    let mut p = [0i32; 2];
    if pipe2(p.as_mut_ptr(), O_NONBLOCK) != 0 {
        dbg0(b"[emfile] pipe2 failed\n\0");
        return report(name, false);
    }
    let (rd, wr) = (p[0], p[1]);
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
        dbg0(b"[emfile] socketpair failed\n\0");
        close(rd); close(wr);
        return report(name, false);
    }
    let (a, b) = (sv[0], sv[1]);

    // Control: empty ring, write end open → EAGAIN, not EOF.
    let mut byte = [0u8; 1];
    let pre = read(rd, byte.as_mut_ptr(), 1);
    let pre_errno = get_errno();

    let sret = send_fd_and_byte(a, wr, b'E');

    // Saturate the fd table: dup until EMFILE, whatever the per-process
    // limit is (it was 256, is 512 now). The bound only stops a kernel that
    // never says EMFILE from looping forever; it is far above any limit.
    const HOG_CAP: usize = 8192;
    static mut HOGS: [i32; HOG_CAP] = [-1; HOG_CAP];
    let hogs = &mut *core::ptr::addr_of_mut!(HOGS);
    let mut nhogs = 0usize;
    let mut hog_errno = 0i32;
    while nhogs < HOG_CAP {
        let d = dup(rd);
        if d < 0 { hog_errno = get_errno(); break; }
        hogs[nhogs] = d;
        nhogs += 1;
    }

    // The import must fail (table full); no fd is delivered and the cmsg
    // truncates. Both are true with and without the fix.
    let (n, mflags, rfd, _clen) = recv_fd_and_byte(b, 32, 0);
    if rfd >= 0 { close(rfd); }
    let ctrunc = (mflags & MSG_CTRUNC) != 0;

    let mut j = 0usize;
    while j < nhogs { close(hogs[j]); j += 1; }

    // THE assertion. This process never closed `wr`, so the ring still has a
    // writer and an empty read must be EAGAIN.
    let post = read(rd, byte.as_mut_ptr(), 1);
    let post_errno = get_errno();

    dbg2(b"[emfile] send=%d hogs=%d (want >0, >0 then EMFILE)\n\0", sret as i64, nhogs as i64);
    dbg2(b"[emfile] dup errno=%d recv n=%d (want 24, 1)\n\0", hog_errno as i64, n as i64);
    dbg2(b"[emfile] rfd=%d ctrunc=%d (want -1, 1)\n\0", rfd as i64, ctrunc as i64);
    dbg2(b"[emfile] pre  ret=%d errno=%d (want -1, 11 EAGAIN)\n\0", pre as i64, pre_errno as i64);
    dbg2(b"[emfile] post ret=%d errno=%d (want -1, 11; 0/0 = DOUBLE RELEASE)\n\0",
         post as i64, post_errno as i64);

    let ok = sret > 0
        && nhogs > 0 && hog_errno == EMFILE
        && n == 1 && rfd < 0 && ctrunc
        && pre == -1 && pre_errno == EAGAIN
        && post == -1 && post_errno == EAGAIN;

    close(a); close(b); close(rd); close(wr);
    report(name, ok)
}

/// A closed TCP connection must hold its port in TIME_WAIT.
///
/// `handle_close` used to `socket_set.remove()` the socket and clear the fd
/// slot, and nothing anywhere remembered the port — so a server could be
/// restarted onto the port it had just been serving on, instantly, where Linux
/// answers EADDRINUSE for 2*MSL (`TCP_TIMEWAIT_LEN`, a fixed 60 s). What is
/// modelled is the port reservation, not the protocol state; nothing lingers to
/// absorb a late segment.
///
/// The port that matters is the *accepted* socket's, not the listener's: an
/// accepted socket shares the listener's local port, and it is that reservation
/// which makes a restarted server fail to rebind on Linux. Closing the listener
/// reserves nothing.
///
/// Five assertions. Exactly ONE of them, (b), reads differently against an
/// unpatched kernel — there the rebind returns 0 instead of -1/EADDRINUSE.
/// Delete the `if let Some(p) = park { time_wait_add(p); }` line at the end of
/// the `InetConnected` arm of `handle_close` and (b) flips back to 0. The other
/// four are shape guards that pass at HEAD too and are deliberately NOT counted
/// as evidence the fix is live:
///   (c) SO_REUSEADDR must lift the reservation — otherwise adding TIME_WAIT
///       would break the restart it models, since a real server sets that
///       option precisely so it *can* rebind.
///   (d) an unrelated bind-to-zero still gets a port, so the fix cannot be
///       "refuse binds".
///   (e) a listener that never carried a connection frees its port at once; a
///       fix that parked every closed port would fail here while still
///       passing (b).
unsafe fn test_tcp_time_wait() -> bool {
    let name = b"tcp_time_wait\0";

    // (a) Set up and complete one real connection over 127.0.0.1.
    let srv = raw_socket(AF_INET, SOCK_STREAM, 0);
    if srv < 0 { return report(name, false); }
    let ba = sockaddr_in::new([127, 0, 0, 1], 0);
    if raw_bind_in(srv, &ba) != 0 {
        dbg1(b"[tw] bind failed errno=%d\n\0", get_errno() as i64);
        close(srv);
        return report(name, false);
    }
    let mut sa = sockaddr_in::new([0, 0, 0, 0], 0);
    let mut slen: u32 = 16;
    if raw_getsockname(srv, &mut sa, &mut slen) != 0 { close(srv); return report(name, false); }
    let port = u16::from_be(sa.sin_port);
    if port == 0 || raw_listen(srv, 8) != 0 { close(srv); return report(name, false); }

    let cli = raw_socket(AF_INET, SOCK_STREAM, 0);
    if cli < 0 { close(srv); return report(name, false); }
    let ca = sockaddr_in::new([127, 0, 0, 1], port);
    if raw_connect_in(cli, &ca) != 0 {
        dbg1(b"[tw] connect failed errno=%d\n\0", get_errno() as i64);
        close(srv); close(cli);
        return report(name, false);
    }
    // accept() only succeeds once smoltcp reports Established, so reaching here
    // is what proves the connection this test then closes was a real one.
    let mut acc = -1;
    let mut tries = 0;
    while tries < 100 {
        sleep_ms(20);
        acc = xret(syscall3(SYS_ACCEPT, srv as usize, 0, 0)) as i32;
        if acc >= 0 || get_errno() != EAGAIN { break; }
        tries += 1;
    }
    if acc < 0 {
        dbg2(b"[tw] accept failed after %d tries errno=%d\n\0", tries as i64, get_errno() as i64);
        close(srv); close(cli);
        return report(name, false);
    }

    // The server side closes first — the active close, the only side that goes
    // through TIME_WAIT. Its local port is `port`.
    close(acc);
    close(cli);
    close(srv);

    // (b) THE assertion: the port is reserved, so a plain rebind is refused.
    let rb = raw_socket(AF_INET, SOCK_STREAM, 0);
    if rb < 0 { return report(name, false); }
    let pa = sockaddr_in::new([127, 0, 0, 1], port);
    let b_rc = raw_bind_in(rb, &pa);
    let b_errno = get_errno();
    close(rb);
    dbg2(b"[tw] (b) rebind rc=%d errno=%d (want -1 98; 0 = NO TIME_WAIT)\n\0",
         b_rc as i64, b_errno as i64);

    // (c) …but SO_REUSEADDR takes it anyway, as it does on Linux.
    let ru = raw_socket(AF_INET, SOCK_STREAM, 0);
    if ru < 0 { return report(name, false); }
    let so = set_reuseaddr(ru);
    let c_rc = raw_bind_in(ru, &pa);
    let c_errno = get_errno();
    close(ru);
    dbg2(b"[tw] (c) setsockopt=%d reuse-rebind rc=%d (want 0 0)\n\0",
         so as i64, c_rc as i64);
    if c_rc != 0 { dbg1(b"[tw] (c) errno=%d\n\0", c_errno as i64); }

    // (d) An unrelated automatic bind still works and does not collide.
    let other = raw_socket(AF_INET, SOCK_STREAM, 0);
    if other < 0 { return report(name, false); }
    let d_rc = raw_bind_in(other, &ba);
    let mut sa2 = sockaddr_in::new([0, 0, 0, 0], 0);
    let mut slen2: u32 = 16;
    let d_gs = raw_getsockname(other, &mut sa2, &mut slen2);
    let other_port = u16::from_be(sa2.sin_port);
    close(other);
    dbg2(b"[tw] (d) fresh bind rc=%d port=%d (want 0 and != reserved)\n\0",
         d_rc as i64, other_port as i64);

    // (e) A listener that never carried a connection reserves nothing.
    let ls = raw_socket(AF_INET, SOCK_STREAM, 0);
    if ls < 0 { return report(name, false); }
    let mut e_ok = false;
    let mut sa3 = sockaddr_in::new([0, 0, 0, 0], 0);
    let mut slen3: u32 = 16;
    if raw_bind_in(ls, &ba) == 0
       && raw_getsockname(ls, &mut sa3, &mut slen3) == 0
       && raw_listen(ls, 8) == 0 {
        let lport = u16::from_be(sa3.sin_port);
        close(ls);
        let ls2 = raw_socket(AF_INET, SOCK_STREAM, 0);
        if ls2 >= 0 {
            let la = sockaddr_in::new([127, 0, 0, 1], lport);
            let e_rc = raw_bind_in(ls2, &la);
            dbg2(b"[tw] (e) idle-listener port=%d rebind rc=%d (want 0)\n\0",
                 lport as i64, e_rc as i64);
            e_ok = e_rc == 0;
            close(ls2);
        }
    } else {
        close(ls);
    }

    let ok = b_rc == -1 && b_errno == EADDRINUSE
        && so == 0 && c_rc == 0
        && d_rc == 0 && d_gs == 0 && other_port != 0 && other_port != port
        && e_ok;
    report(name, ok)
}

// ── AF_INET: unconnected UDP, datagram msghdr, TCP EOF and connect errors ──
//
// Firefox's first network page load needed each of these. Every case runs over
// 127.0.0.1, so none of them needs the NIC.

#[cfg(target_arch = "aarch64")] const SYS_PPOLL: usize = 73;
#[cfg(target_arch = "x86_64")]  const SYS_PPOLL: usize = 271;
const POLLIN_: i16 = 0x1;
const POLLOUT_: i16 = 0x4;
const MSG_TRUNC: i32 = 0x20;

/// poll(2) on one fd with a millisecond timeout; returns (ret, revents).
unsafe fn poll1(fd: i32, events: i16, ms: i64) -> (isize, i16) {
    let mut pfd: [i32; 2] = [fd, (events as u16 as i32)]; // struct pollfd {int; short; short}
    let ts: [i64; 2] = [ms / 1000, (ms % 1000) * 1_000_000];
    let r = xret(syscall6(SYS_PPOLL, pfd.as_mut_ptr() as usize, 1, ts.as_ptr() as usize, 0, 8, 0));
    (r, (pfd[1] >> 16) as i16)
}

unsafe fn raw_sendto_in(fd: i32, buf: *const u8, len: usize, to: *const sockaddr_in) -> isize {
    xret(syscall6(nr::SENDTO, fd as usize, buf as usize, len, 0, to as usize, 16))
}
unsafe fn raw_recvfrom_in(fd: i32, buf: *mut u8, len: usize, from: *mut sockaddr_in, flen: *mut u32) -> isize {
    xret(syscall6(nr::RECVFROM, fd as usize, buf as usize, len, MSG_DONTWAIT as usize,
                  from as usize, flen as usize))
}
/// recvfrom with a bounded wait: loopback packets move on the net daemon's
/// 100 Hz poll.
unsafe fn recvfrom_retry(fd: i32, buf: *mut u8, len: usize, from: *mut sockaddr_in, flen: *mut u32) -> isize {
    for _ in 0..100 {
        let r = raw_recvfrom_in(fd, buf, len, from, flen);
        if r >= 0 || get_errno() != EAGAIN { return r; }
        sleep_ms(20);
    }
    -1
}
unsafe fn local_port(fd: i32) -> u16 {
    let mut sa = sockaddr_in::new([0, 0, 0, 0], 0);
    let mut l: u32 = 16;
    if raw_getsockname(fd, &mut sa, &mut l) != 0 { return 0; }
    u16::from_be(sa.sin_port)
}

/// sendto() on a UDP socket that was never connected, then recvfrom() of the
/// answer. Both used to fail: sendto on an unconnected socket was EPIPE (only
/// connect() created the smoltcp socket) and recvfrom was EBADF. musl's DNS
/// resolver is exactly this sequence, so no host name ever resolved.
unsafe fn test_udp_unconnected() -> bool {
    let name = b"udp_unconnected\0";
    let srv = raw_socket(AF_INET, SOCK_DGRAM, 0);
    let ba = sockaddr_in::new([127, 0, 0, 1], 0);
    let b = raw_bind_in(srv, &ba);
    let sport = local_port(srv);
    let cli = raw_socket(AF_INET, SOCK_DGRAM, 0);
    // Fresh UDP socket: writable before anything else happens.
    let (_, rev0) = poll1(cli, POLLOUT_, 0);
    let to = sockaddr_in::new([127, 0, 0, 1], sport);
    let sn = raw_sendto_in(cli, b"query".as_ptr(), 5, &to);
    let sn_errno = if sn < 0 { get_errno() } else { 0 };
    let cport = local_port(cli);

    let mut buf = [0u8; 32];
    let mut from = sockaddr_in::new([0, 0, 0, 0], 0);
    let mut flen: u32 = 16;
    let rn = recvfrom_retry(srv, buf.as_mut_ptr(), buf.len(), &mut from, &mut flen);
    let got_q = rn == 5 && &buf[..5] == b"query"
        && u16::from_be(from.sin_port) == cport && from.sin_addr == [127, 0, 0, 1];

    // Answer to the source address recvfrom reported.
    let sn2 = raw_sendto_in(srv, b"answer".as_ptr(), 6, &from);
    let mut buf2 = [0u8; 32];
    let mut from2 = sockaddr_in::new([0, 0, 0, 0], 0);
    let mut flen2: u32 = 16;
    let rn2 = recvfrom_retry(cli, buf2.as_mut_ptr(), buf2.len(), &mut from2, &mut flen2);
    let got_a = rn2 == 6 && &buf2[..6] == b"answer" && u16::from_be(from2.sin_port) == sport;

    dbg2(b"[udp] bind=%d server port=%d\n\0", b as i64, sport as i64);
    dbg2(b"[udp] fresh poll revents=0x%x sendto=%d\n\0", rev0 as i64, sn as i64);
    dbg2(b"[udp] sendto errno=%d (EPIPE 32 was the bug) client port=%d\n\0", sn_errno as i64, cport as i64);
    dbg2(b"[udp] query recvd=%d answer recvd=%d (want 1 1)\n\0", got_q as i64, got_a as i64);
    close(srv); close(cli);
    report(name, b == 0 && sport != 0 && (rev0 & POLLOUT_) != 0 && sn == 5 && cport != 0
                 && got_q && sn2 == 6 && got_a)
}

/// sendmsg()/recvmsg() on an AF_INET datagram socket: the iovecs are one
/// datagram, msg_name is its destination/source, and a datagram longer than
/// the buffer comes back cut with MSG_TRUNC. The old code sent each iovec as
/// its own datagram and ignored msg_name (Firefox's QUIC uses exactly this).
unsafe fn test_udp_msghdr() -> bool {
    let name = b"udp_msghdr\0";
    let srv = raw_socket(AF_INET, SOCK_DGRAM, 0);
    let ba = sockaddr_in::new([127, 0, 0, 1], 0);
    raw_bind_in(srv, &ba);
    let sport = local_port(srv);
    let cli = raw_socket(AF_INET, SOCK_DGRAM, 0);

    let mut to = sockaddr_in::new([127, 0, 0, 1], sport);
    let mut p1 = *b"head-";
    let mut p2 = *b"tail";
    let mut iov = [iovec { iov_base: p1.as_mut_ptr(), iov_len: 5 },
                   iovec { iov_base: p2.as_mut_ptr(), iov_len: 4 }];
    let mut mh: msghdr = core::mem::zeroed();
    mh.msg_name = &mut to as *mut sockaddr_in as *mut u8;
    mh.msg_namelen = 16;
    mh.msg_iov = iov.as_mut_ptr();
    mh.msg_iovlen = 2;
    let sn = raw_sendmsg(cli, &mh, 0);
    let cport = local_port(cli);

    // Receive it scattered over two iovecs, with the source in msg_name.
    let mut r1 = [0u8; 3];
    let mut r2 = [0u8; 16];
    let mut from = sockaddr_in::new([0, 0, 0, 0], 0);
    let mut riov = [iovec { iov_base: r1.as_mut_ptr(), iov_len: 3 },
                    iovec { iov_base: r2.as_mut_ptr(), iov_len: 16 }];
    let mut rh: msghdr = core::mem::zeroed();
    rh.msg_name = &mut from as *mut sockaddr_in as *mut u8;
    rh.msg_namelen = 16;
    rh.msg_iov = riov.as_mut_ptr();
    rh.msg_iovlen = 2;
    let mut rn = -1;
    for _ in 0..100 {
        rn = raw_recvmsg(srv, &mut rh, MSG_DONTWAIT);
        if rn >= 0 || get_errno() != EAGAIN { break; }
        sleep_ms(20);
    }
    let one = rn == 9 && &r1 == b"hea" && &r2[..6] == b"d-tail"
        && rh.msg_namelen == 16 && u16::from_be(from.sin_port) == cport
        && (rh.msg_flags & MSG_TRUNC) == 0;
    // Nothing else queued: the 9 bytes were a single datagram.
    let extra = raw_recv(srv, r2.as_mut_ptr(), 16, MSG_DONTWAIT);

    // A 10-byte datagram into a 4-byte buffer: 4 bytes and MSG_TRUNC.
    let to2 = sockaddr_in::new([127, 0, 0, 1], sport);
    raw_sendto_in(cli, b"0123456789".as_ptr(), 10, &to2);
    let mut small = [0u8; 4];
    let mut siov = iovec { iov_base: small.as_mut_ptr(), iov_len: 4 };
    let mut sh: msghdr = core::mem::zeroed();
    sh.msg_iov = &mut siov;
    sh.msg_iovlen = 1;
    let mut tn = -1;
    for _ in 0..100 {
        tn = raw_recvmsg(srv, &mut sh, MSG_DONTWAIT);
        if tn >= 0 || get_errno() != EAGAIN { break; }
        sleep_ms(20);
    }
    let trunc = tn == 4 && &small == b"0123" && (sh.msg_flags & MSG_TRUNC) != 0;

    dbg2(b"[udpmsg] sendmsg=%d recvmsg=%d (want 9 9)\n\0", sn as i64, rn as i64);
    dbg2(b"[udpmsg] one datagram+name=%d extra=%d (want 1 -1)\n\0", one as i64, extra as i64);
    dbg2(b"[udpmsg] truncated recvmsg=%d ok=%d (want 4 1)\n\0", tn as i64, trunc as i64);
    close(srv); close(cli);
    report(name, sn == 9 && one && extra < 0 && trunc)
}

/// A loopback TCP listener + one accepted connection, the client end first.
unsafe fn tcp_pair() -> Option<(i32, i32, i32)> {
    let srv = raw_socket(AF_INET, SOCK_STREAM, 0);
    let ba = sockaddr_in::new([127, 0, 0, 1], 0);
    if raw_bind_in(srv, &ba) != 0 || raw_listen(srv, 4) != 0 { close(srv); return None; }
    let port = local_port(srv);
    let cli = raw_socket(AF_INET, SOCK_STREAM, 0);
    let ca = sockaddr_in::new([127, 0, 0, 1], port);
    if raw_connect_in(cli, &ca) != 0 { close(srv); close(cli); return None; }
    for _ in 0..100 {
        let acc = xret(syscall3(SYS_ACCEPT, srv as usize, 0, 0)) as i32;
        if acc >= 0 { return Some((cli, acc, srv)); }
        if get_errno() != EAGAIN { break; }
        sleep_ms(20);
    }
    close(srv); close(cli);
    None
}

/// The peer answering and closing (HTTP/1.0, `Connection: close`) is EOF:
/// after the data, recv() returns 0 and poll() reports POLLIN. It used to be
/// EAGAIN forever and no POLLIN — the socket sat in CloseWait, which counted
/// as "still active" — and close() itself never sent a FIN at all.
unsafe fn test_tcp_peer_close_eof() -> bool {
    let name = b"tcp_peer_close_eof\0";
    let Some((cli, acc, srv)) = tcp_pair() else {
        dbg0(b"[tcpeof] no connection\n\0");
        return report(name, false);
    };
    let sn = raw_send(acc, b"bye".as_ptr(), 3, 0);
    close(acc);
    let mut buf = [0u8; 16];
    let rn = inet_recv_retry(cli, buf.as_mut_ptr(), buf.len());
    let (pr, rev) = poll1(cli, POLLIN_, 3000);
    let eof = raw_recv(cli, buf.as_mut_ptr(), buf.len(), MSG_DONTWAIT);
    let eof_errno = if eof < 0 { get_errno() } else { 0 };
    dbg2(b"[tcpeof] send=%d recv=%d (want 3 3)\n\0", sn as i64, rn as i64);
    dbg2(b"[tcpeof] poll=%d revents=0x%x (want 1, POLLIN)\n\0", pr as i64, rev as i64);
    dbg2(b"[tcpeof] recv after close=%d errno=%d (want 0; EAGAIN 11 was the bug)\n\0",
         eof as i64, eof_errno as i64);
    close(cli); close(srv);
    report(name, sn == 3 && rn == 3 && &buf[..3] == b"bye" && pr == 1 && (rev & POLLIN_) != 0 && eof == 0)
}

#[cfg(target_arch = "aarch64")] const SYS_GETSOCKOPT: usize = 209;
#[cfg(target_arch = "x86_64")]  const SYS_GETSOCKOPT: usize = 55;
const POLLERR_: i16 = 0x8;
const ECONNREFUSED: i32 = 111;
const EINPROGRESS: i32 = 115;
const SOCK_NONBLOCK: i32 = 0x800;
const SO_ERROR: i32 = 4;

/// connect() to a port nobody listens on. Blocking: -1/ECONNREFUSED (it used
/// to return 0 at once, before any answer). Non-blocking: EINPROGRESS, then
/// poll reports POLLERR|POLLOUT and SO_ERROR reads ECONNREFUSED once.
unsafe fn test_tcp_connect_refused() -> bool {
    let name = b"tcp_connect_refused\0";
    // A port bound but never listened on: nothing accepts its SYN. It stays
    // bound for the whole test, so no connect below can draw it as its own
    // ephemeral source port (a closed probe's port was free again, and a
    // connect in the same tick picked it and connected to itself).
    let probe = raw_socket(AF_INET, SOCK_STREAM, 0);
    let ba = sockaddr_in::new([127, 0, 0, 1], 0);
    raw_bind_in(probe, &ba);
    let port = local_port(probe);
    let to = sockaddr_in::new([127, 0, 0, 1], port);

    let a = raw_socket(AF_INET, SOCK_STREAM, 0);
    let ra = raw_connect_in(a, &to);
    let ea = if ra < 0 { get_errno() } else { 0 };
    close(a);

    let b = raw_socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    let rb = raw_connect_in(b, &to);
    let eb = if rb < 0 { get_errno() } else { 0 };
    let (pr, rev) = poll1(b, POLLOUT_, 3000);
    let mut soerr: i32 = -1;
    let mut sl: u32 = 4;
    let g = xret(syscall6(SYS_GETSOCKOPT, b as usize, SOL_SOCKET as usize, SO_ERROR as usize,
                          &mut soerr as *mut i32 as usize, &mut sl as *mut u32 as usize, 0));
    let mut soerr2: i32 = -1;
    let mut sl2: u32 = 4;
    xret(syscall6(SYS_GETSOCKOPT, b as usize, SOL_SOCKET as usize, SO_ERROR as usize,
                  &mut soerr2 as *mut i32 as usize, &mut sl2 as *mut u32 as usize, 0));
    close(b);

    // And a connect that succeeds is still 0 for a blocking socket, with the
    // connection already established when it returns.
    let ok_pair = tcp_pair();
    let ok = ok_pair.is_some();
    if let Some((c, x, s)) = ok_pair { close(c); close(x); close(s); }
    close(probe);

    dbg2(b"[refused] blocking connect=%d errno=%d (want -1 111; 0 was the bug)\n\0", ra as i64, ea as i64);
    dbg2(b"[refused] nonblocking connect=%d errno=%d (want -1 115)\n\0", rb as i64, eb as i64);
    dbg2(b"[refused] poll=%d revents=0x%x (want 1, POLLERR|POLLOUT)\n\0", pr as i64, rev as i64);
    dbg2(b"[refused] SO_ERROR=%d then %d (want 111 then 0)\n\0", soerr as i64, soerr2 as i64);
    report(name, port != 0 && ra < 0 && ea == ECONNREFUSED
                 && rb < 0 && eb == EINPROGRESS
                 && pr == 1 && (rev & POLLERR_) != 0 && (rev & POLLOUT_) != 0
                 && g == 0 && soerr == ECONNREFUSED && soerr2 == 0 && ok)
}

// ── AF_UNIX in-flight fd lifetime (lane unixdebt) ───────────────────────────

const MSG_PEEK: i32 = 0x02;
const EAGAIN: i32 = 11;
const EBADF: i32 = 9;

/// sendmsg `data` with `fd` attached (SCM_RIGHTS).
unsafe fn send_fd_data(sockfd: i32, fd: i32, data: &[u8]) -> isize {
    let mut cbuf = CmsgBuf { b: [0u8; 32] };
    let clen = build_fd_cmsg(&mut cbuf.b, fd);
    let mut iov = iovec { iov_base: data.as_ptr() as *mut u8, iov_len: data.len() };
    let mut mh: msghdr = core::mem::zeroed();
    mh.msg_iov = &mut iov;
    mh.msg_iovlen = 1;
    mh.msg_control = cbuf.b.as_mut_ptr();
    mh.msg_controllen = clen;
    raw_sendmsg(sockfd, &mh, 0)
}

/// recvmsg into `buf` with room for one fd. Returns (bytes or -1, fd or -1,
/// msg_flags).
unsafe fn recv_data_fd(sockfd: i32, buf: &mut [u8], flags: i32) -> (isize, i32, i32) {
    let mut cbuf = CmsgBuf { b: [0u8; 32] };
    let mut iov = iovec { iov_base: buf.as_mut_ptr(), iov_len: buf.len() };
    let mut mh: msghdr = core::mem::zeroed();
    mh.msg_iov = &mut iov;
    mh.msg_iovlen = 1;
    mh.msg_control = cbuf.b.as_mut_ptr();
    mh.msg_controllen = 32;
    let n = raw_recvmsg(sockfd, &mut mh, flags);
    let ch: cmsghdr = core::ptr::read(cbuf.b.as_ptr() as *const cmsghdr);
    let found = n >= 0 && mh.msg_controllen >= core::mem::size_of::<cmsghdr>()
        && ch.cmsg_level == SOL_SOCKET && ch.cmsg_type == SCM_RIGHTS;
    let off = cmsg_data_off();
    let fd = if found { i32::from_ne_bytes(cbuf.b[off..off + 4].try_into().unwrap()) } else { -1 };
    (n, fd, mh.msg_flags)
}

/// A non-blocking pipe: (read end, write end).
unsafe fn nb_pipe() -> Option<(i32, i32)> {
    let mut p = [0i32; 2];
    if pipe2(p.as_mut_ptr(), O_NONBLOCK) != 0 { return None; }
    Some((p[0], p[1]))
}

/// True when the pipe read end `r` reads EOF: no write end is left anywhere,
/// in a process or in a socket queue. EAGAIN means one is still open.
unsafe fn pipe_at_eof(r: i32) -> bool {
    let mut b = [0u8; 8];
    loop {
        let n = read(r, b.as_mut_ptr(), b.len());
        if n > 0 { continue; } // drain whatever was written
        if n < 0 { dbg1(b"[unixgc] pipe read errno=%ld\n\0", get_errno() as i64); }
        return n == 0;
    }
}

/// Unix GC 1: an end queued for itself. socketpair(a, b), send b over a (so b
/// sits in b's own receive queue, next to a pipe write end), close both. Only
/// the queue keeps b alive, so without the collector the connection and the
/// pipe end leaked for good. The loop runs past MAX_CONNS (512): a leak runs
/// out of connections long before the end. The pipe proves that the queued
/// fds were closed and not only forgotten.
unsafe fn test_unix_gc_self_cycle() -> bool {
    let name = b"unix_gc_self_cycle\0";
    const ROUNDS: usize = 600;
    let mut ok = true;
    let Some((pr, pw)) = nb_pipe() else { return report(name, false) };
    for i in 0..ROUNDS {
        let mut sv = [0i32; 2];
        if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 {
            dbg2(b"[unixgc] self: socketpair failed at round %ld errno=%ld\n\0", i as i64, get_errno() as i64);
            ok = false; break;
        }
        let (a, b) = (sv[0], sv[1]);
        if i == 0 && send_fd_data(a, pw, b"p") != 1 { dbg0(b"[unixgc] self: send pipe failed\n\0"); ok = false; }
        if send_fd_data(a, b, b"s") != 1 { dbg1(b"[unixgc] self: send self failed at %ld\n\0", i as i64); ok = false; }
        if i == 0 { close(pw); }
        // Alternate the close order: either end may be the last process ref.
        if i % 2 == 0 { close(a); close(b); } else { close(b); close(a); }
        if !ok { break; }
    }
    if ok && !pipe_at_eof(pr) { dbg0(b"[unixgc] self: queued pipe end still open\n\0"); ok = false; }
    close(pr);
    report(name, ok)
}

/// Unix GC 2: two connections, each queued for the other. b2 sits in b1's
/// receive queue and b1 in b2's. Neither is in a cycle on its own, so the
/// close-time purge does not free them; only reachability does. Each round
/// holds two connections, so 300 rounds also pass MAX_CONNS.
unsafe fn test_unix_gc_two_conn_cycle() -> bool {
    let name = b"unix_gc_two_conn_cycle\0";
    const ROUNDS: usize = 300;
    let mut ok = true;
    let Some((pr, pw)) = nb_pipe() else { return report(name, false) };
    for i in 0..ROUNDS {
        let mut s1 = [0i32; 2];
        let mut s2 = [0i32; 2];
        if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, s1.as_mut_ptr()) != 0
            || raw_socketpair(AF_UNIX, SOCK_STREAM, 0, s2.as_mut_ptr()) != 0 {
            dbg2(b"[unixgc] pair: socketpair failed at round %ld errno=%ld\n\0", i as i64, get_errno() as i64);
            ok = false; break;
        }
        if i == 0 && send_fd_data(s1[0], pw, b"p") != 1 { ok = false; }
        if send_fd_data(s1[0], s2[1], b"x") != 1 || send_fd_data(s2[0], s1[1], b"y") != 1 {
            dbg1(b"[unixgc] pair: send failed at %ld\n\0", i as i64); ok = false;
        }
        if i == 0 { close(pw); }
        close(s1[0]); close(s2[0]); close(s1[1]); close(s2[1]);
        if !ok { break; }
    }
    if ok && !pipe_at_eof(pr) { dbg0(b"[unixgc] pair: queued pipe end still open\n\0"); ok = false; }
    close(pr);
    report(name, ok)
}

/// Unix GC 3: the collector must not take what a process can still receive.
/// y is queued for itself and also for c1, which this process holds. Once
/// x and y are closed, y's references are all in flight, but c1's queue
/// still reaches it. Receive y from c1, and y's own queue must still hold
/// its byte and fd.
unsafe fn test_unix_gc_keeps_reachable() -> bool {
    let name = b"unix_gc_keeps_reachable\0";
    let mut c = [0i32; 2];
    let mut xy = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, c.as_mut_ptr()) != 0
        || raw_socketpair(AF_UNIX, SOCK_STREAM, 0, xy.as_mut_ptr()) != 0 {
        return report(name, false);
    }
    let (x, y) = (xy[0], xy[1]);
    let mut ok = true;
    let mut step = 0i64;
    let mut check = |cnd: bool, s: &mut i64| { *s += 1; if !cnd && ok { dbg1(b"[unixgc] keep: failed at step %ld\n\0", *s); ok = false; } };
    check(send_fd_data(x, y, b"Q") == 1, &mut step);      // 1 y queued for y
    check(send_fd_data(c[0], y, b"R") == 1, &mut step);   // 2 y queued for c1
    close(y);                                             // every y reference is in flight now
    close(x);                                             // a close: the collector runs
    let mut b = [0u8; 4];
    let (n, y1, _) = recv_data_fd(c[1], &mut b, 0);
    check(n == 1 && b[0] == b'R' && y1 >= 0, &mut step);  // 3 y arrives through c1
    let (n2, y2, _) = if y1 >= 0 { recv_data_fd(y1, &mut b, 0) } else { (-1, -1, 0) };
    check(n2 == 1 && b[0] == b'Q' && y2 >= 0, &mut step); // 4 its own queue survived
    if y1 >= 0 { check(read(y1, b.as_mut_ptr(), 4) == 0, &mut step); } // 5 x closed: EOF
    if y2 >= 0 { close(y2); }
    if y1 >= 0 { close(y1); }
    close(c[0]); close(c[1]);
    report(name, ok)
}

/// read()/recv() with no control buffer on a stream that has fds queued.
/// Linux (unix_stream_read_generic + scm_recv) gives the bytes, closes the fds
/// that ride with them, and ends the read at the end of the bytes that
/// carried them. Before this, the fds stayed queued. The next recvmsg then
/// got them with unrelated bytes, or they leaked if none came.
unsafe fn test_read_discards_fds() -> bool {
    let name = b"read_discards_fds\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()) != 0 { return report(name, false); }
    let (a, b) = (sv[0], sv[1]);
    let mut ok = true;
    let mut step = 0i64;
    let mut check = |cnd: bool, s: &mut i64| { *s += 1; if !cnd && ok { dbg1(b"[rdfds] failed at step %ld\n\0", *s); ok = false; } };
    let mut buf = [0u8; 16];

    // A whole batch read by read(): bytes delivered, fd closed, stop at its end.
    let Some((pr, pw)) = nb_pipe() else { return report(name, false) };
    check(send_fd_data(a, pw, b"AB") == 2, &mut step);                     // 1
    close(pw);
    check(write(a, b"CD".as_ptr(), 2) == 2, &mut step);                     // 2
    let n = read(b, buf.as_mut_ptr(), buf.len());
    check(n == 2 && &buf[..2] == b"AB", &mut step);                         // 3 stops at the batch end
    check(pipe_at_eof(pr), &mut step);                                      // 4 the fd was closed
    close(pr);
    let (n, fd, _) = recv_data_fd(b, &mut buf, 0);
    check(n == 2 && &buf[..2] == b"CD" && fd < 0, &mut step);               // 5 no stale fd later

    // A read that starts before the batch and ends inside it, then plain
    // bytes after it.
    let Some((pr, pw)) = nb_pipe() else { return report(name, false) };
    check(write(a, b"xy".as_ptr(), 2) == 2, &mut step);                     // 6
    check(send_fd_data(a, pw, b"zw") == 2, &mut step);                      // 7
    close(pw);
    check(write(a, b"uv".as_ptr(), 2) == 2, &mut step);                     // 8
    check(raw_recv(b, buf.as_mut_ptr(), 1, 0) == 1 && buf[0] == b'x', &mut step); // 9
    check(raw_recv(b, buf.as_mut_ptr(), 2, 0) == 2 && &buf[..2] == b"yz", &mut step); // 10 into the batch
    check(pipe_at_eof(pr), &mut step);                                      // 11 first byte read: fd closed
    close(pr);
    // The rest of that batch has no fds left, so it joins the plain bytes
    // after it, as on Linux.
    let (n, fd, _) = recv_data_fd(b, &mut buf, 0);
    check(n == 3 && &buf[..3] == b"wuv" && fd < 0, &mut step);              // 12 no fd
    check(raw_recv(b, buf.as_mut_ptr(), 16, MSG_DONTWAIT) < 0 && get_errno() == EAGAIN, &mut step); // 13

    // recvmsg with no control buffer: MSG_CTRUNC, fd closed.
    let Some((pr, pw)) = nb_pipe() else { return report(name, false) };
    check(send_fd_data(a, pw, b"m") == 1, &mut step);                       // 14
    close(pw);
    {
        let mut iov = iovec { iov_base: buf.as_mut_ptr(), iov_len: buf.len() };
        let mut mh: msghdr = core::mem::zeroed();
        mh.msg_iov = &mut iov; mh.msg_iovlen = 1;
        let n = raw_recvmsg(b, &mut mh, 0);
        check(n == 1 && mh.msg_flags & MSG_CTRUNC != 0, &mut step);         // 15
    }
    check(pipe_at_eof(pr), &mut step);                                      // 16
    close(pr);

    // MSG_PEEK: nothing is consumed. recv(MSG_PEEK) leaves the fd queued.
    // recvmsg(MSG_PEEK) installs a duplicate. The real recvmsg then gets the
    // fd once more.
    let Some((pr, pw)) = nb_pipe() else { return report(name, false) };
    check(send_fd_data(a, pw, b"pk") == 2, &mut step);                      // 17
    close(pw);
    check(write(a, b"zz".as_ptr(), 2) == 2, &mut step);                     // 18
    check(raw_recv(b, buf.as_mut_ptr(), 16, MSG_PEEK) == 2 && &buf[..2] == b"pk", &mut step); // 19
    check(!pipe_at_eof(pr), &mut step);                                     // 20 still queued
    let (n, p1, _) = recv_data_fd(b, &mut buf, MSG_PEEK);
    check(n == 2 && &buf[..2] == b"pk" && p1 >= 0, &mut step);              // 21 peeked dup
    let (n, p2, _) = recv_data_fd(b, &mut buf, 0);
    check(n == 2 && &buf[..2] == b"pk" && p2 >= 0 && p2 != p1, &mut step);  // 22 the real one
    check(p1 >= 0 && p2 >= 0 && write(p1, b"1".as_ptr(), 1) == 1
          && write(p2, b"2".as_ptr(), 1) == 1, &mut step);                  // 23 both write the pipe
    if p1 >= 0 { close(p1); }
    check(!pipe_at_eof(pr), &mut step);                                     // 24 p2 still open
    if p2 >= 0 { close(p2); }
    check(pipe_at_eof(pr), &mut step);                                      // 25
    close(pr);
    check(raw_recv(b, buf.as_mut_ptr(), 16, MSG_PEEK) == 2 && &buf[..2] == b"zz", &mut step); // 26
    check(raw_recv(b, buf.as_mut_ptr(), 16, 0) == 2 && &buf[..2] == b"zz", &mut step);        // 27
    check(raw_recv(b, buf.as_mut_ptr(), 16, MSG_DONTWAIT) < 0 && get_errno() == EAGAIN, &mut step); // 28

    close(a); close(b);
    report(name, ok)
}

/// How many aliases `test_exec_prunes_many_aliases` makes: more than the old
/// fixed limit of 16 in `vfs::prune_sock_aliases`.
const PRUNE_ALIASES: i32 = 24;
const PRUNE_BASE: i32 = 120;

/// Helper mode for the test below, after the self-execve. Every alias was
/// close-on-exec, so all of them must be gone. A leftover alias is easiest
/// to catch once its old socket slot is taken again, so new sockets are made
/// first. A dangling alias would then answer F_GETFD for one of them.
unsafe fn alias_prune_helper(n: i32) -> ! {
    let mut sv = [0i32; 2];
    for _ in 0..(n + 8) { let _ = raw_socketpair(AF_UNIX, SOCK_STREAM, 0, sv.as_mut_ptr()); }
    let mut left = 0;
    for i in 0..n {
        let r = raw_fcntl(PRUNE_BASE + i, F_GETFD, 0);
        if r >= 0 || get_errno() != EBADF {
            dbg1(b"[aliasprune:helper] fd %ld survived exec\n\0", (PRUNE_BASE + i) as i64);
            left += 1;
        }
    }
    exit(left);
}

/// execve drops every close-on-exec socket alias, however many there are.
/// `prune_sock_aliases` used to collect at most 16 per exec, and the rest
/// stayed as VFS entries naming socket slots that were already closed.
unsafe fn test_exec_prunes_many_aliases() -> bool {
    let name = b"exec_prunes_many_aliases\0";
    let mut sv = [0i32; 2];
    if raw_socketpair(AF_UNIX, SOCK_STREAM | O_CLOEXEC_FL, 0, sv.as_mut_ptr()) != 0 {
        return report(name, false);
    }
    let mut envbuf = [0u8; 48];
    build_name(&mut envbuf, b"SCMTEST_ALIAS_PRUNE=", PRUNE_ALIASES as usize);
    let pid = fork();
    if pid == 0 {
        for i in 0..PRUNE_ALIASES {
            if dup3(sv[0], PRUNE_BASE + i, O_CLOEXEC_FL) != PRUNE_BASE + i {
                dbg1(b"[aliasprune:child] dup3 %ld failed\n\0", i as i64);
                exit(100);
            }
        }
        let path = b"/bin/scmtest\0";
        let av: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
        let ev: [*const u8; 2] = [envbuf.as_ptr(), core::ptr::null()];
        syscall3(SYS_EXECVE, path.as_ptr() as usize, av.as_ptr() as usize, ev.as_ptr() as usize);
        exit(101);
    }
    close(sv[0]); close(sv[1]);
    let mut status = -1i32;
    wait4(pid, &mut status, 0, core::ptr::null_mut());
    let code = (status >> 8) & 0xff;
    if code != 0 { dbg1(b"[aliasprune] child exit %ld\n\0", code as i64); }
    report(name, status & 0x7f == 0 && code == 0)
}

/// Unix GC 4: a cycle through a listener's backlog. Client C connects to
/// listener L and, before any accept(), sends L over C. That queues L for
/// the embryonic end, which only an accept() on L could ever reach. Once L's
/// fd is closed, L is kept alive only by that queue, so it is garbage, as
/// on Linux. The address must stop resolving (connect: ECONNREFUSED) and be
/// free for a new bind, even with C still open. Without the collector the
/// listener stays bound until C goes away. A second case queues L for a
/// socket the process still holds too: that copy reaches L, so L, its
/// backlog and what is queued there must all survive.
unsafe fn test_unix_gc_listener_backlog() -> bool {
    let name = b"unix_gc_listener_backlog\0";
    let (addr, alen) = sockaddr_un::from_abstract(b"scmtest-gc-backlog");
    let mut ok = true;
    for i in 0..20 {
        let l = raw_socket(AF_UNIX, SOCK_STREAM, 0);
        if l < 0 || raw_bind(l, &addr, alen) != 0 || raw_listen(l, 4) != 0 {
            dbg2(b"[unixgc] backlog: bind failed at round %ld errno=%ld\n\0", i as i64, get_errno() as i64);
            ok = false; if l >= 0 { close(l); } break;
        }
        let c = raw_socket(AF_UNIX, SOCK_STREAM, 0);
        if raw_connect(c, &addr, alen) != 0 || send_fd_data(c, l, b"L") != 1 {
            dbg1(b"[unixgc] backlog: connect/send failed at round %ld\n\0", i as i64);
            ok = false; close(l); close(c); break;
        }
        close(l);
        let d = raw_socket(AF_UNIX, SOCK_STREAM, 0);
        let r = raw_connect(d, &addr, alen);
        if r == 0 || get_errno() != ECONNREFUSED {
            dbg2(b"[unixgc] backlog: connect after close=%ld errno=%ld (want -1 111)\n\0", r as i64, get_errno() as i64);
            ok = false;
        }
        close(d); close(c);
        if !ok { break; }
    }

    // Reachable: L is queued in its own backlog and also for q.
    let (raddr, ralen) = sockaddr_un::from_abstract(b"scmtest-gc-backlog-r");
    let mut pq = [0i32; 2];
    let l = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    let c = raw_socket(AF_UNIX, SOCK_STREAM, 0);
    let mut step = 0i64;
    let mut check = |cnd: bool, s: &mut i64| { *s += 1; if !cnd && ok { dbg1(b"[unixgc] backlog keep: failed at step %ld\n\0", *s); ok = false; } };
    check(raw_socketpair(AF_UNIX, SOCK_STREAM, 0, pq.as_mut_ptr()) == 0, &mut step);   // 1
    check(l >= 0 && raw_bind(l, &raddr, ralen) == 0 && raw_listen(l, 4) == 0, &mut step); // 2
    check(raw_connect(c, &raddr, ralen) == 0, &mut step);                             // 3
    check(send_fd_data(c, l, b"B") == 1, &mut step);                                  // 4 L in its backlog
    check(send_fd_data(pq[0], l, b"Q") == 1, &mut step);                              // 5 L queued for q
    close(l);                                                                          // the collector runs
    let mut b = [0u8; 4];
    let (n, l1, _) = recv_data_fd(pq[1], &mut b, 0);
    check(n == 1 && b[0] == b'Q' && l1 >= 0, &mut step);                              // 6 L arrives
    let acc = if l1 >= 0 { raw_accept(l1) } else { -1 };
    check(acc >= 0, &mut step);                                                        // 7 backlog intact
    let (n2, l2, _) = if acc >= 0 { recv_data_fd(acc, &mut b, 0) } else { (-1, -1, 0) };
    check(n2 == 1 && b[0] == b'B' && l2 >= 0, &mut step);                             // 8 its queue too
    for fd in [l2, acc, l1, c, pq[0], pq[1]] { if fd >= 0 { close(fd); } }
    report(name, ok)
}

unsafe fn raw_recvfrom_flags(fd: i32, buf: *mut u8, len: usize, flags: i32) -> isize {
    xret(syscall6(nr::RECVFROM, fd as usize, buf as usize, len, flags as usize, 0, 0))
}

/// Wait (up to 2 s) for data on an AF_INET socket, by peeking.
unsafe fn inet_peek_retry(fd: i32, buf: *mut u8, len: usize) -> isize {
    for _ in 0..100 {
        let r = raw_recvfrom_flags(fd, buf, len, MSG_PEEK | MSG_DONTWAIT);
        if r >= 0 || get_errno() != EAGAIN { return r; }
        sleep_ms(20);
    }
    -1
}

/// MSG_PEEK on AF_INET returns the data and leaves it queued. It used to be
/// ignored: the "peek" consumed the bytes and the real read found nothing.
unsafe fn test_inet_msg_peek() -> bool {
    let name = b"inet_msg_peek\0";
    let mut ok = true;
    let mut step = 0i64;
    let mut check = |cnd: bool, s: &mut i64| { *s += 1; if !cnd && ok { dbg1(b"[inetpeek] failed at step %ld\n\0", *s); ok = false; } };
    let mut buf = [0u8; 16];

    // TCP over loopback.
    match tcp_pair() {
        None => check(false, &mut step),                                              // 1
        Some((cli, acc, srv)) => {
            check(raw_send(acc, b"peekme".as_ptr(), 6, 0) == 6, &mut step);           // 1
            check(inet_peek_retry(cli, buf.as_mut_ptr(), 16) == 6 && &buf[..6] == b"peekme", &mut step); // 2
            buf = [0u8; 16];
            check(raw_recvfrom_flags(cli, buf.as_mut_ptr(), 3, MSG_PEEK) == 3 && &buf[..3] == b"pee", &mut step); // 3
            buf = [0u8; 16];
            check(raw_recvfrom_flags(cli, buf.as_mut_ptr(), 16, 0) == 6 && &buf[..6] == b"peekme", &mut step); // 4 still there
            check(raw_recvfrom_flags(cli, buf.as_mut_ptr(), 16, MSG_DONTWAIT) < 0 && get_errno() == EAGAIN, &mut step); // 5
            close(cli); close(acc); close(srv);
        }
    }

    // UDP: recvfrom(MSG_PEEK), then recvmsg(MSG_PEEK) (the msghdr path).
    let srv = raw_socket(AF_INET, SOCK_DGRAM, 0);
    let ba = sockaddr_in::new([127, 0, 0, 1], 0);
    check(raw_bind_in(srv, &ba) == 0, &mut step);                                     // 6
    let to = sockaddr_in::new([127, 0, 0, 1], local_port(srv));
    let cli = raw_socket(AF_INET, SOCK_DGRAM, 0);
    check(raw_sendto_in(cli, b"dgram".as_ptr(), 5, &to) == 5, &mut step);             // 7
    check(inet_peek_retry(srv, buf.as_mut_ptr(), 16) == 5 && &buf[..5] == b"dgram", &mut step); // 8
    {
        let mut b2 = [0u8; 16];
        let mut iov = iovec { iov_base: b2.as_mut_ptr(), iov_len: 16 };
        let mut mh: msghdr = core::mem::zeroed();
        mh.msg_iov = &mut iov; mh.msg_iovlen = 1;
        check(raw_recvmsg(srv, &mut mh, MSG_PEEK) == 5 && &b2[..5] == b"dgram", &mut step); // 9
    }
    buf = [0u8; 16];
    check(raw_recvfrom_flags(srv, buf.as_mut_ptr(), 16, 0) == 5 && &buf[..5] == b"dgram", &mut step); // 10
    check(raw_recvfrom_flags(srv, buf.as_mut_ptr(), 16, MSG_DONTWAIT) < 0 && get_errno() == EAGAIN, &mut step); // 11
    close(cli); close(srv);
    report(name, ok)
}
