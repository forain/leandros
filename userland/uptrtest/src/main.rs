//! uptrtest — the kernel reading and writing user memory that is valid but
//! not yet mapped.
//!
//! Since wave 2026-09-24 most user memory is lazy: anonymous mmaps, private
//! file mmaps, the main-thread stack below its first pages, and every
//! private page after fork (shared copy-on-write, read-only). A syscall that
//! copies to or from such a page takes the page fault in kernel mode, and
//! the kernel must resolve it exactly as it would a user access — or, for a
//! pointer no access could make valid, return EFAULT — never kill the
//! caller, halt a CPU or leak the lock it was holding at the time. The bug
//! behind this test: a `send()` from a never-touched page faulted inside
//! `UnixRing::write` with the unix-socket table lock held.
//!
//! Each case runs one syscall against a buffer of one kind:
//!   anon   fresh MAP_PRIVATE|MAP_ANONYMOUS pages
//!   file   fresh MAP_PRIVATE pages of an f2fs file (read on fault)
//!   cow    pages the parent filled before fork (shared read-only)
//!   stack  main-thread stack 3 MiB below the stack pointer
//! in a forked child, with a watchdog: a killed child, a wrong result or a
//! child that never finishes (a deadlocked kernel) is a FAIL. For `cow`, the
//! parent checks afterwards that nothing the kernel wrote for the child
//! reached the parent's copy.
//!
//! The `efault_*` cases hand the kernel PROT_NONE, read-only and unmapped
//! buffers and expect -EFAULT with the caller alive; `after_efault` then
//! checks that unix sockets still work (a lock leaked by a killed caller
//! would hang it).
//!
//! Prints "<name>: PASS|FAIL" per case and returns the number of failures.

#![no_std]
#![no_main]
#![allow(non_camel_case_types, dead_code)]

use core::ffi::c_void;

type c_int = i32;

extern "C" {
    fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;
    fn fork() -> c_int;
    fn _exit(status: c_int) -> !;
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset uptr_main",
    "   and rsp, -16",
    "   call relibc_start_v1",
    "   ud2"
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   mov x29, #0",
    "   mov x30, #0",
    "   mov x0, sp",
    "   adrp x1, uptr_main",
    "   add x1, x1, :lo12:uptr_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { _exit(134) }
}

// ── raw syscalls ─────────────────────────────────────────────────────────────

