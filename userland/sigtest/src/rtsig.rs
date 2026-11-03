//! Real-time signal queueing (lane posixtime): SIGRTMIN..SIGRTMAX instances
//! queue with their own siginfo and are dequeued in order, thread-private
//! before process-level; signalfd reports one record per instance with the
//! `_rt`/`_timer` fields filled; SIGRTMAX (64) is a usable signal. Every
//! expectation was checked against Linux 7.2 (x86_64).

use super::*;

extern "C" { fn __errno_location() -> *mut c_int; }

#[cfg(target_arch = "x86_64")]
mod rnr {
    pub const RT_SIGQUEUEINFO: i64 = 129;
    pub const RT_TGSIGQUEUEINFO: i64 = 297;
    pub const RT_SIGTIMEDWAIT: i64 = 128;
    pub const TIMER_CREATE: i64 = 222;
    pub const TIMER_SETTIME: i64 = 223;
    pub const TIMER_GETOVERRUN: i64 = 225;
    pub const TIMER_DELETE: i64 = 226;
    pub const GETUID: i64 = 102;
}
#[cfg(target_arch = "aarch64")]
mod rnr {
    pub const RT_SIGQUEUEINFO: i64 = 138;
    pub const RT_TGSIGQUEUEINFO: i64 = 240;
    pub const RT_SIGTIMEDWAIT: i64 = 137;
    pub const TIMER_CREATE: i64 = 107;
    pub const TIMER_SETTIME: i64 = 110;
    pub const TIMER_GETOVERRUN: i64 = 109;
    pub const TIMER_DELETE: i64 = 111;
    pub const GETUID: i64 = 174;
}

const SI_QUEUE: c_int = -1;
const SI_TIMER: c_int = -2;
const EAGAIN: c_int = 11;
const EINVAL: c_int = 22;
const SIGRTMAX: c_int = 64;
const SIG_Q: c_int = 48;     // RT signals used only by this file
const SIG_H: c_int = 49;
const SIG_FD: c_int = 50;

fn sb(sig: c_int) -> sigset_t { 1u64 << (sig - 1) }

/// A `siginfo_t` for rt_sigqueueinfo: SI_QUEUE from us, carrying `value`.
unsafe fn queue_info(sig: c_int, value: u64) -> [u8; 128] {
    let mut si = [0u8; 128];
    si[0..4].copy_from_slice(&sig.to_ne_bytes());
    si[8..12].copy_from_slice(&SI_QUEUE.to_ne_bytes());
    si[16..20].copy_from_slice(&getpid().to_ne_bytes());
    si[20..24].copy_from_slice(&(syscall(rnr::GETUID) as u32).to_ne_bytes());
    si[24..32].copy_from_slice(&value.to_ne_bytes());
    si
}

/// sigqueue(getpid(), sig, value): process-directed.
unsafe fn sigqueue_proc(sig: c_int, value: u64) -> c_long {
    let si = queue_info(sig, value);
    syscall(rnr::RT_SIGQUEUEINFO, getpid() as c_long, sig as c_long, si.as_ptr() as c_long)
}

/// rt_tgsigqueueinfo to this very thread: thread-directed.
unsafe fn sigqueue_self_thread(sig: c_int, value: u64) -> c_long {
    let si = queue_info(sig, value);
    let tid = syscall(nr::GETTID);
    syscall(rnr::RT_TGSIGQUEUEINFO, getpid() as c_long, tid, sig as c_long, si.as_ptr() as c_long)
}

/// sigtimedwait(sig, 0): (signo or -1, siginfo bytes).
unsafe fn take(sig: c_int) -> (c_int, [u8; 128]) {
    let set = sb(sig);
    let ts = timespec { tv_sec: 0, tv_nsec: 0 };
    let mut info = [0u8; 128];
    let r = syscall(rnr::RT_SIGTIMEDWAIT, &set as *const sigset_t as c_long, info.as_mut_ptr() as c_long,
                    &ts as *const timespec as c_long, 8 as c_long) as c_int;
    (r, info)
}

fn g32(b: &[u8], o: usize) -> i32 { i32::from_ne_bytes(b[o..o + 4].try_into().unwrap()) }
fn g64(b: &[u8], o: usize) -> u64 { u64::from_ne_bytes(b[o..o + 8].try_into().unwrap()) }

