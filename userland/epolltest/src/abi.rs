//! `epolltest abi` / `epolltest pollbench` (lane epollabi).
//!
//! `abi`: the epoll_ctl error rules Linux applies (`do_epoll_ctl`), checked
//! one by one, and EPOLLEXCLUSIVE's wake-one behaviour:
//!  - EINVAL: epfd == fd (also through a dup of epfd), epfd not an epoll fd,
//!    an unknown op;
//!  - ELOOP: an ADD that would close a cycle of epoll instances, or make a
//!    chain longer than EP_MAX_NESTS (4) links, looking both down from the
//!    added instance and up from the one added to;
//!  - EPOLLEXCLUSIVE: EINVAL on MOD with it, on MOD of an exclusive
//!    registration, on ADD of an epoll fd with it, and on ADD with any bit
//!    outside EPOLLIN|EPOLLOUT|EPOLLERR|EPOLLHUP|EPOLLWAKEUP|EPOLLET;
//!  - wake-one: three threads, each parked in its own instance holding the
//!    same eventfd with EPOLLEXCLUSIVE; one write wakes exactly one of them
//!    (and, without the flag, all three).
//!
//! `pollbench`: what a poll() costs (calls/s, ns per call), for the shapes
//! that matter: Firefox's VA-API frame pool (one eventfd, timeout 0, ready or
//! not), an 8-fd zero-timeout poll, and a blocking poll over 8 eventfds woken
//! through one of them (round trip and kernel probes per woken call, from
//! `/proc/wakestat`'s `# pollx` row when the kernel has it).

use crate::*;

