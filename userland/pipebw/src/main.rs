//! pipebw — pipe read throughput must not depend on the read buffer size
//! (lane pipefix).
//!
//! sys_read used to prefault the WHOLE user buffer on every call while a pipe
//! read delivers at most 4 KiB, and every prefaulted page cost two linear VMA
//! scans: 1 MiB reads crawled at a few MB/s in a process with many VMAs.
//! This pushes 64 MiB through a pipe with 4 KiB and with 1 MiB reads in a
//! reader that owns ~500 separate VMAs, and asserts rate(1 MiB) >= rate(4 KiB)/2.
//! Prints `pipebw: 4k=<MB/s> 1m=<MB/s>`; exits with the number of failures.

#![no_std]
#![no_main]

use core::ffi::c_void;

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;
    pub fn fork() -> i32;
    pub fn waitpid(pid: i32, stat_loc: *mut i32, options: i32) -> i32;
    pub fn _exit(status: i32) -> !;
    pub fn exit(status: i32) -> !;
}

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const READ: usize = 0;
    pub const WRITE: usize = 1;
    pub const CLOSE: usize = 3;
    pub const MMAP: usize = 9;
    pub const PIPE2: usize = 293;
    pub const CLOCK_GETTIME: usize = 228;
    pub const NANOSLEEP: usize = 35;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const READ: usize = 63;
    pub const WRITE: usize = 64;
    pub const CLOSE: usize = 57;
    pub const MMAP: usize = 222;
    pub const PIPE2: usize = 59;
    pub const CLOCK_GETTIME: usize = 113;
    pub const NANOSLEEP: usize = 101;
}