// ── Instances queue, FIFO, private before shared ───────────────────────────
//
// Three process-directed sigqueue()s (values 1, 2, 3) and two thread-directed
// rt_tgsigqueueinfo()s (11, 12) of one blocked RT signal: sigtimedwait takes
// five instances — 11 12 1 2 3, as Linux dequeues the thread's own queue
// first — each SI_QUEUE with its value, then EAGAIN. They used to coalesce
// into one pending bit (one instance, the first payload). A standard signal
// sent twice still coalesces.
pub unsafe fn test_rt_signals_queue_in_order() -> bool {
    let name = b"rt_signals_queue_in_order\0";
    let mut old: sigset_t = 0;
    let blk = sb(SIG_Q) | sb(SIGUSR2);
    sigprocmask(SIG_BLOCK, &blk, &mut old);
    let mut sent = true;
    for v in 1..=3u64 { sent &= sigqueue_proc(SIG_Q, v) == 0; }
    for v in 11..=12u64 { sent &= sigqueue_self_thread(SIG_Q, v) == 0; }
    let want = [11u64, 12, 1, 2, 3];
    let mut ok = sent;
    for &w in want.iter() {
        let (r, info) = take(SIG_Q);
        if r != SIG_Q || g32(&info, 8) != SI_QUEUE || g64(&info, 24) != w || g32(&info, 16) != getpid() {
            print_kv_rt(b"  rtq_mismatch_want=\0", w);
            print_kv_rt(b"  rtq_mismatch_got=\0", g64(&info, 24));
            print_kv_rt(b"  rtq_mismatch_r=\0", r as i64 as u64);
            print_kv_rt(b"  rtq_sent=\0", sent as u64);
            ok = false;
        }
    }
    let (r_end, _) = take(SIG_Q);
    let end_eagain = r_end == -EAGAIN; // raw syscall(): -errno
    kill(getpid(), SIGUSR2);
    kill(getpid(), SIGUSR2);
    let (u1, _) = take(SIGUSR2);
    let (u2, _) = take(SIGUSR2);
    sigprocmask(2, &old, core::ptr::null_mut());
    report(name, ok && end_eagain && u1 == SIGUSR2 && u2 == -EAGAIN)
}

// ── A handler runs once per instance ───────────────────────────────────────
static H_VALS: [AtomicI32; 8] = [const { AtomicI32::new(0) }; 8];
static H_N: AtomicI32 = AtomicI32::new(0);

extern "C" fn rt_info_handler(_sig: c_int, info: *mut siginfo_t, _uc: *mut c_void) {
    let n = H_N.fetch_add(1, Ordering::SeqCst) as usize;
    // sigval sits where the _kill member's si_status would (offset 24).
    let v = unsafe { *((info as *const u8).add(24) as *const i32) };
    if n < 8 { H_VALS[n].store(v, Ordering::SeqCst); }
}

/// Four instances queued while blocked, then unblocked: the SA_SIGINFO
/// handler runs four times, values 1 2 3 4 in order (Linux: n=4: 1 2 3 4).
pub unsafe fn test_rt_handler_runs_per_instance() -> bool {
    let name = b"rt_handler_runs_per_instance\0";
    H_N.store(0, Ordering::SeqCst);
    let act = sigaction_info(rt_info_handler);
    if sigaction(SIG_H, &act as *const _ as *const sigaction, core::ptr::null_mut()) != 0 {
        return report(name, false);
    }
    let mut old: sigset_t = 0;
    let blk = sb(SIG_H);
    sigprocmask(SIG_BLOCK, &blk, &mut old);
    for v in 1..=4u64 { sigqueue_proc(SIG_H, v); }
    sigprocmask(SIG_UNBLOCK, &blk, core::ptr::null_mut());
    let got = wait_until(|| H_N.load(Ordering::SeqCst) >= 4);
    sigprocmask(2, &old, core::ptr::null_mut());
    let order = (0..4).all(|i| H_VALS[i].load(Ordering::SeqCst) == i as i32 + 1);
    report(name, got && H_N.load(Ordering::SeqCst) == 4 && order)
}

#[repr(C)]
struct sigaction_si {
    sa_sigaction: extern "C" fn(c_int, *mut siginfo_t, *mut c_void),
    sa_mask: sigset_t,
    _mask_pad: [u64; 15], // musl sigset_t is 1024 bits
    sa_flags: c_int,
    sa_restorer: Option<unsafe extern "C" fn()>,
}

fn sigaction_info(h: extern "C" fn(c_int, *mut siginfo_t, *mut c_void)) -> sigaction_si {
    sigaction_si { sa_sigaction: h, sa_flags: SA_SIGINFO, sa_restorer: None, sa_mask: 0, _mask_pad: [0; 15] }
}

