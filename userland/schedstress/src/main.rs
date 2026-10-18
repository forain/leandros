//! schedstress — scheduler stress test (lane percpu, 2026-10-18).
//!
//! Every case targets a way a per-CPU run queue can lose or double-run a task:
//!
//!   pingpong  futex / pipe / eventfd ping-pong between thread pairs (1, 2, 4
//!             pairs), latency histogram per round trip; a wait that times out
//!             (2 s) is a LOST WAKE. Prints ns per round trip per pair count.
//!   signals   sigsuspend/kill ping-pong between two processes, and SIGUSR2 at
//!             threads parked in futex wait, pipe read, nanosleep and poll:
//!             each must return EINTR (lost = parked through a signal).
//!   stopcont  SIGSTOP/SIGCONT storm on a child with spinning, blocked and
//!             file-I/O threads: every stop and continue is reported by
//!             waitpid, and every thread progresses after the last SIGCONT.
//!   exitgroup exit_group from a worker / SIGKILL / SIGTERM while siblings
//!             spin, block in a futex, poll and sleep: reaped within 5 s.
//!   forkmt    fork from a process whose siblings write memory: no sibling
//!             write is lost, and the child's copy never changes after fork.
//!   execmt    a non-leader thread execve()s while siblings spin.
//!   fair      2×ncpu spinners + yielders: every spinner progresses in every
//!             250 ms window; spinners' CPU time within ±10 % of the mean.
//!   reap      a storm of short children reaped by waitpid and by pidfd+poll.
//!
//! usage: schedstress [short|full] [case]      (default full, all cases)
//!        schedstress --exec-child             (internal: execmt's image)
//! Prints `<case>: PASS (...)` or `<case>: FAIL ...`, ends with
//! `--- schedstress done --- failures=N`; exit status is N.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

#[repr(C)]
#[derive(Clone, Copy)]
struct Timespec { tv_sec: i64, tv_nsec: i64 }

#[repr(C)]
struct PollFd { fd: i32, events: i16, revents: i16 }

/// musl `sigset_t` (128 bytes on every arch).
#[repr(C)]
#[derive(Clone, Copy)]
struct SigSet([u64; 16]);

/// musl `struct sigaction` (handler, mask, flags, restorer).
#[repr(C)]
struct SigAction { handler: usize, mask: SigSet, flags: i32, restorer: usize }