extern "C" {
    fn dup(fd: c_int) -> c_int;
    fn poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
    fn getppid() -> c_int;
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PollFd { fd: c_int, events: i16, revents: i16 }

const EPOLL_CTL_DEL: c_int = 2;
const EPOLLOUT: c_uint = 0x004;
const EPOLLPRI: c_uint = 0x002;
const EPOLLRDNORM: c_uint = 0x040;
const EPOLLRDHUP: c_uint = 0x2000;
const EPOLLEXCLUSIVE: c_uint = 1 << 28;
const EPOLLWAKEUP: c_uint = 1 << 29;

const EINVAL: c_int = 22;
const EBADF: c_int = 9;
const ELOOP: c_int = 40;
const ENOENT: c_int = 2;

unsafe fn errno() -> c_int { *__errno_location() }

/// epoll_ctl's result: 0, or -errno.
unsafe fn ctl(ep: c_int, op: c_int, fd: c_int, events: c_uint) -> c_int {
    let mut ev = epoll_event { events, data: epoll_data { fd } };
    *__errno_location() = 0;
    if epoll_ctl(ep, op, fd, &mut ev) == 0 { 0 } else { -errno() }
}

/// Print `name: PASS|FAIL (got=..)`.
unsafe fn check(name: &[u8], got: c_int, want: c_int) -> bool {
    let ok = got == want;
    write(1, name.as_ptr(), name.len());
    if ok { write(1, b": PASS".as_ptr(), 6); } else { write(1, b": FAIL".as_ptr(), 6); }
    write(1, b" (got=".as_ptr(), 6); write_i64(1, got as i64);
    write(1, b" want=".as_ptr(), 6); write_i64(1, want as i64);
    write(1, b")\n".as_ptr(), 2);
    ok
}

pub unsafe fn abi_main() -> i32 {
    let mut pass = 0;
    let mut fail = 0;
    let mut t = |ok: bool| if ok { pass += 1 } else { fail += 1 };

    // ── EINVAL / EBADF basics ────────────────────────────────────────────
    let ep = epoll_create1(0);
    let efd = efd_new();
    t(check(b"einval_self", ctl(ep, EPOLL_CTL_ADD, ep, EPOLLIN), -EINVAL));
    let epd = dup(ep);
    t(check(b"einval_self_dup", if epd < 0 { -1000 } else { ctl(ep, EPOLL_CTL_ADD, epd, EPOLLIN) }, -EINVAL));
    let efd2 = efd_new();
    t(check(b"einval_not_epoll", ctl(efd2, EPOLL_CTL_ADD, efd, EPOLLIN), -EINVAL));
    t(check(b"einval_bad_op0", ctl(ep, 0, efd, EPOLLIN), -EINVAL));
    t(check(b"einval_bad_op7", ctl(ep, 7, efd, EPOLLIN), -EINVAL));
    t(check(b"ebadf_closed_fd", ctl(ep, EPOLL_CTL_ADD, 999, EPOLLIN), -EBADF));
    t(check(b"enoent_del_missing", ctl(ep, EPOLL_CTL_DEL, efd, 0), -ENOENT));
    if epd >= 0 { close(epd); }

    // ── ELOOP: cycles ─────────────────────────────────────────────────────
    let a = epoll_create1(0);
    let b = epoll_create1(0);
    let c = epoll_create1(0);
    t(check(b"nest_a_b", ctl(a, EPOLL_CTL_ADD, b, EPOLLIN), 0));
    t(check(b"eloop_cycle2", ctl(b, EPOLL_CTL_ADD, a, EPOLLIN), -ELOOP));
    t(check(b"nest_b_c", ctl(b, EPOLL_CTL_ADD, c, EPOLLIN), 0));
    t(check(b"eloop_cycle3", ctl(c, EPOLL_CTL_ADD, a, EPOLLIN), -ELOOP));
    // The refused ADDs left nothing behind: c still has no registration of a.
    t(check(b"eloop_left_nothing", ctl(c, EPOLL_CTL_DEL, a, 0), -ENOENT));
    // A nested set still works after the refusals: an event in c reaches a.
    efd_bump(efd2);
    t(check(b"nest_c_efd", ctl(c, EPOLL_CTL_ADD, efd2, EPOLLIN), 0));
    let mut out: [epoll_event; 4] = core::mem::zeroed();
    t(check(b"nest_still_fires", epoll_wait(a, out.as_mut_ptr(), 4, 1000), 1));
    // A diamond is not a loop: d watched through two parents.
    let d = epoll_create1(0);
    t(check(b"diamond_b_d", ctl(b, EPOLL_CTL_ADD, d, EPOLLIN), 0));
    t(check(b"diamond_c_d", ctl(c, EPOLL_CTL_ADD, d, EPOLLIN), 0));
    for fd in [a, b, c, d] { close(fd); }

    // ── ELOOP: depth, looking down from the added instance ──────────────
    // e[i+1] watches e[i]. Five instances (four links) are allowed, the
    // sixth link is not (Linux EP_MAX_NESTS = 4).
    let mut e = [-1 as c_int; 6];
    for x in e.iter_mut() { *x = epoll_create1(0); }
    let mut chain_ok = true;
    for i in 0..4 { if ctl(e[i + 1], EPOLL_CTL_ADD, e[i], EPOLLIN) != 0 { chain_ok = false; } }
    t(check(b"depth_4_links_ok", if chain_ok { 0 } else { -1 }, 0));
    t(check(b"eloop_depth_down", ctl(e[5], EPOLL_CTL_ADD, e[4], EPOLLIN), -ELOOP));
    for x in e { close(x); }

    // ── ELOOP: depth, counting up from the instance added to ────────────
    // u0 -> u1 and u2 -> u3 -> u4, then u1 -> u2 joins them (4 links), then
    // u4 -> u5 would make 5: the added u5 has no depth of its own, all of it
    // is above u4.
    let mut u = [-1 as c_int; 6];
    for x in u.iter_mut() { *x = epoll_create1(0); }
    let r1 = ctl(u[0], EPOLL_CTL_ADD, u[1], EPOLLIN);
    let r2 = ctl(u[2], EPOLL_CTL_ADD, u[3], EPOLLIN);
    let r3 = ctl(u[3], EPOLL_CTL_ADD, u[4], EPOLLIN);
    t(check(b"depth_join_ok", if r1 == 0 && r2 == 0 && r3 == 0 { ctl(u[1], EPOLL_CTL_ADD, u[2], EPOLLIN) } else { -1 }, 0));
    t(check(b"eloop_depth_up", ctl(u[4], EPOLL_CTL_ADD, u[5], EPOLLIN), -ELOOP));
    // A plain fd at the bottom of a 4-link chain is fine: it is not an epoll.
    t(check(b"depth_plain_fd_ok", ctl(u[4], EPOLL_CTL_ADD, efd, EPOLLIN), 0));
    for x in u { close(x); }

    // ── EPOLLEXCLUSIVE EINVAL rules ─────────────────────────────────────
    let x = epoll_create1(0);
    let inner = epoll_create1(0);
    let p = efd_new();
    let q = efd_new();
    t(check(b"excl_epoll_target", ctl(x, EPOLL_CTL_ADD, inner, EPOLLIN | EPOLLEXCLUSIVE), -EINVAL));
    t(check(b"excl_oneshot", ctl(x, EPOLL_CTL_ADD, p, EPOLLIN | EPOLLEXCLUSIVE | EPOLLONESHOT), -EINVAL));
    t(check(b"excl_pri", ctl(x, EPOLL_CTL_ADD, p, EPOLLIN | EPOLLEXCLUSIVE | EPOLLPRI), -EINVAL));
    t(check(b"excl_rdhup", ctl(x, EPOLL_CTL_ADD, p, EPOLLIN | EPOLLEXCLUSIVE | EPOLLRDHUP), -EINVAL));
    t(check(b"excl_rdnorm", ctl(x, EPOLL_CTL_ADD, p, EPOLLIN | EPOLLEXCLUSIVE | EPOLLRDNORM), -EINVAL));
    t(check(b"excl_ok_bits", ctl(x, EPOLL_CTL_ADD, p,
        EPOLLIN | EPOLLOUT | EPOLLET | EPOLLWAKEUP | EPOLLEXCLUSIVE), 0));
    t(check(b"excl_mod_of_exclusive", ctl(x, EPOLL_CTL_MOD, p, EPOLLIN), -EINVAL));
    t(check(b"excl_plain_add", ctl(x, EPOLL_CTL_ADD, q, EPOLLIN), 0));
    t(check(b"excl_mod_with_flag", ctl(x, EPOLL_CTL_MOD, q, EPOLLIN | EPOLLEXCLUSIVE), -EINVAL));
    t(check(b"excl_mod_plain_ok", ctl(x, EPOLL_CTL_MOD, q, EPOLLIN | EPOLLOUT), 0));
    t(check(b"excl_del_ok", ctl(x, EPOLL_CTL_DEL, p, 0), 0));
    // An exclusive registration still reports its fd.
    t(check(b"excl_readd", ctl(x, EPOLL_CTL_ADD, p, EPOLLIN | EPOLLEXCLUSIVE), 0));
    efd_bump(p);
    let n = epoll_wait(x, out.as_mut_ptr(), 4, 1000);
    t(check(b"excl_reports", if n >= 1 && out[0].data.fd == p { 1 } else { n }, 1));
    for fd in [x, inner, p, q] { close(fd); }

    // ── EPOLLEXCLUSIVE wake-one ─────────────────────────────────────────
    t(check(b"excl_wake_one", wake_count(true), 1));
    t(check(b"excl_off_wakes_all", wake_count(false), WAKERS as c_int));

    close(efd); close(efd2); close(ep);
    write_summary(pass + fail, fail);
    puts(b"--- epolltest abi done ---\n\0".as_ptr());
    fail
}

const WAKERS: usize = 3;

#[repr(C)]
struct Waker { ep: c_int, shared: c_int, quit: c_int, got_shared_ms: i64, t0: i64, drain: bool }

extern "C" fn waker_thread(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let w = &mut *(arg as *mut Waker);
        let mut out: [epoll_event; 4] = core::mem::zeroed();
        // One wait: the shared fd's wake, or the quit at the end.
        let n = epoll_wait(w.ep, out.as_mut_ptr(), 4, 5000);
        for i in 0..(n.max(0) as usize).min(4) {
            if out[i].data.fd == w.shared && w.got_shared_ms < 0 {
                w.got_shared_ms = now_ms() - w.t0;
                // Consume it, so a later unrelated wake of the others (this
                // thread's exit, say) finds nothing (Linux would show it
                // to them too).
                if w.drain { efd_drain(w.shared); }
            }
        }
    }
    core::ptr::null_mut()
}

