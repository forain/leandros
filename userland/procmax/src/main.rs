//! procmax — the process and task limits, at the limit.
//!
//! A parallel in-guest `cargo build` needs more than the 512 processes / 1024
//! tasks the kernel used to allow, and it exits its children in bursts. This
//! test drives each limit to the wall and checks that the wall is a clean
//! EAGAIN, not a panic or a lost child:
//!
//!   limits — RLIMIT_NPROC reports a finite, real value (not RLIM_INFINITY)
//!            and /proc/sys/kernel/threads-max and pid_max are readable
//!   procs  — fork sleeping children (each blocks reading a pipe) until fork
//!            fails: at least `MIN_PROCS` must succeed and the failure must be
//!            EAGAIN; then close the pipe and reap every child
//!   threads — spawn parked threads until spawn fails: at least `MIN_THREADS`
//!            must start and the failure must be EAGAIN; then release and
//!            join all of them
//!   burst  — fork `BURST` short-lived children that each start and join two
//!            threads and exit with a distinct code, let ALL of them exit
//!            before any wait (well over the old 256-record exit ring, which
//!            thread exits shared), then waitpid each one by pid and check
//!            its code: an overwritten record shows up as ECHILD
//!   after  — a plain fork + wait still works once everything is reaped
//!
//! Prints `<case>: PASS ...` / `<case>: FAIL ...` and `PROCMAX: PASS` or
//! `PROCMAX: FAIL n`; the exit status is the failure count. Needs root (the
//! RLIMIT_NPROC check exempts root, as Linux does) and guest RAM for ~2000
//! processes and ~4000 kernel stacks (128 KiB each): run with 4 GiB.
//!
//! usage: procmax [case]    (default: all cases)

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

