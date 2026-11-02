//! mmapbench (lane mmapA) - us/call of mmap / touch / munmap of 4 MiB.
//!
//! Cases: anon (MAP_PRIVATE|MAP_ANONYMOUS, touch 1 byte/page), memfd
//! (MAP_SHARED of a 4 MiB memfd, touched), memfd-untouched. Each case runs
//! alone and then with 3 sibling threads of this process busy in long
//! syscalls (pread of 1 MiB from a tmpfs file), which is what makes an x86_64
//! munmap shootdown wait for IF=0 targets. Usage: `mmapbench [iters=2000]`.
//! Prints `as_hold_max_us` from /proc/sysstat afterwards.

#![no_std]
#![no_main]
#![feature(c_variadic)]
#![allow(non_camel_case_types)]

#[path = "../../muslrt/rt.rs"]
mod rt;

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, Ordering};

type c_int = i32;
type c_long = i64;
type time_t = i64;
type size_t = usize;
type pthread_t = *mut c_void;

const PROT_RW: c_int = 3;
const MAP_SHARED: c_int = 0x01;
const MAP_PRIVATE: c_int = 0x02;
const MAP_ANONYMOUS: c_int = 0x20;
const PAGE: usize = 4096;
const LEN: usize = 4 << 20;
const CLOCK_MONOTONIC: c_int = 1;
const O_RDWR: c_int = 2;
const O_CREAT: c_int = 0x40;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec { pub tv_sec: time_t, pub tv_nsec: c_long }

extern "C" {
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn _exit(status: c_int) -> !;
    pub fn open(path: *const u8, flags: c_int, mode: c_int) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, n: size_t) -> isize;
    pub fn pread(fd: c_int, buf: *mut c_void, n: size_t, off: i64) -> isize;
    pub fn ftruncate(fd: c_int, len: i64) -> c_int;
    pub fn mmap(addr: *mut c_void, len: size_t, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
    pub fn munmap(addr: *mut c_void, len: size_t) -> c_int;
    pub fn clock_gettime(clk: c_int, tp: *mut timespec) -> c_int;
    pub fn pthread_create(thread: *mut pthread_t, attr: *const c_void,
        start: extern "C" fn(*mut c_void) -> *mut c_void, arg: *mut c_void) -> c_int;
}

// ── Assembly entry point (identical to forktest's) ───────────────────────────



#[cfg(target_arch = "aarch64")]
const NR_MEMFD: usize = 279;
#[cfg(target_arch = "x86_64")]
const NR_MEMFD: usize = 319;

#[cfg(target_arch = "aarch64")]
unsafe fn syscall2(nr: usize, a0: usize, a1: usize) -> isize {
    let ret: isize;
    core::arch::asm!("svc #0", in("x8") nr, inlateout("x0") a0 => ret, in("x1") a1, options(nostack));
    ret
}
#[cfg(target_arch = "x86_64")]
unsafe fn syscall2(nr: usize, a0: usize, a1: usize) -> isize {
    let ret: isize;
    core::arch::asm!("syscall", inlateout("rax") nr => ret, in("rdi") a0, in("rsi") a1,
        out("rcx") _, out("r11") _, options(nostack));
    ret
}
unsafe fn memfd_create(name: *const u8, flags: u32) -> c_int { syscall2(NR_MEMFD, name as usize, flags as usize) as c_int }

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! { unsafe { _exit(134); } }

static STOP: AtomicBool = AtomicBool::new(false);
static mut BIG: [u8; 1 << 20] = [0; 1 << 20];

unsafe fn now_ns() -> u64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

unsafe fn out(s: &[u8]) { write(1, s.as_ptr() as *const c_void, s.len()); }

unsafe fn num(mut v: u64, buf: &mut [u8; 24]) -> usize {
    let mut tmp = [0u8; 24];
    let mut n = 0;
    if v == 0 { tmp[0] = b'0'; n = 1; }
    while v > 0 { tmp[n] = b'0' + (v % 10) as u8; v /= 10; n += 1; }
    for i in 0..n { buf[i] = tmp[n - 1 - i]; }
    n
}

unsafe fn put(v: u64) { let mut b = [0u8; 24]; let n = num(v, &mut b); out(&b[..n]); }