mod nr {
    #[cfg(target_arch = "aarch64")] pub const READ: usize = 63;
    #[cfg(target_arch = "x86_64")]  pub const READ: usize = 0;
    #[cfg(target_arch = "aarch64")] pub const WRITE: usize = 64;
    #[cfg(target_arch = "x86_64")]  pub const WRITE: usize = 1;
    #[cfg(target_arch = "aarch64")] pub const OPENAT: usize = 56;
    #[cfg(target_arch = "x86_64")]  pub const OPENAT: usize = 257;
    #[cfg(target_arch = "aarch64")] pub const CLOSE: usize = 57;
    #[cfg(target_arch = "x86_64")]  pub const CLOSE: usize = 3;
    #[cfg(target_arch = "aarch64")] pub const LSEEK: usize = 62;
    #[cfg(target_arch = "x86_64")]  pub const LSEEK: usize = 8;
    #[cfg(target_arch = "aarch64")] pub const PREAD64: usize = 67;
    #[cfg(target_arch = "x86_64")]  pub const PREAD64: usize = 17;
    #[cfg(target_arch = "aarch64")] pub const PWRITE64: usize = 68;
    #[cfg(target_arch = "x86_64")]  pub const PWRITE64: usize = 18;
    #[cfg(target_arch = "aarch64")] pub const READV: usize = 65;
    #[cfg(target_arch = "x86_64")]  pub const READV: usize = 19;
    #[cfg(target_arch = "aarch64")] pub const WRITEV: usize = 66;
    #[cfg(target_arch = "x86_64")]  pub const WRITEV: usize = 20;
    #[cfg(target_arch = "aarch64")] pub const MMAP: usize = 222;
    #[cfg(target_arch = "x86_64")]  pub const MMAP: usize = 9;
    #[cfg(target_arch = "aarch64")] pub const MUNMAP: usize = 215;
    #[cfg(target_arch = "x86_64")]  pub const MUNMAP: usize = 11;
    #[cfg(target_arch = "aarch64")] pub const MPROTECT: usize = 226;
    #[cfg(target_arch = "x86_64")]  pub const MPROTECT: usize = 10;
    #[cfg(target_arch = "aarch64")] pub const SOCKETPAIR: usize = 199;
    #[cfg(target_arch = "x86_64")]  pub const SOCKETPAIR: usize = 53;
    #[cfg(target_arch = "aarch64")] pub const SENDTO: usize = 206;
    #[cfg(target_arch = "x86_64")]  pub const SENDTO: usize = 44;
    #[cfg(target_arch = "aarch64")] pub const RECVFROM: usize = 207;
    #[cfg(target_arch = "x86_64")]  pub const RECVFROM: usize = 45;
    #[cfg(target_arch = "aarch64")] pub const SENDMSG: usize = 211;
    #[cfg(target_arch = "x86_64")]  pub const SENDMSG: usize = 46;
    #[cfg(target_arch = "aarch64")] pub const RECVMSG: usize = 212;
    #[cfg(target_arch = "x86_64")]  pub const RECVMSG: usize = 47;
    #[cfg(target_arch = "aarch64")] pub const GETSOCKOPT: usize = 209;
    #[cfg(target_arch = "x86_64")]  pub const GETSOCKOPT: usize = 55;
    #[cfg(target_arch = "aarch64")] pub const GETSOCKNAME: usize = 204;
    #[cfg(target_arch = "x86_64")]  pub const GETSOCKNAME: usize = 51;
    #[cfg(target_arch = "aarch64")] pub const PIPE2: usize = 59;
    #[cfg(target_arch = "x86_64")]  pub const PIPE2: usize = 293;
    #[cfg(target_arch = "aarch64")] pub const CLOCK_GETTIME: usize = 113;
    #[cfg(target_arch = "x86_64")]  pub const CLOCK_GETTIME: usize = 228;
    #[cfg(target_arch = "aarch64")] pub const WAIT4: usize = 260;
    #[cfg(target_arch = "x86_64")]  pub const WAIT4: usize = 61;
    #[cfg(target_arch = "aarch64")] pub const KILL: usize = 129;
    #[cfg(target_arch = "x86_64")]  pub const KILL: usize = 62;
    #[cfg(target_arch = "aarch64")] pub const GETCWD: usize = 17;
    #[cfg(target_arch = "x86_64")]  pub const GETCWD: usize = 79;
    #[cfg(target_arch = "aarch64")] pub const UNAME: usize = 160;
    #[cfg(target_arch = "x86_64")]  pub const UNAME: usize = 63;
    #[cfg(target_arch = "aarch64")] pub const FSTAT: usize = 80;
    #[cfg(target_arch = "x86_64")]  pub const FSTAT: usize = 5;
    #[cfg(target_arch = "aarch64")] pub const GETDENTS64: usize = 61;
    #[cfg(target_arch = "x86_64")]  pub const GETDENTS64: usize = 217;
    #[cfg(target_arch = "aarch64")] pub const NANOSLEEP: usize = 101;
    #[cfg(target_arch = "x86_64")]  pub const NANOSLEEP: usize = 35;
    #[cfg(target_arch = "aarch64")] pub const GETPID: usize = 172;
    #[cfg(target_arch = "x86_64")]  pub const GETPID: usize = 39;
    #[cfg(target_arch = "aarch64")] pub const EXIT_GROUP: usize = 94;
    #[cfg(target_arch = "x86_64")]  pub const EXIT_GROUP: usize = 231;
}

#[cfg(target_arch = "x86_64")]
unsafe fn sc(n: usize, a: [usize; 6]) -> isize {
    let r: isize;
    core::arch::asm!("syscall",
        inlateout("rax") n as isize => r,
        in("rdi") a[0], in("rsi") a[1], in("rdx") a[2],
        in("r10") a[3], in("r8") a[4], in("r9") a[5],
        lateout("rcx") _, lateout("r11") _, options(nostack));
    r
}

#[cfg(target_arch = "aarch64")]
unsafe fn sc(n: usize, a: [usize; 6]) -> isize {
    let r: isize;
    core::arch::asm!("svc 0",
        in("x8") n,
        inlateout("x0") a[0] as isize => r,
        in("x1") a[1], in("x2") a[2], in("x3") a[3], in("x4") a[4], in("x5") a[5],
        options(nostack));
    r
}

macro_rules! sys {
    ($n:expr) => { sc($n, [0; 6]) };
    ($n:expr, $a:expr) => { sc($n, [$a as usize, 0, 0, 0, 0, 0]) };
    ($n:expr, $a:expr, $b:expr) => { sc($n, [$a as usize, $b as usize, 0, 0, 0, 0]) };
    ($n:expr, $a:expr, $b:expr, $c:expr) => { sc($n, [$a as usize, $b as usize, $c as usize, 0, 0, 0]) };
    ($n:expr, $a:expr, $b:expr, $c:expr, $d:expr) => { sc($n, [$a as usize, $b as usize, $c as usize, $d as usize, 0, 0]) };
    ($n:expr, $a:expr, $b:expr, $c:expr, $d:expr, $e:expr) => { sc($n, [$a as usize, $b as usize, $c as usize, $d as usize, $e as usize, 0]) };
    ($n:expr, $a:expr, $b:expr, $c:expr, $d:expr, $e:expr, $f:expr) => { sc($n, [$a as usize, $b as usize, $c as usize, $d as usize, $e as usize, $f as usize]) };
}

