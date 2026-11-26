//! POSIX clock/timer ABI cases (lane posixtime): timer_create's clock id,
//! timer_settime's TIMER_ABSTIME, CPU-time timers (ITIMER_VIRTUAL/PROF,
//! CLOCK_*_CPUTIME_ID), times(2), load averages, the musl-style SIGEV_THREAD
//! notify thread (SIGEV_THREAD_ID), and wall-clock waiters following a clock
//! step. Every expectation was checked against Linux 7.2 (x86_64).

use super::*;

#[cfg(target_arch = "x86_64")]
mod pnr {
    pub const TIMER_CREATE: i64 = 222;
    pub const TIMER_DELETE: i64 = 226;
    pub const RT_SIGTIMEDWAIT: i64 = 128;
    pub const TIMES: i64 = 100;
    pub const SYSINFO: i64 = 99;
    pub const GETEUID: i64 = 107;
}
#[cfg(target_arch = "aarch64")]
mod pnr {
    pub const TIMER_CREATE: i64 = 107;
    pub const TIMER_DELETE: i64 = 111;
    pub const RT_SIGTIMEDWAIT: i64 = 137;
    pub const TIMES: i64 = 153;
    pub const SYSINFO: i64 = 179;
    pub const GETEUID: i64 = 175;
}

const TIMER_ABSTIME: c_int = 1;
const CLOCK_PROCESS_CPUTIME_ID: clockid_t = 2;
const CLOCK_THREAD_CPUTIME_ID: clockid_t = 3;
const ITIMER_VIRTUAL: c_int = 1;
const ITIMER_PROF: c_int = 2;
const SIGVTALRM: c_int = 26;
const SIGPROF: c_int = 27;
const EINVAL: c_int = 22;
const EOPNOTSUPP: c_int = 95;
const EPERM: c_int = 1;
const SIGEV_NOTIFY_THREAD: c_int = 2;
const SIG_ABS: c_int = 44;     // RT signals used only by this file
const SIG_CPU: c_int = 45;
const SIG_HELPER: c_int = 46;
const SIG_STEP: c_int = 47;

unsafe fn errno() -> c_int { *__errno_location() }

unsafe fn raw_timer_create(clk: c_long, ev: *const sigevent, id: *mut c_int) -> c_long {
    syscall(pnr::TIMER_CREATE, clk, ev as c_long, id as c_long)
}

fn ev_signal(signo: c_int, value: usize) -> sigevent {
    sigevent {
        sigev_value: sigval { sival_ptr: value as *mut c_void },
        sigev_signo: signo,
        sigev_notify: SIGEV_SIGNAL,
        sigev_notify_thread_id: 0,
        __unused1: [0; 11],
    }
}

/// sigtimedwait on one signal, `ms` timeout: the signal number or -1.
unsafe fn wait_sig(sig: c_int, ms: i64, info: &mut [u8; 128]) -> c_int {
    let set = bit(sig);
    let ts = ts_from_ns(ms * 1_000_000);
    syscall(pnr::RT_SIGTIMEDWAIT, &set as *const sigset_t as c_long, info.as_mut_ptr() as c_long,
            &ts as *const timespec as c_long, 8 as c_long) as c_int
}

unsafe fn cpu_ns(clk: clockid_t) -> i64 {
    let mut ts = core::mem::zeroed::<timespec>();
    clock_gettime(clk, &mut ts);
    ts_to_ns(&ts)
}

/// Burn `ms` of this process's CPU time (bounded at 5 s of wall time).
unsafe fn burn_cpu_ms(ms: i64) {
    let c0 = cpu_ns(CLOCK_PROCESS_CPUTIME_ID);
    let t0 = now_ns();
    let mut x = 0u64;
    while cpu_ns(CLOCK_PROCESS_CPUTIME_ID) - c0 < ms * 1_000_000 && now_ns() - t0 < 5_000_000_000 {
        for _ in 0..1000 {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            core::hint::black_box(x);
        }
    }
}

unsafe fn pending_has(sig: c_int) -> bool {
    extern "C" { fn sigpending(set: *mut sigset_t) -> c_int; }
    let mut p: sigset_t = 0;
    sigpending(&mut p);
    p & bit(sig) != 0
}

