//! smpwaketest — cross-thread wake LATENCY measurement, modelled on
//! wakepolltest's harness shape (relibc_start_v1 entry, raw `syscall(nr::…)`
//! futex/eventfd calls, PASS/FAIL report lines) but reporting a number
//! instead of a boolean: how long does a blocked thread take to return once
//! a peer thread (likely on another CPU) issues the wake?
//!
//! Method: every ping-pong round has the waker stamp a shared `wrote_at_us`
//! timestamp (CLOCK_MONOTONIC, microsecond resolution) immediately before
//! the wake, then the waiter, once it returns, computes
//! `now_us() - wrote_at_us` as that round's latency.
//!
//! Futex protocol note: a pure FUTEX_WAKE against a constant word (compared
//! to an always-`0` FUTEX_WAIT expected value) is racy — a wake issued
//! before the peer has actually entered FUTEX_WAIT is simply dropped, by
//! futex semantics generally, not a LeandrOS bug. Every futex subtest here
//! therefore uses the standard value-change discipline: the waiter reads
//! `v = word.load()` immediately before `FUTEX_WAIT(word, expected = v)`;
//! the waker does `word.fetch_add(1)` THEN `FUTEX_WAKE(word)`. A wake that
//! raced ahead of the wait now makes FUTEX_WAIT return -EAGAIN immediately
//! (the value already moved), which is counted as a normal fast round trip,
//! never as `lost`. `lost=<n>` is now reserved for a genuine dropped wake:
//! FUTEX_WAIT timing out at ~2000ms despite the value having changed.
//!
//! Every ping-pong also ends with an explicit shutdown handshake (a DONE flag
//! checked BEFORE a returned wait is classified, plus a bump-until-the-peer-is
//! -gone pump). Without it the last round is always miscounted: a thread that
//! hits its deadline check right after classifying a round leaves without
//! releasing its partner, and the partner — already parked — records the 2 s
//! timeout as `lost` even though no wake was ever sent to it. That artifact is
//! a fixed cost per subtest, not a per-round probability, which is how it was
//! told apart from a kernel race: it stayed at 1 (2-thread) and 2 (ring)
//! across runs whose iteration counts differed by 2.5x.
//!
//! The eventfd ping-pong does NOT need this discipline — an eventfd counter
//! is level-triggered and persists (unlike a futex word's compare-and-block
//! semantics): if the write lands before the reader's epoll_wait call, the
//! fd is already readable and epoll_wait returns at once rather than
//! blocking. Each thread registers its own epoll interest (epoll_ctl ADD)
//! before any peer could possibly target that fd, so an early write can
//! never be silently lost.
//!
//! Load subtests (`*_under_spin_load` / `*_under_yield_load`) reproduce the
//! same ping-pongs while N background threads (N = the online CPU count via
//! sched_getaffinity, or 4 if that read fails) hog every CPU — either
//! busy-spinning or repeatedly `sched_yield()`ing. The kernel has no
//! wake-up preemption: a woken task is only actually dispatched when a CPU
//! goes idle or at the next 10ms tick, so under saturation these subtests
//! are expected to show latency balloon toward tick granularity; they PASS
//! on `max < 100ms` alone (no `lost` gate) — the printed numbers are the
//! point, not a binary verdict.
//!
//! Each subtest prints "<name>: PASS/FAIL max=<ms> avg_us=<us> iters=<n>"
//! plus "lost=<n>"; `smp_main` returns the failure count as the exit status,
//! printing "--- smpwaketest done ---" last. Total runtime ~35s (7 * 5s).

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]
#![allow(static_mut_refs)]

use core::ffi::c_void;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

type c_int = i32;
type c_long = i64;
type c_uint = u32;
type size_t = usize;
type ssize_t = isize;
type time_t = i64;
type clockid_t = c_int;

const EPOLLIN: c_uint = 0x001;
const EPOLL_CTL_ADD: c_int = 1;
const CLOCK_MONOTONIC: clockid_t = 1;

// FUTEX_WAIT / FUTEX_WAKE opcodes (Linux futex(2) ABI, as used by
// wakepolltest's test_xthread_futex_timed_wake).
const FUTEX_WAIT_OP: i64 = 0;
const FUTEX_WAKE_OP: i64 = 1;

// FUTEX_WAIT's "value already changed" fast-return errno (-EAGAIN). A wake
// that raced ahead of the wait shows up as this, not as a lost wake.
const EAGAIN: i64 = -11;

// Every blocking wait (futex or epoll) uses this finite window. A prompt
// wake returns in well under 100ms; a genuinely lost wake sleeps to ~2000ms
// and is counted separately rather than silently inflating "max".
const WAIT_TIMEOUT_MS: c_int = 2000;
const WAIT_TIMEOUT_TS: timespec = timespec { tv_sec: 2, tv_nsec: 0 };

// PASS threshold shared by every subtest: worst observed latency under 100ms.
const PASS_MAX_US: i64 = 100_000;

// Per-subtest ping-pong duration. 7 subtests * 5s = 35s, under the ~45s budget.
const DURATION_MS: i64 = 5000;

// Bounded startup handshake: give a just-created peer thread a moment to
// reach its first blocking wait before the initiator's first wake, so the
// very first round of a ping-pong isn't a false "lost" from thread-start
// scheduling lag. Never a hang risk — it's a bounded poll, not a wait.
const STARTUP_WAIT_MS: i64 = 200;

// Load subtests: cap on background spin/yield threads and a settle delay
// after spawning them, so the ping-pong doesn't start measuring before the
// load threads have actually begun saturating their CPUs.
const MAX_LOAD_THREADS: usize = 64;
const LOAD_SETTLE_US: c_uint = 50_000;

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const EVENTFD2: i64 = 290;
    pub const FUTEX: i64 = 202;
    pub const SCHED_YIELD: i64 = 24;
    pub const SCHED_GETAFFINITY: i64 = 204;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const EVENTFD2: i64 = 19;
    pub const FUTEX: i64 = 98;
    pub const SCHED_YIELD: i64 = 124;
    pub const SCHED_GETAFFINITY: i64 = 123;
}

pub type pthread_t = *mut c_void;

