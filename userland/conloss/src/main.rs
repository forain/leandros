//! conloss -- console output must be lossless (lane conloss).
//!
//! `conloss [lines] [burst] [sleep_ms]` writes `lines` numbered lines to fd 1
//! (default 20000), `burst` lines per write(2) call (default 40), sleeping
//! `sleep_ms` between bursts (default 0). Each line is
//! `CONLOSS <seq:06> <40 x 'x'>`. A write that returns short is continued
//! like any careful writer would (EAGAIN/EINTR retried), and the counts are
//! printed at the end:
//!
//!     CONLOSS-DONE lines=N bytes=B calls=C short=S retry=R
//!
//! The host (`conlosstest.py`) checks the stream for a gap in the sequence.

#![no_std]
#![no_main]
#![feature(c_variadic)]

#[path = "../../muslrt/rt.rs"]
mod rt;

extern "C" {
    pub fn exit(status: i32) -> !;
}

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const WRITE: usize = 1;
    pub const NANOSLEEP: usize = 35;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const WRITE: usize = 64;
    pub const NANOSLEEP: usize = 101;
}

#[cfg(target_arch = "x86_64")]
unsafe fn sc3(n: usize, a: usize, b: usize, c: usize) -> isize {
    let r: isize;
    core::arch::asm!("syscall", inlateout("rax") n as isize => r, in("rdi") a, in("rsi") b,
        in("rdx") c, out("rcx") _, out("r11") _, options(nostack));
    r
}
#[cfg(target_arch = "aarch64")]
unsafe fn sc3(n: usize, a: usize, b: usize, c: usize) -> isize {
    let r: isize;
    core::arch::asm!("svc #0", in("x8") n, inlateout("x0") a as isize => r, in("x1") b,
        in("x2") c, options(nostack));
    r
}

static mut STATS: [u64; 4] = [0; 4]; // bytes, calls, short, retry

/// Write all of `s` to fd 1, continuing after short writes and retrying
/// EINTR/EAGAIN.
unsafe fn put_all(mut s: &[u8]) {
    while !s.is_empty() {
        let r = sc3(nr::WRITE, 1, s.as_ptr() as usize, s.len());
        STATS[1] += 1;
        if r < 0 {
            if r == -11 || r == -4 { STATS[3] += 1; continue; }
            return;
        }
        let n = r as usize;
        STATS[0] += n as u64;
        if n < s.len() { STATS[2] += 1; }
        s = &s[n..];
    }
}

unsafe fn sleep_ms(ms: u64) {
    if ms == 0 { return; }
    let ts = [ms / 1000, (ms % 1000) * 1_000_000];
    sc3(nr::NANOSLEEP, ts.as_ptr() as usize, 0, 0);
}

fn parse(p: *const u8, dflt: u64) -> u64 {
    if p.is_null() { return dflt; }
    let mut v = 0u64;
    let mut i = 0;
    unsafe {
        while *p.add(i) >= b'0' && *p.add(i) <= b'9' {
            v = v * 10 + (*p.add(i) - b'0') as u64;
            i += 1;
        }
        if i == 0 { dflt } else { v }
    }
}

fn arg(argc: i32, argv: *mut *mut u8, i: i32, dflt: u64) -> u64 {
    if i >= argc { dflt } else { parse(unsafe { *argv.offset(i as isize) }, dflt) }
}

fn fmt_u(buf: &mut [u8], mut n: u64) -> usize {
    let mut t = [0u8; 20];
    let mut i = t.len();
    if n == 0 { i -= 1; t[i] = b'0'; }
    while n > 0 { i -= 1; t[i] = b'0' + (n % 10) as u8; n /= 10; }
    let l = t.len() - i;
    buf[..l].copy_from_slice(&t[i..]);
    l
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let lines = arg(argc, argv, 1, 20000);
    let burst = arg(argc, argv, 2, 40).clamp(1, 100);
    let slp = arg(argc, argv, 3, 0);
    const LINE: usize = 56;
    let mut buf = [0u8; LINE * 100];
    let mut seq = 1u64;
    while seq <= lines {
        let mut off = 0;
        let mut k = 0;
        while k < burst && seq <= lines {
            let l = &mut buf[off..off + LINE];
            l[..8].copy_from_slice(b"CONLOSS ");
            let mut d = seq;
            for j in (0..6).rev() { l[8 + j] = b'0' + (d % 10) as u8; d /= 10; }
            l[14] = b' ';
            for j in 15..LINE - 1 { l[j] = b'x'; }
            l[LINE - 1] = b'\n';
            off += LINE;
            seq += 1;
            k += 1;
        }
        put_all(&buf[..off]);
        sleep_ms(slp);
    }
    let mut o = [0u8; 160];
    let mut n = 0;
    let mut add = |o: &mut [u8; 160], n: &mut usize, s: &[u8]| { o[*n..*n + s.len()].copy_from_slice(s); *n += s.len(); };
    add(&mut o, &mut n, b"CONLOSS-DONE lines=");
    n += fmt_u(&mut o[n..], lines);
    for (name, i) in [(&b" bytes="[..], 0usize), (&b" calls="[..], 1), (&b" short="[..], 2), (&b" retry="[..], 3)] {
        add(&mut o, &mut n, name);
        n += fmt_u(&mut o[n..], STATS[i]);
    }
    add(&mut o, &mut n, b"\n");
    put_all(&o[..n]);
    exit(0)
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(101) }
}
