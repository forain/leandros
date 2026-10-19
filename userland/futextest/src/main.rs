//! futextest — futex keying, robust lists, PI hand-off, CLOCK_REALTIME
//! restarts (lane futex, bug list L10.3).
//!
//!   shared_memfd_two_maps — one memfd mapped twice in one process: a waiter
//!                           on mapping A is woken through mapping B
//!   shared_cross_process  — the same memfd mapped in two processes at
//!                           different addresses: the parent's wake reaches
//!                           the child's waiter
//!   shared_anon_fork      — MAP_SHARED|MAP_ANONYMOUS across fork
//!   private_not_shared    — a MAP_PRIVATE page at the same address in a
//!                           forked child is NOT the parent's futex (a
//!                           non-private wake must not cross processes)
//!   robust_*              — musl's robust mutexes: the owner dies (exits,
//!                           is killed, its process exit_groups from another
//!                           thread, a thread exits) holding the lock; the
//!                           next locker gets EOWNERDEAD, then
//!                           pthread_mutex_consistent makes it usable again
//!   pi_handoff            — FUTEX_UNLOCK_PI hands the lock to the queued
//!                           locker: the word names it before it even runs
//!   realtime_restart      — a FUTEX_WAIT_BITSET|FUTEX_CLOCK_REALTIME waiter
//!                           is stopped, the wall clock is stepped past its
//!                           deadline, and after SIGCONT it times out at once
//!                           instead of waiting out the old monotonic
//!                           deadline (root only; skipped otherwise)
//!
//! Prints `<name>: PASS` or `<name>: FAIL (...)`, ends with
//! `--- futextest done --- failures=N`; exit status is the failure count.

use std::sync::atomic::{AtomicI32, AtomicU32, Ordering::SeqCst};
use std::thread;
use std::time::{Duration, Instant};

#[cfg(target_arch = "aarch64")]
mod nr { pub const FUTEX: i64 = 98; pub const GETTID: i64 = 178; pub const MEMFD_CREATE: i64 = 279; }
#[cfg(target_arch = "x86_64")]
mod nr { pub const FUTEX: i64 = 202; pub const GETTID: i64 = 186; pub const MEMFD_CREATE: i64 = 319; }

const FUTEX_WAIT: i64 = 0;
const FUTEX_WAKE: i64 = 1;
const FUTEX_LOCK_PI: i64 = 6;
const FUTEX_UNLOCK_PI: i64 = 7;
const FUTEX_WAIT_BITSET: i64 = 9;
const FUTEX_PRIVATE: i64 = 128;
const FUTEX_CLOCK_REALTIME: i64 = 256;
const FUTEX_WAITERS: u32 = 0x8000_0000;
const TID_MASK: u32 = 0x3fff_ffff;

const PROT_RW: i32 = 3;
const MAP_SHARED: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MAP_ANON: i32 = 0x20;
const CLOCK_REALTIME: i32 = 0;
const SIGKILL: i32 = 9;
const SIGCONT: i32 = 18;
const SIGSTOP: i32 = 19;
const WUNTRACED: i32 = 2;
const ETIMEDOUT: i32 = 110;
const EOWNERDEAD: i32 = 130;

#[repr(C)]
#[derive(Clone, Copy)]
struct Timespec { tv_sec: i64, tv_nsec: i64 }

/// musl's pthread_mutex_t (40 bytes on 64-bit) and pthread_mutexattr_t.
#[repr(C, align(8))]
struct Mutex([u8; 40]);
#[repr(C)]
struct MutexAttr(u32);