#[repr(C)]
#[derive(Clone, Copy)]
pub union epoll_data {
    pub ptr: *mut c_void,
    pub fd: c_int,
    pub u32: c_uint,
    pub u64: u64,
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

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec {
    pub tv_sec: time_t,
    pub tv_nsec: c_long,
}

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: c_int, buf: *const u8, count: size_t) -> ssize_t;
    pub fn read(fd: c_int, buf: *mut u8, count: size_t) -> ssize_t;
    pub fn close(fd: c_int) -> c_int;
    pub fn exit(status: c_int) -> !;
    pub fn usleep(usec: c_uint) -> c_int;
    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;

    pub fn epoll_create1(flags: c_int) -> c_int;
    pub fn epoll_ctl(epfd: c_int, op: c_int, fd: c_int, event: *mut epoll_event) -> c_int;
    pub fn epoll_wait(epfd: c_int, events: *mut epoll_event, maxevents: c_int, timeout: c_int) -> c_int;

    pub fn clock_gettime(clockid: clockid_t, tp: *mut timespec) -> c_int;

    pub fn pthread_create(
        thread: *mut pthread_t,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
    pub fn pthread_join(thread: pthread_t, retval: *mut *mut c_void) -> c_int;

    pub fn syscall(sysno: c_long, ...) -> c_long;

    // Used only by the pipe herd subtests (8/9).
    pub fn pipe2(fds: *mut c_int, flags: c_int) -> c_int;
    pub fn fcntl(fd: c_int, cmd: c_int, arg: c_int) -> c_int;
    pub fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    pub fn fork() -> c_int;
    pub fn waitpid(pid: c_int, status: *mut c_int, options: c_int) -> c_int;
    pub fn _exit(status: c_int) -> !;
}

// ── entry (identical shim to wakepolltest/pthreadtest) ──────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset smp_main",
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
    "   adrp x1, smp_main",
    "   add x1, x1, :lo12:smp_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── helpers ────────────────────────────────────────────────────────────────

unsafe fn now_ms() -> i64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1000 + ts.tv_nsec / 1_000_000
}

unsafe fn now_us() -> i64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000
}

unsafe fn futex_wait(word: *const u32, expected: u32, ts: &timespec) -> i64 {
    syscall(nr::FUTEX, word as c_long, FUTEX_WAIT_OP, expected as c_long, ts as *const timespec as c_long, 0i64, 0i64)
}

unsafe fn futex_wake(word: *const u32) {
    syscall(nr::FUTEX, word as c_long, FUTEX_WAKE_OP, 1i64, 0i64, 0i64, 0i64);
}

// Bump the futex word (so a racing FUTEX_WAIT sees a value mismatch and
// returns EAGAIN instead of dropping the wake) then FUTEX_WAKE it.
unsafe fn bump_wake(word: &AtomicU32) {
    word.fetch_add(1, Ordering::SeqCst);
    futex_wake(word.as_ptr());
}

// Number of online CPUs via sched_getaffinity's popcount; 4 if unavailable.
unsafe fn cpu_count() -> usize {
    let mut mask = [0u8; 16];
    let r = syscall(nr::SCHED_GETAFFINITY, 0i64, mask.len() as c_long, mask.as_mut_ptr() as c_long, 0i64, 0i64, 0i64);
    if r <= 0 { return 4; }
    let mut n = 0usize;
    for b in mask.iter() { n += b.count_ones() as usize; }
    if n == 0 { 4 } else { n }
}

unsafe fn spawn_load(n: usize, f: extern "C" fn(*mut c_void) -> *mut c_void, threads: &mut [pthread_t; MAX_LOAD_THREADS]) -> usize {
    let n = if n > MAX_LOAD_THREADS { MAX_LOAD_THREADS } else { n };
    let mut created = 0usize;
    for i in 0..n {
        if pthread_create(&mut threads[i], core::ptr::null(), f, core::ptr::null_mut()) != 0 { break; }
        created += 1;
    }
    created
}

unsafe fn join_load(threads: &[pthread_t; MAX_LOAD_THREADS], n: usize) {
    for &t in threads.iter().take(n) { pthread_join(t, core::ptr::null_mut()); }
}

// "<name>: PASS/FAIL max=<ms>ms avg_us=<us>us iters=<n>[ lost=<n>]\n"
fn report_stat(name: &[u8], ok: bool, max_ms: i64, avg_us: i64, iters: i64, lost: Option<i64>) -> bool {
    unsafe {
        let mut line = [0u8; 200];
        let mut p = 0usize;
        macro_rules! put { ($s:expr) => { for &b in $s { line[p] = b; p += 1; } } }
        macro_rules! num { ($v:expr) => {{
            let mut e: i64 = $v;
            let neg = e < 0;
            if neg { e = -e; }
            if neg { line[p] = b'-'; p += 1; }
            let mut d = [0u8; 16];
            let mut k = 0usize;
            if e == 0 { d[k] = b'0'; k += 1; }
            while e > 0 { d[k] = b'0' + (e % 10) as u8; e /= 10; k += 1; }
            while k > 0 { k -= 1; line[p] = d[k]; p += 1; }
        }} }
        put!(name);
        put!(if ok { b": PASS " } else { b": FAIL " });
        put!(b"max="); num!(max_ms); put!(b"ms avg_us="); num!(avg_us);
        put!(b"us iters="); num!(iters);
        if let Some(l) = lost { put!(b" lost="); num!(l); }
        put!(b"\n\0");
        puts(line.as_ptr());
    }
    ok
}

// ── background load generators (used by the *_under_*_load subtests) ───────

static SPIN_STOP: AtomicBool = AtomicBool::new(false);
static SPIN_COUNTER: AtomicU64 = AtomicU64::new(0);

extern "C" fn spin_worker(_arg: *mut c_void) -> *mut c_void {
    while !SPIN_STOP.load(Ordering::Relaxed) {
        SPIN_COUNTER.fetch_add(1, Ordering::Relaxed);
    }
    core::ptr::null_mut()
}

static YIELD_STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn yield_worker(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        while !YIELD_STOP.load(Ordering::Relaxed) {
            syscall(nr::SCHED_YIELD, 0i64, 0i64, 0i64, 0i64, 0i64, 0i64);
        }
    }
    core::ptr::null_mut()
}

// ── 1. futex_pingpong_2threads — A and B ping-pong through two futex words
//    for DURATION_MS, using the value-change discipline described above.

static WORD_A: AtomicU32 = AtomicU32::new(0);
static WORD_B: AtomicU32 = AtomicU32::new(0);
static mut WROTE_A_US: i64 = -1;
static mut WROTE_B_US: i64 = -1;
static mut MAX_A_US: i64 = 0;
static mut MAX_B_US: i64 = 0;
static mut SUM_A_US: i64 = 0;
static mut SUM_B_US: i64 = 0;
static mut CNT_A: u32 = 0;
static mut CNT_B: u32 = 0;
static mut LOST_A: u32 = 0;
static mut LOST_B: u32 = 0;
static mut END_MS_2: i64 = 0;
static STARTED_B2: AtomicBool = AtomicBool::new(false);

// ── Shutdown handshake (shared shape with the ring and eventfd subtests) ────
//
// Without one, the LAST round of every ping-pong is miscounted. A thread that
// reaches its deadline check right after classifying a round breaks out
// *before* releasing its partner, and the partner — already parked in
// FUTEX_WAIT — sits there until the 2 s timeout and records `lost`. That is a
// harness teardown artifact, not a dropped wake: nobody ever issued the wake
// it was waiting for. It is also why the counts were stubbornly constant
// (1 for a 2-thread ping-pong, 2 for the ring) across runs whose iteration
// counts differed by 2.5× — a real per-round kernel race would scale with
// iterations, a teardown artifact cannot.
//
// So: announce the shutdown, mark ourselves gone, then keep bumping the
// partner's word until it is gone too. The partner wakes (or gets EAGAIN),
// sees the flag *before* classifying, and leaves without recording anything.
// Checking the flag before classifying is what makes this airtight — the
// bump-pump only makes it fast, it is not load-bearing for correctness.
static PP_DONE: AtomicBool = AtomicBool::new(false);
static PP_EXIT: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