// ── timer_create's clock id ────────────────────────────────────────────────
//
// It used to be ignored (every id worked, every timer ran on the monotonic
// clock). Linux: REALTIME/MONOTONIC/both CPU clocks/BOOTTIME/TAI arm;
// MONOTONIC_RAW and the COARSE clocks have no timers (EOPNOTSUPP); the
// _ALARM clocks need CAP_WAKE_ALARM (EPERM otherwise); anything else EINVAL.
// timer_delete of a dead id is EINVAL.
pub unsafe fn test_timer_create_clockids() -> bool {
    let name = b"timer_create_clockids\0";
    let root = syscall(pnr::GETEUID) == 0;
    let ev = ev_signal(SIG_ABS, 0);
    let mut ok = true;
    let cases: [(c_long, c_int); 14] = [
        (0, 0), (1, 0), (2, 0), (3, 0), (7, 0), (11, 0),
        (4, EOPNOTSUPP), (5, EOPNOTSUPP), (6, EOPNOTSUPP),
        (8, if root { 0 } else { EPERM }), (9, if root { 0 } else { EPERM }),
        (12, EINVAL), (99, EINVAL), (-1, EINVAL),
    ];
    for &(clk, want) in cases.iter() {
        let mut id: c_int = 0;
        let r = raw_timer_create(clk, &ev, &mut id);
        let got = if r == 0 { 0 } else { (-r) as c_int }; // raw syscall(): -errno
        if r == 0 { syscall(pnr::TIMER_DELETE, id as c_long); }
        if got != want {
            print_kv(b"  clockid_mismatch_clk=\0", clk as u64);
            print_kv(b"  clockid_mismatch_errno=\0", got as u64);
            ok = false;
        }
    }
    let mut id: c_int = 0;
    let created = raw_timer_create(1, &ev, &mut id) == 0;
    let del1 = syscall(pnr::TIMER_DELETE, id as c_long) == 0;
    let del2 = syscall(pnr::TIMER_DELETE, id as c_long) == -(EINVAL as c_long);
    report(name, ok && created && del1 && del2)
}

// ── TIMER_ABSTIME ──────────────────────────────────────────────────────────
//
// timer_settime ignored the flag, so an absolute instant was armed as a
// relative interval of that many seconds (decades for CLOCK_REALTIME, the
// uptime for CLOCK_MONOTONIC). Now: an instant 30 ms ahead fires at 30 ms on
// either clock, one in the past fires at once, and timer_gettime reports the
// remaining relative time.
pub unsafe fn test_timer_settime_abstime() -> bool {
    let name = b"timer_settime_abstime\0";
    let mut old: sigset_t = 0;
    let blk = bit(SIG_ABS);
    sigprocmask(SIG_BLOCK, &blk, &mut old);
    let mut ok = true;
    for &(clk, past) in [(CLOCK_MONOTONIC, false), (CLOCK_REALTIME, false), (CLOCK_REALTIME, true)].iter() {
        let mut ev = ev_signal(SIG_ABS, 0x77);
        let mut tid: timer_t = core::ptr::null_mut();
        if timer_create(clk, &mut ev, &mut tid) != 0 { ok = false; continue; }
        let now = if clk == CLOCK_REALTIME { realtime_ns() } else { now_ns() };
        let at = if past { 1_000_000_000 } else { now + 30_000_000 };
        let spec = itimerspec { it_interval: ts_from_ns(0), it_value: ts_from_ns(at) };
        let t0 = now_ns();
        let set = timer_settime(tid, TIMER_ABSTIME, &spec, core::ptr::null_mut()) == 0;
        let mut cur = core::mem::zeroed::<itimerspec>();
        timer_gettime(tid, &mut cur);
        let rem = ts_to_ns(&cur.it_value);
        let mut info = [0u8; 128];
        let r = wait_sig(SIG_ABS, 2000, &mut info);
        let el = now_ns() - t0;
        timer_delete(tid);
        let case = set && r == SIG_ABS && if past {
            el < 100_000_000
        } else {
            el >= 29_000_000 && el < 30_000_000 + LATE_BOUND_NS && rem > 0 && rem <= 30_000_000
        };
        print_kv(if past { b"  abstime_past_fire_us=\0" } else if clk == CLOCK_REALTIME {
            b"  abstime_realtime30_fire_us=\0" } else { b"  abstime_monotonic30_fire_us=\0" },
            el.max(0) as u64 / 1000);
        ok &= case;
    }
    sigprocmask(SIG_SETMASK, &old, core::ptr::null_mut());
    report(name, ok)
}