extern "C" {
    fn syscall(n: i64, ...) -> i64;
    fn __errno_location() -> *mut i32;
    fn fork() -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
    fn waitpid(pid: i32, status: *mut i32, options: i32) -> i32;
    fn _exit(code: i32) -> !;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn ftruncate(fd: i32, len: i64) -> i32;
    fn close(fd: i32) -> i32;
    fn clock_gettime(clk: i32, ts: *mut Timespec) -> i32;
    fn clock_settime(clk: i32, ts: *const Timespec) -> i32;
    fn geteuid() -> u32;
    fn pthread_mutexattr_init(a: *mut MutexAttr) -> i32;
    fn pthread_mutexattr_setpshared(a: *mut MutexAttr, v: i32) -> i32;
    fn pthread_mutexattr_setrobust(a: *mut MutexAttr, v: i32) -> i32;
    fn pthread_mutexattr_setprotocol(a: *mut MutexAttr, v: i32) -> i32;
    fn pthread_mutex_init(m: *mut Mutex, a: *const MutexAttr) -> i32;
    fn pthread_mutex_lock(m: *mut Mutex) -> i32;
    fn pthread_mutex_timedlock(m: *mut Mutex, ts: *const Timespec) -> i32;
    fn pthread_mutex_unlock(m: *mut Mutex) -> i32;
    fn pthread_mutex_consistent(m: *mut Mutex) -> i32;
}

fn errno() -> i32 { unsafe { *__errno_location() } }
fn gettid() -> u32 { unsafe { syscall(nr::GETTID) as u32 } }

/// Raw futex call: the result, or -errno.
fn futex(w: *const AtomicU32, op: i64, val: u32, ts: *const Timespec, val3: u32) -> i64 {
    let r = unsafe { syscall(nr::FUTEX, w, op, val as u64, ts, 0usize, val3 as u64) };
    if r < 0 { -(errno() as i64) } else { r }
}

fn rel(ms: u64) -> Timespec {
    Timespec { tv_sec: (ms / 1000) as i64, tv_nsec: ((ms % 1000) * 1_000_000) as i64 }
}

fn realtime_plus(ms: u64) -> Timespec {
    let mut t = Timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { clock_gettime(CLOCK_REALTIME, &mut t); }
    t.tv_nsec += ((ms % 1000) * 1_000_000) as i64;
    t.tv_sec += (ms / 1000) as i64 + t.tv_nsec / 1_000_000_000;
    t.tv_nsec %= 1_000_000_000;
    t
}

fn map(len: usize, flags: i32, fd: i32) -> *mut u8 {
    let p = unsafe { mmap(core::ptr::null_mut(), len, PROT_RW, flags, fd, 0) };
    if p as isize == -1 { panic!("mmap failed: errno {}", errno()); }
    p
}

fn memfd() -> i32 {
    let fd = unsafe { syscall(nr::MEMFD_CREATE, b"futextest\0".as_ptr(), 0usize) } as i32;
    if fd < 0 { panic!("memfd_create failed: errno {}", errno()); }
    if unsafe { ftruncate(fd, 4096) } != 0 { panic!("ftruncate failed: errno {}", errno()); }
    fd
}

fn word<'a>(p: *mut u8) -> &'a AtomicU32 { unsafe { &*(p as *const AtomicU32) } }

/// Wake one waiter on `w` (non-private) until one is woken or `ms` pass.
fn wake_until(w: &AtomicU32, ms: u64) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(ms) {
        if futex(w, FUTEX_WAKE, 1, core::ptr::null(), 0) == 1 { return true; }
        thread::sleep(Duration::from_millis(5));
    }
    false
}

fn wait_child(pid: i32) -> i32 {
    let mut st = 0;
    unsafe { waitpid(pid, &mut st, 0); }
    if st & 0x7f == 0 { (st >> 8) & 0xff } else { 128 + (st & 0x7f) }
}

fn report(name: &str, ok: bool, detail: String) -> bool {
    if ok { println!("{}: PASS", name); } else { println!("{}: FAIL ({})", name, detail); }
    ok
}

// ── Shared-futex keying ──────────────────────────────────────────────────────

fn shared_memfd_two_maps() -> bool {
    let fd = memfd();
    let a = map(4096, MAP_SHARED, fd);
    let b = map(4096, MAP_SHARED, fd);
    word(a).store(0, SeqCst);
    let pa = a as usize;
    let h = thread::spawn(move || futex(pa as *const AtomicU32, FUTEX_WAIT, 0, &rel(3000), 0));
    let woke = wake_until(word(b), 2000);
    let r = h.join().unwrap();
    unsafe { munmap(a, 4096); munmap(b, 4096); close(fd); }
    report("shared_memfd_two_maps", woke && r == 0 && a != b,
           format!("woke={} wait={} a={:p} b={:p}", woke, r, a, b))
}