unsafe fn pp2_shutdown(me: usize, peer_word: &AtomicU32) {
    PP_DONE.store(true, Ordering::Release);
    // Ours first, so the two threads can never pump each other forever.
    PP_EXIT[me].store(true, Ordering::Release);
    while !PP_EXIT[1 - me].load(Ordering::Acquire) {
        bump_wake(peer_word);
        usleep(500);
    }
}

extern "C" fn pp2_a(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let wait_start = now_ms();
        while !STARTED_B2.load(Ordering::Acquire) && now_ms() - wait_start < STARTUP_WAIT_MS { usleep(200); }

        // Snapshot our own word BEFORE releasing the peer for the first
        // time: if we snapshotted after bump_wake(&WORD_B), a fast B could
        // already have processed, bumped+woken WORD_A and gone back to
        // sleep before we ever read WORD_A — we'd then snapshot the
        // already-incremented value and block on a wake that already
        // happened (a 2s mutual stall counted as lost on both sides).
        let mut v = WORD_A.load(Ordering::SeqCst);
        WROTE_B_US = now_us();
        bump_wake(&WORD_B);
        loop {
            if now_ms() >= END_MS_2 || PP_DONE.load(Ordering::Acquire) { break; }
            let r = futex_wait(WORD_A.as_ptr(), v, &WAIT_TIMEOUT_TS);
            // B has stopped: this return is a shutdown bump (or the timeout
            // racing it), not a round. Leave without classifying it.
            if PP_DONE.load(Ordering::Acquire) { break; }
            let now = now_us();
            if r == 0 || r == EAGAIN {
                let lat = now - WROTE_A_US;
                if lat > MAX_A_US { MAX_A_US = lat; }
                SUM_A_US += lat;
                CNT_A += 1;
            } else {
                LOST_A += 1;
            }
            if now_ms() >= END_MS_2 { break; }
            // Snapshot the NEXT round's expected value before releasing B
            // again, for the same reason as above.
            v = WORD_A.load(Ordering::SeqCst);
            WROTE_B_US = now_us();
            bump_wake(&WORD_B);
        }
        pp2_shutdown(0, &WORD_B);
    }
    core::ptr::null_mut()
}

extern "C" fn pp2_b(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        // Snapshot before announcing we've started, so A's initial kick
        // can't race ahead of this baseline read either.
        let mut v = WORD_B.load(Ordering::SeqCst);
        STARTED_B2.store(true, Ordering::Release);
        loop {
            if now_ms() >= END_MS_2 || PP_DONE.load(Ordering::Acquire) { break; }
            let r = futex_wait(WORD_B.as_ptr(), v, &WAIT_TIMEOUT_TS);
            if PP_DONE.load(Ordering::Acquire) { break; }
            let now = now_us();
            if r == 0 || r == EAGAIN {
                let lat = now - WROTE_B_US;
                if lat > MAX_B_US { MAX_B_US = lat; }
                SUM_B_US += lat;
                CNT_B += 1;
            } else {
                LOST_B += 1;
            }
            if now_ms() >= END_MS_2 { break; }
            v = WORD_B.load(Ordering::SeqCst);
            WROTE_A_US = now_us();
            bump_wake(&WORD_A);
        }
        pp2_shutdown(1, &WORD_A);
    }
    core::ptr::null_mut()
}

// Shared by the plain subtest and the two *_under_*_load variants.
// `require_no_lost` gates whether a nonzero `lost` count fails the subtest:
// true for the plain wake-path check, false for the load variants (which
// PASS on max<100ms alone — the point there is the printed numbers, not a
// binary verdict, since a saturated kernel may legitimately need the full
// timeout to reschedule a woken thread).
unsafe fn run_futex_pingpong_2(name: &[u8], require_no_lost: bool) -> bool {
    WORD_A.store(0, Ordering::SeqCst);
    WORD_B.store(0, Ordering::SeqCst);
    WROTE_A_US = -1; WROTE_B_US = -1;
    MAX_A_US = 0; MAX_B_US = 0;
    SUM_A_US = 0; SUM_B_US = 0;
    CNT_A = 0; CNT_B = 0;
    LOST_A = 0; LOST_B = 0;
    STARTED_B2.store(false, Ordering::Release);
    PP_DONE.store(false, Ordering::Release);
    PP_EXIT[0].store(false, Ordering::Release);
    PP_EXIT[1].store(false, Ordering::Release);
    END_MS_2 = now_ms() + DURATION_MS;

    let mut tb: pthread_t = core::ptr::null_mut();
    let mut ta: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut tb, core::ptr::null(), pp2_b, core::ptr::null_mut()) != 0 {
        return report_stat(name, false, 0, 0, 0, Some(0));
    }
    if pthread_create(&mut ta, core::ptr::null(), pp2_a, core::ptr::null_mut()) != 0 {
        pthread_join(tb, core::ptr::null_mut());
        return report_stat(name, false, 0, 0, 0, Some(0));
    }
    pthread_join(ta, core::ptr::null_mut());
    pthread_join(tb, core::ptr::null_mut());

    let iters = (CNT_A + CNT_B) as i64;
    let sum = SUM_A_US + SUM_B_US;
    let avg_us = if iters > 0 { sum / iters } else { 0 };
    let max_us = if MAX_A_US > MAX_B_US { MAX_A_US } else { MAX_B_US };
    let lost = (LOST_A + LOST_B) as i64;
    let ok = max_us < PASS_MAX_US && (!require_no_lost || lost == 0);
    report_stat(name, ok, max_us / 1000, avg_us, iters, Some(lost))
}

unsafe fn test_futex_pingpong_2threads() -> bool {
    run_futex_pingpong_2(b"futex_pingpong_2threads", true)
}

// ── 2. futex_pingpong_4threads — same shape but a 4-thread ring A→B→C→D→A,
//    so more than one CPU is involved in the wake chain at once.

static RWORD: [AtomicU32; 4] = [AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0), AtomicU32::new(0)];
static mut RWROTE_US: [i64; 4] = [-1; 4];
static mut RMAX_US: [i64; 4] = [0; 4];
static mut RSUM_US: [i64; 4] = [0; 4];
static mut RCNT: [u32; 4] = [0; 4];
static mut RLOST: [u32; 4] = [0; 4];
static RSTARTED: [AtomicBool; 4] = [
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
];
static mut REND_MS: i64 = 0;