/// How many of `WAKERS` threads, each parked in its own instance on the same
/// eventfd (with or without EPOLLEXCLUSIVE), one write wakes within 300 ms.
unsafe fn wake_count(exclusive: bool) -> c_int {
    let shared = efd_new();
    let quit = efd_new();
    let mut ws: [Waker; WAKERS] = core::mem::zeroed();
    let mut th: [pthread_t; WAKERS] = [core::ptr::null_mut(); WAKERS];
    let t0 = now_ms();
    for i in 0..WAKERS {
        let ep = epoll_create1(0);
        let flags = EPOLLIN | if exclusive { EPOLLEXCLUSIVE } else { 0 };
        if ctl(ep, EPOLL_CTL_ADD, shared, flags) != 0 || ctl(ep, EPOLL_CTL_ADD, quit, EPOLLIN) != 0 { return -1; }
        ws[i] = Waker { ep, shared, quit, got_shared_ms: -1, t0, drain: exclusive };
    }
    for i in 0..WAKERS {
        pthread_create(&mut th[i], core::ptr::null(), waker_thread, &mut ws[i] as *mut _ as *mut c_void);
    }
    usleep(200_000); // every waiter parks
    let x0 = pollx5()[4];
    efd_bump(shared);
    usleep(300_000);
    let woken = ws.iter().filter(|w| core::ptr::read_volatile(&w.got_shared_ms) >= 0).count() as c_int;
    write(1, b"  excl: exclusive=".as_ptr(), 18); write_i64(1, exclusive as i64);
    write(1, b" woken=".as_ptr(), 7); write_i64(1, woken as i64);
    write(1, b" exclskip=".as_ptr(), 10); write_i64(1, pollx5()[4] - x0);
    write(1, b"\n".as_ptr(), 1);
    efd_bump(quit);
    for i in 0..WAKERS { pthread_join(th[i], core::ptr::null_mut()); }
    for w in ws.iter() { close(w.ep); }
    close(shared); close(quit);
    woken
}