const PAGE: usize = 4096;
const PROT_NONE: usize = 0;
const PROT_READ: usize = 1;
const PROT_RW: usize = 3;
const MAP_PRIVATE: usize = 0x02;
const MAP_ANON: usize = 0x20;
const AT_FDCWD: isize = -100;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_CREAT: usize = 0o100;
const O_TRUNC: usize = 0o1000;
const O_DIRECTORY: usize = 0o200000;
const AF_UNIX: usize = 1;
const SOCK_STREAM: usize = 1;
const SOCK_DGRAM: usize = 2;
const SOL_SOCKET: usize = 1;
const SO_PEERCRED: usize = 17;
const SCM_RIGHTS: i32 = 1;
const EFAULT: isize = -14;

// ── output ───────────────────────────────────────────────────────────────────

unsafe fn out(s: &[u8]) { sys!(nr::WRITE, 1, s.as_ptr(), s.len()); }

unsafe fn out_num(mut v: usize) {
    let mut b = [0u8; 20];
    let mut i = b.len();
    if v == 0 { i -= 1; b[i] = b'0'; }
    while v > 0 { i -= 1; b[i] = b'0' + (v % 10) as u8; v /= 10; }
    out(&b[i..]);
}

// ── buffers ──────────────────────────────────────────────────────────────────

const ANON: usize = 0;
const FILE: usize = 1;
const COW: usize = 2;
const STACK: usize = 3;
const KIND_NAMES: [&[u8]; 4] = [b"anon", b"file", b"cow", b"stack"];

/// Bytes of lazy space each slot owns (a buffer at `BUF_OFF` straddles the
/// first page boundary, so one access spans two absent pages).
const SLOT: usize = 3 * PAGE;
const SLOTS: usize = 4;
const BUF_OFF: usize = PAGE - 700;
const N: usize = 3000;
const DATA_FILE: &[u8] = b"/root/.uptrtest.dat\0";
const SCRATCH_FILE: &[u8] = b"/root/.uptrtest.out\0";

fn file_pat(i: usize) -> u8 { (i.wrapping_mul(7) + 3) as u8 }
fn cow_pat(i: usize) -> u8 { ((i.wrapping_mul(13) + 5) as u8) ^ 0x5a }
fn msg_pat(i: usize) -> u8 { (i.wrapping_mul(31) + 17) as u8 | 1 }

static mut COW_REGION: usize = 0;
static mut STACK_DEEP: usize = 0;
static mut DATA_FD: isize = -1;

/// Slot `s` of kind `k`: the first byte of `SLOT` bytes of never-touched
/// (for `cow`: shared-with-parent) memory, or 0.
unsafe fn slot(k: usize, s: usize) -> usize {
    match k {
        ANON => {
            let p = sys!(nr::MMAP, 0, SLOT, PROT_RW, MAP_PRIVATE | MAP_ANON, usize::MAX, 0);
            if p < 0 { 0 } else { p as usize }
        }
        FILE => {
            let p = sys!(nr::MMAP, 0, SLOT, PROT_RW, MAP_PRIVATE, DATA_FD, s * SLOT);
            if p < 0 { 0 } else { p as usize }
        }
        COW => COW_REGION + s * SLOT,
        _ => STACK_DEEP - (s + 1) * SLOT,
    }
}

/// What a never-written slot of kind `k` holds at byte `off`.
unsafe fn expect(k: usize, s: usize, off: usize) -> u8 {
    match k {
        FILE => file_pat(s * SLOT + off),
        COW => cow_pat(s * SLOT + off),
        _ => 0,
    }
}

unsafe fn buf(k: usize, s: usize) -> usize {
    let b = slot(k, s);
    if b == 0 { 0 } else { b + BUF_OFF }
}

unsafe fn socketpair(ty: usize) -> Option<(usize, usize)> {
    let mut sv = [0i32; 2];
    if sys!(nr::SOCKETPAIR, AF_UNIX, ty, 0, sv.as_mut_ptr()) != 0 { return None; }
    Some((sv[0] as usize, sv[1] as usize))
}

unsafe fn eq_msg(p: usize, n: usize) -> bool {
    (0..n).all(|i| *((p + i) as *const u8) == msg_pat(i))
}

unsafe fn eq_expect(k: usize, s: usize, p: *const u8, n: usize) -> bool {
    (0..n).all(|i| *p.add(i) == expect(k, s, BUF_OFF + i))
}