// ── CPU-time timers ────────────────────────────────────────────────────────
//
// ITIMER_VIRTUAL/ITIMER_PROF were accepted and ignored, and a
// CLOCK_PROCESS_CPUTIME_ID/THREAD_CPUTIME_ID timer ran on the monotonic clock.
// Linux: none of them advance while the process sleeps; each fires once its
// CPU time is used. setitimer(3) is EINVAL.
static VT_COUNT: AtomicI32 = AtomicI32::new(0);
static PROF_COUNT: AtomicI32 = AtomicI32::new(0);
extern "C" fn vt_handler(_s: c_int) { VT_COUNT.fetch_add(1, Ordering::SeqCst); }
extern "C" fn prof_handler(_s: c_int) { PROF_COUNT.fetch_add(1, Ordering::SeqCst); }

// ── vCPU-delay canary (diagnostic only; does not change any limit) ──────────
//
// A forked child spins on CLOCK_MONOTONIC and writes (end_ns, gap_ns) for each
// gap > 1 ms into a pipe. A gap while the child was spinning means its vCPU
// did not run: host scheduling delay, not a guest wake-up bug. It is a child
// (not a thread) because the tests below use process CPU-time clocks.
struct Canary { pid: c_int, rd: c_int }
unsafe fn canary_start() -> Canary {
    let mut fds = [0 as c_int; 2];
    if pipe(fds.as_mut_ptr()) != 0 { return Canary { pid: -1, rd: -1 }; }
    let pid = fork();
    if pid == 0 {
        close(fds[0]);
        let mut prev = now_ns();
        let mut sent = 0;
        loop {
            let t = now_ns();
            if t - prev > 1_000_000 && sent < 1500 {
                let mut rec = [0u8; 16];
                rec[..8].copy_from_slice(&t.to_ne_bytes());
                rec[8..].copy_from_slice(&(t - prev).to_ne_bytes());
                write(fds[1], rec.as_ptr(), 16);
                sent += 1;
            }
            prev = t;
        }
    }
    close(fds[1]);
    Canary { pid, rd: fds[0] }
}
/// Stop the canary and print the largest spin gap seen in [t0, t1] (ns).
unsafe fn canary_finish(c: Canary, t0: i64, t1: i64) {
    if c.pid <= 0 { print_kv(b"  canary_unavailable=\0", 1); return; }
    kill(c.pid, 9);
    let mut st = 0;
    waitpid(c.pid, &mut st, 0);
    let mut rec = [0u8; 16];
    let (mut maxgap, mut n) = (0i64, 0u64);
    while read(c.rd, rec.as_mut_ptr(), 16) == 16 {
        let end = i64::from_ne_bytes(rec[..8].try_into().unwrap());
        let len = i64::from_ne_bytes(rec[8..].try_into().unwrap());
        if end >= t0 && end - len <= t1 { n += 1; if len > maxgap { maxgap = len; } }
    }
    close(c.rd);
    print_kv(b"  canary_spin_gaps_over_1ms=\0", n);
    print_kv(b"  canary_max_spin_gap_us=\0", (maxgap / 1000) as u64);
    print_kv(b"  canary_window_ms=\0", ((t1 - t0) / 1_000_000) as u64);
}