// ── pollbench ────────────────────────────────────────────────────────────

/// ns per call of `f` run `n` times (wall clock).
unsafe fn per_call(n: usize, mut f: impl FnMut()) -> i64 {
    let mut t0: timespec = core::mem::zeroed();
    let mut t1: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut t0);
    for _ in 0..n { f(); }
    clock_gettime(CLOCK_MONOTONIC, &mut t1);
    let ns = (t1.tv_sec - t0.tv_sec) * 1_000_000_000 + (t1.tv_nsec - t0.tv_nsec);
    ns / n as i64
}

unsafe fn row(name: &[u8], ns: i64) {
    write(1, b"pollbench ".as_ptr(), 10);
    write(1, name.as_ptr(), name.len());
    write(1, b": ns_per_call=".as_ptr(), 14); write_i64(1, ns);
    write(1, b" calls_per_s=".as_ptr(), 13); write_i64(1, if ns > 0 { 1_000_000_000 / ns } else { -1 });
    write(1, b"\n".as_ptr(), 1);
}

/// `# pollx calls C probes P parks K` from /proc/wakestat: (calls, probes, parks), zeros when absent.
unsafe fn pollx() -> (i64, i64, i64) { let v = pollx5(); (v[0], v[1], v[2]) }

/// All five `# pollx` numbers (calls probes parks useless exclskip).
unsafe fn pollx5() -> [i64; 5] {
    let fd = open(b"/proc/wakestat\0".as_ptr(), 0);
    if fd < 0 { return [0; 5]; }
    static mut BUF: [u8; 65536] = [0; 65536];
    let buf = &mut *core::ptr::addr_of_mut!(BUF);
    let mut len = 0usize;
    while len < buf.len() {
        let r = read(fd, buf.as_mut_ptr().add(len), buf.len() - len);
        if r <= 0 { break; }
        len += r as usize;
    }
    close(fd);
    for line in buf[..len].split(|&c| c == b'\n') {
        if !line.starts_with(b"# pollx ") { continue; }
        let mut v = [0i64; 5];
        let mut k = 0;
        for f in line[8..].split(|&c| c == b' ').filter(|f| !f.is_empty()) {
            if f[0].is_ascii_digit() && k < 5 {
                let mut x = 0i64;
                for &c in f { x = x * 10 + (c - b'0') as i64; }
                v[k] = x; k += 1;
            }
        }
        return v;
    }
    [0; 5]
}

#[cfg(target_arch = "aarch64")]
const NR_PPOLL: c_long = 73;
#[cfg(target_arch = "x86_64")]
const NR_PPOLL: c_long = 271;