// ── signalfd: a record per instance, fields filled ─────────────────────────
//
// Three queued instances come back from ONE read of a 4-record buffer as
// three records (384 bytes) with ssi_code SI_QUEUE, ssi_pid, ssi_int and
// ssi_ptr = the queued value. A periodic POSIX timer's signal read through a
// signalfd carries ssi_code SI_TIMER, ssi_tid = the timer id, ssi_ptr /
// ssi_int = its sigev_value, ssi_pid 0, and an ssi_overrun > 0 that
// timer_getoverrun then repeats. ssi_overrun/ssi_ptr/ssi_int used to be 0.
pub unsafe fn test_signalfd_rt_fields() -> bool {
    let name = b"signalfd_rt_fields\0";
    let mask = sb(SIG_FD);
    let mut old: sigset_t = 0;
    sigprocmask(SIG_BLOCK, &mask, &mut old);
    let sfd = syscall(nr::SIGNALFD4, -1i64, &mask as *const sigset_t as c_long, 8i64, SFD_NONBLOCK) as c_int;
    if sfd < 0 { sigprocmask(2, &old, core::ptr::null_mut()); return report(name, false); }
    for v in 1..=3u64 { sigqueue_proc(SIG_FD, 0x1000 + v); }
    let mut buf = [0u8; 512];
    let n = read(sfd, buf.as_mut_ptr(), 512);
    let mut q_ok = n == 384;
    for i in 0..3usize {
        let r = &buf[i * 128..];
        q_ok &= g32(r, 0) == SIG_FD && g32(r, 8) == SI_QUEUE && g32(r, 12) == getpid()
            && g32(r, 44) == 0x1001 + i as i32 && g64(r, 48) == 0x1001 + i as u64;
    }
    // The timer: 2 ms period, read after ~30 ms → several overruns.
    #[repr(C)]
    struct Sev { value: u64, signo: c_int, notify: c_int, tid: c_int, pad: [c_int; 11] }
    let ev = Sev { value: 0xabcdef, signo: SIG_FD, notify: 0, tid: 0, pad: [0; 11] };
    let mut id: c_int = 0;
    let mut t_ok = syscall(rnr::TIMER_CREATE, 1 as c_long, &ev as *const Sev as c_long,
                           &mut id as *mut c_int as c_long) == 0;
    let spec = [0i64, 2_000_000, 0, 2_000_000];
    t_ok &= syscall(rnr::TIMER_SETTIME, id as c_long, 0 as c_long, spec.as_ptr() as c_long, 0 as c_long) == 0;
    let ts = timespec { tv_sec: 0, tv_nsec: 30_000_000 };
    nanosleep(&ts, core::ptr::null_mut());
    let n2 = read(sfd, buf.as_mut_ptr(), 512);
    let overrun = g32(&buf, 32);
    let getov = syscall(rnr::TIMER_GETOVERRUN, id as c_long) as i32;
    syscall(rnr::TIMER_DELETE, id as c_long);
    t_ok &= n2 >= 128 && g32(&buf, 8) == SI_TIMER && g32(&buf, 24) == id && g32(&buf, 12) == 0
        && g64(&buf, 48) == 0xabcdef && g32(&buf, 44) == 0xabcdef && overrun > 0 && getov == overrun;
    print_kv_rt(b"  sfd_timer_overrun=\0", overrun.max(0) as u64);
    // Drain anything the timer queued before it was deleted.
    while read(sfd, buf.as_mut_ptr(), 512) > 0 {}
    close(sfd);
    sigprocmask(2, &old, core::ptr::null_mut());
    report(name, q_ok && t_ok)
}

// ── SIGRTMAX is a signal ───────────────────────────────────────────────────
//
// Signal 64 was refused (EINVAL) by rt_sigaction, kill, tgkill and
// rt_sigqueueinfo, which all checked `sig >= 64`. Linux: 1..=64 are valid,
// 65 is EINVAL.
static S64: AtomicI32 = AtomicI32::new(0);
extern "C" fn h64(_s: c_int) { S64.fetch_add(1, Ordering::SeqCst); }

pub unsafe fn test_sigrtmax_is_usable() -> bool {
    let name = b"sigrtmax_is_usable\0";
    S64.store(0, Ordering::SeqCst);
    let act = zeroed_sigaction(Some(h64));
    let installed = sigaction(SIGRTMAX, &act, core::ptr::null_mut()) == 0;
    let refused65 = sigaction(65, &act, core::ptr::null_mut()) == -1 && *__errno_location() == EINVAL;
    let killed = kill(getpid(), SIGRTMAX) == 0;
    let got_kill = wait_until(|| S64.load(Ordering::SeqCst) >= 1);
    let tg = syscall(nr::TGKILL, getpid() as c_long, syscall(nr::GETTID), SIGRTMAX as c_long) == 0;
    let got_tg = wait_until(|| S64.load(Ordering::SeqCst) >= 2);
    let q = sigqueue_proc(SIGRTMAX, 5) == 0;
    let got_q = wait_until(|| S64.load(Ordering::SeqCst) >= 3);
    let kill65 = kill(getpid(), 65) == -1 && *__errno_location() == EINVAL;
    let dfl = zeroed_sigaction(None);
    sigaction(SIGRTMAX, &dfl, core::ptr::null_mut());
    report(name, installed && refused65 && killed && got_kill && tg && got_tg && q && got_q && kill65)
}

unsafe fn print_kv_rt(label: &[u8], v: u64) {
    write(1, label.as_ptr(), label.len() - 1);
    let mut buf = [0u8; 20];
    let mut n = 0;
    let mut x = v;
    loop { buf[n] = b'0' + (x % 10) as u8; n += 1; x /= 10; if x == 0 { break; } }
    for i in (0..n).rev() { write(1, &buf[i], 1); }
    write(1, b"\n".as_ptr(), 1);
}
