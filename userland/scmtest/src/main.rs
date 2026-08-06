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

// epoll_event wire layout must match the kernel's per-arch struct exactly
// (kernel/src/syscall.rs EPOLL_EVENT_SIZE/EPOLL_EVENT_DATA_OFF): x86_64 uses the
// packed 12-byte form (data at +4); aarch64 the natural 16-byte form (data at +8).
#[cfg(target_arch = "x86_64")]  const EPOLL_EVENT_SIZE:     usize = 12;
#[cfg(target_arch = "x86_64")]  const EPOLL_EVENT_DATA_OFF: usize = 4;
#[cfg(target_arch = "aarch64")] const EPOLL_EVENT_SIZE:     usize = 16;
#[cfg(target_arch = "aarch64")] const EPOLL_EVENT_DATA_OFF: usize = 8;
const EPOLLIN: u32 = 0x0001;
const EPOLL_CTL_ADD: usize = 1;
const F_SETFD: i32 = 2;

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

// ── AF_INET socket wrappers (TODO item 9) ───────────────────────────────────

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

/// Parent side shared by both fork+exec+inherit deciders: epoll_wait(5s) on `b`,
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

    let mut failures = 0;

    if !test_fd_pass() { failures += 1; }
    if !test_fork_child_exit_keeps_socket() { failures += 1; }
    if !test_fork_exec_inherit() { failures += 1; }
    if !test_fork_exec_child_clears_cloexec() { failures += 1; }
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

    // ── K1-C: AF_UNIX VFS socket nodes, tmpfs mounts, cap raise, fd-cap ──────
    if !test_socket_node_roundtrip() { failures += 1; }
    if !test_socket_node_devshm() { failures += 1; }
    if !test_unlink_rebind() { failures += 1; }
    if !test_many_socketpairs_and_listeners() { failures += 1; }
    if !test_tmpfs_mounts_exist() { failures += 1; }
    if !test_devshm_shared_mmap() { failures += 1; }
    if !test_queued_fd_cap() { failures += 1; }
    if !test_full_ring_eagain() { failures += 1; }

    // ── M7u: mincore residency probe (Mesa EGL pointer-dereferenceable signal) ──
    if !test_mincore() { failures += 1; }

    // ── TODO item 9: AF_INET TCP over the loopback interface ────────────────
    if !test_inet_loopback_tcp() { failures += 1; }

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
    let buf = [b'Z'; 256];
    let mut total: isize = 0;
    let mut got_eagain = false;
    let mut bogus_zero = false;
    // 4096-byte ring / 256 = 16 full chunks; loop well past that.
    for _ in 0..256 {
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
    dbg2(b"[fre] total=%d eagain=%d (want ~4096 then EAGAIN, never 0)\n\0",
         total as i64, got_eagain as i64);
    let ok = got_eagain && !bogus_zero && total > 0 && total <= 4096;
    close(a); close(b);
    report(name, ok)
}

/// TODO item 9 regression: a full AF_INET TCP round-trip over 127.0.0.1.
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
