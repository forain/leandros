// `epolltest churn [secs]` (lane piepollsrc): thread / process churn around
// epoll_wait, shaped like the multi-threaded /bin/sh whose kept epoll wait
// snapshot (the per-thread `EpCache`, kept in the scheduler slot) was found
// torn on a Pi 5. Threads park in epoll_wait and come back, short-lived
// threads are created and exit (slot reuse), the main thread forks while
// siblings wait, and forked children exec from a NON-leader thread while
// their other threads sit in epoll_wait (`take_over_leader`). A torn
// snapshot shows as a child killed by a signal, the test itself dying, or
// `[EPOLL] corrupt wait snapshot` in the kernel log (runtests greps it).

use crate::*;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

extern "C" {
    fn fork() -> c_int;
    fn waitpid(pid: c_int, stat_loc: *mut c_int, options: c_int) -> c_int;
    fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> c_int;
    fn _exit(code: c_int) -> !;
    fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
}

static SIGS: AtomicU32 = AtomicU32::new(0);
extern "C" fn on_chld(_s: c_int) { SIGS.fetch_add(1, Relaxed); }
static FBAD: AtomicU32 = AtomicU32::new(0);
static FORKS: AtomicU32 = AtomicU32::new(0);

/// Fork from a NON-leader thread while its siblings wait in epoll_wait
/// (the shell forks from a runtime worker thread).
extern "C" fn forker(_a: *mut c_void) -> *mut c_void {
    unsafe {
        let mut i = 1u32;
        while !STOP.load(Relaxed) {
            let pid = fork();
            if pid == 0 {
                // Only async-signal-safe calls after a fork from a thread.
                if i % 2 == 0 { _exit(0); }
                let argv = [b"/bin/epolltest\0".as_ptr(), b"churnx\0".as_ptr(), core::ptr::null()];
                let envp = [core::ptr::null::<u8>()];
                execve(b"/bin/epolltest\0".as_ptr(), argv.as_ptr(), envp.as_ptr());
                _exit(3);
            }
            if pid > 0 {
                FORKS.fetch_add(1, Relaxed);
                let mut st: c_int = 0;
                let mut r = waitpid(pid, &mut st, 0);
                while r < 0 && *__errno_location() == 4 { r = waitpid(pid, &mut st, 0); }
                if r != pid || st != 0 {
                    FBAD.fetch_add(1, Relaxed);
                    let mut b = [0u8; 80];
                    let n = fmt(&mut b, b"churn: thread-forked child died wrong, mode ", (i % 3) as u64, b" status ", st as u32 as u64);
                    write(1, b.as_ptr(), n);
                }
            }
            i += 1;
        }
    }
    core::ptr::null_mut()
}

static STOP: AtomicBool = AtomicBool::new(false);
static WAITS: AtomicU32 = AtomicU32::new(0);
static SPAWNS: AtomicU32 = AtomicU32::new(0);
static mut EPFD: c_int = -1;
static mut PIPE: [c_int; 2] = [-1, -1];

fn now_ms() -> u64 {
    let mut t = timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(CLOCK_MONOTONIC, &mut t); }
    t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000
}

unsafe fn new_ep(rfd: c_int) -> c_int {
    let ep = epoll_create1(0);
    if ep >= 0 {
        let mut ev: epoll_event = core::mem::zeroed();
        ev.events = 1; // EPOLLIN
        ev.data.fd = rfd;
        epoll_ctl(ep, EPOLL_CTL_ADD, rfd, &mut ev);
    }
    ep
}

/// Park in epoll_wait (shared instance, plus a private one every few
/// rounds) for `rounds` rounds, or until STOP when `rounds` is 0.
unsafe fn wait_rounds(rounds: u32, tmo: c_int) {
    let mut evs: [epoll_event; 4] = core::mem::zeroed();
    let mut i = 0u32;
    while (rounds == 0 && !STOP.load(Relaxed)) || (rounds != 0 && i < rounds) {
        if i % 8 == 7 {
            let ep = new_ep(PIPE[0]);
            if ep >= 0 { epoll_wait(ep, evs.as_mut_ptr(), 4, 0); close(ep); }
        } else {
            let n = epoll_wait(EPFD, evs.as_mut_ptr(), 4, tmo);
            if n > 0 { let mut b = [0u8; 64]; read(PIPE[0], b.as_mut_ptr(), 64); }
        }
        WAITS.fetch_add(1, Relaxed);
        i += 1;
    }
}

extern "C" fn waiter(_a: *mut c_void) -> *mut c_void {
    unsafe { wait_rounds(0, 2); }
    core::ptr::null_mut()
}

extern "C" fn short_waiter(_a: *mut c_void) -> *mut c_void {
    unsafe { wait_rounds(6, 1); }
    core::ptr::null_mut()
}

extern "C" fn spawner(_a: *mut c_void) -> *mut c_void {
    unsafe {
        while !STOP.load(Relaxed) {
            let mut th: [pthread_t; 3] = [core::ptr::null_mut(); 3];
            for t in th.iter_mut() {
                if pthread_create(t, core::ptr::null(), short_waiter, core::ptr::null_mut()) != 0 {
                    *t = core::ptr::null_mut();
                } else { SPAWNS.fetch_add(1, Relaxed); }
            }
            for t in th.iter() { if !t.is_null() { pthread_join(*t, core::ptr::null_mut()); } }
        }
    }
    core::ptr::null_mut()
}