extern "C" fn busy(_: *mut c_void) -> *mut c_void {
    unsafe {
        let fd = open(b"/tmp/mmapbench.dat\0".as_ptr(), O_RDWR, 0);
        let mut buf = [0u8; 4096];
        let big = core::ptr::addr_of_mut!(BIG) as *mut c_void;
        while !STOP.load(Ordering::Relaxed) {
            if fd >= 0 { pread(fd, big, 1 << 20, 0); } else { read(0, buf.as_mut_ptr() as *mut c_void, 0); }
        }
    }
    core::ptr::null_mut()
}

/// kind 0 anon touched, 1 memfd touched, 2 memfd untouched. Returns (mmap, munmap) ns/call.
unsafe fn run(kind: u32, memfd: c_int, iters: u64) -> (u64, u64) {
    let (mut tm, mut tu) = (0u64, 0u64);
    for _ in 0..iters {
        let t0 = now_ns();
        let p = if kind == 0 {
            mmap(core::ptr::null_mut(), LEN, PROT_RW, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0)
        } else {
            mmap(core::ptr::null_mut(), LEN, PROT_RW, MAP_SHARED, memfd, 0)
        };
        let t1 = now_ns();
        if p as isize == -1 { out(b"mmap failed\n"); _exit(1); }
        if kind != 2 {
            let mut o = 0;
            while o < LEN { core::ptr::write_volatile((p as *mut u8).add(o), 1); o += PAGE; }
        }
        let t2 = now_ns();
        munmap(p, LEN);
        let t3 = now_ns();
        tm += t1 - t0;
        tu += t3 - t2;
    }
    (tm / iters / 1000, tu / iters / 1000)
}

unsafe fn case(name: &[u8], kind: u32, memfd: c_int, iters: u64, sib: &[u8]) {
    let (m, u) = run(kind, memfd, iters);
    out(b"mmapbench: "); out(name); out(sib);
    out(b" mmap_us="); put(m); out(b" munmap_us="); put(u); out(b"\n");
}

unsafe fn sysstat() {
    let fd = open(b"/proc/sysstat\0".as_ptr(), 0, 0);
    if fd < 0 { return; }
    static mut SB: [u8; 16384] = [0; 16384];
    let sb = core::ptr::addr_of_mut!(SB) as *mut u8;
    let n = read(fd, sb as *mut c_void, 16384);
    close(fd);
    if n <= 0 { return; }
    let s = core::slice::from_raw_parts(sb, n as usize);
    let pat = b"as_hold_max_us";
    let mut i = 0;
    while i + pat.len() < s.len() {
        if &s[i..i + pat.len()] == pat {
            let e = (i + 32).min(s.len());
            out(b"mmapbench: "); out(&s[i..e]); out(b"\n");
            return;
        }
        i += 1;
    }
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let mut iters = 2000u64;
    if _argc > 1 {
        let a = *argv.add(1);
        let mut v = 0u64; let mut k = 0;
        while *a.add(k) >= b'0' && *a.add(k) <= b'9' { v = v * 10 + (*a.add(k) - b'0') as u64; k += 1; }
        if v > 0 { iters = v; }
    }
    // Backing file for the busy siblings.
    let f = open(b"/tmp/mmapbench.dat\0".as_ptr(), O_RDWR | O_CREAT, 0o644);
    if f >= 0 {
        write(f, core::ptr::addr_of!(BIG) as *const c_void, 1 << 20);
        close(f);
    }
    let memfd = memfd_create(b"mmapbench\0".as_ptr(), 0);
    if memfd < 0 || ftruncate(memfd, LEN as i64) < 0 { out(b"memfd failed\n"); return 1; }

    case(b"anon", 0, memfd, iters, b"");
    case(b"memfd", 1, memfd, iters, b"");
    case(b"memfd-untouched", 2, memfd, iters, b"");
    sysstat();

    let mut th: pthread_t = core::ptr::null_mut();
    for _ in 0..3 { pthread_create(&mut th, core::ptr::null(), busy, core::ptr::null_mut()); }
    case(b"anon", 0, memfd, iters, b" +3busy");
    case(b"memfd", 1, memfd, iters, b" +3busy");
    case(b"memfd-untouched", 2, memfd, iters, b" +3busy");
    STOP.store(true, Ordering::Relaxed);
    sysstat();
    out(b"mmapbench: done\n");
    0
}