pub unsafe fn test_cpu_time_timers() -> bool {
    let name = b"cpu_time_timers\0";
    let canary = canary_start();
    let win0 = now_ns();
    VT_COUNT.store(0, Ordering::SeqCst);
    PROF_COUNT.store(0, Ordering::SeqCst);
    let mut ok = set_handler(SIGVTALRM, vt_handler, 0) && set_handler(SIGPROF, prof_handler, 0);
    let v50 = itimerval { it_interval: timeval { tv_sec: 0, tv_usec: 0 },
                          it_value: timeval { tv_sec: 0, tv_usec: 50_000 } };
    // ITIMER_VIRTUAL: sleeping does not count, burning does.
    ok &= setitimer(ITIMER_VIRTUAL, &v50, core::ptr::null_mut()) == 0;
    sleep_ms(120);
    let vt_after_sleep = VT_COUNT.load(Ordering::SeqCst);
    burn_cpu_ms(100);
    let vt_after_burn = VT_COUNT.load(Ordering::SeqCst);
    // ITIMER_PROF: reads back armed (0 < value <= 50 ms + a tick), fires on burn.
    ok &= setitimer(ITIMER_PROF, &v50, core::ptr::null_mut()) == 0;
    let mut cur = core::mem::zeroed::<itimerval>();
    ok &= getitimer(ITIMER_PROF, &mut cur) == 0;
    let prof_rem_us = cur.it_value.tv_sec * 1_000_000 + cur.it_value.tv_usec;
    sleep_ms(80);
    let prof_after_sleep = PROF_COUNT.load(Ordering::SeqCst);
    burn_cpu_ms(100);
    let prof_after_burn = PROF_COUNT.load(Ordering::SeqCst);
    let bad_which = setitimer(3, &v50, core::ptr::null_mut()) == -1 && errno() == EINVAL
        && getitimer(3, &mut cur) == -1 && errno() == EINVAL;

    // CLOCK_PROCESS_CPUTIME_ID / CLOCK_THREAD_CPUTIME_ID timers.
    let mut old: sigset_t = 0;
    let blk = bit(SIG_CPU);
    sigprocmask(SIG_BLOCK, &blk, &mut old);
    let mut info = [0u8; 128];
    while wait_sig(SIG_CPU, 0, &mut info) == SIG_CPU {}
    let mut cpu_case = true;
    for &clk in [CLOCK_PROCESS_CPUTIME_ID, CLOCK_THREAD_CPUTIME_ID].iter() {
        let mut ev = ev_signal(SIG_CPU, 7);
        let mut tid: timer_t = core::ptr::null_mut();
        if timer_create(clk, &mut ev, &mut tid) != 0 { cpu_case = false; continue; }
        let spec = itimerspec { it_interval: ts_from_ns(0), it_value: ts_from_ns(40_000_000) };
        timer_settime(tid, 0, &spec, core::ptr::null_mut());
        sleep_ms(100);
        let early = pending_has(SIG_CPU);
        burn_cpu_ms(60);
        let r = wait_sig(SIG_CPU, 500, &mut info);
        timer_delete(tid);
        let code = i32::from_ne_bytes(info[8..12].try_into().unwrap());
        cpu_case &= !early && r == SIG_CPU && code == SI_TIMER;
    }
    sigprocmask(SIG_SETMASK, &old, core::ptr::null_mut());

    canary_finish(canary, win0, now_ns());
    print_kv(b"  vt_after_sleep=\0", vt_after_sleep as u64);
    print_kv(b"  vt_after_burn=\0", vt_after_burn as u64);
    print_kv(b"  prof_getitimer_us=\0", prof_rem_us.max(0) as u64);
    print_kv(b"  prof_after_burn=\0", prof_after_burn as u64);
    print_kv(b"  prof_after_sleep=\0", prof_after_sleep as u64);
    print_kv(b"  ok_bad_which_cpu_case=\0", (ok as u64) * 100 + (bad_which as u64) * 10 + cpu_case as u64);
    report(name, ok && vt_after_sleep == 0 && vt_after_burn == 1
        && prof_rem_us > 0 && prof_rem_us <= 60_000 && prof_after_sleep == 0 && prof_after_burn == 1
        && bad_which && cpu_case)
}

// ── times(2) and the load averages ─────────────────────────────────────────
//
// times() reports the process's user time in 100 Hz ticks: 200 ms of burn is
// at least ~10. /proc/loadavg and sysinfo's loads were constant zeros: with
// two CPU-bound children for > 5 s (one 5 s sample at least), the 1-minute
// average is visibly above zero in both, and /proc/loadavg's fourth field is
// "running/total".
unsafe fn read_file(path: &[u8], buf: &mut [u8]) -> usize {
    let fd = open(path.as_ptr(), 0);
    if fd < 0 { return 0; }
    let n = read(fd, buf.as_mut_ptr(), buf.len());
    close(fd);
    n.max(0) as usize
}

/// The first field of /proc/loadavg in hundredths, and whether field 4 is "a/b".
unsafe fn proc_loadavg() -> (u64, bool) {
    let mut buf = [0u8; 128];
    let n = read_file(b"/proc/loadavg\0", &mut buf);
    let s = &buf[..n];
    let mut v = 0u64;
    let mut i = 0;
    while i < s.len() && s[i].is_ascii_digit() { v = v * 10 + (s[i] - b'0') as u64; i += 1; }
    let mut frac = 0u64;
    if i < s.len() && s[i] == b'.' {
        i += 1;
        let mut d = 0;
        while i < s.len() && s[i].is_ascii_digit() && d < 2 { frac = frac * 10 + (s[i] - b'0') as u64; i += 1; d += 1; }
    }
    let f4 = s.split(|&c| c == b' ').nth(3).unwrap_or(&[]);
    (v * 100 + frac, f4.contains(&b'/'))
}

