//! evsplit — two concurrent readers of ONE evdev node.
//!
//! Linux gives every `open("/dev/input/eventN")` its own event queue and
//! broadcasts each event to all of them, so two processes holding the same
//! device both see the whole stream. LeandrOS had a single ring per device,
//! which two readers *split*: the `[EVSTAT]` census recorded
//! `dev=0 push=128 conspop=112 deliv=16` — the in-kernel console drain and a
//! userspace client robbing each other of the same keystrokes.
//!
//! This is the regression test for that. Parent and child each open
//! /dev/input/event1 independently (the child opens after the fork, so the two
//! are separate opens, not a shared file description), epoll it, and drain it
//! for a fixed window while the host injects absolute pointer motion via QMP
//! `input-send-event`. Each injected move produces exactly one ABS_X event.
//!
//! The two counts alone cannot tell broadcast from a fair split — 60 moves come
//! out as 30/30 under a shared ring and 60/60 under per-open queues, and both
//! are "equal". So the injected count is the yardstick and is passed in:
//! `evsplit <injected>`. Without it the run still reports its counts, and the
//! host compares.
//!
//! `wakes` counts epoll_wait returns with the fd ready, and exists for the same
//! reason: readiness has to be answered from the caller's OWN queue, or one
//! reader's drain silently un-readies the other.
//!
//! Each reader prints its own `evsplit <role> ...` line; the child also returns
//! its ABS_X count as its exit status, which is how the parent gets it (a
//! MAP_SHARED page written by the child read back as zeroes here, so the tally
//! travels by the one channel `forktest` already proves works). The parent
//! prints the `evsplit result=` verdict. Exit code is the number of readers
//! that saw nothing.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_uint = u32;
type size_t = usize;
type pid_t = i32;

const O_RDONLY: c_int = 0;
const O_NONBLOCK: c_int = 0o4000;

const EPOLL_CTL_ADD: c_int = 1;
const EPOLLIN: c_uint = 0x001;

const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;
const ABS_X: u16 = 0;
/// `SYN_DROPPED` — "your queue was discarded, your state is stale, resync".
/// Counted because the VT gate is required to force exactly one of these onto a
/// queue that has just been un-backgrounded, and a resync that never arrives
/// looks identical to one that did until a client acts on stale state.
const SYN_DROPPED: u16 = 3;

const CLOCK_MONOTONIC: c_int = 1;

/// Drain window. The host injects for the middle few seconds of it; the
/// margins absorb process start-up and the f2fs exec of the child.
const WINDOW_MS: i64 = 14_000;

#[repr(C)]
#[derive(Clone, Copy)]
struct input_event {
    tv_sec: i64,
    tv_usec: i64,
    type_: u16,
    code: u16,
    value: i32,
}

#[repr(C)]
pub struct timespec {
    tv_sec: i64,
    tv_nsec: i64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub union epoll_data {
    pub ptr: *mut c_void,
    pub fd: c_int,
    pub u32_: c_uint,
    pub u64_: u64,
}

#[cfg(target_arch = "x86_64")]
#[repr(C, packed)]
#[derive(Clone, Copy)]
pub struct epoll_event {
    pub events: c_uint,
    pub data: epoll_data,
}
#[cfg(not(target_arch = "x86_64"))]
#[repr(C)]
#[derive(Clone, Copy)]
pub struct epoll_event {
    pub events: c_uint,
    pub data: epoll_data,
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;
    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: c_int, buf: *const c_void, count: size_t) -> isize;
    pub fn exit(status: c_int) -> !;
    pub fn _exit(status: c_int) -> !;
    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn close(fd: c_int) -> c_int;
    pub fn read(fd: c_int, buf: *mut c_void, count: size_t) -> isize;
    pub fn fork() -> pid_t;
    pub fn getpid() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn clock_gettime(clk: c_int, ts: *mut timespec) -> c_int;
    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    pub fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   xor rbp, rbp", "   mov rdi, rsp", "   mov rsi, offset evsplit_main",
    "   and rsp, -16", "   call relibc_start_v1", "   ud2"
);
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   mov x29, #0", "   mov x30, #0", "   mov x0, sp",
    "   adrp x1, evsplit_main", "   add x1, x1, :lo12:evsplit_main",
    "   and sp, x0, #-16", "   bl relibc_start_v1", "   brk #0"
);

/// What one reader observed.
#[repr(C)]
#[derive(Clone, Copy)]
struct Tally {
    pid: u32,
    absx: u32,
    total: u32,
    wakes: u32,
    syndrop: u32,
}

fn now_ms() -> i64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(CLOCK_MONOTONIC, &mut ts as *mut _) };
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

fn put_dec(v: u64) {
    let mut buf = [0u8; 24];
    let mut i = buf.len();
    let mut n = v;
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 { break; }
    }
    unsafe { write(1, buf.as_ptr().add(i) as *const c_void, buf.len() - i) };
}