extern "C" {
    fn fork() -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn read(fd: i32, buf: *mut u8, n: usize) -> isize;
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn getpid() -> i32;
    fn _exit(code: i32) -> !;
    fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> i32;
    fn nanosleep(req: *const Timespec, rem: *mut Timespec) -> i32;
    fn syscall(nr: i64, ...) -> i64;
    fn sigaction(sig: i32, act: *const SigAction, old: *mut SigAction) -> i32;
    fn sigemptyset(set: *mut SigSet) -> i32;
    fn sigaddset(set: *mut SigSet, sig: i32) -> i32;
    fn sigprocmask(how: i32, set: *const SigSet, old: *mut SigSet) -> i32;
    fn sigsuspend(mask: *const SigSet) -> i32;
    fn sigtimedwait(set: *const SigSet, info: *mut u8, ts: *const Timespec) -> i32;
    fn pthread_kill(t: usize, sig: i32) -> i32;
    fn poll(fds: *mut PollFd, n: u64, timeout: i32) -> i32;
    fn eventfd(init: u32, flags: i32) -> i32;
    fn clock_gettime(clk: i32, ts: *mut Timespec) -> i32;
    fn sched_yield() -> i32;
    fn alarm(s: u32) -> u32;
    fn mmap(a: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn open(path: *const u8, flags: i32, ...) -> i32;
    fn fsync(fd: i32) -> i32;
    fn unlink(path: *const u8) -> i32;
    fn __errno_location() -> *mut i32;
}

#[cfg(target_arch = "x86_64")]
mod nr { pub const FUTEX: i64 = 202; pub const EXIT_GROUP: i64 = 231; pub const PIDFD_OPEN: i64 = 434; }
#[cfg(target_arch = "aarch64")]
mod nr { pub const FUTEX: i64 = 98; pub const EXIT_GROUP: i64 = 94; pub const PIDFD_OPEN: i64 = 434; }

const SIGKILL: i32 = 9;
const SIGUSR1: i32 = 10;
const SIGUSR2: i32 = 12;
const SIGTERM: i32 = 15;
const SIGCONT: i32 = 18;
const SIGSTOP: i32 = 19;
const EINTR: i32 = 4;
const WNOHANG: i32 = 1;
const WUNTRACED: i32 = 2;
const WCONTINUED: i32 = 8;
const POLLIN: i16 = 1;
const CLOCK_THREAD_CPUTIME_ID: i32 = 3;

fn errno() -> i32 { unsafe { *__errno_location() } }

fn futex_wait(w: &AtomicU32, val: u32, ms: u64) -> i64 {
    let ts = Timespec { tv_sec: (ms / 1000) as i64, tv_nsec: ((ms % 1000) * 1_000_000) as i64 };
    unsafe { syscall(nr::FUTEX, w as *const AtomicU32, 128 /* WAIT|PRIVATE */, val, &ts as *const Timespec, 0usize, 0u32) }
}
fn futex_wake(w: &AtomicU32, n: u32) {
    unsafe { syscall(nr::FUTEX, w as *const AtomicU32, 129 /* WAKE|PRIVATE */, n, 0usize, 0usize, 0u32); }
}

fn sleep_ms(ms: u64) { thread::sleep(Duration::from_millis(ms)); }

fn thread_cpu_ns() -> u64 {
    let mut ts = Timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// poll one fd for POLLIN; false = timed out.
fn wait_readable(fd: i32, ms: i32) -> bool {
    let mut p = PollFd { fd, events: POLLIN, revents: 0 };
    loop {
        let r = unsafe { poll(&mut p, 1, ms) };
        if r > 0 { return true; }
        if r == 0 { return false; }
        if errno() != EINTR { return false; }
    }
}

/// waitpid with a deadline (WNOHANG polling). Some(status) or None on timeout.
fn waitpid_bound(pid: i32, opts: i32, ms: u64) -> Option<i32> {
    let t0 = Instant::now();
    loop {
        let mut st = 0i32;
        let r = unsafe { waitpid(pid, &mut st, opts | WNOHANG) };
        if r == pid { return Some(st); }
        if r < 0 && errno() != EINTR { return None; }
        if t0.elapsed() > Duration::from_millis(ms) { return None; }
        sleep_ms(1);
    }
}

fn pipe2() -> (i32, i32) {
    let mut f = [0i32; 2];
    unsafe { pipe(f.as_mut_ptr()) };
    (f[0], f[1])
}

#[allow(dead_code)]
fn shared_u64s(n: usize) -> &'static [AtomicU64] {
    // PROT_READ|WRITE, MAP_SHARED|MAP_ANONYMOUS
    let p = unsafe { mmap(core::ptr::null_mut(), n * 8, 3, 0x01 | 0x20, -1, 0) };
    assert!(!p.is_null() && p as isize != -1, "mmap");
    unsafe { core::slice::from_raw_parts(p as *const AtomicU64, n) }
}

extern "C" fn noop_handler(_s: i32) {}

/// Install `noop_handler` for `sig` WITHOUT SA_RESTART (parked syscalls
/// return EINTR).
fn install_eintr_handler(sig: i32) {
    let mut a = SigAction { handler: noop_handler as usize, mask: SigSet([0; 16]), flags: 0, restorer: 0 };
    unsafe { sigemptyset(&mut a.mask); sigaction(sig, &a, core::ptr::null_mut()); }
}

struct Ctx { short: bool, ncpu: usize }

// ── 1. pingpong ─────────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq)]
enum Pp { Futex, Pipe, Eventfd }

const HB: [u64; 5] = [10_000, 100_000, 1_000_000, 10_000_000, 100_000_000];

/// One pair; returns (round trips, total ns, histogram, max ns, lost wakes).
fn pp_pair(kind: Pp, rounds: usize) -> (usize, u64, [u64; 6], u64, u32) {
    let lost = Arc::new(AtomicU32::new(0));
    match kind {
        Pp::Futex => {
            let w = Arc::new(AtomicU32::new(0));
            let (w2, lost2) = (w.clone(), lost.clone());
            let b = thread::spawn(move || {
                for i in 0..rounds as u32 {
                    let want = 2 * i + 1;
                    let mut spins = 0;
                    while w2.load(Ordering::Acquire) != want {
                        if futex_wait(&w2, 2 * i, 2000) < 0 && errno() == 110 && w2.load(Ordering::Acquire) != want {
                            spins += 1;
                            if spins > 1 { lost2.fetch_add(1, Ordering::Relaxed); return; }
                        }
                    }
                    w2.store(want + 1, Ordering::Release);
                    futex_wake(&w2, 1);
                }
            });
            let mut h = [0u64; 6];
            let (mut tot, mut mx) = (0u64, 0u64);
            for i in 0..rounds as u32 {
                let t0 = Instant::now();
                w.store(2 * i + 1, Ordering::Release);
                futex_wake(&w, 1);
                let want = 2 * i + 2;
                let mut to = 0;
                while w.load(Ordering::Acquire) != want {
                    if futex_wait(&w, 2 * i + 1, 2000) < 0 && w.load(Ordering::Acquire) != want
                        && errno() == 110 { to += 1; if to > 1 { lost.fetch_add(1, Ordering::Relaxed); break; } }
                }
                if lost.load(Ordering::Relaxed) != 0 { break; }
                let ns = t0.elapsed().as_nanos() as u64;
                tot += ns; mx = mx.max(ns);
                h[HB.iter().position(|&b| ns < b).unwrap_or(5)] += 1;
            }
            let _ = b.join();
            (rounds, tot, h, mx, lost.load(Ordering::Relaxed))
        }
        Pp::Pipe | Pp::Eventfd => {
            let (ar, aw, br, bw) = if kind == Pp::Pipe {
                let (a0, a1) = pipe2(); let (b0, b1) = pipe2(); (a0, a1, b0, b1)
            } else {
                let a = unsafe { eventfd(0, 0) }; let b = unsafe { eventfd(0, 0) }; (a, a, b, b)
            };
            let n = if kind == Pp::Pipe { 1 } else { 8 };
            let lost2 = lost.clone();
            let b = thread::spawn(move || {
                let mut buf = [0u8; 8];
                let one = 1u64.to_ne_bytes();
                for _ in 0..rounds {
                    if !wait_readable(ar, 2000) { lost2.fetch_add(1, Ordering::Relaxed); return; }
                    unsafe { read(ar, buf.as_mut_ptr(), n); write(bw, one.as_ptr(), n); }
                }
            });
            let mut h = [0u64; 6];
            let (mut tot, mut mx) = (0u64, 0u64);
            let mut buf = [0u8; 8];
            let one = 1u64.to_ne_bytes();
            for _ in 0..rounds {
                let t0 = Instant::now();
                unsafe { write(aw, one.as_ptr(), n); }
                if !wait_readable(br, 2000) { lost.fetch_add(1, Ordering::Relaxed); break; }
                unsafe { read(br, buf.as_mut_ptr(), n); }
                let ns = t0.elapsed().as_nanos() as u64;
                tot += ns; mx = mx.max(ns);
                h[HB.iter().position(|&b| ns < b).unwrap_or(5)] += 1;
            }
            let _ = b.join();
            unsafe { close(ar); close(br); if kind == Pp::Pipe { close(aw); close(bw); } }
            (rounds, tot, h, mx, lost.load(Ordering::Relaxed))
        }
    }
}

fn case_pingpong(c: &Ctx) -> bool {
    let rounds = if c.short { 1000 } else { 10000 };
    let mut ok = true;
    for kind in [Pp::Futex, Pp::Pipe, Pp::Eventfd] {
        let kname = match kind { Pp::Futex => "futex", Pp::Pipe => "pipe", Pp::Eventfd => "eventfd" };
        for pairs in [1usize, 2, 4] {
            let t0 = Instant::now();
            let hs: Vec<_> = (0..pairs).map(|_| thread::spawn(move || pp_pair(kind, rounds))).collect();
            let res: Vec<_> = hs.into_iter().map(|h| h.join().unwrap()).collect();
            let wall = t0.elapsed().as_nanos() as u64;
            let lost: u32 = res.iter().map(|r| r.4).sum();
            let mx = res.iter().map(|r| r.3).max().unwrap_or(0);
            let tot: u64 = res.iter().map(|r| r.1).sum();
            let mut h = [0u64; 6];
            for r in &res { for i in 0..6 { h[i] += r.2[i]; } }
            println!("  pingpong {} pairs={} ns/rt={} wall_ns/rt={} max_us={} hist(<10us,<100us,<1ms,<10ms,<100ms,>=)={:?} lost={}",
                kname, pairs, tot / (rounds * pairs) as u64, wall / rounds as u64, mx / 1000, h, lost);
            if lost != 0 { ok = false; println!("pingpong_{}: FAIL lost wake ({} pairs)", kname, pairs); }
        }
    }
    ok
}

// ── 2. signals ──────────────────────────────────────────────────────────────

static GOT: AtomicU32 = AtomicU32::new(0);
extern "C" fn usr1_handler(_s: i32) { GOT.fetch_add(1, Ordering::Relaxed); }

fn case_signals(c: &Ctx) -> bool {
    let mut ok = true;
    // (a) sigsuspend / kill ping-pong between two processes.
    let rounds = if c.short { 300 } else { 3000 };
    let mut set = SigSet([0; 16]);
    unsafe { sigemptyset(&mut set); sigaddset(&mut set, SIGUSR1); sigprocmask(0 /* BLOCK */, &set, core::ptr::null_mut()); }
    let a = SigAction { handler: usr1_handler as usize, mask: SigSet([0; 16]), flags: 0, restorer: 0 };
    unsafe { sigaction(SIGUSR1, &a, core::ptr::null_mut()); }
    let parent = unsafe { getpid() };
    let t0 = Instant::now();
    let child = unsafe { fork() };
    if child == 0 {
        unsafe { alarm(60); }
        let mut empty = SigSet([0; 16]);
        unsafe { sigemptyset(&mut empty); }
        for _ in 0..rounds {
            let before = GOT.load(Ordering::Relaxed);
            while GOT.load(Ordering::Relaxed) == before { unsafe { sigsuspend(&empty); } }
            unsafe { kill(parent, SIGUSR1); }
        }
        unsafe { _exit(0); }
    }
    let ts = Timespec { tv_sec: 2, tv_nsec: 0 };
    let mut lost = 0;
    for _ in 0..rounds {
        unsafe { kill(child, SIGUSR1); }
        let r = unsafe { sigtimedwait(&set, core::ptr::null_mut(), &ts) };
        if r != SIGUSR1 { lost += 1; break; }
    }
    let st = waitpid_bound(child, 0, 10_000);
    let ns = t0.elapsed().as_nanos() as u64 / rounds as u64;
    unsafe { sigprocmask(1 /* UNBLOCK */, &set, core::ptr::null_mut()); }
    if lost != 0 || st != Some(0) {
        ok = false;
        println!("signals_pingpong: FAIL lost={} child status={:?}", lost, st);
    } else {
        println!("  signals pingpong {} rounds, {} ns/round trip", rounds, ns);
    }

    // (b) SIGUSR2 at threads parked in futex, pipe read, nanosleep, poll.
    install_eintr_handler(SIGUSR2);
    let stop = Arc::new(AtomicBool::new(false));
    let names = ["futex", "read", "nanosleep", "poll"];
    let counts: Arc<Vec<AtomicU64>> = Arc::new((0..4).map(|_| AtomicU64::new(0)).collect());
    let (pr, pw) = pipe2();
    let mut hs = Vec::new();
    for k in 0..4 {
        let (stop, counts) = (stop.clone(), counts.clone());
        hs.push(thread::spawn(move || {
            let w = AtomicU32::new(0);
            let mut b = [0u8; 1];
            while !stop.load(Ordering::Acquire) {
                let intr = match k {
                    0 => futex_wait(&w, 0, 10_000) < 0 && errno() == EINTR,
                    1 => (unsafe { read(pr, b.as_mut_ptr(), 1) }) < 0 && errno() == EINTR,
                    2 => { let t = Timespec { tv_sec: 10, tv_nsec: 0 };
                           (unsafe { nanosleep(&t, core::ptr::null_mut()) }) < 0 && errno() == EINTR }
                    _ => { let mut p = PollFd { fd: pr, events: POLLIN, revents: 0 };
                           (unsafe { poll(&mut p, 1, 10_000) }) < 0 && errno() == EINTR }
                };
                if intr { counts[k].fetch_add(1, Ordering::Release); }
            }
        }));
    }
    sleep_ms(50);
    let n = if c.short { 50 } else { 300 };
    let mut fail = None;
    'outer: for _ in 0..n {
        for k in 0..4 {
            let before = counts[k].load(Ordering::Acquire);
            let t0 = Instant::now();
            // Re-send while not acknowledged: the signal may land just before
            // the thread parks again (then it is consumed in user space).
            while counts[k].load(Ordering::Acquire) == before {
                unsafe { pthread_kill(std::os::unix::thread::JoinHandleExt::as_pthread_t(&hs[k]) as usize, SIGUSR2); }
                let t1 = Instant::now();
                while counts[k].load(Ordering::Acquire) == before && t1.elapsed() < Duration::from_millis(200) {
                    thread::yield_now();
                }
                if t0.elapsed() > Duration::from_secs(3) { fail = Some(k); break 'outer; }
            }
        }
    }
    stop.store(true, Ordering::Release);
    for k in 0..4 {
        for _ in 0..50 {
            if hs[k].is_finished() { break; }
            unsafe { pthread_kill(std::os::unix::thread::JoinHandleExt::as_pthread_t(&hs[k]) as usize, SIGUSR2); }
            sleep_ms(5);
        }
    }
    unsafe { write(pw, b"x".as_ptr(), 1); }
    for h in hs { let _ = h.join(); }
    unsafe { close(pr); close(pw); }
    match fail {
        Some(k) => { ok = false; println!("signals_parked: FAIL thread parked in {} slept through SIGUSR2", names[k]); }
        None => println!("  signals parked: {} x 4 EINTRs ({:?})", n,
                         (0..4).map(|k| counts[k].load(Ordering::Relaxed)).collect::<Vec<_>>()),
    }
    ok
}

// ── 3. stopcont ─────────────────────────────────────────────────────────────

/// Latest progress record `[spin0, spin1, fs]` the child wrote to `fd`
/// (24-byte atomic pipe writes), waiting up to `ms` for at least one.
fn latest_progress(fd: i32, ms: i32) -> Option<[u64; 3]> {
    let mut last = None;
    let mut wait = ms;
    let mut buf = [0u8; 24 * 64];
    while wait_readable(fd, wait) {
        let n = unsafe { read(fd, buf.as_mut_ptr(), buf.len()) };
        if n <= 0 { break; }
        let n = n as usize / 24 * 24;
        if n >= 24 {
            let r = &buf[n - 24..n];
            let g = |k: usize| u64::from_ne_bytes(r[k * 8..k * 8 + 8].try_into().unwrap());
            last = Some([g(0), g(1), g(2)]);
        }
        wait = 0;
    }
    last
}

fn case_stopcont(c: &Ctx) -> bool {
    let rounds = if c.short { 30 } else { 200 };
    // Progress goes through a pipe: MAP_SHARED|MAP_ANONYMOUS is not shared
    // across fork in this kernel (treated as private, syscall.rs mmap).
    let (sr, sw) = pipe2();
    let (pr, pw) = pipe2();
    let child = unsafe { fork() };
    if child == 0 {
        unsafe { close(pw); close(sr); }
        let ctr: &'static [AtomicU64; 3] = Box::leak(Box::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(0)]));
        for k in 0..2 {
            thread::spawn(move || loop { for _ in 0..1000 { core::hint::spin_loop(); } ctr[k].fetch_add(1, Ordering::Relaxed); });
        }
        thread::spawn(move || {
            let path = format!("/tmp/schedstress-{}\0", unsafe { getpid() });
            let buf = [7u8; 4096];
            loop {
                let fd = unsafe { open(path.as_ptr(), 0o1 | 0o100 | 0o1000, 0o644) };
                if fd >= 0 { unsafe { write(fd, buf.as_ptr(), buf.len()); fsync(fd); close(fd); } }
                ctr[2].fetch_add(1, Ordering::Relaxed);
            }
        });
        let mut b = [0u8; 1];
        thread::spawn(move || loop { unsafe { read(pr, b.as_mut_ptr(), 1); } });
        loop {
            let mut rec = [0u8; 24];
            for k in 0..3 { rec[k * 8..k * 8 + 8].copy_from_slice(&ctr[k].load(Ordering::Relaxed).to_ne_bytes()); }
            unsafe { write(sw, rec.as_ptr(), 24); }
            sleep_ms(20);
        }
    }
    unsafe { close(pr); close(sw); }
    let mut fail = None;
    if latest_progress(sr, 10_000).is_none() { fail = Some("child never reported progress".to_string()); }
    for i in 0..rounds {
        if fail.is_some() { break; }
        unsafe { kill(child, SIGSTOP); }
        match waitpid_bound(child, WUNTRACED, 5000) {
            Some(st) if st & 0xff == 0x7f => {}
            other => { fail = Some(format!("round {}: no stop report ({:?})", i, other)); break; }
        }
        if i % 3 == 0 { sleep_ms(2); }
        unsafe { kill(child, SIGCONT); }
        match waitpid_bound(child, WCONTINUED, 5000) {
            Some(0xffff) => {}
            other => { fail = Some(format!("round {}: no continue report ({:?})", i, other)); break; }
        }
        if i % 2 == 0 { unsafe { write(pw, b"x".as_ptr(), 1); } }
        let _ = latest_progress(sr, 0); // keep the pipe drained
    }
    if fail.is_none() {
        let before = latest_progress(sr, 5000).unwrap_or([0; 3]);
        let t0 = Instant::now();
        loop {
            sleep_ms(100);
            let now = latest_progress(sr, 5000).unwrap_or(before);
            let stuck: Vec<usize> = (0..3).filter(|&k| now[k] == before[k]).collect();
            if stuck.is_empty() { break; }
            if t0.elapsed() > Duration::from_secs(5) {
                fail = Some(format!("threads {:?} (0,1 spin, 2 fs) made no progress after SIGCONT", stuck)); break;
            }
        }
    }
    unsafe { kill(child, SIGKILL); }
    let st = waitpid_bound(child, 0, 5000);
    unsafe { close(pw); close(sr); }
    let path = format!("/tmp/schedstress-{}\0", child);
    unsafe { unlink(path.as_ptr()); }
    if st.is_none() && fail.is_none() { fail = Some("SIGKILL not reaped within 5 s".into()); }
    match fail {
        Some(f) => { println!("stopcont: FAIL {}", f); false }
        None => { println!("  stopcont: {} stop/cont rounds", rounds); true }
    }
}

// ── 4. exitgroup ────────────────────────────────────────────────────────────

fn case_exitgroup(c: &Ctx) -> bool {
    let rounds = if c.short { 12 } else { 60 };
    let mut worst = 0u128;
    for i in 0..rounds {
        let how = i % 3; // 0 exit_group from a worker, 1 SIGKILL, 2 SIGTERM
        let (rr, rw) = pipe2();
        let child = unsafe { fork() };
        if child == 0 {
            unsafe { close(rr); }
            let (pr, _pw) = pipe2();
            let started = Arc::new(AtomicU32::new(0));
            for k in 0..4 {
                let s = started.clone();
                thread::spawn(move || {
                    s.fetch_add(1, Ordering::Release);
                    match k {
                        0 => loop { core::hint::spin_loop(); },
                        1 => { let w = AtomicU32::new(0); loop { futex_wait(&w, 0, 100_000); } }
                        2 => loop { let mut p = PollFd { fd: pr, events: POLLIN, revents: 0 }; unsafe { poll(&mut p, 1, 100_000); } },
                        _ => loop { sleep_ms(100_000); },
                    }
                });
            }
            while started.load(Ordering::Acquire) < 4 { thread::yield_now(); }
            unsafe { write(rw, b"r".as_ptr(), 1); }
            if how == 0 {
                thread::spawn(|| { sleep_ms(5); unsafe { syscall(nr::EXIT_GROUP, 7i64); } });
            }
            loop { core::hint::spin_loop(); }
        }
        unsafe { close(rw); }
        if !wait_readable(rr, 10_000) {
            println!("exitgroup: FAIL round {}: child never reported ready", i);
            unsafe { kill(child, SIGKILL); waitpid_bound(child, 0, 5000); close(rr); }
            return false;
        }
        unsafe { close(rr); }
        let t0 = Instant::now();
        if how == 1 { unsafe { kill(child, SIGKILL); } } else if how == 2 { unsafe { kill(child, SIGTERM); } }
        let st = waitpid_bound(child, 0, 5000);
        worst = worst.max(t0.elapsed().as_millis());
        let good = match (how, st) {
            (0, Some(s)) => s & 0x7f == 0 && (s >> 8) & 0xff == 7,
            (1, Some(s)) => s & 0x7f == SIGKILL,
            (2, Some(s)) => s & 0x7f == SIGTERM,
            _ => false,
        };
        if !good {
            println!("exitgroup: FAIL round {} (mode {}): status {:?}", i, how, st);
            unsafe { kill(child, SIGKILL); waitpid_bound(child, 0, 5000); }
            return false;
        }
    }
    println!("  exitgroup: {} rounds, worst reap {} ms", rounds, worst);
    true
}

// ── 5. forkmt ───────────────────────────────────────────────────────────────

fn case_forkmt(c: &Ctx) -> bool {
    let rounds = if c.short { 10 } else { 50 };
    const N: usize = 16384;
    let bufs: Arc<Vec<Vec<AtomicU64>>> = Arc::new((0..3).map(|_| (0..N).map(|_| AtomicU64::new(0)).collect()).collect());
    let stop = Arc::new(AtomicBool::new(false));
    let lostw = Arc::new(AtomicU64::new(0));
    let mut hs = Vec::new();
    for k in 0..3 {
        let (b, s, l) = (bufs.clone(), stop.clone(), lostw.clone());
        hs.push(thread::spawn(move || {
            let mut seq = 1u64;
            while !s.load(Ordering::Relaxed) {
                for i in 0..N {
                    let v = (seq << 20) | i as u64;
                    b[k][i].store(v, Ordering::Relaxed);
                    if b[k][i].load(Ordering::Relaxed) != v { l.fetch_add(1, Ordering::Relaxed); }
                }
                seq += 1;
            }
        }));
    }
    sleep_ms(20);
    let mut fail = None;
    for r in 0..rounds {
        let child = unsafe { fork() };
        if child == 0 {
            let sum = |b: &Vec<Vec<AtomicU64>>| b.iter().flatten().fold(0u64, |a, x| a.wrapping_mul(31).wrapping_add(x.load(Ordering::Relaxed)));
            let a = sum(&bufs);
            sleep_ms(20);
            let b2 = sum(&bufs);
            unsafe { _exit(if a == b2 { 0 } else { 1 }); }
        }
        match waitpid_bound(child, 0, 10_000) {
            Some(0) => {}
            Some(s) if (s >> 8) & 0xff == 1 => { fail = Some(format!("round {}: child's copy changed after fork (sibling write leaked)", r)); break; }
            other => { fail = Some(format!("round {}: child status {:?}", r, other)); break; }
        }
    }
    stop.store(true, Ordering::Relaxed);
    for h in hs { let _ = h.join(); }
    let l = lostw.load(Ordering::Relaxed);
    if l != 0 && fail.is_none() { fail = Some(format!("{} sibling writes lost (read back differently)", l)); }
    match fail {
        Some(f) => { println!("forkmt: FAIL {}", f); false }
        None => { println!("  forkmt: {} forks under 3 writing siblings", rounds); true }
    }
}

// ── 6. execmt ───────────────────────────────────────────────────────────────

fn case_execmt(c: &Ctx) -> bool {
    let rounds = if c.short { 4 } else { 20 };
    for r in 0..rounds {
        let child = unsafe { fork() };
        if child == 0 {
            for _ in 0..2 { thread::spawn(|| loop { core::hint::spin_loop(); }); }
            let h = thread::spawn(|| {
                sleep_ms(5);
                let path = b"/bin/schedstress\0";
                let a0 = b"schedstress\0"; let a1 = b"--exec-child\0";
                let argv = [a0.as_ptr(), a1.as_ptr(), core::ptr::null()];
                let envp = [core::ptr::null()];
                unsafe { execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr()); _exit(99); }
            });
            let _ = h.join();
            loop { core::hint::spin_loop(); }
        }
        match waitpid_bound(child, 0, 10_000) {
            Some(s) if s & 0x7f == 0 && (s >> 8) & 0xff == 42 => {}
            other => {
                println!("execmt: FAIL round {}: status {:?}", r, other);
                unsafe { kill(child, SIGKILL); waitpid_bound(child, 0, 5000); }
                return false;
            }
        }
    }
    println!("  execmt: {} non-leader execs", rounds);
    true
}