pub unsafe fn test_times_and_loadavg() -> bool {
    let name = b"times_and_loadavg\0";
    burn_cpu_ms(200);
    let mut tms = [0i64; 4];
    syscall(pnr::TIMES, tms.as_mut_ptr() as c_long);
    let utime = tms[0];
    let mut kids = [0 as c_int; 2];
    for k in kids.iter_mut() {
        let pid = fork();
        if pid == 0 {
            let t0 = now_ns();
            let mut x = 0u64;
            while now_ns() - t0 < 5_600_000_000 {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                core::hint::black_box(x);
            }
            _exit(0);
        }
        *k = pid;
    }
    for &k in kids.iter() { let mut st = 0; waitpid(k, &mut st, 0); }
    let (load100, slash) = proc_loadavg();
    let mut si = [0u64; 16];
    syscall(pnr::SYSINFO, si.as_mut_ptr() as c_long);
    let si_load0 = si[1]; // loads[0], << 16
    print_kv(b"  times_utime_ticks=\0", utime.max(0) as u64);
    print_kv(b"  loadavg1_x100=\0", load100);
    print_kv(b"  sysinfo_load0=\0", si_load0);
    report(name, utime >= 10 && load100 >= 10 && slash && si_load0 >= 6553)
}

// ── SIGEV_THREAD, the way musl/glibc build it ───────────────────────────────
//
// A notify thread that blocks the timer signal and sigwaitinfo()s it, each
// timer created SIGEV_THREAD_ID at that thread: two timers on ONE signal and
// ONE thread must both be delivered (each timer owns its pending instance;
// a real-time signal queues), each with its own sigev_value. A raw
// SIGEV_THREAD reaching the kernel is SIGEV_SIGNAL, as on Linux: EINVAL
// without a signal number, a signal otherwise (it was silently a no-op).
static HELPER_TID: AtomicI32 = AtomicI32::new(0);
static HELPER_GOT: [AtomicI32; 2] = [AtomicI32::new(0), AtomicI32::new(0)];

extern "C" fn notify_thread(_a: *mut c_void) -> *mut c_void {
    unsafe {
        let blk = bit(SIG_HELPER);
        sigprocmask(SIG_BLOCK, &blk, core::ptr::null_mut());
        HELPER_TID.store(syscall(nr::GETTID) as i32, Ordering::SeqCst);
        for slot in HELPER_GOT.iter() {
            let mut info = [0u8; 128];
            if wait_sig(SIG_HELPER, 2000, &mut info) != SIG_HELPER { break; }
            let code = i32::from_ne_bytes(info[8..12].try_into().unwrap());
            let v = i32::from_ne_bytes(info[24..28].try_into().unwrap());
            slot.store(if code == SI_TIMER { v } else { -1 }, Ordering::SeqCst);
        }
    }
    core::ptr::null_mut()
}

