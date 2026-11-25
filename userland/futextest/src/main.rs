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
//!   pi_handoff_stress     — (by name only) `[rounds] [settle_ms] [bitonly]
//!                           [hogs]`: pi_handoff rounds under CPU hogs
//!   realtime_restart      — a FUTEX_WAIT_BITSET|FUTEX_CLOCK_REALTIME waiter
//!                           is stopped, the wall clock is stepped past its
//!                           deadline, and after SIGCONT it times out at once
//!                           instead of waiting out the old monotonic
//!                           deadline (root only; skipped otherwise)
//!
//! Prints `<name>: PASS` or `<name>: FAIL (...)`, ends with
//! `--- futextest done --- failures=N`; exit status is the failure count.

use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering::SeqCst};
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
/// The locker may release the lock only once the main thread has finished
/// observing the word after its UNLOCK_PI; otherwise a locker that runs at
/// once (idle host) can unlock before the observation and make a correct
/// hand-off look like a release to 0 / a steal.
static PI_GO: AtomicU32 = AtomicU32::new(0);

/// The scheduler state letter of thread `tid` (from /proc/self/task/<tid>/stat),
/// or '?' if it cannot be read.
fn thread_state(tid: u32) -> char {
    let s = std::fs::read_to_string(format!("/proc/self/task/{}/stat", tid))
        .or_else(|_| std::fs::read_to_string(format!("/proc/{}/stat", tid)))
        .unwrap_or_default();
    s.rfind(')').and_then(|i| s[i + 1..].trim_start().chars().next()).unwrap_or('?')
}

/// One hand-off round. FUTEX_WAITERS in the word is NOT proof that the
/// locker is queued: FUTEX_LOCK_PI sets the bit with a user-space cas and only
/// then registers in the futex table, so a locker preempted in between is
/// invisible to FUTEX_UNLOCK_PI, which then (correctly) releases the word to 0
/// instead of handing it over. The round therefore waits until the locker is
/// also asleep in the kernel (state S), which it only is once registered —
/// unless `bit_only` asks for the old, racy signal (stress mode, to show the
/// window). `settle_ms` is the extra sleep before the unlock.
fn pi_handoff_round(settle_ms: u64, bit_only: bool) -> (bool, String) {
    let me = gettid();
    PI_WORD.store(me, SeqCst);
    PI_TID.store(0, SeqCst);
    PI_RES.store(-1, SeqCst);
    PI_OWNED.store(0, SeqCst);
    PI_GO.store(0, SeqCst);
    let h = thread::spawn(|| {
        let tid = gettid();
        PI_TID.store(tid, SeqCst);
        let r = futex(&PI_WORD, FUTEX_LOCK_PI | FUTEX_PRIVATE, 0, core::ptr::null(), 0);
        PI_RES.store(r as i32, SeqCst);
        PI_OWNED.store((PI_WORD.load(SeqCst) & TID_MASK == tid) as u32, SeqCst);
        let g = Instant::now();
        while PI_GO.load(SeqCst) == 0 && g.elapsed() < Duration::from_secs(10) { thread::sleep(Duration::from_millis(1)); }
        futex(&PI_WORD, FUTEX_UNLOCK_PI | FUTEX_PRIVATE, 0, core::ptr::null(), 0);
    });
    // Wait until the locker set FUTEX_WAITERS and is parked in the kernel.
    let t0 = Instant::now();
    let mut state = '?';
    let parked = loop {
        let tid = PI_TID.load(SeqCst);
        if PI_WORD.load(SeqCst) & FUTEX_WAITERS != 0 && tid != 0 {
            if bit_only { break true; }
            state = thread_state(tid);
            if state == 'S' { break true; }
        }
        if t0.elapsed() > Duration::from_secs(10) { break false; }
        thread::sleep(Duration::from_millis(2));
    };
    let waited_ms = t0.elapsed().as_millis();
    if settle_ms > 0 { thread::sleep(Duration::from_millis(settle_ms)); }
    let un = futex(&PI_WORD, FUTEX_UNLOCK_PI | FUTEX_PRIVATE, 0, core::ptr::null(), 0);
    // Right after the unlock, before the locker has necessarily run: the
    // word must already name it, so a user-space cas cannot take the lock.
    let after = PI_WORD.load(SeqCst);
    let stolen = PI_WORD.compare_exchange(0, me, SeqCst, SeqCst).is_ok();
    PI_GO.store(1, SeqCst);
    // A steal means no hand-off happened: give the lock back, or the locker
    // (now queued behind us) and the join below would hang forever.
    if stolen { futex(&PI_WORD, FUTEX_UNLOCK_PI | FUTEX_PRIVATE, 0, core::ptr::null(), 0); }
    h.join().unwrap();
    let tid = PI_TID.load(SeqCst);
    let ok = parked && un == 0 && after == (tid | FUTEX_WAITERS) && !stolen
        && PI_RES.load(SeqCst) == 0 && PI_OWNED.load(SeqCst) == 1 && PI_WORD.load(SeqCst) == 0;
    let why = if !parked { "locker never parked (WAITERS+state S) within 10s" }
        else if after == 0 { "UNLOCK_PI released the word to 0: no queued locker found (not registered yet?)" }
        else if after & TID_MASK != tid { "word names neither nobody nor the locker after UNLOCK_PI" }
        else if after & FUTEX_WAITERS == 0 { "locker already re-took the free word itself (no hand-off)" }
        else if PI_RES.load(SeqCst) != 0 || PI_OWNED.load(SeqCst) != 1 { "locker's LOCK_PI result/ownership wrong" }
        else if PI_WORD.load(SeqCst) != 0 { "locker's UNLOCK_PI did not release to 0" }
        else { "-" };
    (ok, format!("{}: unlock={} word_after={:#x} locker={} state={} waited={}ms stolen={} lock={} owned={} final={:#x}",
        why, un, after, tid, state, waited_ms, stolen, PI_RES.load(SeqCst), PI_OWNED.load(SeqCst), PI_WORD.load(SeqCst)))
}