// ── 7. fair ─────────────────────────────────────────────────────────────────

fn case_fair(c: &Ctx) -> bool {
    let ns = 2 * c.ncpu;
    // Short mode (regress) runs 3 s and allows ±15 %: over 2 s the spread
    // was ±11 % on an unchanged kernel. Full mode enforces the spec's ±10 %.
    let secs_ms = if c.short { 3000 } else { 5000 };
    let tol = if c.short { 15.0 } else { 10.0 };
    let stop = Arc::new(AtomicBool::new(false));
    let go = Arc::new(AtomicBool::new(false));
    let ctr: Arc<Vec<AtomicU64>> = Arc::new((0..ns).map(|_| AtomicU64::new(0)).collect());
    let cpu: Arc<Vec<AtomicU64>> = Arc::new((0..ns).map(|_| AtomicU64::new(0)).collect());
    let mut hs = Vec::new();
    for k in 0..ns {
        let (s, g, ct, cp) = (stop.clone(), go.clone(), ctr.clone(), cpu.clone());
        hs.push(thread::spawn(move || {
            while !g.load(Ordering::Acquire) { thread::yield_now(); }
            let c0 = thread_cpu_ns();
            while !s.load(Ordering::Relaxed) {
                for _ in 0..2000 { core::hint::spin_loop(); }
                ct[k].fetch_add(1, Ordering::Relaxed);
            }
            cp[k].store(thread_cpu_ns() - c0, Ordering::Relaxed);
        }));
    }
    for _ in 0..2 {
        let s = stop.clone();
        hs.push(thread::spawn(move || { while !s.load(Ordering::Relaxed) { unsafe { sched_yield(); } } }));
    }
    go.store(true, Ordering::Release);
    sleep_ms(100);
    let mut last: Vec<u64> = (0..ns).map(|k| ctr[k].load(Ordering::Relaxed)).collect();
    let mut starved = Vec::new();
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(secs_ms) {
        sleep_ms(250);
        for k in 0..ns {
            let v = ctr[k].load(Ordering::Relaxed);
            if v == last[k] && !starved.contains(&k) { starved.push(k); }
            last[k] = v;
        }
    }
    stop.store(true, Ordering::Release);
    for h in hs { let _ = h.join(); }
    let v: Vec<u64> = (0..ns).map(|k| cpu[k].load(Ordering::Relaxed)).collect();
    let mean = v.iter().sum::<u64>() as f64 / ns as f64;
    let (mn, mx) = (*v.iter().min().unwrap() as f64, *v.iter().max().unwrap() as f64);
    let spread = ((mx - mean).max(mean - mn)) / mean.max(1.0) * 100.0;
    println!("  fair: {} spinners on {} CPUs, cpu ms min {:.0} mean {:.0} max {:.0} (±{:.1}%), starved windows {:?}",
             ns, c.ncpu, mn / 1e6, mean / 1e6, mx / 1e6, spread, starved);
    let mut ok = true;
    if !starved.is_empty() { ok = false; println!("fair_progress: FAIL spinners {:?} made no progress in a 250 ms window", starved); }
    if spread > tol { ok = false; println!("fair_share: FAIL spinner CPU time spread ±{:.1}% > {}%", spread, tol); }
    ok
}