pub unsafe fn test_sigev_thread_notify_thread() -> bool {
    let name = b"sigev_thread_notify_thread\0";
    HELPER_TID.store(0, Ordering::SeqCst);
    for g in HELPER_GOT.iter() { g.store(0, Ordering::SeqCst); }
    let mut th: pthread_t = core::ptr::null_mut();
    if pthread_create(&mut th, core::ptr::null(), notify_thread, core::ptr::null_mut()) != 0 {
        return report(name, false);
    }
    let mut htid = 0;
    for _ in 0..500 { htid = HELPER_TID.load(Ordering::SeqCst); if htid != 0 { break; } sleep_ms(2); }
    let mut ids = [core::ptr::null_mut::<c_void>(); 2];
    let mut created = htid != 0;
    for (i, id) in ids.iter_mut().enumerate() {
        let mut ev = sigevent {
            sigev_value: sigval { sival_ptr: (100 * (i + 1)) as *mut c_void },
            sigev_signo: SIG_HELPER,
            sigev_notify: SIGEV_THREAD_ID,
            sigev_notify_thread_id: htid,
            __unused1: [0; 11],
        };
        created &= timer_create(CLOCK_MONOTONIC, &mut ev, id) == 0;
    }
    if created {
        let spec = itimerspec { it_interval: ts_from_ns(0), it_value: ts_from_ns(20_000_000) };
        for &id in ids.iter() { timer_settime(id, 0, &spec, core::ptr::null_mut()); }
    }
    pthread_join(th, core::ptr::null_mut());
    for &id in ids.iter() { if !id.is_null() { timer_delete(id); } }
    let (g0, g1) = (HELPER_GOT[0].load(Ordering::SeqCst), HELPER_GOT[1].load(Ordering::SeqCst));
    let both = (g0 == 100 && g1 == 200) || (g0 == 200 && g1 == 100);

    // Raw SIGEV_THREAD (2): EINVAL with signo 0, a plain signal with one.
    let mut ev = ev_signal(0, 0);
    ev.sigev_notify = SIGEV_NOTIFY_THREAD;
    let mut id: c_int = 0;
    let einval = raw_timer_create(1, &ev, &mut id) == -(EINVAL as c_long);
    ev.sigev_signo = SIG_HELPER;
    let mut old: sigset_t = 0;
    let blk = bit(SIG_HELPER);
    sigprocmask(SIG_BLOCK, &blk, &mut old);
    let mut raw_fired = false;
    if raw_timer_create(1, &ev, &mut id) == 0 {
        let spec = itimerspec { it_interval: ts_from_ns(0), it_value: ts_from_ns(10_000_000) };
        syscall(nr_timer_settime(), id as c_long, 0 as c_long, &spec as *const itimerspec as c_long, 0 as c_long);
        let mut info = [0u8; 128];
        raw_fired = wait_sig(SIG_HELPER, 1000, &mut info) == SIG_HELPER;
        syscall(pnr::TIMER_DELETE, id as c_long);
    }
    sigprocmask(SIG_SETMASK, &old, core::ptr::null_mut());
    print_kv(b"  notify_thread_got0=\0", g0.max(0) as u64);
    print_kv(b"  notify_thread_got1=\0", g1.max(0) as u64);
    report(name, created && both && einval && raw_fired)
}

#[cfg(target_arch = "x86_64")]
fn nr_timer_settime() -> c_long { 223 }
#[cfg(target_arch = "aarch64")]
fn nr_timer_settime() -> c_long { 110 }

// ── A clock step moves wall-clock waiters ──────────────────────────────────
//
// Parked until a CLOCK_REALTIME instant 3 s ahead, a clock_nanosleep
// (TIMER_ABSTIME), a FUTEX_WAIT_BITSET|FUTEX_CLOCK_REALTIME wait and an
// absolute CLOCK_REALTIME POSIX timer must all end promptly when the clock
// is stepped 10 s forward — their instant has passed — not 3 s later at the
// pre-step monotonic deadline. Stepped back 10 s instead, a waiter whose
// instant was 400 ms ahead must still be asleep at 700 ms, and end at once
// when the clock is stepped forward again. Needs root (clock_settime).
const FUTEX_WAIT_BITSET: c_long = 9;
const FUTEX_PRIVATE: c_long = 128;
const FUTEX_CLOCK_REALTIME: c_long = 256;
const ETIMEDOUT: c_int = 110;

/// Forked waiter: 0 = nanosleep, 1 = futex, 2 = POSIX timer. Exits 0 when
/// its wait ended the expected way.
unsafe fn spawn_rt_waiter(kind: u32, at_rt: i64) -> c_int {
    let pid = fork();
    if pid != 0 { return pid; }
    let ts = ts_from_ns(at_rt);
    let ok = match kind {
        0 => syscall(nr::CLOCK_NANOSLEEP, CLOCK_REALTIME as c_long, TIMER_ABSTIME as c_long,
                     &ts as *const timespec as c_long, 0 as c_long) == 0,
        1 => {
            let word: u32 = 0;
            let r = syscall(nr::FUTEX, &word as *const u32 as c_long,
                            FUTEX_WAIT_BITSET | FUTEX_PRIVATE | FUTEX_CLOCK_REALTIME, 0 as c_long,
                            &ts as *const timespec as c_long, 0 as c_long, 0xffff_ffff as c_long);
            r == -(ETIMEDOUT as c_long)
        }
        _ => {
            let blk = bit(SIG_STEP);
            sigprocmask(SIG_BLOCK, &blk, core::ptr::null_mut());
            let mut ev = ev_signal(SIG_STEP, 0);
            let mut tid: timer_t = core::ptr::null_mut();
            timer_create(CLOCK_REALTIME, &mut ev, &mut tid) == 0 && {
                let spec = itimerspec { it_interval: ts_from_ns(0), it_value: ts };
                timer_settime(tid, TIMER_ABSTIME, &spec, core::ptr::null_mut()) == 0
            } && {
                let mut info = [0u8; 128];
                wait_sig(SIG_STEP, 20_000, &mut info) == SIG_STEP
            }
        }
    };
    _exit(if ok { 0 } else { 1 });
}