/// LOCK_PI on a word that still names a TID that has exited must be ESRCH
/// (strictness of the re-check that tolerates a word that moved on).
fn pi_dead_owner() -> bool {
    static W: AtomicU32 = AtomicU32::new(0);
    static DEAD: AtomicU32 = AtomicU32::new(0);
    thread::spawn(|| DEAD.store(gettid(), SeqCst)).join().unwrap();
    let dead = DEAD.load(SeqCst);
    W.store(dead, SeqCst);
    let r = futex(&W, FUTEX_LOCK_PI | FUTEX_PRIVATE, 0, core::ptr::null(), 0);
    let after = W.load(SeqCst);
    let ok = r == -3 && after == dead;
    report("pi_dead_owner", ok, format!("dead tid {} lock={} (want -3) word_after={:#x}", dead, r, after))
}

fn pi_handoff() -> bool {
    let (ok, detail) = pi_handoff_round(50, false);
    report("pi_handoff", ok, detail)
}

/// `futextest pi_handoff_stress [rounds] [settle_ms] [bitonly] [hogs]`: repeat
/// the hand-off round with CPU hogs (default 2 per CPU) spinning alongside.
/// bitonly=1 uses the pre-fix "FUTEX_WAITERS is set" wait to show the race.
fn pi_handoff_stress() -> bool {
    let a: Vec<String> = std::env::args().collect();
    let arg = |i: usize, d: u64| a.get(i).and_then(|s| s.parse().ok()).unwrap_or(d);
    let rounds = arg(2, 200);
    let settle = arg(3, 0);
    let bit_only = arg(4, 0) != 0;
    let ncpu = thread::available_parallelism().map(|n| n.get() as u64).unwrap_or(2);
    let hogs = arg(5, 2 * ncpu);
    static STOP: AtomicU32 = AtomicU32::new(0);
    STOP.store(0, SeqCst);
    let hs: Vec<_> = (0..hogs).map(|_| thread::spawn(|| {
        let mut x = 0u64;
        while STOP.load(SeqCst) == 0 { for _ in 0..1000 { x = x.wrapping_mul(31).wrapping_add(7); } core::hint::black_box(x); }
    })).collect();
    let mut fails = 0;
    let mut first = String::new();
    for i in 0..rounds {
        let (ok, d) = pi_handoff_round(settle, bit_only);
        if !ok { fails += 1; if first.is_empty() { first = format!("round {}: {}", i, d); } println!("pi_handoff_stress round {} FAIL ({})", i, d); }
    }
    STOP.store(1, SeqCst);
    for h in hs { h.join().unwrap(); }
    report("pi_handoff_stress", fails == 0, format!("{}/{} rounds failed (settle={}ms bitonly={} hogs={}); first {}",
        fails, rounds, settle, bit_only, hogs, first))
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

/// L10.11: a shared-keyed FUTEX_WAKE must not cost a VMA scan. Map 1000
/// filler VMAs first, so the word's VMA sits at a high slot, then time
/// uncontended wakes with and without FUTEX_PRIVATE.
fn wake_cost() -> bool {
    const FILL: usize = 1000; const N: u32 = 100_000;
    let mut fill = Vec::new();
    for i in 0..FILL {   // alternate prot so neighbours cannot merge
        let p = unsafe { mmap(core::ptr::null_mut(), 4096, if i % 2 == 0 { 1 } else { PROT_RW },
                              MAP_PRIVATE | MAP_ANON, -1, 0) };
        if p as isize == -1 { break; }
        fill.push(p);
    }
    let a = map(4096, MAP_PRIVATE | MAP_ANON, -1);
    word(a).store(0, SeqCst);
    let time = |op: i64| { let t0 = Instant::now();
        for _ in 0..N { futex(word(a), op, 1, core::ptr::null(), 0); }
        t0.elapsed().as_nanos() as u64 / N as u64 };
    let _ = time(FUTEX_WAKE);                       // warm
    let shared = time(FUTEX_WAKE); let private = time(FUTEX_WAKE | FUTEX_PRIVATE);
    println!("wake_cost: vmas={} shared={}ns private={}ns", fill.len() + 1, shared, private);
    for p in fill { unsafe { munmap(p, 4096); } }
    unsafe { munmap(a, 4096); }
    report("wake_cost", shared <= private.saturating_mul(3).max(1), format!("shared {}ns > 3x private {}ns", shared, private))
}

/// L10.11: a slot reused at the same address must be keyed by its new
/// mapping, not by what the VMA lookup hint saw before.
/// `# playcost` row of /proc/rqhold: [widen_skip, widen_deref, fwake_empty, fwake_locked, fwait_tidy].
fn playcost_row() -> Option<[u64; 5]> {
    let t = std::fs::read_to_string("/proc/rqhold").ok()?;
    let l = t.lines().find(|l| l.starts_with("# playcost "))?;
    let v: Vec<u64> = l.split_whitespace().filter_map(|w| w.parse().ok()).collect();
    if v.len() == 5 { Some([v[0], v[1], v[2], v[3], v[4]]) } else { None }
}

/// Lane playcost guard: a FUTEX_WAKE with no waiter on its key must not take
/// RUN_QUEUE (the global TASKS lock): it counts as fwake_empty, not fwake_locked.
fn wake_empty_no_tasks() -> bool {
    const N: u64 = 2000;
    let a = map(4096, MAP_PRIVATE | MAP_ANON, -1);
    word(a).store(0, SeqCst);
    let before = playcost_row();
    for _ in 0..N { futex(word(a), FUTEX_WAKE | FUTEX_PRIVATE, 1, core::ptr::null(), 0); }
    let after = playcost_row();
    unsafe { munmap(a, 4096); }
    let (ok, detail) = match (before, after) {
        (Some(b), Some(x)) => {
            let (e, l) = (x[2] - b[2], x[3] - b[3]);
            (e >= N && l < N / 2, format!("fwake_empty +{} fwake_locked +{} for {} empty wakes", e, l, N))
        }
        _ => (false, "no # playcost row in /proc/rqhold".into()),
    };
    println!("wake_empty_no_tasks: {}", detail);
    report("wake_empty_no_tasks", ok, detail)
}

/// Lane playcost guard: a FUTEX_WAIT ended by a FUTEX_WAKE that found it
/// parked returns without a second RUN_QUEUE hold (counted as fwait_tidy).
fn wait_woken_single_tasks_hold() -> bool {
    const N: u32 = 400;
    static W: AtomicU32 = AtomicU32::new(0);
    W.store(0, SeqCst);
    let before = playcost_row();
    let h = thread::spawn(|| {
        for _ in 0..N {
            while W.load(SeqCst) == 0 { futex(&W, FUTEX_WAIT | FUTEX_PRIVATE, 0, core::ptr::null(), 0); }
            W.store(0, SeqCst);
        }
    });
    for _ in 0..N {
        for _ in 0..2000 { std::hint::spin_loop(); }
        W.store(1, SeqCst);
        futex(&W, FUTEX_WAKE | FUTEX_PRIVATE, 1, core::ptr::null(), 0);
        while W.load(SeqCst) != 0 { thread::yield_now(); }
    }
    let _ = h.join();
    let after = playcost_row();
    let (ok, detail) = match (before, after) {
        (Some(b), Some(x)) => {
            let (t, l) = (x[4] - b[4], x[3] - b[3]);
            (l > 0 && t * 2 >= l, format!("fwait_tidy +{} for {} locked wakes ({} rounds)", t, l, N))
        }
        _ => (false, "no 5-field # playcost row in /proc/rqhold".into()),
    };
    println!("wait_woken_single_tasks_hold: {}", detail);
    report("wait_woken_single_tasks_hold", ok, detail)
}

fn hint_remap_shared() -> bool {
    let a = map(4096, MAP_PRIVATE | MAP_ANON, -1);
    futex(word(a), FUTEX_WAKE, 1, core::ptr::null(), 0);        // warm the hint on a private VMA
    unsafe { munmap(a, 4096); }
    let fd = memfd();
    let b = unsafe { mmap(a, 4096, PROT_RW, MAP_SHARED | 0x10 /* MAP_FIXED */, fd, 0) };
    let c = map(4096, MAP_SHARED, fd);                           // second view of the same page
    word(b).store(0, SeqCst);
    let pid = unsafe { fork() };
    if pid == 0 {
        let r = futex(word(c), FUTEX_WAIT, 0, &rel(2000), 0);
        unsafe { _exit(if r == 0 { 0 } else { 1 }); }
    }
    let ok = wake_until(word(b), 1500) && wait_child(pid) == 0;
    report("hint_remap_shared", ok, "wake through the remapped address missed the shared waiter".into())
}

/// `# dlscan` row of /proc/rqhold: [pass, slots, full].
fn dlscan_row() -> Option<[u64; 3]> {
    let t = std::fs::read_to_string("/proc/rqhold").ok()?;
    let l = t.lines().find(|l| l.starts_with("# dlscan "))?;
    let v: Vec<u64> = l.split_whitespace().filter_map(|w| w.parse().ok()).collect();
    if v.len() == 3 { Some([v[0], v[1], v[2]]) } else { None }
}

/// Lane playcost3 guard: a poll-deadline pass that finds one timed waiter due
/// must not walk every parked task. 64 threads park in long timed futex
/// waits; 200 short timed waits then expire one at a time. The deadline index
/// keeps the slots examined per pass near the number due (was O(parked)).
fn deadline_scan_indexed() -> bool {
    const T: usize = 64;
    const N: u64 = 200;
    static FAR: AtomicU32 = AtomicU32::new(0);
    static PARKED: AtomicU32 = AtomicU32::new(0);
    FAR.store(0, SeqCst);
    PARKED.store(0, SeqCst);
    let hs: Vec<_> = (0..T).map(|_| thread::spawn(|| {
        PARKED.fetch_add(1, SeqCst);
        while FAR.load(SeqCst) == 0 {
            let ts = rel(30_000);
            futex(&FAR, FUTEX_WAIT | FUTEX_PRIVATE, 0, &ts, 0);
        }
    })).collect();
    while PARKED.load(SeqCst) < T as u32 { thread::yield_now(); }
    thread::sleep(std::time::Duration::from_millis(50));
    let short = AtomicU32::new(0);
    let before = dlscan_row();
    for _ in 0..N {
        let ts = rel(1);
        futex(&short, FUTEX_WAIT | FUTEX_PRIVATE, 0, &ts, 0);
    }
    let after = dlscan_row();
    FAR.store(1, SeqCst);
    futex(&FAR, FUTEX_WAKE | FUTEX_PRIVATE, i32::MAX as u32, core::ptr::null(), 0);
    for h in hs { let _ = h.join(); }
    let (ok, detail) = match (before, after) {
        (Some(b), Some(x)) => {
            let (p, s, f) = (x[0] - b[0], x[1] - b[1], x[2] - b[2]);
            let per = if p > 0 { s / p } else { u64::MAX };
            (p >= N / 2 && per <= 16,
             format!("{} passes, {} slots ({} per pass, {} full scans) with {} parked", p, s, per, f, T))
        }
        _ => (false, "no 3-field # dlscan row in /proc/rqhold".into()),
    };
    println!("deadline_scan_indexed: {}", detail);
    report("deadline_scan_indexed", ok, detail)
}

/// `# w4scan` row of /proc/rqhold: [calls, slots, fallback].
fn w4scan_row() -> Option<[u64; 3]> {
    let t = std::fs::read_to_string("/proc/rqhold").ok()?;
    let l = t.lines().find(|l| l.starts_with("# w4scan "))?;
    let v: Vec<u64> = l.split_whitespace().filter_map(|w| w.parse().ok()).collect();
    if v.len() == 3 { Some([v[0], v[1], v[2]]) } else { None }
}

/// Lane playcost3 guard: wait4 must not dereference every task on the run
/// queue. With one running child and 64 of the caller's own threads parked,
/// WNOHANG polls should examine about one slot each (was every live task).
fn wait4_scan_children_only() -> bool {
    const T: usize = 64;
    const N: u64 = 300;
    static GATE: AtomicU32 = AtomicU32::new(0);
    GATE.store(0, SeqCst);
    let child = unsafe { fork() };
    if child == 0 {
        thread::sleep(Duration::from_millis(1500));
        std::process::exit(0);
    }
    let hs: Vec<_> = (0..T).map(|_| thread::spawn(|| {
        while GATE.load(SeqCst) == 0 { futex(&GATE, FUTEX_WAIT | FUTEX_PRIVATE, 0, core::ptr::null(), 0); }
    })).collect();
    thread::sleep(Duration::from_millis(50));
    let before = w4scan_row();
    let mut running = 0;
    for _ in 0..N {
        let mut st = 0;
        if unsafe { waitpid(child, &mut st, 1) } == 0 { running += 1; }
    }
    let after = w4scan_row();
    GATE.store(1, SeqCst);
    futex(&GATE, FUTEX_WAKE | FUTEX_PRIVATE, i32::MAX as u32, core::ptr::null(), 0);
    for h in hs { let _ = h.join(); }
    let code = wait_child(child);
    let (ok, detail) = match (before, after) {
        (Some(b), Some(x)) => {
            let (c, s) = (x[0] - b[0], x[1] - b[1]);
            let per = if c > 0 { s / c } else { u64::MAX };
            (c >= N && per <= 4 && running >= N / 2 && code == 0,
             format!("{} scans, {} slots ({} per scan), child running {}/{} exit {}", c, s, per, running, N, code))
        }
        _ => (false, "no 3-field # w4scan row in /proc/rqhold".into()),
    };
    println!("wait4_scan_children_only: {}", detail);
    report("wait4_scan_children_only", ok, detail)
}

extern "C" {
    fn eventfd(init: u32, flags: i32) -> i32;
    fn epoll_create1(flags: i32) -> i32;
    fn epoll_ctl(ep: i32, op: i32, fd: i32, ev: *mut EpEvent) -> i32;
    fn epoll_wait(ep: i32, evs: *mut EpEvent, n: i32, ms: i32) -> i32;
    #[link_name = "write"] fn c_write(fd: i32, b: *const u8, n: usize) -> isize;
    #[link_name = "read"] fn c_read(fd: i32, b: *mut u8, n: usize) -> isize;
}

#[cfg_attr(target_arch = "x86_64", repr(C, packed))]
#[cfg_attr(not(target_arch = "x86_64"), repr(C))]
#[derive(Clone, Copy)]
struct EpEvent { events: u32, data: u64 }

extern "C" {
    fn timerfd_create(clock: i32, flags: i32) -> i32;
    fn timerfd_settime(fd: i32, flags: i32, new: *const [Timespec; 2], old: *mut [Timespec; 2]) -> i32;
}

/// Lane playcost5 load: the laptop playback pattern of many threads in short
/// timed epoll waits (1-4 ms), a pinger waking some early, and timerfds
/// expiring every millisecond in other threads' epoll sets. Reports the
/// `# dlsplit` breakdown of the deadline-pass slots; the guard bounds the
/// slots each timerfd pass walks (was every parked/listening slot of the
/// words whose summary mask matched).
fn deadline_short_waiters() -> bool {
    const T: usize = 48;
    const TF: usize = 4;
    static STOP: AtomicU32 = AtomicU32::new(0);
    STOP.store(0, SeqCst);
    let efds: Vec<i32> = (0..T).map(|_| unsafe { eventfd(0, 0x800) }).collect();
    let mut hs = Vec::new();
    for (n, &e) in efds.iter().enumerate() {
        hs.push(thread::spawn(move || unsafe {
            let ep = epoll_create1(0);
            let mut ev = EpEvent { events: 1, data: e as u64 };
            epoll_ctl(ep, 1, e, &mut ev);
            let mut out = [EpEvent { events: 0, data: 0 }; 4];
            let mut b = [0u8; 8];
            while STOP.load(SeqCst) == 0 {
                if epoll_wait(ep, out.as_mut_ptr(), 4, 1 + (n % 4) as i32) > 0 { c_read(e, b.as_mut_ptr(), 8); }
            }
            close(ep);
        }));
    }
    for _ in 0..TF {
        hs.push(thread::spawn(|| unsafe {
            let tf = timerfd_create(1, 0x800);
            let it = [rel(1), rel(1)];
            timerfd_settime(tf, 0, &it, core::ptr::null_mut());
            let ep = epoll_create1(0);
            let mut ev = EpEvent { events: 1, data: tf as u64 };
            epoll_ctl(ep, 1, tf, &mut ev);
            let mut out = [EpEvent { events: 0, data: 0 }; 4];
            let mut b = [0u8; 8];
            while STOP.load(SeqCst) == 0 {
                if epoll_wait(ep, out.as_mut_ptr(), 4, 100) > 0 { c_read(tf, b.as_mut_ptr(), 8); }
            }
            close(ep); close(tf);
        }));
    }
    thread::sleep(Duration::from_millis(200));
    let (b0, s0) = (dlscan_row(), rq_row("# dlsplit "));
    let one = 1u64.to_ne_bytes();
    let t0 = std::time::Instant::now();
    let mut k = 0;
    while t0.elapsed() < Duration::from_millis(1500) {
        unsafe { c_write(efds[k % T], one.as_ptr(), 8); }
        k += 7;
        thread::sleep(Duration::from_micros(500));
    }
    let (b1, s1) = (dlscan_row(), rq_row("# dlsplit "));
    STOP.store(1, SeqCst);
    for h in hs { let _ = h.join(); }
    for e in efds { unsafe { close(e); } }
    let (ok, detail) = match (b0, b1, s0, s1) {
        (Some(b), Some(x), Some(sa), Some(sb)) if sa.len() >= 6 && sb.len() == sa.len() => {
            let (p, sl) = (x[0] - b[0], x[1] - b[1]);
            let d: Vec<u64> = (0..sa.len()).map(|i| sb[i] - sa[i]).collect();
            let tper = if d[3] > 0 { d[4] / d[3] } else { 0 };
            let miss = d.get(6).copied().unwrap_or(0);
            (p > 100 && tper <= 8 && miss == 0,
             format!("{} passes {} slots ({}/pass): due {} stale {} repush {} tfdpass {} tfdslots {} ({}/tfdpass) drop {} miss {}",
                     p, sl, if p > 0 { sl / p } else { 0 }, d[0], d[1], d[2], d[3], d[4], tper, d[5], miss))
        }
        _ => (false, "no # dlscan / # dlsplit row in /proc/rqhold".into()),
    };
    println!("deadline_short_waiters: {}", detail);
    report("deadline_short_waiters", ok, detail)
}

/// A 4-field `# kwake` row or 3-field `# widen` row of /proc/rqhold.
fn rq_row(tag: &str) -> Option<Vec<u64>> {
    let t = std::fs::read_to_string("/proc/rqhold").ok()?;
    let l = t.lines().find(|l| l.starts_with(tag))?;
    Some(l.split_whitespace().filter_map(|w| w.parse().ok()).collect())
}

/// Lane playcost4 guard: a keyed poll wake (eventfd write/read) and an
/// epoll_ctl widening must visit only the parkers that can hold the key, not
/// every parker whose mask shares the key's 1-of-64 bloom bit (keyed wake)
/// or every parked task (widen). 32 threads park in epoll_wait on 16 idle
/// eventfds each (their masks cover ~14 bloom bits apiece).
fn kwake_scan_indexed() -> bool {
    const T: usize = 32;
    const K: usize = 16;
    const N: u64 = 300;
    static PARKED: AtomicU32 = AtomicU32::new(0);
    PARKED.store(0, SeqCst);
    let stop = unsafe { eventfd(0, 0x800) };
    let (e, ep2) = unsafe { (eventfd(0, 0x800), epoll_create1(0)) };
    let one = 1u64.to_ne_bytes();
    let mut buf = [0u8; 8];
    let mut measure = || {
        let kb = rq_row("# kwake ");
        for _ in 0..N { unsafe { c_write(e, one.as_ptr(), 8); c_read(e, buf.as_mut_ptr(), 8); } }
        let ka = rq_row("# kwake ");
        let wb = rq_row("# widen ");
        for _ in 0..N {
            let mut ev = EpEvent { events: 1, data: 0 };
            unsafe { epoll_ctl(ep2, 1, e, &mut ev); epoll_ctl(ep2, 2, e, &mut ev); }
        }
        (kb, ka, wb, rq_row("# widen "))
    };
    let rates = |m: &(Option<Vec<u64>>, Option<Vec<u64>>, Option<Vec<u64>>, Option<Vec<u64>>)| -> Option<(u64, f64, u64, u64, f64, u64, u64)> {
        let (b, a, wb, wa) = (m.0.as_ref()?, m.1.as_ref()?, m.2.as_ref()?, m.3.as_ref()?);
        if b.len() != 4 || a.len() != 4 || wb.len() != 3 || wa.len() != 3 { return None; }
        let (c, sl, d, f) = (a[0] - b[0], a[1] - b[1], a[2] - b[2], a[3] - b[3]);
        let (wc, ws, wf) = (wa[0] - wb[0], wa[1] - wb[1], wa[2] - wb[2]);
        let kp = if c > 0 { sl as f64 / c as f64 } else { f64::MAX };
        let wp = if wc > 0 { ws as f64 / wc as f64 } else { 0.0 };
        Some((c, kp, d, f, wp, wc, wf))
    };
    // The kernel counters are system-wide: other processes' parkers (broadcast
    // masks, bloom collisions) add slots per wake. Measure that background
    // first with none of our own parkers, and assert only what the 32x16
    // parkers ADD on top of it.
    let base = rates(&measure());
    let hs: Vec<_> = (0..T).map(|_| thread::spawn(move || unsafe {
        let ep = epoll_create1(0);
        let mut fds = vec![stop];
        for _ in 0..K { fds.push(eventfd(0, 0x800)); }
        for &f in &fds {
            let mut ev = EpEvent { events: 1, data: f as u64 };
            epoll_ctl(ep, 1, f, &mut ev);
        }
        PARKED.fetch_add(1, SeqCst);
        let mut out = [EpEvent { events: 0, data: 0 }; 4];
        epoll_wait(ep, out.as_mut_ptr(), 4, 30_000);
        for &f in &fds[1..] { close(f); }
        close(ep);
    })).collect();
    while PARKED.load(SeqCst) < T as u32 { thread::yield_now(); }
    thread::sleep(Duration::from_millis(100));
    let m = measure();
    unsafe { c_write(stop, one.as_ptr(), 8); }
    for h in hs { let _ = h.join(); }
    unsafe { close(e); close(ep2); close(stop); }
    let (bkp, bwp) = base.map(|b| (b.1, b.4)).unwrap_or((0.0, 0.0));
    let (bkp, bwp) = (if bkp == f64::MAX { 0.0 } else { bkp }, bwp);
    let (ok, detail) = match rates(&m) {
        Some((c, kp, d, f, wp, wc, wf)) => {
            (c >= N && kp - bkp <= 2.0 && f == 0 && wp - bwp <= 2.0 && wf == 0,
             format!("{} keyed wakes, {:.2} slots/wake (background {:.2}; {} deref, {} unindexed); {} widens, {:.2} slots/widen (background {:.2}; {} unindexed); {}x{} parked",
                     c, kp, bkp, d, f, wc, wp, bwp, wf, T, K))
        }
        None => (false, "no # kwake / # widen row in /proc/rqhold".into()),
    };
    println!("kwake_scan_indexed: {}", detail);
    report("kwake_scan_indexed", ok, detail)
}

extern "C" {
    #[link_name = "poll"] fn c_poll(fds: *mut PollFd, n: u64, ms: i32) -> i32;
    fn pipe2(fds: *mut i32, flags: i32) -> i32;
    fn socketpair(domain: i32, ty: i32, proto: i32, sv: *mut i32) -> i32;
}

#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd { fd: i32, events: i16, revents: i16 }

/// One side of a `poll_lost_wake_stress` pair: waits on `rfd` (through epoll
/// `ep` when >= 0, else poll()), writes the token to `wfd`.
#[derive(Clone, Copy)]
struct PwSide { rfd: i32, wfd: i32, ep: i32 }

/// Lost-wake guard for keyed poll wakes: ping-pong pairs over eventfds
/// (epoll), pipes (poll) and unix sockets (both), so the reply often lands
/// while the waiter is between its readiness probe and its park/listen
/// install - the window any wake-side shortcut must not lose a wake in. A wait that
/// times out (2 s) or sleeps >= 1 s in a pair that answers within
/// microseconds lost (or was delayed past) its wake.
fn poll_lost_wake_stress() -> bool {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    const N: u64 = 16000;
    let done = Arc::new(AtomicBool::new(false));
    let lost = Arc::new(AtomicU64::new(0));
    let trips = Arc::new(AtomicU64::new(0));
    let mut pairs: Vec<(PwSide, PwSide)> = Vec::new();
    let mut fds: Vec<i32> = Vec::new();
    unsafe {
        let ep_on = |f: i32| { let ep = epoll_create1(0); let mut ev = EpEvent { events: 1, data: f as u64 };
                               epoll_ctl(ep, 1, f, &mut ev); ep };
        for _ in 0..3 { // eventfd + epoll
            let (a, b) = (eventfd(0, 0x800), eventfd(0, 0x800));
            let (ea, eb) = (ep_on(a), ep_on(b));
            fds.extend([a, b, ea, eb]);
            pairs.push((PwSide { rfd: a, wfd: b, ep: ea }, PwSide { rfd: b, wfd: a, ep: eb }));
        }
        for _ in 0..3 { // pipes + poll
            let (mut p1, mut p2) = ([0i32; 2], [0i32; 2]);
            if pipe2(p1.as_mut_ptr(), 0x800) != 0 || pipe2(p2.as_mut_ptr(), 0x800) != 0 {
                return report("poll_lost_wake_stress", false, "pipe2 failed".into());
            }
            fds.extend(p1); fds.extend(p2);
            pairs.push((PwSide { rfd: p1[0], wfd: p2[1], ep: -1 }, PwSide { rfd: p2[0], wfd: p1[1], ep: -1 }));
        }
        for i in 0..2 { // unix stream sockets: poll on one end, epoll on the other
            let mut sv = [0i32; 2];
            if socketpair(1, 1 | 0x800, 0, sv.as_mut_ptr()) != 0 {
                return report("poll_lost_wake_stress", false, "socketpair failed".into());
            }
            let e1 = ep_on(sv[1]);
            fds.extend([sv[0], sv[1], e1]);
            let _ = i;
            pairs.push((PwSide { rfd: sv[0], wfd: sv[0], ep: -1 }, PwSide { rfd: sv[1], wfd: sv[1], ep: e1 }));
        }
    }
    let t0 = Instant::now();
    let mut hs = Vec::new();
    for &(a, b) in &pairs {
        for (side, init) in [(a, true), (b, false)] {
            let (done, lost, trips) = (done.clone(), lost.clone(), trips.clone());
            hs.push(thread::spawn(move || unsafe {
                let tok = 1u64.to_ne_bytes();
                let mut buf = [0u8; 64];
                for _ in 0..N {
                    if init { c_write(side.wfd, tok.as_ptr(), 8); }
                    loop {
                        if done.load(SeqCst) { return; }
                        let w0 = Instant::now();
                        let r = if side.ep >= 0 {
                            let mut out = [EpEvent { events: 0, data: 0 }; 2];
                            epoll_wait(side.ep, out.as_mut_ptr(), 2, 2000)
                        } else {
                            let mut pf = PollFd { fd: side.rfd, events: 1, revents: 0 };
                            c_poll(&mut pf, 1, 2000)
                        };
                        if r == 0 || w0.elapsed().as_millis() >= 1000 {
                            lost.fetch_add(1, SeqCst);
                            done.store(true, SeqCst);
                            return;
                        }
                        if r > 0 && c_read(side.rfd, buf.as_mut_ptr(), 64) > 0 { break; }
                    }
                    if !init { c_write(side.wfd, tok.as_ptr(), 8); } else { trips.fetch_add(1, SeqCst); }
                }
            }));
        }
    }
    let mut hung = false;
    for h in hs { if !join_deadline(h, t0, 120) { hung = true; } }
    for f in fds { unsafe { close(f); } }
    let (l, t) = (lost.load(SeqCst), trips.load(SeqCst));
    let want = N * pairs.len() as u64;
    let detail = format!("{} pairs, round trips {}/{}, lost {}, hung {}, {} ms", pairs.len(), t, want, l, hung,
                         t0.elapsed().as_millis());
    println!("poll_lost_wake_stress: {}", detail);
    report("poll_lost_wake_stress", l == 0 && !hung && t == want, detail)
}


const FUTEX_CMP_REQUEUE: i64 = 4;

/// Full futex call with uaddr2 and val3. `ts` is passed as a raw integer so
/// requeue can put val2 in the timeout slot.
fn futex6(w: *const AtomicU32, op: i64, val: u32, ts: usize, w2: *const AtomicU32, val3: u32) -> i64 {
    let r = unsafe { syscall(nr::FUTEX, w, op, val as u64, ts, w2, val3 as u64) };
    if r < 0 { -(errno() as i64) } else { r }
}

/// /proc/rqhold futex.rs rows `file:line holds ns_held max_ns ns_spun`
/// as (line, holds, ns_held, ns_spun), plus the `# lock TASKS` row as
/// (holds, ns_held, ns_spun).
fn futex_rq_rows() -> Option<(Vec<(String, u64, u64, u64)>, (u64, u64, u64))> {
    let t = std::fs::read_to_string("/proc/rqhold").ok()?;
    let mut rows = Vec::new();
    let mut tasks = None;
    for l in t.lines() {
        let f: Vec<&str> = l.split_whitespace().collect();
        if l.starts_with("# lock TASKS") && tasks.is_none() {
            let g = |k: &str| f.iter().position(|w| *w == k).and_then(|i| f.get(i + 1)).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
            tasks = Some((g("holds"), g("ns_held"), g("ns_spun")));
        } else if !l.starts_with('#') && f.len() >= 5 {
            let (file, _) = match f[0].rsplit_once(':') { Some(x) => x, None => continue };
            if !file.ends_with("futex.rs") { continue; }
            let p = |i: usize| f[i].parse::<u64>().unwrap_or(0);
            rows.push((f[0].to_string(), p(1), p(2), p(4)));
        }
    }
    Some((rows, tasks?))
}

/// Join with a deadline; false if the thread did not finish in `secs`.
fn join_deadline(h: thread::JoinHandle<()>, t0: Instant, secs: u64) -> bool {
    while !h.is_finished() {
        if t0.elapsed() > Duration::from_secs(secs) { return false; }
        thread::sleep(Duration::from_millis(5));
    }
    let _ = h.join();
    true
}

/// Microbenchmark: how many RUN_QUEUE (TASKS) holds does a futex wait/wake
/// ping-pong cost per futex syscall.
fn pingpong_holds() -> bool {
    const N: u64 = 20000;
    static WA: AtomicU32 = AtomicU32::new(0);
    static WB: AtomicU32 = AtomicU32::new(0);
    static OPS: AtomicU64 = AtomicU64::new(0);
    WA.store(0, SeqCst);
    WB.store(0, SeqCst);
    OPS.store(0, SeqCst);
    let before = futex_rq_rows();
    let t0 = Instant::now();
    let ha = thread::spawn(|| {
        let mut ops = 0u64;
        for _ in 0..N {
            WB.store(1, SeqCst);
            futex(&WB, FUTEX_WAKE | FUTEX_PRIVATE, 1, core::ptr::null(), 0);
            ops += 1;
            while WA.load(SeqCst) == 0 {
                futex(&WA, FUTEX_WAIT | FUTEX_PRIVATE, 0, core::ptr::null(), 0);
                ops += 1;
            }
            WA.store(0, SeqCst);
        }
        OPS.fetch_add(ops, SeqCst);
    });
    let hb = thread::spawn(|| {
        let mut ops = 0u64;
        for _ in 0..N {
            while WB.load(SeqCst) == 0 {
                futex(&WB, FUTEX_WAIT | FUTEX_PRIVATE, 0, core::ptr::null(), 0);
                ops += 1;
            }
            WB.store(0, SeqCst);
            WA.store(1, SeqCst);
            futex(&WA, FUTEX_WAKE | FUTEX_PRIVATE, 1, core::ptr::null(), 0);
            ops += 1;
        }
        OPS.fetch_add(ops, SeqCst);
    });
    let oka = join_deadline(ha, t0, 30);
    let okb = oka && join_deadline(hb, t0, 30);
    let ns = t0.elapsed().as_nanos() as u64;
    if !(oka && okb) {
        println!("pingpong_holds: HANG after {} s", t0.elapsed().as_secs());
        return report("pingpong_holds", false, "thread hung >30 s".into());
    }
    let after = futex_rq_rows();
    let ops = OPS.load(SeqCst).max(1);
    match (before, after) {
        (Some((br, bt)), Some((ar, at))) => {
            let delta = |r: &Vec<(String, u64, u64, u64)>, k: &str| r.iter().find(|x| x.0 == k).map(|x| (x.1, x.2, x.3)).unwrap_or((0, 0, 0));
            let (mut h, mut nh, mut sp) = (0u64, 0u64, 0u64);
            let mut sites = Vec::new();
            for a in &ar {
                let b = delta(&br, &a.0);
                let (dh, dn, ds) = (a.1.saturating_sub(b.0), a.2.saturating_sub(b.1), a.3.saturating_sub(b.2));
                h += dh; nh += dn; sp += ds;
                if dh > 0 { sites.push(format!("  site {} +{} +{}", a.0, dh, dn)); }
            }
            let th = at.0.saturating_sub(bt.0);
            println!("pingpong_holds: rounds={} futex_ops={} futex_rq_holds={} holds_per_op={:.2} futex_rq_ns_per_hold={} futex_rq_spun_ns={} tasks_holds_per_round={:.2} ns_per_round={}",
                     N, ops, h, h as f64 / ops as f64, if h > 0 { nh / h } else { 0 }, sp, th as f64 / N as f64, ns / N);
            for s in sites { println!("{}", s); }
        }
        _ => println!("pingpong_holds: rounds={} futex_ops={} (no /proc/rqhold futex rows) ns_per_round={}", N, ops, ns / N),
    }
    report("pingpong_holds", true, String::new())
}

fn xorshift(s: &mut u64) -> u64 {
    *s ^= *s << 13; *s ^= *s >> 7; *s ^= *s << 17; *s
}

/// Correctness guard: a wake the kernel counted must be seen by exactly one
/// waiter (A); ring hand-offs with long timeouts never lose a wake (B);
/// CMP_REQUEUE moves waiters that later wake one by one (C).
fn futex_lost_wake_stress() -> bool {
    use std::sync::Arc;
    use std::sync::atomic::AtomicBool;
    let mut fail = Vec::new();

    // Phase A
    static W: AtomicU32 = AtomicU32::new(0);
    static CLAIMED: AtomicU64 = AtomicU64::new(0);
    static WOKEN: AtomicU64 = AtomicU64::new(0);
    static ALIVE: AtomicU32 = AtomicU32::new(0);
    static STOP: AtomicBool = AtomicBool::new(false);
    W.store(0, SeqCst);
    CLAIMED.store(0, SeqCst);
    WOKEN.store(0, SeqCst);
    ALIVE.store(0, SeqCst);
    STOP.store(false, SeqCst);
    let mut hs = Vec::new();
    for i in 0..8u64 {
        ALIVE.fetch_add(1, SeqCst);
        hs.push(thread::spawn(move || {
            let mut s = 0x9e3779b97f4a7c15u64 ^ (i + 1).wrapping_mul(0x1234567);
            let mut woken = 0u64;
            while !STOP.load(SeqCst) {
                let us = 200 + xorshift(&mut s) % 2800;
                let ts = Timespec { tv_sec: 0, tv_nsec: (us * 1000) as i64 };
                if futex(&W, FUTEX_WAIT | FUTEX_PRIVATE, 0, &ts, 0) == 0 { woken += 1; }
            }
            WOKEN.fetch_add(woken, SeqCst);
            ALIVE.fetch_sub(1, SeqCst);
        }));
    }
    let mut wk = Vec::new();
    for i in 0..2u64 {
        wk.push(thread::spawn(move || {
            let mut s = 0xdeadbeefcafef00du64 ^ (i + 1).wrapping_mul(0x777);
            let mut c = 0u64;
            while !STOP.load(SeqCst) {
                let n = match xorshift(&mut s) % 3 { 0 => 1, 1 => 2, _ => i32::MAX as u32 };
                let r = futex(&W, FUTEX_WAKE | FUTEX_PRIVATE, n, core::ptr::null(), 0);
                if r > 0 { c += r as u64; }
                thread::sleep(Duration::from_micros(100 + xorshift(&mut s) % 400));
            }
            CLAIMED.fetch_add(c, SeqCst);
        }));
    }
    thread::sleep(Duration::from_millis(1500));
    STOP.store(true, SeqCst);
    for h in wk { let _ = h.join(); }
    let t0 = Instant::now();
    while ALIVE.load(SeqCst) > 0 && t0.elapsed() < Duration::from_secs(10) {
        let r = futex(&W, FUTEX_WAKE | FUTEX_PRIVATE, i32::MAX as u32, core::ptr::null(), 0);
        if r > 0 { CLAIMED.fetch_add(r as u64, SeqCst); }
        thread::sleep(Duration::from_millis(1));
    }
    for h in hs { let _ = h.join(); }
    let (claimed, woken) = (CLAIMED.load(SeqCst), WOKEN.load(SeqCst));
    if claimed != woken { fail.push(format!("A claimed={} woken={}", claimed, woken)); }

    // Phase B
    const RING: usize = 6;
    const TOTAL: u64 = 3000;
    let words: Arc<Vec<AtomicU32>> = Arc::new((0..RING).map(|_| AtomicU32::new(0)).collect());
    let done = Arc::new(AtomicBool::new(false));
    let hand = Arc::new(AtomicU64::new(0));
    let lost = Arc::new(AtomicU64::new(0));
    words[0].store(1, SeqCst);
    let tb = Instant::now();
    let mut rs = Vec::new();
    for i in 0..RING {
        let (words, done, hand, lost) = (words.clone(), done.clone(), hand.clone(), lost.clone());
        rs.push(thread::spawn(move || {
            let ts = rel(2000);
            loop {
                while words[i].load(SeqCst) == 0 {
                    if done.load(SeqCst) { return; }
                    // A wait that times out, or sleeps >= 1 s in a ring that
                    // hands the token on within microseconds, slept through
                    // (or was delayed past) its wake.
                    let t0 = Instant::now();
                    let r = futex(&words[i], FUTEX_WAIT | FUTEX_PRIVATE, 0, &ts, 0);
                    if r == -(ETIMEDOUT as i64) || t0.elapsed().as_millis() >= 1000 {
                        lost.fetch_add(1, SeqCst);
                        done.store(true, SeqCst);
                        return;
                    }
                }
                words[i].store(0, SeqCst);
                if hand.fetch_add(1, SeqCst) + 1 >= TOTAL {
                    done.store(true, SeqCst);
                    for w in words.iter() {
                        w.store(1, SeqCst);
                        futex(w, FUTEX_WAKE | FUTEX_PRIVATE, 1, core::ptr::null(), 0);
                    }
                    return;
                }
                let n = (i + 1) % RING;
                words[n].store(1, SeqCst);
                futex(&words[n], FUTEX_WAKE | FUTEX_PRIVATE, 1, core::ptr::null(), 0);
            }
        }));
    }
    let mut b_hung = false;
    for h in rs {
        if !join_deadline(h, tb, 30) { b_hung = true; break; }
    }
    let (handoffs, lost_n) = (hand.load(SeqCst), lost.load(SeqCst));
    if lost_n > 0 || b_hung || handoffs < TOTAL {
        fail.push(format!("B handoffs={} lost={} hung={}", handoffs, lost_n, b_hung));
    }

    // Phase C
    static C1: AtomicU32 = AtomicU32::new(0);
    static C2: AtomicU32 = AtomicU32::new(0);
    static CRES: AtomicU32 = AtomicU32::new(0); // waiters returning 0
    static CBAD: AtomicU32 = AtomicU32::new(0);
    C1.store(0, SeqCst);
    C2.store(0, SeqCst);
    CRES.store(0, SeqCst);
    CBAD.store(0, SeqCst);
    let mut cs = Vec::new();
    for _ in 0..8 {
        cs.push(thread::spawn(|| {
            let ts = rel(5000);
            if futex(&C1, FUTEX_WAIT | FUTEX_PRIVATE, 0, &ts, 0) == 0 { CRES.fetch_add(1, SeqCst); } else { CBAD.fetch_add(1, SeqCst); }
        }));
    }
    thread::sleep(Duration::from_millis(300));
    let rq = futex6(&C1, FUTEX_CMP_REQUEUE | FUTEX_PRIVATE, 1, i32::MAX as usize, &C2, 0);
    let mut c_woken: i64 = if rq >= 1 { 1 } else { 0 };
    let tc = Instant::now();
    while c_woken < 8 && tc.elapsed() < Duration::from_secs(3) {
        let r = futex(&C2, FUTEX_WAKE | FUTEX_PRIVATE, 1, core::ptr::null(), 0);
        if r > 0 { c_woken += r; }
        thread::sleep(Duration::from_millis(1));
    }
    let tcj = Instant::now();
    for h in cs { if !join_deadline(h, tcj, 10) { fail.push("C waiter hung".into()); break; } }
    let c_ret = CRES.load(SeqCst);
    if c_woken != 8 || c_ret != 8 || CBAD.load(SeqCst) != 0 || rq < 0 {
        fail.push(format!("C requeue_ret={} woken={} ret0={} bad={}", rq, c_woken, c_ret, CBAD.load(SeqCst)));
    }

    println!("futex_lost_wake_stress: A claimed={} woken={} B handoffs={} lost={} C woken={}/8", claimed, woken, handoffs, lost_n, c_ret);
    report("futex_lost_wake_stress", fail.is_empty(), fail.join("; "))
}

fn main() {
    let only = std::env::args().nth(1);
    let tests: &[(&str, fn() -> bool)] = &[
        ("shared_memfd_two_maps", shared_memfd_two_maps),
        ("shared_cross_process", shared_cross_process),
        ("shared_anon_fork", shared_anon_fork),
        ("private_not_shared", private_not_shared),
        ("wake_cost", wake_cost),
        ("hint_remap_shared", hint_remap_shared),
        ("wake_empty_no_tasks", wake_empty_no_tasks),
        ("wait_woken_single_tasks_hold", wait_woken_single_tasks_hold),
        ("deadline_scan_indexed", deadline_scan_indexed),
        ("wait4_scan_children_only", wait4_scan_children_only),
        ("kwake_scan_indexed", kwake_scan_indexed),
        ("deadline_short_waiters", deadline_short_waiters),
        ("pingpong_holds", pingpong_holds),
        ("futex_lost_wake_stress", futex_lost_wake_stress),
        ("poll_lost_wake_stress", poll_lost_wake_stress),
        ("robust_exit", || robust_process("robust_exit", Death::Exit, false)),
        ("robust_exit_waited", || robust_process("robust_exit_waited", Death::ExitWhileWaited, false)),
        ("robust_killed", || robust_process("robust_killed", Death::Killed, false)),
        ("robust_group_exit", || robust_process("robust_group_exit", Death::GroupExitFromThread, false)),
        ("robust_thread_exit", robust_thread_exit),
        ("robust_pi_exit", || robust_process("robust_pi_exit", Death::ExitWhileWaited, true)),
        ("pi_handoff", pi_handoff),
        ("pi_dead_owner", pi_dead_owner),
        ("realtime_restart", realtime_restart),
    ];
    let mut failures = 0;
    if only.as_deref() == Some("pi_handoff_stress") {
        let f = pi_handoff_stress();
        println!("--- futextest done --- failures={}", !f as i32);
        unsafe { _exit(!f as i32); }
    }
    for (name, f) in tests {
        if only.as_deref().map_or(false, |o| o != *name) { continue; }
        if !f() { failures += 1; }
    }
    println!("--- futextest done --- failures={}", failures);
    unsafe { _exit(failures); }
}