// Same teardown handshake as pp2_shutdown, around the ring. Marking ourselves
// gone BEFORE pumping the successor is what keeps it acyclic: thread k pumps
// k+1 until k+1 has left, and the lap that comes back to thread 0 finds
// REXIT[0] already set instead of waiting on a thread that is waiting on it.
static RDONE: AtomicBool = AtomicBool::new(false);
static REXIT: [AtomicBool; 4] = [
    AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false), AtomicBool::new(false),
];

unsafe fn ring_shutdown(idx: usize) {
    let next = (idx + 1) % 4;
    RDONE.store(true, Ordering::Release);
    REXIT[idx].store(true, Ordering::Release);
    while !REXIT[next].load(Ordering::Acquire) {
        bump_wake(&RWORD[next]);
        usleep(500);
    }
}

extern "C" fn ring_thread(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let idx = arg as usize;
        let next = (idx + 1) % 4;

        // Snapshot our own word BEFORE announcing we've started (idx!=0's
        // upstream neighbor can only bump RWORD[idx] after seeing us ready,
        // and idx==0's upstream can only do so after a full lap of the
        // ring), so no upstream bump can race ahead of this baseline read.
        let mut v = RWORD[idx].load(Ordering::SeqCst);
        RSTARTED[idx].store(true, Ordering::Release);

        if idx == 0 {
            let wait_start = now_ms();
            while !RSTARTED[1].load(Ordering::Acquire) && now_ms() - wait_start < STARTUP_WAIT_MS { usleep(200); }
            RWROTE_US[next] = now_us();
            bump_wake(&RWORD[next]);
        }

        loop {
            if now_ms() >= REND_MS || RDONE.load(Ordering::Acquire) { break; }
            let r = futex_wait(RWORD[idx].as_ptr(), v, &WAIT_TIMEOUT_TS);
            // Our upstream neighbour has stopped: a shutdown bump, not a round.
            if RDONE.load(Ordering::Acquire) { break; }
            let now = now_us();
            if r == 0 || r == EAGAIN {
                let lat = now - RWROTE_US[idx];
                if lat > RMAX_US[idx] { RMAX_US[idx] = lat; }
                RSUM_US[idx] += lat;
                RCNT[idx] += 1;
            } else {
                RLOST[idx] += 1;
            }
            if now_ms() >= REND_MS { break; }
            // Snapshot the NEXT round's expected value before releasing
            // the downstream neighbor, same rationale as pp2_a/pp2_b: once
            // released, the wake can propagate all the way around the ring
            // and back to us before we'd otherwise re-read RWORD[idx].
            v = RWORD[idx].load(Ordering::SeqCst);
            RWROTE_US[next] = now_us();
            bump_wake(&RWORD[next]);
        }
        ring_shutdown(idx);
    }
    core::ptr::null_mut()
}

unsafe fn test_futex_pingpong_4threads() -> bool {
    let name = b"futex_pingpong_4threads";
    for i in 0..4 {
        RWORD[i].store(0, Ordering::SeqCst);
        RWROTE_US[i] = -1; RMAX_US[i] = 0; RSUM_US[i] = 0;
        RCNT[i] = 0; RLOST[i] = 0;
        RSTARTED[i].store(false, Ordering::Release);
        REXIT[i].store(false, Ordering::Release);
    }
    RDONE.store(false, Ordering::Release);
    REND_MS = now_ms() + DURATION_MS;

    let mut threads: [pthread_t; 4] = [core::ptr::null_mut(); 4];
    // Start the ring's tail (1,2,3) before its head (0), so index 0's
    // bounded startup handshake actually has someone to find already running.
    for &i in &[1usize, 2, 3, 0] {
        if pthread_create(&mut threads[i], core::ptr::null(), ring_thread, i as *mut c_void) != 0 {
            for &t in threads.iter() {
                if !t.is_null() { pthread_join(t, core::ptr::null_mut()); }
            }
            return report_stat(name, false, 0, 0, 0, Some(0));
        }
    }
    for &t in threads.iter() { pthread_join(t, core::ptr::null_mut()); }

    let mut iters: i64 = 0;
    let mut sum: i64 = 0;
    let mut max_us: i64 = 0;
    let mut lost: i64 = 0;
    for i in 0..4 {
        iters += RCNT[i] as i64;
        sum += RSUM_US[i];
        if RMAX_US[i] > max_us { max_us = RMAX_US[i]; }
        lost += RLOST[i] as i64;
    }
    let avg_us = if iters > 0 { sum / iters } else { 0 };
    let ok = max_us < PASS_MAX_US && lost == 0;
    report_stat(name, ok, max_us / 1000, avg_us, iters, Some(lost))
}

// ── 3. eventfd_pingpong_2threads — same shape as subtest 1, using two
//    eventfds and blocking read/write. No value-change discipline needed:
//    each thread's epoll_ctl ADD happens before any peer could write to
//    that fd (gated by ESTARTED_B / the fact that a thread always registers
//    its own listen fd before entering its loop), and eventfd's counter is
//    level-triggered — an already-readable fd makes epoll_wait return at
//    once rather than requiring the write to race a not-yet-armed wait.

static mut EFD_A: c_int = -1;
static mut EFD_B: c_int = -1;
static mut EWROTE_A_US: i64 = -1;
static mut EWROTE_B_US: i64 = -1;
static mut EMAX_A_US: i64 = 0;
static mut EMAX_B_US: i64 = 0;
static mut ESUM_A_US: i64 = 0;
static mut ESUM_B_US: i64 = 0;
static mut ECNT_A: u32 = 0;
static mut ECNT_B: u32 = 0;
static mut ELOST_A: u32 = 0;
static mut ELOST_B: u32 = 0;
static mut EEND_MS: i64 = 0;
static ESTARTED_B: AtomicBool = AtomicBool::new(false);

// Same teardown handshake as the futex ping-pongs. This subtest happened to
// report lost=0 without it, but only by luck: whether the departing thread
// strands its partner depends on which of the two `now_ms()` deadline checks
// straddles the millisecond boundary, and a stranded epoll_wait returns n==0
// at the 2 s timeout exactly like a stranded FUTEX_WAIT. Make it structural
// rather than lucky, so a future lost=1 here means something.
static EPP_DONE: AtomicBool = AtomicBool::new(false);
static EPP_EXIT: [AtomicBool; 2] = [AtomicBool::new(false), AtomicBool::new(false)];

unsafe fn epp_shutdown(me: usize, peer_fd: c_int) {
    EPP_DONE.store(true, Ordering::Release);
    EPP_EXIT[me].store(true, Ordering::Release);
    let one: u64 = 1;
    while !EPP_EXIT[1 - me].load(Ordering::Acquire) {
        write(peer_fd, &one as *const u64 as *const u8, 8);
        usleep(500);
    }
}