/// Receive exactly `n` bytes from `fd` into `dst` (plain memory).
unsafe fn recv_all(fd: usize, dst: *mut u8, n: usize) -> bool {
    let mut got = 0;
    while got < n {
        let r = sys!(nr::RECVFROM, fd, dst.add(got), n - got, 0, 0, 0);
        if r <= 0 { return false; }
        got += r as usize;
    }
    true
}

#[repr(C)]
struct Iov { base: usize, len: usize }

#[repr(C)]
struct MsgHdr {
    name: usize, namelen: u32, _pad: u32,
    iov: *const Iov, iovlen: usize,
    control: usize, controllen: usize,
    flags: i32, _pad2: i32,
}

fn msghdr(iov: *const Iov, iovlen: usize, control: usize, controllen: usize) -> MsgHdr {
    MsgHdr { name: 0, namelen: 0, _pad: 0, iov, iovlen, control, controllen, flags: 0, _pad2: 0 }
}

// ── the operations (run in the child; 0 = pass, else a failure code) ────────

static mut PLAIN: [u8; 8192] = [0; 8192];
static mut MSG: [u8; 8192] = [0; 8192];

unsafe fn plain() -> *mut u8 { core::ptr::addr_of_mut!(PLAIN) as *mut u8 }
unsafe fn msg() -> *const u8 { core::ptr::addr_of!(MSG) as *const u8 }

/// Kernel reads the lazy buffer (send side), result checked on the peer.
unsafe fn op_send_side(k: usize, how: usize) -> i32 {
    let b = buf(k, 0);
    if b == 0 { return 90; }
    let ty = if how == 4 { SOCK_DGRAM } else { SOCK_STREAM };
    let (a, z) = match socketpair(ty) { Some(p) => p, None => return 91 };
    let r = match how {
        0 => sys!(nr::SENDTO, a, b, N, 0, 0, 0),
        1 => sys!(nr::WRITE, a, b, N),
        2 => {
            let iov = [Iov { base: b, len: 1000 }, Iov { base: b + 1000, len: N - 1000 }];
            sys!(nr::WRITEV, a, iov.as_ptr(), 2)
        }
        3 => {
            let iov = [Iov { base: b, len: 1500 }, Iov { base: b + 1500, len: N - 1500 }];
            let m = msghdr(iov.as_ptr(), 2, 0, 0);
            sys!(nr::SENDMSG, a, &m as *const MsgHdr, 0)
        }
        _ => sys!(nr::SENDTO, a, b, N, 0, 0, 0),
    };
    if r != N as isize { return 1; }
    let p = plain();
    if how == 4 {
        if sys!(nr::RECVFROM, z, p, 8192, 0, 0, 0) != N as isize { return 2; }
    } else if !recv_all(z, p, N) { return 2; }
    if !eq_expect(k, 0, p, N) { return 3; }
    0
}

/// Kernel writes the lazy buffer (receive side).
unsafe fn op_recv_side(k: usize, how: usize) -> i32 {
    let b = buf(k, 0);
    if b == 0 { return 90; }
    let ty = if how == 4 { SOCK_DGRAM } else { SOCK_STREAM };
    let (a, z) = match socketpair(ty) { Some(p) => p, None => return 91 };
    if sys!(nr::SENDTO, a, msg(), N, 0, 0, 0) != N as isize { return 92; }
    let mut got: usize = 0;
    // Streams may return short; loop until all N bytes are in.
    for _ in 0..16 {
        if got >= N { break; }
        let dst = b + got;
        let left = N - got;
        let r = match how {
            0 => sys!(nr::RECVFROM, z, dst, left, 0, 0, 0),
            1 => sys!(nr::READ, z, dst, left),
            2 => {
                let first = if left > 1000 { 1000 } else { left };
                let iov = [Iov { base: dst, len: first }, Iov { base: dst + first, len: left - first }];
                sys!(nr::READV, z, iov.as_ptr(), 2)
            }
            3 => {
                let first = if left > 1500 { 1500 } else { left };
                let iov = [Iov { base: dst, len: first }, Iov { base: dst + first, len: left - first }];
                let mut m = msghdr(iov.as_ptr(), 2, 0, 0);
                sys!(nr::RECVMSG, z, &mut m as *mut MsgHdr, 0)
            }
            4 => sys!(nr::RECVFROM, z, dst, 8192, 0, 0, 0),
            _ => {
                // recvfrom with the source address written to a lazy slot.
                let addr = buf(k, 1);
                let mut alen: u32 = 110;
                sys!(nr::RECVFROM, z, dst, left, 0, addr, &mut alen as *mut u32)
            }
        };
        if r <= 0 { return 1; }
        got += r as usize;
    }
    if got != N || !eq_msg(b, N) { return 2; }
    0
}