/// Wait for `pid` up to `ms`: Some(exit code) or None (still running).
unsafe fn reap_within(pid: c_int, ms: i64) -> Option<c_int> {
    let t0 = now_ns();
    loop {
        let mut st = 0;
        if waitpid(pid, &mut st, 1 /* WNOHANG */) == pid { return Some((st >> 8) & 0xff); }
        if now_ns() - t0 >= ms * 1_000_000 { return None; }
        sleep_ms(5);
    }
}

pub unsafe fn test_clock_step_moves_realtime_waiters() -> bool {
    let name = b"clock_step_moves_realtime_waiters\0";
    if syscall(pnr::GETEUID) != 0 { return report(name, true); } // needs root
    // Forward: three waiters 3 s out, the clock stepped +10 s at ~150 ms.
    let at = realtime_ns() + 3_000_000_000;
    let kids = [spawn_rt_waiter(0, at), spawn_rt_waiter(1, at), spawn_rt_waiter(2, at)];
    sleep_ms(150);
    let t0 = now_ns();
    let stepped = step_realtime(10_000_000_000);
    let mut fwd_ok = stepped;
    let mut worst = 0i64;
    for &k in kids.iter() {
        let r = reap_within(k, 2500);
        worst = worst.max(now_ns() - t0);
        if r != Some(0) { fwd_ok = false; }
        if r.is_none() { kill(k, 9); let mut st = 0; waitpid(k, &mut st, 0); }
    }
    let back_fwd = step_realtime(-10_000_000_000);
    print_kv(b"  step_fwd_release_worst_us=\0", worst.max(0) as u64 / 1000);
    fwd_ok &= worst < 1_000_000_000 && back_fwd;

    // Backward: a nanosleep 400 ms out, the clock stepped -10 s at 100 ms:
    // still asleep at 700 ms; stepped +10 s again, it ends at once.
    let k = spawn_rt_waiter(0, realtime_ns() + 400_000_000);
    sleep_ms(100);
    let sb = step_realtime(-10_000_000_000);
    let early = reap_within(k, 600);
    let sf = step_realtime(10_000_000_000);
    let t1 = now_ns();
    let late = if early.is_none() { reap_within(k, 2000) } else { early };
    let back_release = now_ns() - t1;
    if late.is_none() { kill(k, 9); let mut st = 0; waitpid(k, &mut st, 0); }
    print_kv(b"  step_back_release_us=\0", back_release.max(0) as u64 / 1000);
    let back_ok = sb && sf && early.is_none() && late == Some(0) && back_release < 1_000_000_000;
    report(name, fwd_ok && back_ok)
}

/// relibc's sigevent with the SIGEV_THREAD union member (offset 16).
#[repr(C)]
#[derive(Clone, Copy)]
struct sigevent_thr {
    sigev_value: sigval,
    sigev_signo: c_int,
    sigev_notify: c_int,
    func: Option<extern "C" fn(sigval)>,
    attr: *mut c_void,
    _pad: [c_int; 8],
}
const SIGEV_THREAD_NOTIFY: c_int = 2;

static THR_HITS: AtomicI32 = AtomicI32::new(0);
static THR_VAL: AtomicI32 = AtomicI32::new(0);
static THR_TID: AtomicI32 = AtomicI32::new(0);
static THR_SELF_ID: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

extern "C" fn on_thr(v: sigval) {
    unsafe {
        THR_VAL.store(v.sival_ptr as usize as i32, Ordering::SeqCst);
        THR_TID.store(syscall(nr::GETTID, 0 as c_long) as i32, Ordering::SeqCst);
    }
    THR_HITS.fetch_add(1, Ordering::SeqCst);
}

extern "C" fn on_thr_selfdel(_v: sigval) {
    THR_HITS.fetch_add(1, Ordering::SeqCst);
    unsafe { timer_delete(THR_SELF_ID.load(Ordering::SeqCst) as *mut c_void) };
}