// ── 8. reap ─────────────────────────────────────────────────────────────────

fn case_reap(c: &Ctx) -> bool {
    let total = if c.short { 1000 } else { 10000 };
    let t0 = Instant::now();
    let mut done = 0usize;
    let mut pidfd_used = 0usize;
    let mut batch_no = 0;
    while done < total {
        let n = 32.min(total - done);
        let mut pids = Vec::with_capacity(n);
        for i in 0..n {
            let p = unsafe { fork() };
            if p == 0 { unsafe { _exit(((done + i) & 0x7f) as i32); } }
            if p < 0 { println!("reap: FAIL fork failed at {} (errno {})", done + i, errno()); return false; }
            pids.push((p, ((done + i) & 0x7f) as i32));
        }
        if batch_no % 2 == 0 {
            for _ in 0..n {
                let mut st = 0;
                let r = unsafe { waitpid(-1, &mut st, 0) };
                let Some(&(_, code)) = pids.iter().find(|x| x.0 == r) else {
                    println!("reap: FAIL waitpid(-1) returned {} errno {}", r, errno()); return false;
                };
                if st & 0x7f != 0 || (st >> 8) & 0xff != code {
                    println!("reap: FAIL pid {} status {:#x} want exit {}", r, st, code); return false;
                }
            }
        } else {
            for &(p, code) in &pids {
                let fd = unsafe { syscall(nr::PIDFD_OPEN, p, 0) } as i32;
                if fd >= 0 {
                    pidfd_used += 1;
                    if !wait_readable(fd, 5000) {
                        println!("reap: FAIL pidfd of {} never readable", p);
                        return false;
                    }
                    unsafe { close(fd); }
                }
                match waitpid_bound(p, 0, 5000) {
                    Some(st) if st & 0x7f == 0 && (st >> 8) & 0xff == code => {}
                    other => { println!("reap: FAIL pid {} status {:?} want exit {}", p, other, code); return false; }
                }
            }
        }
        done += n;
        batch_no += 1;
    }
    let mut st = 0;
    if unsafe { waitpid(-1, &mut st, WNOHANG) } > 0 {
        println!("reap: FAIL a child left over"); return false;
    }
    println!("  reap: {} children in {} ms ({} via pidfd)", total, t0.elapsed().as_millis(), pidfd_used);
    true
}