/// SCM_RIGHTS with the data and the control buffer both lazy on receive.
unsafe fn op_scm(k: usize) -> i32 {
    let b = buf(k, 0);
    let c = buf(k, 1);
    if b == 0 || c == 0 { return 90; }
    let (a, z) = match socketpair(SOCK_STREAM) { Some(p) => p, None => return 91 };
    let mut pfd = [0i32; 2];
    if sys!(nr::PIPE2, pfd.as_mut_ptr(), 0) != 0 { return 92; }
    // Send side: plain memory.
    let mut ctl = [0u64; 3]; // cmsghdr (16 bytes) + one int, padded
    let cp = ctl.as_mut_ptr() as *mut u8;
    *(cp as *mut usize) = 16 + 4;
    *(cp.add(8) as *mut i32) = SOL_SOCKET as i32;
    *(cp.add(12) as *mut i32) = SCM_RIGHTS;
    *(cp.add(16) as *mut i32) = pfd[1];
    let iov = [Iov { base: msg() as usize, len: 100 }];
    let m = msghdr(iov.as_ptr(), 1, cp as usize, 24);
    if sys!(nr::SENDMSG, a, &m as *const MsgHdr, 0) != 100 { return 93; }
    // Receive side: lazy data + lazy control.
    let riov = [Iov { base: b, len: 100 }];
    let mut rm = msghdr(riov.as_ptr(), 1, c, 64);
    let r = sys!(nr::RECVMSG, z, &mut rm as *mut MsgHdr, 0);
    if r != 100 { return 1; }
    if !eq_msg(b, 100) { return 2; }
    if rm.controllen < 20 { return 3; }
    let lvl = *((c + 8) as *const i32);
    let ty = *((c + 12) as *const i32);
    if lvl != SOL_SOCKET as i32 || ty != SCM_RIGHTS { return 4; }
    let nfd = *((c + 16) as *const i32);
    if nfd < 0 { return 5; }
    if sys!(nr::WRITE, nfd, b"x".as_ptr(), 1) != 1 { return 6; }
    let mut one = [0u8; 1];
    if sys!(nr::READ, pfd[0], one.as_mut_ptr(), 1) != 1 || one[0] != b'x' { return 7; }
    0
}

