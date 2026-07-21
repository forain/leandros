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
use leandros_libc::syscall::{nr, syscall2, syscall3, syscall4, syscall6};

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
const MSG_CMSG_CLOEXEC: i32 = 0x40000000;

const AF_UNIX: i32 = 1;
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

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut failures = 0;

    if !test_fd_pass() { failures += 1; }
    if !test_cmsg_flags() { failures += 1; }
    if !test_shared_memfd_pixels() { failures += 1; }
    if !test_seals() { failures += 1; }

    // ── K1-B VMO tests (single-process + fork; no SCM_RIGHTS needed) ─────────
    if !test_double_mmap_alias() { failures += 1; }
    if !test_read_mmap_coherence() { failures += 1; }
    if !test_big_memfd() { failures += 1; }
    if !test_fork_visibility() { failures += 1; }
    if !test_partial_munmap() { failures += 1; }
    if !test_close_while_mapped() { failures += 1; }
    if !test_ftruncate_grow_shrink() { failures += 1; }
    if !test_teardown_loop() { failures += 1; }

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
