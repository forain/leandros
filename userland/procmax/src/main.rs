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
//!   fsthreads — 4000 threads each stat a file on the root f2fs mount (one
//!            IPC reply port each) and park: no stat may fail (the port table
//!            was a fixed 512 entries system-wide until 2026-10-19)
//!   unixconns — 1200 AF_UNIX socketpairs at once, each round-trips a byte
//!            (the connection pool was a fixed 512 until 2026-10-19)
//!   appendv — two processes writev 3-iovec records to one O_APPEND file;
//!            no record may be torn (writev appended per iovec until 2026-10-19)
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
    fn socketpair(domain: i32, ty: i32, proto: i32, sv: *mut i32) -> i32;
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    fn writev(fd: i32, iov: *const u8, cnt: i32) -> isize;
    fn open(path: *const u8, flags: i32, mode: u32) -> i32;
    fn unlink(path: *const u8) -> i32;
    fn setrlimit(res: i32, rl: *const u64) -> i32;
    fn getrlimit(res: i32, rlim: *mut [u64; 2]) -> i32;
    fn __errno_location() -> *mut i32;
}

const EAGAIN: i32 = 11;
const ECHILD: i32 = 10;
const RLIMIT_NPROC: i32 = 6;
const RLIMIT_NOFILE: i32 = 7;
/// 2048 processes minus what a serial-console boot already runs.
const MIN_PROCS: usize = 1900;
/// 4096 tasks minus the boot's threads and this test's own processes.
const MIN_THREADS: usize = 3800;
/// Stop forking here even if the kernel would allow more (a regression to
/// "no limit" must not exhaust guest RAM before it is noticed).
const PROC_CAP: usize = 2600;
const THREAD_CAP: usize = 5000;
const BURST: usize = 400;
const FS_THREADS: usize = 4000;

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

/// Threads that each touch a mounted filesystem. Every such thread lazily
/// takes a kernel IPC reply port and keeps it until it exits; the port table
/// used to be a fixed 512 buckets for the whole system, so thread ~500's
/// first `stat` failed with ENOMEM. Each thread stats a file on the root
/// filesystem (an f2fs mount, i.e. a `call_port` round trip) and then parks,
/// so all the reply ports are live at once.
fn case_fsthreads() -> u32 {
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
    let t0 = Instant::now();
    let gate = Arc::new(std::sync::Mutex::new(false));
    let cv = Arc::new(std::sync::Condvar::new());
    let bad = Arc::new(AtomicUsize::new(0));
    let bad_errno = Arc::new(AtomicU32::new(0));
    let mut hs = Vec::with_capacity(FS_THREADS);
    let mut stop_errno = 0;
    while hs.len() < FS_THREADS {
        let (g, c, b, be) = (gate.clone(), cv.clone(), bad.clone(), bad_errno.clone());
        match thread::Builder::new().stack_size(64 * 1024).spawn(move || {
            if let Err(e) = std::fs::metadata("/bin/procmax") {
                b.fetch_add(1, Ordering::Relaxed);
                be.store(e.raw_os_error().unwrap_or(-1) as u32, Ordering::Relaxed);
            }
            let mut open = g.lock().unwrap();
            while !*open { open = c.wait(open).unwrap(); }
        }) {
            Ok(h) => hs.push(h),
            Err(e) => { stop_errno = e.raw_os_error().unwrap_or(-1); break; }
        }
    }
    let n = hs.len();
    *gate.lock().unwrap() = true;
    cv.notify_all();
    let mut joined = 0;
    for h in hs { if h.join().is_ok() { joined += 1; } }
    // The threads are gone, so their reply ports must be too: one more stat
    // from a fresh thread still works.
    let again = thread::spawn(|| std::fs::metadata("/bin/procmax").is_ok()).join().unwrap_or(false);
    let nbad = bad.load(Ordering::Relaxed);
    let msg = format!("threads={} stop_errno={} stat_failures={} (errno {}) joined={} again={} total={:.1}s",
                      n, stop_errno, nbad, bad_errno.load(Ordering::Relaxed), joined, again,
                      t0.elapsed().as_secs_f32());
    if n >= MIN_THREADS && nbad == 0 && joined == n && again {
        pass("fsthreads", msg)
    } else {
        fail("fsthreads", msg)
    }
}