unsafe fn op_misc(k: usize, how: usize) -> i32 {
    let c = buf(k, 1);
    if c == 0 { return 90; }
    match how {
        // getsockopt(SO_PEERCRED) into a lazy optval.
        0 => {
            let (a, _z) = match socketpair(SOCK_STREAM) { Some(p) => p, None => return 91 };
            let mut ol: u32 = 12;
            if sys!(nr::GETSOCKOPT, a, SOL_SOCKET, SO_PEERCRED, c, &mut ol as *mut u32) != 0 { return 1; }
            if *(c as *const i32) as isize != sys!(nr::GETPID) { return 2; }
        }
        // getsockname into a lazy sockaddr.
        1 => {
            let (a, _z) = match socketpair(SOCK_STREAM) { Some(p) => p, None => return 91 };
            let mut al: u32 = 110;
            if sys!(nr::GETSOCKNAME, a, c, &mut al as *mut u32) != 0 { return 1; }
            // An unnamed socket: the family word only (LeandrOS reports it
            // zeroed), and addrlen 2.
            if al != 2 || *(c as *const u16) != 0 { return 2; }
        }
        // socketpair() writing sv[] into lazy memory.
        2 => {
            if sys!(nr::SOCKETPAIR, AF_UNIX, SOCK_STREAM, 0, c) != 0 { return 1; }
            let a = *(c as *const i32);
            let z = *((c + 4) as *const i32);
            if a < 0 || z < 0 || a == z { return 2; }
            if sys!(nr::SENDTO, a, msg(), 10, 0, 0, 0) != 10 { return 3; }
            if !recv_all(z as usize, plain(), 10) || !eq_msg(plain() as usize, 10) { return 4; }
        }
        // pipe2() writing fds into lazy memory.
        3 => {
            if sys!(nr::PIPE2, c, 0) != 0 { return 1; }
            let r = *(c as *const i32);
            let w = *((c + 4) as *const i32);
            if sys!(nr::WRITE, w, b"y".as_ptr(), 1) != 1 { return 2; }
            let mut one = [0u8; 1];
            if sys!(nr::READ, r, one.as_mut_ptr(), 1) != 1 || one[0] != b'y' { return 3; }
        }
        // pipe: write from a lazy buffer, read into another lazy buffer.
        4 => {
            let b = buf(k, 0);
            if b == 0 { return 90; }
            let mut p = [0i32; 2];
            if sys!(nr::PIPE2, p.as_mut_ptr(), 0) != 0 { return 91; }
            if sys!(nr::WRITE, p[1], b, N) != N as isize { return 1; }
            let mut got = 0usize;
            while got < N {
                let r = sys!(nr::READ, p[0], c + got, N - got);
                if r <= 0 { return 2; }
                got += r as usize;
            }
            if !eq_expect(k, 0, c as *const u8, N) {
                // c is slot 1's buffer; compare against slot 0's expected bytes.
                return 3;
            }
        }
        // pwrite from a lazy buffer into an f2fs file, read back.
        5 => {
            let b = buf(k, 0);
            if b == 0 { return 90; }
            let fd = sys!(nr::OPENAT, AT_FDCWD, SCRATCH_FILE.as_ptr(), O_RDWR | O_CREAT | O_TRUNC, 0o600);
            if fd < 0 { return 91; }
            if sys!(nr::PWRITE64, fd, b, N, 0) != N as isize { return 1; }
            let p = plain();
            if sys!(nr::PREAD64, fd, p, N, 0) != N as isize { return 2; }
            if !eq_expect(k, 0, p, N) { return 3; }
            sys!(nr::CLOSE, fd);
        }
        // pread from the f2fs data file into a lazy buffer.
        6 => {
            if sys!(nr::PREAD64, DATA_FD, c, N, 100) != N as isize { return 1; }
            if !(0..N).all(|i| *((c + i) as *const u8) == file_pat(100 + i)) { return 2; }
        }
        // write(2) from a lazy buffer into an f2fs file, read(2) back into lazy.
        7 => {
            let b = buf(k, 0);
            if b == 0 { return 90; }
            let fd = sys!(nr::OPENAT, AT_FDCWD, SCRATCH_FILE.as_ptr(), O_RDWR | O_CREAT | O_TRUNC, 0o600);
            if fd < 0 { return 91; }
            if sys!(nr::WRITE, fd, b, N) != N as isize { return 1; }
            if sys!(nr::LSEEK, fd, 0, 0) != 0 { return 92; }
            if sys!(nr::READ, fd, c, N) != N as isize { return 2; }
            if !eq_expect(k, 0, c as *const u8, N) { return 3; }
            sys!(nr::CLOSE, fd);
        }
        // clock_gettime into lazy memory.
        8 => {
            // 8-aligned (the kernel insists), straddling the page boundary.
            let t = c - BUF_OFF + PAGE - 8;
            if sys!(nr::CLOCK_GETTIME, 1, t) != 0 { return 1; }
            if *(t as *const i64) == 0 && *((t + 8) as *const i64) == 0 { return 2; }
        }
        // wait4 status into lazy memory.
        9 => {
            let pid = fork();
            if pid == 0 { sys!(nr::EXIT_GROUP, 7); loop {} }
            if pid < 0 { return 91; }
            if sys!(nr::WAIT4, pid, c, 0, 0) != pid as isize { return 1; }
            if *(c as *const i32) != 7 << 8 { return 2; }
        }
        // fstat into lazy memory (st_size at offset 48 on both ABIs).
        10 => {
            if sys!(nr::FSTAT, DATA_FD, c) != 0 { return 1; }
            if *((c + 48) as *const i64) != (SLOTS * SLOT) as i64 { return 2; }
        }
        // getcwd / uname / getdents64 into lazy memory.
        11 => {
            if sys!(nr::GETCWD, c, 256) <= 0 || *(c as *const u8) != b'/' { return 1; }
        }
        12 => {
            if sys!(nr::UNAME, c) != 0 || *(c as *const u8) == 0 { return 1; }
        }
        _ => {
            let d = sys!(nr::OPENAT, AT_FDCWD, b"/\0".as_ptr(), O_RDONLY | O_DIRECTORY, 0);
            if d < 0 { return 91; }
            if sys!(nr::GETDENTS64, d, c, 2048) <= 0 { return 1; }
            sys!(nr::CLOSE, d);
        }
    }
    0
}