unsafe fn efd_wait_drain(fd: c_int, ep: c_int) -> bool {
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    // Level-triggered EPOLLIN: if `fd`'s counter is already nonzero (the
    // peer's write landed before we got here), this returns immediately
    // instead of blocking — no arm/write race is possible.
    let n = epoll_wait(ep, out.as_mut_ptr(), 4, WAIT_TIMEOUT_MS);
    if n >= 1 {
        let mut v: u64 = 0;
        read(fd, &mut v as *mut u64 as *mut u8, 8);
        true
    } else {
        false
    }
}

extern "C" fn epp_a(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let ep = epoll_create1(0);
        let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: EFD_A } };
        epoll_ctl(ep, EPOLL_CTL_ADD, EFD_A, &mut ev);

        let wait_start = now_ms();
        while !ESTARTED_B.load(Ordering::Acquire) && now_ms() - wait_start < STARTUP_WAIT_MS { usleep(200); }

        let one: u64 = 1;
        EWROTE_B_US = now_us();
        write(EFD_B, &one as *const u64 as *const u8, 8);
        loop {
            if now_ms() >= EEND_MS || EPP_DONE.load(Ordering::Acquire) { break; }
            let ok = efd_wait_drain(EFD_A, ep);
            // B has stopped: a shutdown write (or the timeout racing it),
            // not a round. Leave without classifying it.
            if EPP_DONE.load(Ordering::Acquire) { break; }
            let now = now_us();
            if ok {
                let lat = now - EWROTE_A_US;
                if lat > EMAX_A_US { EMAX_A_US = lat; }
                ESUM_A_US += lat;
                ECNT_A += 1;
            } else {
                ELOST_A += 1;
            }
            if now_ms() >= EEND_MS { break; }
            EWROTE_B_US = now_us();
            write(EFD_B, &one as *const u64 as *const u8, 8);
        }
        epp_shutdown(0, EFD_B);
        close(ep);
    }
    core::ptr::null_mut()
}

extern "C" fn epp_b(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let ep = epoll_create1(0);
        let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd: EFD_B } };
        epoll_ctl(ep, EPOLL_CTL_ADD, EFD_B, &mut ev);
        ESTARTED_B.store(true, Ordering::Release);
        loop {
            if now_ms() >= EEND_MS || EPP_DONE.load(Ordering::Acquire) { break; }
            let ok = efd_wait_drain(EFD_B, ep);
            if EPP_DONE.load(Ordering::Acquire) { break; }
            let now = now_us();
            if ok {
                let lat = now - EWROTE_B_US;
                if lat > EMAX_B_US { EMAX_B_US = lat; }
                ESUM_B_US += lat;
                ECNT_B += 1;
            } else {
                ELOST_B += 1;
            }
            if now_ms() >= EEND_MS { break; }
            let one: u64 = 1;
            EWROTE_A_US = now_us();
            write(EFD_A, &one as *const u64 as *const u8, 8);
        }
        epp_shutdown(1, EFD_A);
        close(ep);
    }
    core::ptr::null_mut()
}

// `require_no_lost`: see run_futex_pingpong_2's doc — same rationale.
unsafe fn run_eventfd_pingpong_2(name: &[u8], require_no_lost: bool) -> bool {
    EFD_A = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
    EFD_B = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
    if EFD_A < 0 || EFD_B < 0 { return report_stat(name, false, 0, 0, 0, Some(0)); }
    EWROTE_A_US = -1; EWROTE_B_US = -1;
    EMAX_A_US = 0; EMAX_B_US = 0;
    ESUM_A_US = 0; ESUM_B_US = 0;
    ECNT_A = 0; ECNT_B = 0;
    ELOST_A = 0; ELOST_B = 0;
    ESTARTED_B.store(false, Ordering::Release);
    EPP_DONE.store(false, Ordering::Release);
    EPP_EXIT[0].store(false, Ordering::Release);
    EPP_EXIT[1].store(false, Ordering::Release);
    EEND_MS = now_ms() + DURATION_MS;

    let mut tb: pthread_t = core::ptr::null_mut();
    let mut ta: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut tb, core::ptr::null(), epp_b, core::ptr::null_mut()) != 0 {
        close(EFD_A); close(EFD_B);
        return report_stat(name, false, 0, 0, 0, Some(0));
    }
    if pthread_create(&mut ta, core::ptr::null(), epp_a, core::ptr::null_mut()) != 0 {
        pthread_join(tb, core::ptr::null_mut());
        close(EFD_A); close(EFD_B);
        return report_stat(name, false, 0, 0, 0, Some(0));
    }
    pthread_join(ta, core::ptr::null_mut());
    pthread_join(tb, core::ptr::null_mut());
    close(EFD_A); close(EFD_B);

    let iters = (ECNT_A + ECNT_B) as i64;
    let sum = ESUM_A_US + ESUM_B_US;
    let avg_us = if iters > 0 { sum / iters } else { 0 };
    let max_us = if EMAX_A_US > EMAX_B_US { EMAX_A_US } else { EMAX_B_US };
    let lost = (ELOST_A + ELOST_B) as i64;
    let ok = max_us < PASS_MAX_US && (!require_no_lost || lost == 0);
    report_stat(name, ok, max_us / 1000, avg_us, iters, Some(lost))
}

unsafe fn test_eventfd_pingpong_2threads() -> bool {
    run_eventfd_pingpong_2(b"eventfd_pingpong_2threads", true)
}

// ── 4. wake_from_wfi_idle — main thread nanosleeps 50ms between wakes (so
//    the peer is likely parked in WFI when the wake lands) for 100 rounds.

static WWORD: AtomicU32 = AtomicU32::new(0);
static mut WWROTE_US: i64 = -1;
static mut WMAX_US: i64 = 0;
static mut WSUM_US: i64 = 0;
static mut WCNT: u32 = 0;
static mut WLOST: u32 = 0;
static WSTARTED: AtomicBool = AtomicBool::new(false);
static WDONE: AtomicBool = AtomicBool::new(false);
/// Rounds the peer has finished (woken, classified and published). Main waits
/// on this before sending the next bump, so `WWROTE_US` is never overwritten
/// under a still-pending wait.
///
/// It is an atomic, and — critically — main samples it BEFORE `bump_wake`.
/// Sampling it after was the whole `lost=9`: at ~24 us wake latency the peer
/// sometimes finished the round before main got back from its FUTEX_WAKE
/// syscall, so main's baseline already included that round's ack and it then
/// waited 2.1 s for an ack that was never coming. Meanwhile the peer, having
/// re-parked on a word nobody was going to bump, took the full 2 s FUTEX_WAIT
/// timeout and recorded a `lost` — an entirely self-inflicted stall, one per
/// round main lost the race, which is exactly the ~9% that showed up.
static WACK: AtomicU32 = AtomicU32::new(0);
/// Set by the peer as it leaves, so main's shutdown can pump `WWORD` until the
/// peer is actually gone instead of firing one bump and hoping. Without the
/// pump, a peer that was between its snapshot and its FUTEX_WAIT when the
/// single shutdown bump landed parks on a word nobody will touch again and
/// burns the full 2 s timeout before noticing WDONE.
static WEXIT: AtomicBool = AtomicBool::new(false);