// ── main ────────────────────────────────────────────────────────────────────

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--exec-child") { unsafe { _exit(42); } }
    let short = args.get(1).map(|s| s.as_str()) == Some("short");
    let only = args.get(2).cloned();
    let ncpu = thread::available_parallelism().map(|n| n.get()).unwrap_or(1);
    let c = Ctx { short, ncpu };
    unsafe { alarm(if short { 600 } else { 3000 }); }
    println!("schedstress: {} mode, {} CPUs, pid {}", if short { "short" } else { "full" }, ncpu, unsafe { getpid() });
    let cases: [(&str, fn(&Ctx) -> bool); 8] = [
        ("pingpong", case_pingpong), ("signals", case_signals), ("stopcont", case_stopcont),
        ("exitgroup", case_exitgroup), ("forkmt", case_forkmt), ("execmt", case_execmt),
        ("fair", case_fair), ("reap", case_reap),
    ];
    let mut failures = 0;
    for (name, f) in cases {
        if let Some(o) = &only { if o != name { continue; } }
        let t0 = Instant::now();
        let ok = f(&c);
        if ok { println!("{}: PASS ({} ms)", name, t0.elapsed().as_millis()); } else { failures += 1; }
    }
    println!("--- schedstress done --- failures={}", failures);
    unsafe { _exit(failures); }
}