/// Kernel accesses that must fail with EFAULT, not kill the caller.
unsafe fn op_efault(how: usize) -> i32 {
    let (a, z) = match socketpair(SOCK_STREAM) { Some(p) => p, None => return 91 };
    let none = sys!(nr::MMAP, 0, 2 * PAGE, PROT_NONE, MAP_PRIVATE | MAP_ANON, usize::MAX, 0);
    let ro = sys!(nr::MMAP, 0, 2 * PAGE, PROT_RW, MAP_PRIVATE | MAP_ANON, usize::MAX, 0);
    if none < 0 || ro < 0 { return 92; }
    let (none, ro) = (none as usize, ro as usize);
    for i in 0..2 * PAGE { *((ro + i) as *mut u8) = 0xA5; }
    if sys!(nr::MPROTECT, ro, 2 * PAGE, PROT_READ) != 0 { return 93; }
    // An address with nothing mapped: a page we map and unmap again.
    let gone = sys!(nr::MMAP, 0, PAGE, PROT_RW, MAP_PRIVATE | MAP_ANON, usize::MAX, 0) as usize;
    sys!(nr::MUNMAP, gone, PAGE);
    if sys!(nr::SENDTO, a, msg(), 64, 0, 0, 0) != 64 { return 94; }
    let r = match how {
        0 => sys!(nr::RECVFROM, z, none + 100, 64, 0, 0, 0),
        1 => sys!(nr::SENDTO, a, none + 100, 64, 0, 0, 0),
        2 => sys!(nr::RECVFROM, z, ro + 100, 64, 0, 0, 0),
        3 => {
            let iov = [Iov { base: gone, len: 64 }];
            let mut m = msghdr(iov.as_ptr(), 1, 0, 0);
            sys!(nr::RECVMSG, z, &mut m as *mut MsgHdr, 0)
        }
        4 => {
            let mut ol: u32 = 12;
            sys!(nr::GETSOCKOPT, a, SOL_SOCKET, SO_PEERCRED, none, &mut ol as *mut u32)
        }
        5 => {
            let mut p = [0i32; 2];
            if sys!(nr::PIPE2, p.as_mut_ptr(), 0) != 0 { return 95; }
            sys!(nr::WRITE, p[1], msg(), 64);
            sys!(nr::READ, p[0], ro + 100, 64)
        }
        _ => {
            // A buffer that starts valid and runs into PROT_NONE: the part
            // before the hole may or may not be filled, but the result must
            // be EFAULT or a short count, and the caller must survive.
            let v = sys!(nr::MMAP, 0, 3 * PAGE, PROT_RW, MAP_PRIVATE | MAP_ANON, usize::MAX, 0);
            if v < 0 { return 96; }
            let v = v as usize;
            sys!(nr::MPROTECT, v + PAGE, PAGE, PROT_NONE);
            let r = sys!(nr::RECVFROM, z, v + PAGE - 32, 64, 0, 0, 0);
            if r == EFAULT || (r > 0 && r <= 32) { return 0; }
            return 1;
        }
    };
    if r != EFAULT { return 1; }
    // The read-only page must be untouched.
    if !(0..2 * PAGE).all(|i| *((ro + i) as *const u8) == 0xA5) { return 2; }
    0
}

// ── runner ───────────────────────────────────────────────────────────────────

const TIMEOUT_MS: usize = 30_000;

unsafe fn sleep_ms(ms: usize) {
    let ts = [0i64, (ms * 1_000_000) as i64];
    sys!(nr::NANOSLEEP, ts.as_ptr(), 0);
}

/// Run `f` in a forked child under a watchdog. Returns the child's exit
/// code, 1000 + signal if it was killed, or 2000 on timeout.
unsafe fn in_child(f: &dyn Fn() -> i32) -> i32 {
    let pid = fork();
    if pid == 0 {
        let c = f();
        sys!(nr::EXIT_GROUP, c);
        loop {}
    }
    if pid < 0 { return 3000; }
    let mut status: i32 = 0;
    let mut waited = 0;
    loop {
        let r = sys!(nr::WAIT4, pid, &mut status as *mut i32, 1 /* WNOHANG */, 0);
        if r == pid as isize { break; }
        if r < 0 { return 3001; }
        if waited >= TIMEOUT_MS {
            sys!(nr::KILL, pid, 9);
            sys!(nr::WAIT4, pid, &mut status as *mut i32, 0, 0);
            return 2000;
        }
        sleep_ms(10);
        waited += 10;
    }
    if status & 0x7f != 0 { 1000 + (status & 0x7f) } else { (status >> 8) & 0xff }
}

static mut FAILS: i32 = 0;
static mut PASSES: i32 = 0;

unsafe fn verdict(name: &[u8], kind: Option<usize>, code: i32) {
    out(name);
    if let Some(k) = kind { out(b"/"); out(KIND_NAMES[k]); }
    if code == 0 {
        out(b": PASS\n");
        PASSES += 1;
    } else {
        out(b": FAIL code=");
        out_num(code as usize);
        if code >= 2000 { out(b" (timeout)"); } else if code >= 1000 { out(b" (killed by signal)"); }
        out(b"\n");
        FAILS += 1;
    }
}

unsafe fn cow_region_intact() -> bool {
    (0..SLOTS * SLOT).all(|i| *((COW_REGION + i) as *const u8) == cow_pat(i))
}