/// Decimal argv value; 0 for anything unparseable (which reads as "no yardstick").
unsafe fn atou(mut p: *const u8) -> u32 {
    let mut v: u32 = 0;
    while *p >= b'0' && *p <= b'9' {
        v = v.wrapping_mul(10).wrapping_add((*p - b'0') as u32);
        p = p.add(1);
    }
    v
}

fn put(s: &[u8]) {
    unsafe { write(1, s.as_ptr() as *const c_void, s.len()) };
}

fn report(role: &[u8], t: &Tally) {
    put(b"evsplit ");
    put(role);
    put(b" pid=");   put_dec(t.pid as u64);
    put(b" absx=");  put_dec(t.absx as u64);
    put(b" total="); put_dec(t.total as u64);
    put(b" wakes="); put_dec(t.wakes as u64);
    put(b" syndrop="); put_dec(t.syndrop as u64);
    put(b"\n");
}

/// Open node `dev`, epoll it, and drain until the window closes.
///
/// The counted event is the one the injector produces exactly one of per
/// action: ABS_X on the tablet (`input-send-event`), a key-down on the keyboard
/// (`sendkey`).
unsafe fn drain(dev: u32, deadline_ms: i64) -> Tally {
    let mut t = Tally { pid: getpid() as u32, absx: 0, total: 0, wakes: 0, syndrop: 0 };

    let path: &[u8] = if dev == 0 { b"/dev/input/event0\0" } else { b"/dev/input/event1\0" };
    let fd = open(path.as_ptr(), O_RDONLY | O_NONBLOCK);
    if fd < 0 { return t; }
    let epfd = epoll_create1(0);
    let mut reg = epoll_event { events: EPOLLIN, data: epoll_data { fd } };
    epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &mut reg as *mut _);

    let mut evs = [epoll_event { events: 0, data: epoll_data { u64_: 0 } }; 8];
    let mut buf = [0u8; 24 * 64];
    while now_ms() < deadline_ms {
        if epoll_wait(epfd, evs.as_mut_ptr(), 8, 200) > 0 { t.wakes += 1; }
        // Read unconditionally, not only after a wakeup: `wakes` measures
        // readiness and `absx` measures delivery, and conflating them would
        // let a readiness bug masquerade as a delivery bug.
        loop {
            let n = read(fd, buf.as_mut_ptr() as *mut c_void, buf.len());
            if n <= 0 { break; }
            let cnt = (n as usize) / 24;
            for i in 0..cnt {
                let e = core::ptr::read_unaligned(buf.as_ptr().add(i * 24) as *const input_event);
                t.total += 1;
                let counted = if dev == 0 { e.type_ == EV_KEY && e.value == 1 }
                              else { e.type_ == EV_ABS && e.code == ABS_X };
                if counted { t.absx += 1; }
                if e.type_ == EV_SYN && e.code == SYN_DROPPED { t.syndrop += 1; }
            }
            if cnt < 64 { break; }
        }
    }
    close(epfd);
    close(fd);
    t
}

#[no_mangle]
pub unsafe extern "C" fn evsplit_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    let expect = if argc > 1 { atou(*argv.offset(1)) } else { 0 };
    // Node to drain: 1 (tablet) by default, 0 (keyboard) to put the two readers
    // up against the in-kernel console drain, which is the third consumer of
    // that node and the one the census caught robbing the others.
    let dev = if argc > 2 { atou(*argv.offset(2)) } else { 1 };
    let deadline = now_ms() + WINDOW_MS;
    put(b"evsplit: two readers draining /dev/input/event");
    put_dec(dev as u64);
    put(b"\n");

    let pid = fork();
    if pid == 0 {
        let t = drain(dev, deadline);
        report(b"B", &t);
        // Exit status is the child's ABS_X count, capped to the 8 bits a wait
        // status carries. The injected move count stays well under that.
        _exit(if t.absx > 200 { 200 } else { t.absx as c_int });
    }

    let a = drain(dev, deadline);
    let mut status: c_int = 0;
    waitpid(pid, &mut status as *mut _, 0);
    let b_absx = ((status >> 8) & 0xff) as u32;

    report(b"A", &a);

    // The verdict a shared ring cannot produce: both readers saw the same
    // stream. `min/max` rather than equality — the two windows do not close on
    // the same event, so a small skew is not a failure.
    let lo = if a.absx < b_absx { a.absx } else { b_absx };
    let hi = if a.absx > b_absx { a.absx } else { b_absx };
    put(b"evsplit result=");
    if lo == 0 {
        put(b"STARVED");
    } else if expect == 0 {
        put(b"UNJUDGED"); // no yardstick: 30/30 and 60/60 look the same
    } else if lo + 3 >= expect {
        put(b"BROADCAST");
    } else {
        put(b"SPLIT");
    }
    put(b" expect="); put_dec(expect as u64);
    put(b" min="); put_dec(lo as u64);
    put(b" max="); put_dec(hi as u64);
    put(b" sum="); put_dec((a.absx + b_absx) as u64);
    put(b"\n");
    puts(b"--- evsplit done ---\0".as_ptr());

    (if a.absx == 0 { 1 } else { 0 }) + (if b_absx == 0 { 1 } else { 0 })
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