const WFI_ITERS: usize = 100;
const WFI_SLEEP_TS: timespec = timespec { tv_sec: 0, tv_nsec: 50_000_000 };

extern "C" fn wfi_peer(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        // Snapshot our own word BEFORE announcing we're ready, so the main
        // thread's very first bump_wake can't race ahead of this baseline
        // read either.
        let mut v = WWORD.load(Ordering::SeqCst);
        WSTARTED.store(true, Ordering::Release);
        loop {
            let r = futex_wait(WWORD.as_ptr(), v, &WAIT_TIMEOUT_TS);
            // Checked right after the wait returns (wake, EAGAIN, or
            // timeout), before recording anything, so the shutdown wake
            // never gets counted as an extra (bogus) round.
            if WDONE.load(Ordering::Acquire) { WEXIT.store(true, Ordering::Release); break; }
            let now = now_us();
            // Snapshot the NEXT round's expected value before publishing
            // this round's completion (WCNT/WLOST) — that publish is what
            // unblocks main's bounded ack-wait and lets it send the next
            // bump_wake, so the snapshot must happen first.
            v = WWORD.load(Ordering::SeqCst);
            if r == 0 || r == EAGAIN {
                let lat = now - WWROTE_US;
                if lat > WMAX_US { WMAX_US = lat; }
                WSUM_US += lat;
                WCNT += 1;
            } else {
                WLOST += 1;
            }
            // Publish the round LAST: this is what releases main to send the
            // next bump, and both the snapshot above and the stats must be
            // settled before that happens.
            WACK.fetch_add(1, Ordering::Release);
        }
    }
    core::ptr::null_mut()
}

unsafe fn test_wake_from_wfi_idle() -> bool {
    let name = b"wake_from_wfi_idle";
    WWORD.store(0, Ordering::SeqCst);
    WWROTE_US = -1; WMAX_US = 0; WSUM_US = 0;
    WCNT = 0; WLOST = 0;
    WSTARTED.store(false, Ordering::Release);
    WDONE.store(false, Ordering::Release);
    WACK.store(0, Ordering::Release);
    WEXIT.store(false, Ordering::Release);

    let mut peer: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut peer, core::ptr::null(), wfi_peer, core::ptr::null_mut()) != 0 {
        return report_stat(name, false, 0, 0, 0, Some(0));
    }
    let wait_start = now_ms();
    while !WSTARTED.load(Ordering::Acquire) && now_ms() - wait_start < STARTUP_WAIT_MS { usleep(200); }

    for _ in 0..WFI_ITERS {
        nanosleep(&WFI_SLEEP_TS, core::ptr::null_mut());
        // Sample the ack baseline BEFORE the wake it belongs to — see WACK.
        let before = WACK.load(Ordering::Acquire);
        WWROTE_US = now_us();
        bump_wake(&WWORD);
        // Bounded wait for this round's ack before sending the next wake, so
        // WWROTE_US is never overwritten out from under a still-pending wait.
        let round_start = now_ms();
        while WACK.load(Ordering::Acquire) == before
            && now_ms() - round_start < (WAIT_TIMEOUT_MS as i64 + 100)
        {
            usleep(200);
        }
    }
    WDONE.store(true, Ordering::Release);
    while !WEXIT.load(Ordering::Acquire) {
        bump_wake(&WWORD);
        usleep(500);
    }
    pthread_join(peer, core::ptr::null_mut());

    let iters = WCNT as i64;
    let avg_us = if iters > 0 { WSUM_US / iters } else { 0 };
    let lost = WLOST as i64;
    let ok = WMAX_US < PASS_MAX_US && lost == 0;
    report_stat(name, ok, WMAX_US / 1000, avg_us, iters, Some(lost))
}

// ── 5/6/7. ping-pongs under background CPU load — same cores, saturated ────

unsafe fn test_futex_pingpong_under_spin_load() -> bool {
    let name = b"futex_pingpong_under_spin_load";
    let n = cpu_count();
    SPIN_STOP.store(false, Ordering::Relaxed);
    let mut threads: [pthread_t; MAX_LOAD_THREADS] = [core::ptr::null_mut(); MAX_LOAD_THREADS];
    let created = spawn_load(n, spin_worker, &mut threads);
    usleep(LOAD_SETTLE_US);
    let ok = run_futex_pingpong_2(name, false);
    SPIN_STOP.store(true, Ordering::Relaxed);
    join_load(&threads, created);
    ok
}

unsafe fn test_futex_pingpong_under_yield_load() -> bool {
    let name = b"futex_pingpong_under_yield_load";
    let n = cpu_count();
    YIELD_STOP.store(false, Ordering::Relaxed);
    let mut threads: [pthread_t; MAX_LOAD_THREADS] = [core::ptr::null_mut(); MAX_LOAD_THREADS];
    let created = spawn_load(n, yield_worker, &mut threads);
    usleep(LOAD_SETTLE_US);
    let ok = run_futex_pingpong_2(name, false);
    YIELD_STOP.store(true, Ordering::Relaxed);
    join_load(&threads, created);
    ok
}

unsafe fn test_eventfd_pingpong_under_spin_load() -> bool {
    let name = b"eventfd_pingpong_under_spin_load";
    let n = cpu_count();
    SPIN_STOP.store(false, Ordering::Relaxed);
    let mut threads: [pthread_t; MAX_LOAD_THREADS] = [core::ptr::null_mut(); MAX_LOAD_THREADS];
    let created = spawn_load(n, spin_worker, &mut threads);
    usleep(LOAD_SETTLE_US);
    let ok = run_eventfd_pingpong_2(name, false);
    SPIN_STOP.store(true, Ordering::Relaxed);
    join_load(&threads, created);
    ok
}

// ── 8/9. pipe EPOLLET wake latency while a herd of pollers is parked ───────
//
// WHAT THESE MEASURE. The kernel has exactly one poll wait-channel
// (`sched::POLL_WAIT_CHANNEL`), so `sched::wake_poll` — which every pipe write
// calls — wakes EVERY parked poller in the system, and each woken poller then
// re-probes its entire interest set through the global FD_TABLES / PIPE_RINGS
// locks. The cost of a single 512-byte write therefore scales with
// (parked pollers x fds each), not with the bytes written. These two subtests
// make that scaling visible instead of leaving it as an inference: K background
// threads park in epoll_wait over M eventfds apiece while a writer pushes
// 64 KiB through a pipe in 512-byte chunks to an EPOLLET | O_NONBLOCK reader
// that drains to EAGAIN on each wake, mio style.
//
// A kernel whose pipe wake is targeted or coalesced finishes this in tens of
// milliseconds whatever K is. One that pays a system-wide herd per write slows
// down roughly linearly in K*M — raise HERD_POLLERS and watch total_ms track
// it; that slope IS the diagnosis. 64 KiB against a 16 KiB PIPE_RING_SIZE also
// forces genuine writer backpressure, so the reader's drain is what releases
// the writer and the full->not-full wake is exercised too.
//
// Subtest 9 repeats it with the writer in a FORKED CHILD writing to fd 1 after
// dup2(wfd, 1) — the exact shape cosmic-session/launch-pad gives every
// component it spawns (Stdio::piped() on stdout, read back by a tokio task
// registered EPOLLET). The child cannot stamp timestamps into the parent's
// address space, so its gap is measured parent-side as the longest interval
// between two consecutive reader wakes while bytes are still owed.