#[cfg(target_arch = "x86_64")]
unsafe fn sc6(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> isize {
    let r: isize;
    core::arch::asm!("syscall", inlateout("rax") n as isize => r, in("rdi") a, in("rsi") b,
        in("rdx") c, in("r10") d, in("r8") e, in("r9") f, out("rcx") _, out("r11") _,
        options(nostack));
    r
}
#[cfg(target_arch = "aarch64")]
unsafe fn sc6(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> isize {
    let r: isize;
    core::arch::asm!("svc #0", in("x8") n, inlateout("x0") a as isize => r, in("x1") b,
        in("x2") c, in("x3") d, in("x4") e, in("x5") f, options(nostack));
    r
}

const TOTAL: usize = 64 << 20;
const PROT_RW: usize = 3;
const PROT_R: usize = 1;
const MAP_PRIVATE_ANON: usize = 0x22;

unsafe fn puts(s: &[u8]) { sc6(nr::WRITE, 1, s.as_ptr() as usize, s.len(), 0, 0, 0); }
unsafe fn putn(mut n: u64) {
    let mut b = [0u8; 24];
    let mut i = b.len();
    if n == 0 { i -= 1; b[i] = b'0'; }
    while n > 0 { i -= 1; b[i] = b'0' + (n % 10) as u8; n /= 10; }
    puts(&b[i..]);
}
unsafe fn now_ns() -> u64 {
    let mut ts = [0u64; 2];
    sc6(nr::CLOCK_GETTIME, 1, ts.as_mut_ptr() as usize, 0, 0, 0, 0);
    ts[0] * 1_000_000_000 + ts[1]
}
unsafe fn map(len: usize, prot: usize) -> usize {
    sc6(nr::MMAP, 0, len, prot, MAP_PRIVATE_ANON, usize::MAX, 0) as usize
}

/// Push TOTAL bytes through a fresh pipe, reading with `bs`-byte reads; MB/s.
unsafe fn run(bs: usize, buf: usize) -> u64 {
    let mut p = [0i32; 2];
    if sc6(nr::PIPE2, p.as_mut_ptr() as usize, 0, 0, 0, 0, 0) < 0 { return 0; }
    let pid = fork();
    if pid == 0 {
        sc6(nr::CLOSE, p[0] as usize, 0, 0, 0, 0, 0);
        let src = map(16384, PROT_RW);
        let mut sent = 0;
        while sent < TOTAL {
            let n = sc6(nr::WRITE, p[1] as usize, src, (TOTAL - sent).min(16384), 0, 0, 0);
            if n <= 0 { _exit(1); }
            sent += n as usize;
        }
        _exit(0);
    }
    sc6(nr::CLOSE, p[1] as usize, 0, 0, 0, 0, 0);
    let t0 = now_ns();
    let mut got = 0usize;
    loop {
        let n = sc6(nr::READ, p[0] as usize, buf, bs, 0, 0, 0);
        if n <= 0 { break; }
        got += n as usize;
    }
    let dt = now_ns() - t0;
    sc6(nr::CLOSE, p[0] as usize, 0, 0, 0, 0, 0);
    let mut st = 0i32;
    waitpid(pid, &mut st, 0);
    if got != TOTAL || dt == 0 {
        puts(b"pipebw: bad run bs="); putn(bs as u64); puts(b" got="); putn(got as u64); puts(b" dt="); putn(dt); puts(b"\n");
        return 0;
    }
    (got as u64) * 1000 / dt // bytes/ns * 1000 = MB/s
}

/// A blocking 1 MiB write() to a slow reader must return the full count.
unsafe fn full_write(buf: usize) -> bool {
    let mut p = [0i32; 2];
    if sc6(nr::PIPE2, p.as_mut_ptr() as usize, 0, 0, 0, 0, 0) < 0 { return false; }
    let pid = fork();
    if pid == 0 {
        sc6(nr::CLOSE, p[0] as usize, 0, 0, 0, 0, 0);
        let n = sc6(nr::WRITE, p[1] as usize, buf, 1 << 20, 0, 0, 0);
        _exit(if n == (1 << 20) { 0 } else { 1 });
    }
    sc6(nr::CLOSE, p[1] as usize, 0, 0, 0, 0, 0);
    let mut got = 0usize;
    loop {
        let n = sc6(nr::READ, p[0] as usize, buf, 4096, 0, 0, 0);
        if n <= 0 { break; }
        got += n as usize;
        // slow reader: yield now and then so the writer fills the ring
        if got % (64 << 10) == 0 { let ts = [0u64, 1_000_000]; sc6(nr::NANOSLEEP, ts.as_ptr() as usize, 0, 0, 0, 0, 0); }
    }
    sc6(nr::CLOSE, p[0] as usize, 0, 0, 0, 0, 0);
    let mut st = 0i32;
    waitpid(pid, &mut st, 0);
    puts(b"pipebw: full_write got="); putn(got as u64); puts(b" status="); putn(st as u64); puts(b"\n");
    got == (1 << 20) && st == 0
}

#[no_mangle]
pub unsafe extern "C" fn pipebw_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    // ~500 separate VMAs (alternating protections defeat merging), touched.
    for i in 0..500 {
        let a = map(8192, if i & 1 == 0 { PROT_RW } else { PROT_R });
        if i & 1 == 0 && (a as isize) > 0 { *(a as *mut u8) = 1; }
    }
    let buf = map(1 << 20, PROT_RW);
    let r4k = run(4096, buf);
    let r1m = run(1 << 20, buf);
    puts(b"pipebw: 4k="); putn(r4k); puts(b" 1m="); putn(r1m); puts(b" MB/s\n");
    let fw = full_write(buf);
    puts(if fw { b"pipebw: full_write PASS\n" } else { b"pipebw: full_write FAIL\n" });
    let ok = fw && r4k != 0 && r1m != 0 && r1m * 2 >= r4k;
    puts(if ok { b"pipebw: PASS\n" } else { b"pipebw: FAIL\n" });
    if ok { 0 } else { 1 }
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   xor rbp, rbp", "   mov rdi, rsp", "   mov rsi, offset pipebw_main",
    "   and rsp, -16", "   call relibc_start_v1", "   ud2"
);
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   mov x29, #0", "   mov x30, #0", "   mov x0, sp",
    "   adrp x1, pipebw_main", "   add x1, x1, :lo12:pipebw_main",
    "   and sp, x0, #-16", "   bl relibc_start_v1", "   brk #0"
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