extern "C" {
    fn fork() -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn pipe(fds: *mut i32) -> i32;
    fn read(fd: i32, buf: *mut u8, n: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn _exit(code: i32) -> !;
    fn getrlimit(res: i32, rlim: *mut [u64; 2]) -> i32;
    fn __errno_location() -> *mut i32;
}

const EAGAIN: i32 = 11;
const ECHILD: i32 = 10;
const RLIMIT_NPROC: i32 = 6;
/// 2048 processes minus what a serial-console boot already runs.
const MIN_PROCS: usize = 1900;
/// 4096 tasks minus the boot's threads and this test's own processes.
const MIN_THREADS: usize = 3800;
/// Stop forking here even if the kernel would allow more (a regression to
/// "no limit" must not exhaust guest RAM before it is noticed).
const PROC_CAP: usize = 2600;
const THREAD_CAP: usize = 5000;
const BURST: usize = 400;

fn errno() -> i32 { unsafe { *__errno_location() } }

fn pass(case: &str, msg: String) -> u32 { println!("{case}: PASS {msg}"); 0 }
fn fail(case: &str, msg: String) -> u32 { println!("{case}: FAIL {msg}"); 1 }

fn read_file(p: &str) -> Option<String> {
    std::fs::read_to_string(p).ok().map(|s| s.trim().to_string())
}

fn case_limits() -> u32 {
    let mut r = [0u64; 2];
    if unsafe { getrlimit(RLIMIT_NPROC, &mut r) } != 0 {
        return fail("limits", format!("getrlimit errno {}", errno()));
    }
    if r[0] == u64::MAX || r[0] < 1024 {
        return fail("limits", format!("RLIMIT_NPROC cur={} max={}", r[0], r[1]));
    }
    let tm = read_file("/proc/sys/kernel/threads-max");
    let pm = read_file("/proc/sys/kernel/pid_max");
    match (tm.as_deref().and_then(|s| s.parse::<u64>().ok()), pm.as_deref().and_then(|s| s.parse::<u64>().ok())) {
        (Some(t), Some(p)) if t >= 4096 && p > 0 =>
            pass("limits", format!("RLIMIT_NPROC={} threads-max={} pid_max={}", r[0], t, p)),
        _ => fail("limits", format!("threads-max={:?} pid_max={:?}", tm, pm)),
    }
}

fn reap_all(pids: &[i32]) -> (usize, usize) {
    let (mut ok, mut bad) = (0, 0);
    for &p in pids {
        let mut st = 0;
        let r = unsafe { waitpid(p, &mut st, 0) };
        if r == p && (st & 0x7f) == 0 && ((st >> 8) & 0xff) == 0 { ok += 1 } else { bad += 1 }
    }
    (ok, bad)
}

fn case_procs() -> u32 {
    let mut fds = [0i32; 2];
    if unsafe { pipe(fds.as_mut_ptr()) } != 0 {
        return fail("procs", format!("pipe errno {}", errno()));
    }
    let t0 = Instant::now();
    let mut pids = Vec::with_capacity(PROC_CAP);
    let mut stop_errno = 0;
    while pids.len() < PROC_CAP {
        let p = unsafe { fork() };
        if p == 0 {
            unsafe {
                close(fds[1]);
                let mut b = 0u8;
                // Sleeps until the parent closes the write end (EOF).
                while read(fds[0], &mut b, 1) > 0 {}
                _exit(0);
            }
        }
        if p < 0 { stop_errno = errno(); break; }
        pids.push(p);
    }
    let forked = pids.len();
    let fork_s = t0.elapsed().as_secs_f32();
    // One more attempt right at the wall must fail the same way.
    let again = if stop_errno != 0 {
        let p = unsafe { fork() };
        if p == 0 { unsafe { _exit(0) } }
        if p > 0 { pids.push(p); 0 } else { errno() }
    } else { 0 };
    unsafe { close(fds[1]); close(fds[0]); }
    let t1 = Instant::now();
    let (ok, bad) = reap_all(&pids);
    let msg = format!("forked={} stop_errno={} again_errno={} reaped={} bad={} fork={:.1}s reap={:.1}s",
                      forked, stop_errno, again, ok, bad, fork_s, t1.elapsed().as_secs_f32());
    if forked >= MIN_PROCS && forked < PROC_CAP && stop_errno == EAGAIN && again == EAGAIN
        && ok == pids.len() && bad == 0 {
        pass("procs", msg)
    } else {
        fail("procs", msg)
    }
}

fn case_threads() -> u32 {
    let t0 = Instant::now();
    // Every spawned thread plus this one meet here twice: once to prove all
    // started, once to release them.
    let gate = Arc::new(std::sync::Mutex::new(false));
    let cv = Arc::new(std::sync::Condvar::new());
    let mut hs = Vec::with_capacity(THREAD_CAP);
    let mut stop_errno = 0;
    while hs.len() < THREAD_CAP {
        let (g, c) = (gate.clone(), cv.clone());
        match thread::Builder::new().stack_size(64 * 1024).spawn(move || {
            let mut open = g.lock().unwrap();
            while !*open { open = c.wait(open).unwrap(); }
        }) {
            Ok(h) => hs.push(h),
            Err(e) => { stop_errno = e.raw_os_error().unwrap_or(-1); break; }
        }
    }
    let n = hs.len();
    let spawn_s = t0.elapsed().as_secs_f32();
    *gate.lock().unwrap() = true;
    cv.notify_all();
    let mut joined = 0;
    for h in hs { if h.join().is_ok() { joined += 1; } }
    let msg = format!("threads={} stop_errno={} joined={} spawn={:.1}s total={:.1}s",
                      n, stop_errno, joined, spawn_s, t0.elapsed().as_secs_f32());
    if n >= MIN_THREADS && n < THREAD_CAP && stop_errno == EAGAIN && joined == n {
        pass("threads", msg)
    } else {
        fail("threads", msg)
    }
}

fn case_burst() -> u32 {
    let t0 = Instant::now();
    let mut pids = Vec::with_capacity(BURST);
    for i in 0..BURST {
        let p = unsafe { fork() };
        if p == 0 {
            // Two thread exits per child: they used to share the exit ring
            // with process exits and push unwaited records out of it.
            let hs: Vec<_> = (0..2).map(|_| thread::spawn(|| {})).collect();
            for h in hs { let _ = h.join(); }
            unsafe { _exit((i % 200 + 1) as i32) }
        }
        if p < 0 {
            let e = errno();
            reap_all(&pids);
            return fail("burst", format!("fork #{} errno {}", i, e));
        }
        pids.push(p);
    }
    // Delay every wait until all children are gone: poll WNOHANG would reap,
    // so just give them time (they only start two threads each).
    thread::sleep(Duration::from_secs(5));
    let (mut ok, mut echild, mut wrong) = (0, 0, 0);
    for (i, &p) in pids.iter().enumerate() {
        let mut st = 0;
        let r = unsafe { waitpid(p, &mut st, 0) };
        if r == p {
            if (st & 0x7f) == 0 && ((st >> 8) & 0xff) as usize == i % 200 + 1 { ok += 1 } else { wrong += 1 }
        } else if r < 0 && errno() == ECHILD {
            echild += 1;
        } else {
            wrong += 1;
        }
    }
    // Nothing may be left over either.
    let mut st = 0;
    let extra = unsafe { waitpid(-1, &mut st, 1) };
    let extra_errno = if extra < 0 { errno() } else { 0 };
    let msg = format!("children={} ok={} echild={} wrong={} leftover_wait={} errno={} t={:.1}s",
                      BURST, ok, echild, wrong, extra, extra_errno, t0.elapsed().as_secs_f32());
    if ok == BURST && extra < 0 && extra_errno == ECHILD { pass("burst", msg) } else { fail("burst", msg) }
}

fn case_after() -> u32 {
    let p = unsafe { fork() };
    if p == 0 { unsafe { _exit(7) } }
    if p < 0 { return fail("after", format!("fork errno {}", errno())); }
    let mut st = 0;
    let r = unsafe { waitpid(p, &mut st, 0) };
    if r == p && ((st >> 8) & 0xff) == 7 { pass("after", String::new()) } else { fail("after", format!("r={} st={:#x}", r, st)) }
}

fn main() {
    let only = std::env::args().nth(1);
    let cases: [(&str, fn() -> u32); 5] = [
        ("limits", case_limits), ("procs", case_procs), ("threads", case_threads),
        ("burst", case_burst), ("after", case_after),
    ];
    let mut fails = 0;
    for (name, f) in cases {
        if only.as_deref().map_or(true, |o| o == name) { fails += f(); }
    }
    if fails == 0 { println!("PROCMAX: PASS"); } else { println!("PROCMAX: FAIL {fails}"); }
    std::process::exit(fails as i32);
}