/// More AF_UNIX connections than the old fixed 512-entry pool: 1200
/// socketpairs (2400 fds, RLIMIT_NOFILE raised first), each one carrying a
/// byte. Before lane pools the 513th socketpair (fewer with a desktop
/// session up) failed with ENOMEM.
fn case_unixconns() -> u32 {
    const PAIRS: usize = 1200;
    let mut rl = [0u64; 2];
    unsafe {
        getrlimit(RLIMIT_NOFILE, &mut rl);
        let want = [8192u64, 8192u64];
        if setrlimit(RLIMIT_NOFILE, want.as_ptr()) != 0 {
            return fail("unixconns", format!("setrlimit errno {}", errno()));
        }
    }
    let mut fds: Vec<[i32; 2]> = Vec::with_capacity(PAIRS);
    let mut err = 0;
    while fds.len() < PAIRS {
        let mut sv = [-1i32; 2];
        if unsafe { socketpair(1 /* AF_UNIX */, 1 /* SOCK_STREAM */, 0, sv.as_mut_ptr()) } != 0 {
            err = errno();
            break;
        }
        fds.push(sv);
    }
    let made = fds.len();
    let mut bad = 0;
    for sv in &fds {
        let b = [0x5au8];
        let mut r = [0u8];
        if unsafe { write(sv[0], b.as_ptr(), 1) } != 1 || unsafe { read(sv[1], r.as_mut_ptr(), 1) } != 1 || r[0] != 0x5a {
            bad += 1;
        }
    }
    for sv in &fds { unsafe { close(sv[0]); close(sv[1]); } }
    unsafe { setrlimit(RLIMIT_NOFILE, rl.as_ptr()); }
    let msg = format!("pairs={} errno={} bad_roundtrips={}", made, err, bad);
    if made == PAIRS && bad == 0 { pass("unixconns", msg) } else { fail("unixconns", msg) }
}

/// writev(2) on an O_APPEND file is ONE append: two processes each writev
/// 2000 records of three iovecs (tag, 60-byte body, newline) to the same
/// file; every line must come out whole. Before lane pools each iovec was a
/// separate append and the other writer's iovecs landed in between.
fn case_appendv() -> u32 {
    const RECS: usize = 2000;
    let path = b"/root/.procmax-appendv\0";
    unsafe { unlink(path.as_ptr()); }
    let mut kids = Vec::new();
    for tag in [b'A', b'B'] {
        let p = unsafe { fork() };
        if p == 0 {
            let fd = unsafe { open(path.as_ptr(), 0o2101 /* O_WRONLY|O_CREAT|O_APPEND */, 0o644) };
            if fd < 0 { unsafe { _exit(2) } }
            let head = [tag];
            let body = [tag; 60];
            let nl = [b'\n'];
            let iov: [[usize; 2]; 3] = [
                [head.as_ptr() as usize, 1], [body.as_ptr() as usize, 60], [nl.as_ptr() as usize, 1],
            ];
            for _ in 0..RECS {
                if unsafe { writev(fd, iov.as_ptr() as *const u8, 3) } != 62 { unsafe { _exit(3) } }
            }
            unsafe { _exit(0) }
        }
        if p < 0 { return fail("appendv", format!("fork errno {}", errno())); }
        kids.push(p);
    }
    let mut kid_bad = 0;
    for p in kids {
        let mut st = 0;
        unsafe { waitpid(p, &mut st, 0); }
        if st != 0 { kid_bad += 1; }
    }
    let data = std::fs::read("/root/.procmax-appendv").unwrap_or_default();
    unsafe { unlink(path.as_ptr()); }
    let mut lines = 0;
    let mut torn = 0;
    for l in data.split(|&b| b == b'\n') {
        if l.is_empty() { continue; }
        lines += 1;
        if l.len() != 61 || l.iter().any(|&b| b != l[0]) { torn += 1; }
    }
    let msg = format!("bytes={} lines={} torn={} writer_failures={}", data.len(), lines, torn, kid_bad);
    if kid_bad == 0 && lines == 2 * RECS && torn == 0 && data.len() == 2 * RECS * 62 {
        pass("appendv", msg)
    } else {
        fail("appendv", msg)
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
    let cases: [(&str, fn() -> u32); 8] = [
        ("limits", case_limits), ("procs", case_procs), ("threads", case_threads),
        ("fsthreads", case_fsthreads), ("unixconns", case_unixconns), ("appendv", case_appendv),
        ("burst", case_burst), ("after", case_after),
    ];
    let mut fails = 0;
    for (name, f) in cases {
        if only.as_deref().map_or(true, |o| o == name) { fails += f(); }
    }
    if fails == 0 { println!("PROCMAX: PASS"); } else { println!("PROCMAX: FAIL {fails}"); }
    std::process::exit(fails as i32);
}