const HERD_POLLERS: usize = 24; // K: threads parked in epoll_wait
const HERD_EVENTFDS: usize = 8; // M: eventfds each of them watches
const PIPE_CHUNK: usize = 512;
const PIPE_CHUNKS: usize = 128; // 128 * 512 = 64 KiB, 4x a 16 KiB ring
const PIPE_TOTAL: usize = PIPE_CHUNK * PIPE_CHUNKS;
const PIPE_PASS_TOTAL_MS: i64 = 2000; // PASS iff the whole transfer fits in this
const PIPE_PASS_GAP_US: i64 = 100_000; // ...and no single wake took longer
const PIPE_CAP_MS: i64 = 15_000; // hard bound: a herding kernel must still exit
const EPOLLET: c_uint = 0x8000_0000;
const O_NONBLOCK_FL: c_int = 0o4000;
const F_SETFL: c_int = 4;

static HERD_STOP: AtomicBool = AtomicBool::new(false);
static HERD_READY: AtomicU32 = AtomicU32::new(0);

extern "C" fn herd_poller(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let ep = epoll_create1(0);
        if ep < 0 {
            HERD_READY.fetch_add(1, Ordering::Release);
            return core::ptr::null_mut();
        }
        let mut efds = [-1 as c_int; HERD_EVENTFDS];
        for i in 0..HERD_EVENTFDS {
            let fd = syscall(nr::EVENTFD2, 0i64, 0i64) as c_int;
            if fd < 0 { break; }
            efds[i] = fd;
            let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { fd } };
            epoll_ctl(ep, EPOLL_CTL_ADD, fd, &mut ev);
        }
        HERD_READY.fetch_add(1, Ordering::Release);
        let mut out: [epoll_event; 4] = core::mem::zeroed();
        // A finite timeout, not -1, so these threads stay joinable. 200 ms is
        // long enough that they are parked (and therefore in the herd) for
        // essentially the whole subtest, short enough to bound teardown.
        while !HERD_STOP.load(Ordering::Relaxed) {
            epoll_wait(ep, out.as_mut_ptr(), 4, 200);
        }
        for &fd in efds.iter() { if fd >= 0 { close(fd); } }
        close(ep);
        core::ptr::null_mut()
    }
}

unsafe fn herd_start(threads: &mut [pthread_t; MAX_LOAD_THREADS]) -> usize {
    HERD_STOP.store(false, Ordering::Relaxed);
    HERD_READY.store(0, Ordering::Relaxed);
    let created = spawn_load(HERD_POLLERS, herd_poller, threads);
    let start = now_ms();
    while (HERD_READY.load(Ordering::Acquire) as usize) < created
        && now_ms() - start < STARTUP_WAIT_MS * 5
    {
        usleep(500);
    }
    usleep(LOAD_SETTLE_US); // let every one of them actually reach epoll_wait
    created
}

unsafe fn herd_stop(threads: &[pthread_t; MAX_LOAD_THREADS], created: usize) {
    HERD_STOP.store(true, Ordering::Relaxed);
    join_load(threads, created);
}

static mut PIPE_WFD: c_int = -1;
static PIPE_WRITTEN: AtomicU32 = AtomicU32::new(0); // chunks whose write(2) returned
static mut PIPE_WROTE_US: [i64; PIPE_CHUNKS] = [0; PIPE_CHUNKS];

extern "C" fn pipe_writer(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let buf = [0x61u8; PIPE_CHUNK];
        'chunks: for i in 0..PIPE_CHUNKS {
            let mut off = 0usize;
            // A pipe write is short whenever the ring fills, so this is a
            // write_all, not a write. n <= 0 means the reader is gone (EPIPE) —
            // give up rather than spin through the remaining chunks.
            while off < PIPE_CHUNK {
                let n = write(PIPE_WFD, buf.as_ptr().add(off), PIPE_CHUNK - off);
                if n <= 0 { break 'chunks; }
                off += n as usize;
            }
            PIPE_WROTE_US[i] = now_us();
            PIPE_WRITTEN.store((i + 1) as u32, Ordering::Release);
        }
        core::ptr::null_mut()
    }
}

/// Drain loop shared by subtests 8 and 9. Returns (total_ms, max_gap_us, bytes).
///
/// `stamped` picks the gap definition. With an in-process writer we know when
/// each chunk's `write(2)` returned, so the gap is how long the oldest
/// not-yet-consumed chunk waited for its epoll_wait to return — the number the
/// bug report is about. With a forked writer we cannot see its clock, so the
/// gap is the longest interval between consecutive reader wakes.
unsafe fn pipe_drain(rfd: c_int, ep: c_int, stamped: bool) -> (i64, i64, usize) {
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    let mut buf = [0u8; 4096];
    let mut consumed = 0usize;
    let mut max_gap_us: i64 = 0;
    let start_ms = now_ms();
    let mut last_wake_us = now_us();
    while consumed < PIPE_TOTAL && now_ms() - start_ms < PIPE_CAP_MS {
        let n = epoll_wait(ep, out.as_mut_ptr(), 4, WAIT_TIMEOUT_MS);
        let ret_us = now_us();
        if stamped {
            let idx = consumed / PIPE_CHUNK;
            if idx < PIPE_CHUNKS && (PIPE_WRITTEN.load(Ordering::Acquire) as usize) > idx {
                let gap = ret_us - PIPE_WROTE_US[idx];
                if gap > max_gap_us { max_gap_us = gap; }
            }
        } else {
            let gap = ret_us - last_wake_us;
            if gap > max_gap_us { max_gap_us = gap; }
        }
        last_wake_us = ret_us;
        if n <= 0 { continue; } // timeout — keep going until the cap
        // The EPOLLET contract: drain until EAGAIN. A short read is not a
        // reason to stop (the kernel caps one pipe read at 4096 bytes); only
        // -1/EAGAIN or 0/EOF is.
        loop {
            let r = read(rfd, buf.as_mut_ptr(), buf.len());
            if r <= 0 { break; }
            consumed += r as usize;
        }
    }
    (now_ms() - start_ms, max_gap_us, consumed)
}