extern "C" fn poker(_a: *mut c_void) -> *mut c_void {
    unsafe { while !STOP.load(Relaxed) { write(PIPE[1], b"x".as_ptr(), 1); usleep(300); } }
    core::ptr::null_mut()
}

/// Forked child: waiters in epoll_wait, then a non-leader thread execs.
extern "C" fn exec_from_thread(_a: *mut c_void) -> *mut c_void {
    unsafe {
        wait_rounds(3, 1);
        let argv = [b"/bin/epolltest\0".as_ptr(), b"churnx\0".as_ptr(), core::ptr::null()];
        let envp = [core::ptr::null::<u8>()];
        execve(b"/bin/epolltest\0".as_ptr(), argv.as_ptr(), envp.as_ptr());
        _exit(3);
    }
}

unsafe fn child(mode: u32) -> ! {
    // The child has its own instance (the shared one is inherited too).
    let ep = new_ep(PIPE[0]);
    if ep >= 0 { EPFD = ep; }
    let mut th: [pthread_t; 3] = [core::ptr::null_mut(); 3];
    for t in th.iter_mut().take(2) {
        if pthread_create(t, core::ptr::null(), waiter, core::ptr::null_mut()) != 0 { _exit(4); }
    }
    match mode % 3 {
        0 => {
            if pthread_create(&mut th[2], core::ptr::null(), exec_from_thread, core::ptr::null_mut()) != 0 { _exit(4); }
            wait_rounds(0, 3); // the leader waits until the exec takes it over
            _exit(5);
        }
        1 => { wait_rounds(4, 1); _exit(0); } // exit_group with waiters parked
        _ => {
            let argv = [b"/bin/epolltest\0".as_ptr(), b"churnx\0".as_ptr(), core::ptr::null()];
            let envp = [core::ptr::null::<u8>()];
            execve(b"/bin/epolltest\0".as_ptr(), argv.as_ptr(), envp.as_ptr()); // leader exec
            _exit(3);
        }
    }
}

pub unsafe fn churn_main(secs: u64, tfork: bool) -> i32 {
    let mut p = [0 as c_int; 2];
    if pipe2(p.as_mut_ptr(), O_NONBLOCK) != 0 { puts(b"churn: FAIL (pipe)\0".as_ptr()); return 1; }
    PIPE = p;
    EPFD = new_ep(p[0]);
    if EPFD < 0 { puts(b"churn: FAIL (epoll_create1)\0".as_ptr()); return 1; }
    if tfork { signal(17, on_chld); } // SIGCHLD interrupts the waiters
    let mut th: [pthread_t; 6] = [core::ptr::null_mut(); 6];
    let fns: [extern "C" fn(*mut c_void) -> *mut c_void; 6] = [waiter, waiter, waiter, spawner, poker, forker];
    for (t, f) in th.iter_mut().zip(fns.iter()).take(if tfork { 6 } else { 5 }) {
        if pthread_create(t, core::ptr::null(), *f, core::ptr::null_mut()) != 0 {
            puts(b"churn: FAIL (pthread_create)\0".as_ptr()); return 1;
        }
    }
    let end = now_ms() + secs * 1000;
    let (mut forks, mut bad, mut i) = (0u32, 0u32, 0u32);
    while now_ms() < end {
        let pid = fork();
        if pid == 0 { child(i); }
        if pid < 0 { usleep(1000); continue; }
        forks += 1;
        let mut st: c_int = 0;
        let mut r = waitpid(pid, &mut st, 0);
        while r < 0 && *__errno_location() == 4 { r = waitpid(pid, &mut st, 0); }
        // Exit codes: 0 = clean (exit or the exec'd image), anything else or
        // a signal = the child died wrong.
        if r != pid || st != 0 {
            bad += 1;
            let mut b = [0u8; 80];
            let n = fmt(&mut b, b"churn: child died wrong, mode ", (i % 3) as u64, b" status ", st as u32 as u64);
            write(1, b.as_ptr(), n);
        }
        i += 1;
    }
    STOP.store(true, Relaxed);
    for t in th.iter() { if !t.is_null() { pthread_join(*t, core::ptr::null_mut()); } }
    let mut b = [0u8; 96];
    let n = fmt(&mut b, b"churn: forks ", (forks + FORKS.load(Relaxed)) as u64, b" thread spawns ", SPAWNS.load(Relaxed) as u64);
    write(1, b.as_ptr(), n);
    if bad == 0 && FBAD.load(Relaxed) == 0 && forks > 0 { puts(b"churn: PASS\0".as_ptr()); 0 } else { puts(b"churn: FAIL\0".as_ptr()); 1 }
}

fn put_dec(b: &mut [u8], mut at: usize, v: u64) -> usize {
    let mut d = [0u8; 20];
    let mut n = 0;
    let mut x = v;
    loop { d[n] = b'0' + (x % 10) as u8; n += 1; x /= 10; if x == 0 { break; } }
    while n > 0 && at < b.len() { n -= 1; b[at] = d[n]; at += 1; }
    at
}

fn fmt(b: &mut [u8], a: &[u8], x: u64, c: &[u8], y: u64) -> usize {
    let mut at = 0;
    for &ch in a { if at < b.len() { b[at] = ch; at += 1; } }
    at = put_dec(b, at, x);
    for &ch in c { if at < b.len() { b[at] = ch; at += 1; } }
    at = put_dec(b, at, y);
    if at < b.len() { b[at] = b'\n'; at += 1; }
    at
}