fn shared_cross_process() -> bool {
    let fd = memfd();
    let a = map(4096, MAP_SHARED, fd);
    word(a).store(0, SeqCst);
    let pid = unsafe { fork() };
    if pid == 0 {
        // A second mapping of the same object, at another address.
        let c = map(4096, MAP_SHARED, fd);
        if c == a { unsafe { _exit(3); } }
        let r = futex(word(c), FUTEX_WAIT, 0, &rel(3000), 0);
        unsafe { _exit(if r == 0 { 0 } else if r == -(ETIMEDOUT as i64) { 1 } else { 2 }); }
    }
    let woke = wake_until(word(a), 2000);
    let st = wait_child(pid);
    unsafe { munmap(a, 4096); close(fd); }
    report("shared_cross_process", woke && st == 0, format!("woke={} child={}", woke, st))
}

fn shared_anon_fork() -> bool {
    let a = map(4096, MAP_SHARED | MAP_ANON, -1);
    word(a).store(0, SeqCst);
    let pid = unsafe { fork() };
    if pid == 0 {
        let r = futex(word(a), FUTEX_WAIT, 0, &rel(3000), 0);
        unsafe { _exit(if r == 0 { 0 } else { 1 }); }
    }
    let woke = wake_until(word(a), 2000);
    let st = wait_child(pid);
    unsafe { munmap(a, 4096); }
    report("shared_anon_fork", woke && st == 0, format!("woke={} child={}", woke, st))
}

fn private_not_shared() -> bool {
    let a = map(4096, MAP_PRIVATE | MAP_ANON, -1);
    word(a).store(0, SeqCst);
    let pid = unsafe { fork() };
    if pid == 0 {
        let r = futex(word(a), FUTEX_WAIT, 0, &rel(500), 0);
        unsafe { _exit(if r == -(ETIMEDOUT as i64) { 0 } else { 1 }); }
    }
    thread::sleep(Duration::from_millis(100));
    let mut woken = 0;
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(250) {
        woken += futex(word(a), FUTEX_WAKE, 1, core::ptr::null(), 0).max(0);
        thread::sleep(Duration::from_millis(10));
    }
    let st = wait_child(pid);
    unsafe { munmap(a, 4096); }
    report("private_not_shared", woken == 0 && st == 0, format!("woken={} child={}", woken, st))
}

// ── Robust mutexes ───────────────────────────────────────────────────────────

/// A robust mutex (+ a flag word) in a shared page; `pi` adds
/// PTHREAD_PRIO_INHERIT.
fn robust_mutex(pshared: bool, pi: bool) -> (*mut Mutex, &'static AtomicU32) {
    let p = map(4096, MAP_SHARED | MAP_ANON, -1);
    let m = p as *mut Mutex;
    let mut a = MutexAttr(0);
    unsafe {
        pthread_mutexattr_init(&mut a);
        if pshared { pthread_mutexattr_setpshared(&mut a, 1); }
        pthread_mutexattr_setrobust(&mut a, 1);
        if pi { pthread_mutexattr_setprotocol(&mut a, 1); }
        pthread_mutex_init(m, &a);
    }
    (m, word(unsafe { p.add(64) }))
}

fn spin_until(flag: &AtomicU32, ms: u64) -> bool {
    let t0 = Instant::now();
    while flag.load(SeqCst) == 0 {
        if t0.elapsed() > Duration::from_millis(ms) { return false; }
        thread::sleep(Duration::from_millis(2));
    }
    true
}

/// The next locker after an owner death: EOWNERDEAD, then (after
/// pthread_mutex_consistent) an ordinary lock/unlock.
fn check_recovery(m: *mut Mutex) -> (bool, String) {
    let r1 = unsafe { pthread_mutex_timedlock(m, &realtime_plus(5000)) };
    let c = if r1 == EOWNERDEAD { unsafe { pthread_mutex_consistent(m) } } else { -1 };
    let u1 = unsafe { pthread_mutex_unlock(m) };
    let r2 = unsafe { pthread_mutex_timedlock(m, &realtime_plus(2000)) };
    let u2 = unsafe { pthread_mutex_unlock(m) };
    (r1 == EOWNERDEAD && c == 0 && u1 == 0 && r2 == 0 && u2 == 0,
     format!("lock={} consistent={} unlock={} relock={} unlock2={}", r1, c, u1, r2, u2))
}