fn thr_ev(f: Option<extern "C" fn(sigval)>, v: usize) -> sigevent_thr {
    sigevent_thr {
        sigev_value: sigval { sival_ptr: v as *mut c_void },
        sigev_signo: 0,
        sigev_notify: SIGEV_THREAD_NOTIFY,
        func: f,
        attr: core::ptr::null_mut(),
        _pad: [0; 8],
    }
}

unsafe fn thr_reset() {
    THR_HITS.store(0, Ordering::SeqCst);
    THR_VAL.store(0, Ordering::SeqCst);
    THR_TID.store(0, Ordering::SeqCst);
}

/// SIGEV_THREAD: libc runs sigev_notify_function on its own thread with
/// sigev_value; periodic timers keep calling; timer_delete stops them, also
/// from inside the callback; a NULL function is EINVAL (relibc) or accepted (musl).
pub unsafe fn test_sigev_thread_callback() -> bool {
    let name = b"sigev_thread_callback\0";
    let me = syscall(nr::GETTID, 0 as c_long) as i32;
    let mut id: *mut c_void = core::ptr::null_mut();

    // 1. One-shot: exactly one call, right value, not on this thread.
    thr_reset();
    let mut ev = thr_ev(Some(on_thr), 4242);
    if timer_create(CLOCK_MONOTONIC, &mut ev as *mut sigevent_thr as *mut sigevent, &mut id) != 0 {
        return report(name, false);
    }
    let once = itimerspec { it_interval: ts_from_ns(0), it_value: ts_from_ns(20_000_000) };
    timer_settime(id, 0, &once, core::ptr::null_mut());
    for _ in 0..500 { if THR_HITS.load(Ordering::SeqCst) != 0 { break; } sleep_ms(2); }
    sleep_ms(50);
    let t = THR_TID.load(Ordering::SeqCst);
    let ok1 = THR_HITS.load(Ordering::SeqCst) == 1
        && THR_VAL.load(Ordering::SeqCst) == 4242
        && t != 0 && t != me
        && timer_delete(id) == 0;

    // 2. Periodic 10 ms: >= 3 calls in 120 ms; none (but one in flight) after delete.
    thr_reset();
    let mut ev = thr_ev(Some(on_thr), 7);
    let mut ok2 = timer_create(CLOCK_MONOTONIC, &mut ev as *mut sigevent_thr as *mut sigevent, &mut id) == 0;
    if ok2 {
        let per = itimerspec { it_interval: ts_from_ns(10_000_000), it_value: ts_from_ns(10_000_000) };
        timer_settime(id, 0, &per, core::ptr::null_mut());
        sleep_ms(120);
        ok2 &= THR_HITS.load(Ordering::SeqCst) >= 3;
        ok2 &= timer_delete(id) == 0;
        let after = THR_HITS.load(Ordering::SeqCst);
        sleep_ms(60);
        ok2 &= THR_HITS.load(Ordering::SeqCst) <= after + 1;
    }

    // 3. Callback deletes its own timer: exactly one call, no crash.
    thr_reset();
    let mut ev = thr_ev(Some(on_thr_selfdel), 0);
    let mut ok3 = timer_create(CLOCK_MONOTONIC, &mut ev as *mut sigevent_thr as *mut sigevent, &mut id) == 0;
    if ok3 {
        THR_SELF_ID.store(id as usize, Ordering::SeqCst);
        let per = itimerspec { it_interval: ts_from_ns(10_000_000), it_value: ts_from_ns(10_000_000) };
        timer_settime(id, 0, &per, core::ptr::null_mut());
        sleep_ms(100);
        ok3 &= THR_HITS.load(Ordering::SeqCst) == 1;
    }

    // 4. NULL sigev_notify_function -> EINVAL.
    let mut ev = thr_ev(None, 0);
    // musl does not validate it (POSIX leaves it undefined): creation succeeds
    // and the never-armed timer is simply deleted again.
    let r4 = timer_create(CLOCK_MONOTONIC, &mut ev as *mut sigevent_thr as *mut sigevent, &mut id);
    let ok4 = if r4 == 0 { timer_delete(id) == 0 } else { r4 == -1 && *__errno_location() == 22 };

    let sub = [b'0' + ok1 as u8, b'0' + ok2 as u8, b'0' + ok3 as u8, b'0' + ok4 as u8, b'\n'];
    write(1, b"sigev_thread_callback sub-cases: ".as_ptr(), 33);
    write(1, sub.as_ptr(), 5);
    report(name, ok1 && ok2 && ok3 && ok4)
}