/// The kernel's ppoll, bypassing libc (whose poll() may not be the kernel's).
unsafe fn kpoll(fds: *mut PollFd, nfds: usize, timeout_ms: c_int) -> c_int {
    let ts = timespec { tv_sec: (timeout_ms / 1000) as i64, tv_nsec: ((timeout_ms % 1000) as i64) * 1_000_000 };
    let tsp = if timeout_ms < 0 { core::ptr::null() } else { &ts as *const timespec };
    syscall(NR_PPOLL, fds, nfds, tsp, core::ptr::null::<u64>(), 8usize) as c_int
}

const RT_FDS: usize = 8;

#[repr(C)]
struct RtArgs { efds: [c_int; RT_FDS], ack: c_int, rounds: usize }

extern "C" fn rt_pinger(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &mut *(arg as *mut RtArgs);
        for i in 0..a.rounds {
            efd_bump(a.efds[i % RT_FDS]);
            efd_drain(a.ack); // blocking: the poller's reply
        }
    }
    core::ptr::null_mut()
}

pub unsafe fn pollbench() -> i32 {
    const N: usize = 200_000;
    row(b"getppid", per_call(N, || { getppid(); }));

    let idle = efd_new();
    let mut p1 = [PollFd { fd: idle, events: 1, revents: 0 }];
    row(b"poll1_t0_idle", per_call(N, || { poll(p1.as_mut_ptr(), 1, 0); }));
    let ready = efd_new();
    efd_bump(ready);
    let mut p2 = [PollFd { fd: ready, events: 1, revents: 0 }];
    row(b"poll1_t0_ready", per_call(N, || { poll(p2.as_mut_ptr(), 1, 0); }));

    let mut p8 = [PollFd { fd: -1, events: 1, revents: 0 }; RT_FDS];
    let mut efds = [-1 as c_int; RT_FDS];
    for i in 0..RT_FDS { efds[i] = efd_new(); p8[i].fd = efds[i]; }
    row(b"poll8_t0_idle", per_call(N / 4, || { poll(p8.as_mut_ptr(), RT_FDS as u64, 0); }));
    row(b"ppoll1_t0_idle", per_call(N, || { kpoll(p1.as_mut_ptr(), 1, 0); }));
    row(b"ppoll1_t0_ready", per_call(N, || { kpoll(p2.as_mut_ptr(), 1, 0); }));
    row(b"ppoll8_t0_idle", per_call(N / 4, || { kpoll(p8.as_mut_ptr(), RT_FDS, 0); }));

    // Blocking poll over 8 eventfds, woken through one at a time.
    let ack = efd_new();
    let mut a = RtArgs { efds, ack, rounds: 5000 };
    let (c0, pr0, k0) = pollx();
    let mut th: pthread_t = core::ptr::null_mut();
    let t0 = now_ms();
    pthread_create(&mut th, core::ptr::null(), rt_pinger, &mut a as *mut _ as *mut c_void);
    let mut done = 0usize;
    let mut calls = 0i64;
    while done < a.rounds {
        let n = kpoll(p8.as_mut_ptr(), RT_FDS, 5000);
        calls += 1;
        if n <= 0 { break; }
        for p in p8.iter_mut() {
            if p.revents & 1 != 0 { efd_drain(p.fd); done += 1; efd_bump(ack); }
        }
    }
    pthread_join(th, core::ptr::null_mut());
    let ms = now_ms() - t0;
    let (c1, pr1, k1) = pollx();
    write(1, b"pollbench ppoll8_block_roundtrip: rounds=".as_ptr(), 41); write_i64(1, done as i64);
    write(1, b" calls=".as_ptr(), 7); write_i64(1, calls);
    write(1, b" wall_us_per_round=".as_ptr(), 19); write_i64(1, ms * 1000 / done.max(1) as i64);
    write(1, b" kernel_calls=".as_ptr(), 14); write_i64(1, c1 - c0);
    write(1, b" probes_per_call_x100=".as_ptr(), 22);
    write_i64(1, if c1 > c0 { (pr1 - pr0) * 100 / (c1 - c0) } else { -1 });
    write(1, b" parks=".as_ptr(), 7); write_i64(1, k1 - k0);
    write(1, b"\n".as_ptr(), 1);
    0
}