/// How the child that holds the lock dies.
#[derive(Clone, Copy, PartialEq)]
enum Death { Exit, ExitWhileWaited, Killed, GroupExitFromThread }

fn robust_process(name: &str, death: Death, pi: bool) -> bool {
    let (m, flag) = robust_mutex(true, pi);
    let pid = unsafe { fork() };
    if pid == 0 {
        match death {
            Death::GroupExitFromThread => {
                // A non-main thread holds the lock; the main thread exits the
                // process, so the holder is reaped by the group kill.
                let mp = m as usize;
                thread::spawn(move || unsafe {
                    pthread_mutex_lock(mp as *mut Mutex);
                    flag.store(1, SeqCst);
                    loop { thread::sleep(Duration::from_secs(10)); }
                });
                spin_until(flag, 3000);
                unsafe { syscall(94 + if cfg!(target_arch = "x86_64") { 137 } else { 0 }, 0usize); } // exit_group(0)
                unsafe { _exit(0); }
            }
            _ => unsafe {
                pthread_mutex_lock(m);
                flag.store(1, SeqCst);
                match death {
                    Death::Exit => _exit(0),
                    Death::ExitWhileWaited => { thread::sleep(Duration::from_millis(200)); _exit(0) }
                    _ => loop { thread::sleep(Duration::from_secs(10)); },
                }
            },
        }
    }
    let ready = spin_until(flag, 3000);
    match death {
        Death::Exit => { wait_child(pid); }
        Death::Killed => { unsafe { kill(pid, SIGKILL); } }
        _ => {}
    }
    let (ok, detail) = check_recovery(m);
    if death != Death::Exit { wait_child(pid); }
    report(name, ready && ok, format!("ready={} {}", ready, detail))
}

fn robust_thread_exit() -> bool {
    let (m, flag) = robust_mutex(false, false);
    let mp = m as usize;
    thread::spawn(move || unsafe {
        pthread_mutex_lock(mp as *mut Mutex);
        flag.store(1, SeqCst);
    }).join().unwrap();
    let (ok, detail) = check_recovery(m);
    report("robust_thread_exit", ok, detail)
}

// ── PI hand-off ──────────────────────────────────────────────────────────────

static PI_WORD: AtomicU32 = AtomicU32::new(0);
static PI_TID: AtomicU32 = AtomicU32::new(0);
static PI_RES: AtomicI32 = AtomicI32::new(-1);
static PI_OWNED: AtomicU32 = AtomicU32::new(0);

fn pi_handoff() -> bool {
    let me = gettid();
    PI_WORD.store(me, SeqCst);
    let h = thread::spawn(|| {
        let tid = gettid();
        PI_TID.store(tid, SeqCst);
        let r = futex(&PI_WORD, FUTEX_LOCK_PI | FUTEX_PRIVATE, 0, core::ptr::null(), 0);
        PI_RES.store(r as i32, SeqCst);
        PI_OWNED.store((PI_WORD.load(SeqCst) & TID_MASK == tid) as u32, SeqCst);
        futex(&PI_WORD, FUTEX_UNLOCK_PI | FUTEX_PRIVATE, 0, core::ptr::null(), 0);
    });
    // Wait until the locker is queued (it set FUTEX_WAITERS) and parked.
    let t0 = Instant::now();
    while PI_WORD.load(SeqCst) & FUTEX_WAITERS == 0 && t0.elapsed() < Duration::from_secs(3) {
        thread::sleep(Duration::from_millis(2));
    }
    thread::sleep(Duration::from_millis(50));
    let un = futex(&PI_WORD, FUTEX_UNLOCK_PI | FUTEX_PRIVATE, 0, core::ptr::null(), 0);
    // Right after the unlock, before the locker has necessarily run: the
    // word must already name it, so a user-space cas cannot take the lock.
    let after = PI_WORD.load(SeqCst);
    let stolen = PI_WORD.compare_exchange(0, me, SeqCst, SeqCst).is_ok();
    h.join().unwrap();
    let tid = PI_TID.load(SeqCst);
    let ok = un == 0 && after == (tid | FUTEX_WAITERS) && !stolen
        && PI_RES.load(SeqCst) == 0 && PI_OWNED.load(SeqCst) == 1 && PI_WORD.load(SeqCst) == 0;
    report("pi_handoff", ok, format!("unlock={} word_after={:#x} locker={} stolen={} lock={} owned={} final={:#x}",
        un, after, tid, stolen, PI_RES.load(SeqCst), PI_OWNED.load(SeqCst), PI_WORD.load(SeqCst)))
}