unsafe fn run(name: &[u8], f: &dyn Fn(usize) -> i32) {
    for k in [ANON, FILE, COW, STACK] {
        // cow: repeat so the child lands on different CPUs.
        let reps = if k == COW { 3 } else { 1 };
        let mut code = 0;
        for _ in 0..reps {
            code = in_child(&|| f(k));
            if code == 0 && k == COW && !cow_region_intact() { code = 50; }
            if code != 0 { break; }
        }
        verdict(name, Some(k), code);
    }
}

#[no_mangle]
pub unsafe extern "C" fn uptr_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let only_efault = argc > 1 && **argv.add(1) == b'e';
    for i in 0..8192 { *(core::ptr::addr_of_mut!(MSG) as *mut u8).add(i) = msg_pat(i); }

    // The data file behind the `file` kind.
    let fd = sys!(nr::OPENAT, AT_FDCWD, DATA_FILE.as_ptr(), O_RDWR | O_CREAT | O_TRUNC, 0o600);
    if fd < 0 { out(b"uptrtest: cannot create data file\n"); return 1; }
    let mut chunk = [0u8; 4096];
    for c in 0..(SLOTS * SLOT) / 4096 {
        for (i, v) in chunk.iter_mut().enumerate() { *v = file_pat(c * 4096 + i); }
        if sys!(nr::WRITE, fd, chunk.as_ptr(), 4096) != 4096 { out(b"uptrtest: data write failed\n"); return 1; }
    }
    DATA_FD = fd;

    // The `cow` kind: filled here, so every page is present in this process
    // and shared read-only with each child.
    let r = sys!(nr::MMAP, 0, SLOTS * SLOT, PROT_RW, MAP_PRIVATE | MAP_ANON, usize::MAX, 0);
    if r < 0 { out(b"uptrtest: mmap failed\n"); return 1; }
    COW_REGION = r as usize;
    for i in 0..SLOTS * SLOT { *((COW_REGION + i) as *mut u8) = cow_pat(i); }

    // The `stack` kind: 3 MiB below here, inside the 8 MiB main stack VMA.
    let here = 0u8;
    STACK_DEEP = ((&here as *const u8 as usize) - 3 * 1024 * 1024) & !(PAGE - 1);

    if !only_efault {
        run(b"send", &|k| op_send_side(k, 0));
        run(b"write_sock", &|k| op_send_side(k, 1));
        run(b"writev_sock", &|k| op_send_side(k, 2));
        run(b"sendmsg", &|k| op_send_side(k, 3));
        run(b"dgram_send", &|k| op_send_side(k, 4));
        run(b"recv", &|k| op_recv_side(k, 0));
        run(b"read_sock", &|k| op_recv_side(k, 1));
        run(b"readv_sock", &|k| op_recv_side(k, 2));
        run(b"recvmsg", &|k| op_recv_side(k, 3));
        run(b"dgram_recv", &|k| op_recv_side(k, 4));
        run(b"recvfrom_addr", &|k| op_recv_side(k, 5));
        run(b"scm_rights", &|k| op_scm(k));
        run(b"getsockopt", &|k| op_misc(k, 0));
        run(b"getsockname", &|k| op_misc(k, 1));
        run(b"socketpair_sv", &|k| op_misc(k, 2));
        run(b"pipe2_fds", &|k| op_misc(k, 3));
        run(b"pipe_rw", &|k| op_misc(k, 4));
        run(b"file_pwrite", &|k| op_misc(k, 5));
        run(b"file_pread", &|k| op_misc(k, 6));
        run(b"file_rw", &|k| op_misc(k, 7));
        run(b"clock_gettime", &|k| op_misc(k, 8));
        run(b"wait4_status", &|k| op_misc(k, 9));
        run(b"fstat", &|k| op_misc(k, 10));
        run(b"getcwd", &|k| op_misc(k, 11));
        run(b"uname", &|k| op_misc(k, 12));
        run(b"getdents64", &|k| op_misc(k, 13));
    }

    let names: [&[u8]; 7] = [b"efault_recv_protnone", b"efault_send_protnone", b"efault_recv_readonly",
        b"efault_recvmsg_unmapped", b"efault_getsockopt_protnone", b"efault_pipe_read_readonly",
        b"efault_recv_straddle"];
    for (i, n) in names.iter().enumerate() {
        verdict(n, None, in_child(&|| op_efault(i)));
    }
    // Nothing above may have left the socket layer wedged.
    verdict(b"after_efault", None, in_child(&|| op_send_side(ANON, 0)));

    sys!(nr::CLOSE, DATA_FD);
    out(b"uptrtest: ");
    out_num(PASSES as usize);
    out(b" passed, ");
    out_num(FAILS as usize);
    out(b" failed\n--- uptrtest done ---\n");
    FAILS
}