// "<name>: PASS/FAIL total_ms=<n> max_gap_ms=<n> max_gap_us=<n> K=<n> M=<n> bytes=<n>\n"
fn report_pipe(name: &[u8], ok: bool, total_ms: i64, max_gap_us: i64, bytes: usize) -> bool {
    unsafe {
        let mut line = [0u8; 200];
        let mut p = 0usize;
        macro_rules! put { ($s:expr) => { for &b in $s { line[p] = b; p += 1; } } }
        macro_rules! num { ($v:expr) => {{
            let mut e: i64 = $v;
            let neg = e < 0;
            if neg { e = -e; }
            if neg { line[p] = b'-'; p += 1; }
            let mut d = [0u8; 20];
            let mut k = 0usize;
            if e == 0 { d[k] = b'0'; k += 1; }
            while e > 0 { d[k] = b'0' + (e % 10) as u8; e /= 10; k += 1; }
            while k > 0 { k -= 1; line[p] = d[k]; p += 1; }
        }} }
        put!(name);
        put!(if ok { b": PASS " } else { b": FAIL " });
        put!(b"total_ms="); num!(total_ms);
        put!(b" max_gap_ms="); num!(max_gap_us / 1000);
        put!(b" max_gap_us="); num!(max_gap_us);
        put!(b" K="); num!(HERD_POLLERS as i64);
        put!(b" M="); num!(HERD_EVENTFDS as i64);
        put!(b" bytes="); num!(bytes as i64);
        put!(b"\n\0");
        puts(line.as_ptr());
    }
    ok
}

/// Create the measured pipe: blocking write end, O_NONBLOCK read end
/// registered EPOLLIN | EPOLLET. Returns (rfd, wfd, epfd) or all -1.
unsafe fn pipe_epollet_setup() -> (c_int, c_int, c_int) {
    let mut fds: [c_int; 2] = [-1, -1];
    if pipe2(fds.as_mut_ptr(), 0) != 0 { return (-1, -1, -1); }
    let (rfd, wfd) = (fds[0], fds[1]);
    fcntl(rfd, F_SETFL, O_NONBLOCK_FL);
    let ep = epoll_create1(0);
    if ep < 0 {
        close(rfd);
        close(wfd);
        return (-1, -1, -1);
    }
    let mut ev = epoll_event { events: EPOLLIN | EPOLLET, data: epoll_data { fd: rfd } };
    if epoll_ctl(ep, EPOLL_CTL_ADD, rfd, &mut ev) != 0 {
        close(ep);
        close(rfd);
        close(wfd);
        return (-1, -1, -1);
    }
    (rfd, wfd, ep)
}

unsafe fn test_pipe_epollet_under_poller_herd() -> bool {
    let name = b"pipe_epollet_under_poller_herd";
    let (rfd, wfd, ep) = pipe_epollet_setup();
    if ep < 0 { return report_pipe(name, false, 0, 0, 0); }

    let mut herd: [pthread_t; MAX_LOAD_THREADS] = [core::ptr::null_mut(); MAX_LOAD_THREADS];
    let created = herd_start(&mut herd);

    PIPE_WFD = wfd;
    PIPE_WRITTEN.store(0, Ordering::Relaxed);
    let mut writer: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut writer, core::ptr::null(), pipe_writer, core::ptr::null_mut()) != 0 {
        herd_stop(&herd, created);
        close(ep);
        close(rfd);
        close(wfd);
        return report_pipe(name, false, 0, 0, 0);
    }

    let (total_ms, max_gap_us, consumed) = pipe_drain(rfd, ep, true);

    // Close the read end BEFORE joining. If the drain gave up at PIPE_CAP_MS
    // the writer is still parked on a full ring; dropping the last reader makes
    // its next write fail with EPIPE so it can exit, which is what keeps this
    // subtest from turning a slow kernel into a hung test binary.
    close(ep);
    close(rfd);
    pthread_join(writer, core::ptr::null_mut());
    close(wfd);
    PIPE_WFD = -1;
    herd_stop(&herd, created);

    let ok = consumed == PIPE_TOTAL
        && total_ms < PIPE_PASS_TOTAL_MS
        && max_gap_us < PIPE_PASS_GAP_US;
    report_pipe(name, ok, total_ms, max_gap_us, consumed)
}

unsafe fn test_pipe_epollet_child_stdout_herd() -> bool {
    let name = b"pipe_epollet_child_stdout_herd";
    let (rfd, wfd, ep) = pipe_epollet_setup();
    if ep < 0 { return report_pipe(name, false, 0, 0, 0); }

    let mut herd: [pthread_t; MAX_LOAD_THREADS] = [core::ptr::null_mut(); MAX_LOAD_THREADS];
    let created = herd_start(&mut herd);

    let pid = fork();
    if pid == 0 {
        // Child: stdout IS the pipe, exactly as launch-pad leaves a component.
        // These writes take sys_write's fd-1 path, which routes to the VFS
        // rather than the console because fd 1 is now redirected.
        close(rfd);
        dup2(wfd, 1);
        close(wfd);
        let buf = [0x61u8; PIPE_CHUNK];
        'chunks: for _ in 0..PIPE_CHUNKS {
            let mut off = 0usize;
            while off < PIPE_CHUNK {
                let n = write(1, buf.as_ptr().add(off), PIPE_CHUNK - off);
                if n <= 0 { break 'chunks; }
                off += n as usize;
            }
        }
        _exit(0);
    }
    // The parent's own write end must go, or the reader never sees EOF.
    close(wfd);
    if pid < 0 {
        herd_stop(&herd, created);
        close(ep);
        close(rfd);
        return report_pipe(name, false, 0, 0, 0);
    }

    let (total_ms, max_gap_us, consumed) = pipe_drain(rfd, ep, false);

    // Same ordering rule as subtest 8: drop the read end first so a child still
    // blocked on a full ring is released by EPIPE before we wait for it.
    close(ep);
    close(rfd);
    let mut status: c_int = 0;
    waitpid(pid, &mut status, 0);
    herd_stop(&herd, created);

    let ok = consumed == PIPE_TOTAL
        && total_ms < PIPE_PASS_TOTAL_MS
        && max_gap_us < PIPE_PASS_GAP_US;
    report_pipe(name, ok, total_ms, max_gap_us, consumed)
}

// ── entry ────────────────────────────────────────────────────────────────

#[no_mangle]
pub unsafe extern "C" fn smp_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    puts(b"--- smpwaketest start ---\n\0".as_ptr());

    let mut failures = 0;
    if !test_futex_pingpong_2threads() { failures += 1; }
    if !test_futex_pingpong_4threads() { failures += 1; }
    if !test_eventfd_pingpong_2threads() { failures += 1; }
    if !test_wake_from_wfi_idle() { failures += 1; }
    if !test_futex_pingpong_under_spin_load() { failures += 1; }
    if !test_futex_pingpong_under_yield_load() { failures += 1; }
    if !test_eventfd_pingpong_under_spin_load() { failures += 1; }
    if !test_pipe_epollet_under_poller_herd() { failures += 1; }
    if !test_pipe_epollet_child_stdout_herd() { failures += 1; }

    puts(b"--- smpwaketest done ---\n\0".as_ptr());
    failures
}