// ── CLOCK_REALTIME deadline across a stop ────────────────────────────────────

fn step_realtime(delta_s: i64) -> bool {
    let mut t = Timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe {
        clock_gettime(CLOCK_REALTIME, &mut t);
        t.tv_sec += delta_s;
        clock_settime(CLOCK_REALTIME, &t) == 0
    }
}

fn realtime_restart() -> bool {
    if unsafe { geteuid() } != 0 {
        println!("realtime_restart: SKIP (needs root for clock_settime)");
        return true;
    }
    let w = map(4096, MAP_SHARED | MAP_ANON, -1);
    word(w).store(0, SeqCst);
    let pid = unsafe { fork() };
    if pid == 0 {
        let dl = realtime_plus(3000);
        let t0 = Instant::now();
        let r = futex(word(w), FUTEX_WAIT_BITSET | FUTEX_PRIVATE | FUTEX_CLOCK_REALTIME, 0, &dl, u32::MAX);
        let ms = t0.elapsed().as_millis() as u64;
        // Stepped 10 s forward while stopped (at ~0.3 s): the deadline is
        // past at the SIGCONT, so the wait ends within ~1 s, not at 3 s.
        unsafe { _exit(if r != -(ETIMEDOUT as i64) { 2 } else if ms < 2000 { 0 } else { 1 }); }
    }
    thread::sleep(Duration::from_millis(300));
    let mut st = 0;
    unsafe { kill(pid, SIGSTOP); waitpid(pid, &mut st, WUNTRACED); }
    let stopped = st & 0xff == 0x7f;
    let stepped = step_realtime(10);
    thread::sleep(Duration::from_millis(100));
    unsafe { kill(pid, SIGCONT); }
    let rc = wait_child(pid);
    let restored = !stepped || step_realtime(-10);
    unsafe { munmap(w, 4096); }
    report("realtime_restart", stopped && stepped && restored && rc == 0,
           format!("stopped={} stepped={} restored={} child={} (1 = waited out the old deadline)",
                   stopped, stepped, restored, rc))
}

fn main() {
    let only = std::env::args().nth(1);
    let tests: &[(&str, fn() -> bool)] = &[
        ("shared_memfd_two_maps", shared_memfd_two_maps),
        ("shared_cross_process", shared_cross_process),
        ("shared_anon_fork", shared_anon_fork),
        ("private_not_shared", private_not_shared),
        ("robust_exit", || robust_process("robust_exit", Death::Exit, false)),
        ("robust_exit_waited", || robust_process("robust_exit_waited", Death::ExitWhileWaited, false)),
        ("robust_killed", || robust_process("robust_killed", Death::Killed, false)),
        ("robust_group_exit", || robust_process("robust_group_exit", Death::GroupExitFromThread, false)),
        ("robust_thread_exit", robust_thread_exit),
        ("robust_pi_exit", || robust_process("robust_pi_exit", Death::ExitWhileWaited, true)),
        ("pi_handoff", pi_handoff),
        ("realtime_restart", realtime_restart),
    ];
    let mut failures = 0;
    for (name, f) in tests {
        if only.as_deref().map_or(false, |o| o != *name) { continue; }
        if !f() { failures += 1; }
    }
    println!("--- futextest done --- failures={}", failures);
    unsafe { _exit(failures); }
}
