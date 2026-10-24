//! `epolltest stat` (lane epattr): `/proc/epollstat` attributes parks, useful
//! and useless wakes to the epoll instance (thread group + epfd) and pins
//! useless wakes on registered fds. The check parks a wait on an idle
//! eventfd (timeout), then a helper thread blips a second eventfd (write then
//! drain back to back, so the wait is woken and finds nothing when the drain
//! wins the race), and reads our row back.

use crate::*;

extern "C" { fn _exit(code: c_int) -> !; }

fn parse_u(s: &[u8]) -> Option<i64> {
    if s.is_empty() { return None; }
    let mut x = 0i64;
    for &c in s { if !c.is_ascii_digit() { return None; } x = x * 10 + (c - b'0') as i64; }
    Some(x)
}

/// Our row for `epfd`: (parks, useful, useless, line has "<blip>:" entry).
unsafe fn row(epfd: c_int, blip: c_int) -> Option<(i64, i64, i64, bool)> {
    let fd = open(b"/proc/epollstat\0".as_ptr(), 0);
    if fd < 0 { return None; }
    static mut BUF: [u8; 65536] = [0; 65536];
    let buf = &mut *core::ptr::addr_of_mut!(BUF);
    let mut len = 0usize;
    while len < buf.len() {
        let r = read(fd, buf.as_mut_ptr().add(len), buf.len() - len);
        if r <= 0 { break; }
        len += r as usize;
    }
    close(fd);
    let me = getpid() as i64;
    // A closed instance's row outlives it; the guards never close theirs,
    // so at most one row names a live epfd (keep the highest handle).
    let mut best: Option<(i64, (i64, i64, i64, bool))> = None;
    for line in buf[..len].split(|&c| c == b'\n') {
        if line.starts_with(b"#") { continue; }
        let mut it = line.split(|&c| c == b' ').filter(|f| !f.is_empty());
        let f: [Option<i64>; 7] = [0; 7].map(|_| it.next().and_then(parse_u));
        // tgid == our pid on the main thread
        if f[0] != Some(me) || f[1] != Some(epfd as i64) { continue; }
        let mut key = [0u8; 12];
        let mut n = 0;
        let mut v = blip as u32;
        let mut tmp = [0u8; 10]; let mut t = 0;
        loop { tmp[t] = b'0' + (v % 10) as u8; t += 1; v /= 10; if v == 0 { break; } }
        while t > 0 { t -= 1; key[n] = tmp[t]; n += 1; }
        key[n] = b':'; n += 1;
        let has = line.windows(n + 1).any(|w| w[0] == b' ' && &w[1..] == &key[..n]);
        let (Some(h), Some(p), Some(u), Some(x)) = (f[2], f[3], f[4], f[5]) else { continue };
        if best.map_or(true, |(bh, _)| h > bh) { best = Some((h, (p, u, x, has))); }
    }
    best.map(|(_, r)| r)
}

/// A zero-timeout wait claims the instance's /proc/epollstat row, so a
/// `before` read names this instance, not a closed one with the same epfd.
unsafe fn prime(ep: c_int) {
    let mut ev = epoll_event { events: 0, data: epoll_data { fd: 0 } };
    epoll_wait(ep, &mut ev, 1, 0);
}

extern "C" fn blipper(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let fd = *(arg as *const c_int);
        for _ in 0..40 {
            usleep(5_000);
            efd_bump(fd);
            efd_drain(fd);
        }
    }
    core::ptr::null_mut()
}

pub unsafe fn stat_main() -> i32 {
    let ep = epoll_create1(0);
    let idle = syscall(nr::EVENTFD2, 0i64, 0o4000i64) as c_int;
    let mut blip = syscall(nr::EVENTFD2, 0i64, 0o4000i64) as c_int;
    let ok_setup = ep >= 0 && idle >= 0 && blip >= 0 && ep_add(ep, idle, EPOLLIN) && ep_add(ep, blip, EPOLLIN);
    let mut fail = 0;
    let mut rep = |name: &[u8], ok: bool| {
        write(1, name.as_ptr(), name.len());
        if ok { write(1, b": PASS\n".as_ptr(), 7); } else { write(1, b": FAIL\n".as_ptr(), 7); fail += 1; }
    };
    rep(b"stat_setup", ok_setup);
    if !ok_setup { return 1; }
    let before = row(ep, blip);
    let mut ev = epoll_event { events: 0, data: epoll_data { fd: 0 } };
    let mut th: pthread_t = core::ptr::null_mut();
    pthread_create(&mut th, core::ptr::null(), blipper, &mut blip as *mut c_int as *mut c_void);
    let t0 = now_ms();
    while now_ms() - t0 < 400 {
        epoll_wait(ep, &mut ev, 1, 50);
        efd_drain(blip);
    }
    pthread_join(th, core::ptr::null_mut());
    let after = row(ep, blip);
    rep(b"stat_row_present", after.is_some());
    if let Some((p, u, x, has)) = after {
        let (p0, u0, x0) = before.map(|b| (b.0, b.1, b.2)).unwrap_or((0, 0, 0));
        write(1, b"  parks=".as_ptr(), 8); write_i64(1, p - p0);
        write(1, b" useful=".as_ptr(), 8); write_i64(1, u - u0);
        write(1, b" useless=".as_ptr(), 9); write_i64(1, x - x0);
        write(1, b"\n".as_ptr(), 1);
        rep(b"stat_parks_counted", p - p0 >= 1);
        // Every blip wakes the wait: it either returned events (useful) or
        // found nothing (useless); together they cover the wakes.
        rep(b"stat_wakes_counted", (u - u0) + (x - x0) >= 1);
        // A useless wake from a ring-named wake must be pinned on the blip fd.
        rep(b"stat_useless_attributed", x - x0 == 0 || has);
    }
    // Two tries: boot-time ambient wakes (see exit_wake_keyed) can land in the
    // first window; the bug reads 25+ on every try.
    rep(b"stat_exit_wake_keyed", exit_wake_twice());
    rep(b"stat_ctl_foreign_keyed", ctl_foreign_twice());
    rep(b"stat_pipe_drain_keyed", pipe_drain_keyed() || pipe_drain_keyed());
    if fail == 0 { write(1, b"SUMMARY stat pass\n".as_ptr(), 18); }
    fail
}

/// Lane panelwake guard: a child's exit wakes its parent's wait4 park
/// (`poll_class::WAIT`) and its pidfd pollers (`PIDFD`). Those wakes used to
/// go out bit-only, so every parked poller whose bloom mask held the same
/// bit through an unrelated KEYED interest (an eventfd here) woke, re-probed
/// and re-parked. We park on 96 idle eventfds (keyed bits covering most of
/// the 64 buckets) while 24 children exit; with keyed exit wakes the
/// fingerprints reject every one of them, so the park takes no useless wake.
/// `epolltest xwake`: run `exit_wake_keyed` by itself, so `/proc/wakestat`
/// attributes only its wakes.
pub unsafe fn exit_wake_main() -> i32 {
    let ok = exit_wake_twice();
    let msg: &[u8] = if ok { b"stat_exit_wake_keyed: PASS\n" } else { b"stat_exit_wake_keyed: FAIL\n" };
    write(1, msg.as_ptr(), msg.len());
    if ok { 0 } else { 1 }
}

unsafe fn exit_wake_twice() -> bool {
    // One instance across both tries: a retry on a fresh epoll fd could get
    // the closed one's number and read its stale /proc/epollstat row.
    let ep = epoll_create1(0);
    let ok = ep >= 0 && (exit_wake_keyed(ep) || exit_wake_keyed(ep));
    // Never closed: a later guard's instance must not reuse this fd and
    // handle, whose /proc/epollstat row outlives the close.
    ok
}

unsafe fn exit_wake_keyed(ep: c_int) -> bool {
    const NFD: usize = 96;
    const NKID: usize = 24;
    let mut fds = [-1 as c_int; NFD];
    for f in fds.iter_mut() {
        *f = syscall(nr::EVENTFD2, 0i64, 0o4000i64) as c_int;
        if *f < 0 || !ep_add(ep, *f, EPOLLIN) { return false; }
    }
    let mut kids = [0 as c_int; NKID];
    for (i, k) in kids.iter_mut().enumerate() {
        let pid = crate::cost::fork();
        if pid == 0 { usleep(40_000 + i as u32 * 6_000); _exit(0); }
        *k = pid;
    }
    prime(ep);
    let before = row(ep, -1);
    let mut ev = epoll_event { events: 0, data: epoll_data { fd: 0 } };
    // One wait spanning every exit: a useless wake re-parks in the kernel.
    let n = epoll_wait(ep, &mut ev, 1, 40 + NKID as c_int * 6 + 300);
    let after = row(ep, -1);
    for &k in &kids { let mut st = 0; crate::cost::waitpid(k, &mut st, 0); }
    for &f in &fds { close(f); }
    let b = before.unwrap_or((0, 0, 0, false));
    let Some(a) = after else { return false };
    write(1, b"  exit_wake: useless=".as_ptr(), 21); write_i64(1, a.2 - b.2);
    write(1, b" parks=".as_ptr(), 7); write_i64(1, a.0 - b.0);
    write(1, b"\n".as_ptr(), 1);
    // Bound 1, not 0: the first run after boot (3/3, and only that one) takes
    // exactly one useless wake that is not an exit wake. `epolltest xwake`
    // run alone, or `stat` run a second time, reads 0; /proc/wakestat pins
    // it on an eventfd write (vfs EventFd write site) from the boot-time
    // session teardown. With 96 u16 fingerprints parked, a foreign keyed
    // wake on a shared bloom bit matches one with p ~ 96/65536, and the
    // boot window carries several hundred keyed wakes: a fingerprint
    // collision, inherent to the filter. Bit-only exit/signal wakes (the
    // bug) read 25 here, one per child plus one.
    // parks >= 1: the row is live (the wait parked), not a stale one.
    n == 0 && a.0 - b.0 >= 1 && a.2 - b.2 <= 1
}

/// Lane panelpipe guard: a pipe reader parked in epoll for POLLIN must be
/// woken by data (or EOF), never by its own drain. Both pipe ends used to
/// share one poll key, and every read that did not free a full ring paid a
/// deferred keyed wake owed to the WRITE end; the reader, re-parked by then,
/// took it, re-probed an empty pipe and parked again: one useless wake per
/// drain (cosmic-panel's waker pipe, ~45 per 10 s on the idle desktop).
/// A helper writes one byte NBLIP times; we drain each to EAGAIN. Expect
/// NBLIP useful wakes and no useless one.
pub unsafe fn pipe_wake_main() -> i32 {
    let ok = pipe_drain_keyed() || pipe_drain_keyed();
    let msg: &[u8] = if ok { b"stat_pipe_drain_keyed: PASS\n" } else { b"stat_pipe_drain_keyed: FAIL\n" };
    write(1, msg.as_ptr(), msg.len());
    if ok { 0 } else { 1 }
}

const NBLIP: usize = 20;

extern "C" fn pipe_writer(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let fd = *(arg as *const c_int);
        for _ in 0..NBLIP {
            usleep(10_000);
            write(fd, b"x".as_ptr(), 1);
        }
    }
    core::ptr::null_mut()
}

unsafe fn pipe_drain_keyed() -> bool {
    let ep = epoll_create1(0);
    let mut p = [-1 as c_int; 2];
    // O_NONBLOCK | O_CLOEXEC
    if ep < 0 || pipe2(p.as_mut_ptr(), 0o4000 | 0o2000000) != 0 || !ep_add(ep, p[0], EPOLLIN) { return false; }
    prime(ep);
    let before = row(ep, p[0]);
    let mut wfd = p[1];
    let mut th: pthread_t = core::ptr::null_mut();
    pthread_create(&mut th, core::ptr::null(), pipe_writer, &mut wfd as *mut c_int as *mut c_void);
    let mut ev = epoll_event { events: 0, data: epoll_data { fd: 0 } };
    let mut got = 0usize;
    let t0 = now_ms();
    while got < NBLIP && now_ms() - t0 < 2_000 {
        if epoll_wait(ep, &mut ev, 1, 500) <= 0 { continue; }
        let mut b = [0u8; 64];
        loop {
            let r = read(p[0], b.as_mut_ptr(), b.len());
            if r <= 0 { break; }
            got += r as usize;
        }
    }
    pthread_join(th, core::ptr::null_mut());
    // Let any deferred wake owed by the last drain land while we are parked.
    epoll_wait(ep, &mut ev, 1, 50);
    let after = row(ep, p[0]);
    // `ep` stays open: a retry reusing its fd number would read the closed
    // instance's /proc/epollstat row (useful=0 useless=0, a false pass).
    close(p[0]); close(p[1]);
    let b = before.unwrap_or((0, 0, 0, false));
    let Some(a) = after else { return false };
    write(1, b"  pipe_drain: useful=".as_ptr(), 21); write_i64(1, a.1 - b.1);
    write(1, b" useless=".as_ptr(), 9); write_i64(1, a.2 - b.2);
    write(1, b" bytes=".as_ptr(), 7); write_i64(1, got as i64);
    write(1, b"\n".as_ptr(), 1);
    // Bound 1 for the same inherent fingerprint collision as exit_wake_keyed;
    // the bug reads one useless wake per drain (~NBLIP).
    got == NBLIP && a.1 - b.1 >= 1 && a.2 - b.2 <= 1
}

/// Lane epnameless guard: an epoll_ctl on ANOTHER instance must not wake, or
/// widen the filter of, a waiter that only shares that instance's tag bit.
/// epoll_ctl used to wake bit-only on `poll_tag(EPOLL, slot)` (ADD/MOD of a
/// ready fd) and, for a not-ready fd, to OR the fd's bits into every parked
/// mask holding that bit, with the bit's key refinement dropped: the new fd's
/// later wakes then reached unrelated parkers too, which re-probed nothing
/// they had registered (desktop: ~60 % of all useless epoll wakes). The
/// waiter parks on 96 idle eventfds (its mask covers almost every bit); a
/// helper does NCTL rounds of ADD-not-ready + write, and ADD-ready, on a
/// second instance nobody waits on. Expect no useless wake.
const NCTL: usize = 20;

extern "C" fn ctl_helper(_arg: *mut c_void) -> *mut c_void {
    unsafe {
        let ep = epoll_create1(0);
        for i in 0..NCTL {
            usleep(15_000);
            let e = syscall(nr::EVENTFD2, 0i64, 0o4000i64) as c_int;
            if i % 2 == 0 {
                ep_add(ep, e, EPOLLIN);          // not ready: the widening path
                usleep(5_000);
                let one = 1u64;
                write(e, &one as *const u64 as *const u8, 8);
            } else {
                let one = 1u64;
                write(e, &one as *const u64 as *const u8, 8);
                ep_add(ep, e, EPOLLIN);          // ready now: the wake path
            }
            usleep(5_000);
            close(e);
        }
        close(ep);
    }
    core::ptr::null_mut()
}

unsafe fn ctl_foreign_twice() -> bool {
    let ep = epoll_create1(0); // kept across both tries (stale-row hazard)
    let ok = ep >= 0 && (ctl_foreign_keyed(ep) || ctl_foreign_keyed(ep));
    // Never closed: a later guard's instance must not reuse this fd and
    // handle, whose /proc/epollstat row outlives the close.
    ok
}

unsafe fn ctl_foreign_keyed(ep: c_int) -> bool {
    const NFD: usize = 96;
    let mut fds = [-1 as c_int; NFD];
    for f in fds.iter_mut() {
        *f = syscall(nr::EVENTFD2, 0i64, 0o4000i64) as c_int;
        if *f < 0 || !ep_add(ep, *f, EPOLLIN) { return false; }
    }
    prime(ep);
    let before = row(ep, -1);
    let mut th: pthread_t = core::ptr::null_mut();
    pthread_create(&mut th, core::ptr::null(), ctl_helper, core::ptr::null_mut());
    let mut ev = epoll_event { events: 0, data: epoll_data { fd: 0 } };
    let n = epoll_wait(ep, &mut ev, 1, (NCTL * 25 + 300) as c_int);
    pthread_join(th, core::ptr::null_mut());
    let after = row(ep, -1);
    for &f in &fds { close(f); }
    let b = before.unwrap_or((0, 0, 0, false));
    let Some(a) = after else { return false };
    write(1, b"  ctl_foreign: useless=".as_ptr(), 23); write_i64(1, a.2 - b.2);
    write(1, b" parks=".as_ptr(), 7); write_i64(1, a.0 - b.0);
    write(1, b"\n".as_ptr(), 1);
    // parks >= 1: a live row. Bound 1: the fingerprint-collision residual of
    // exit_wake_keyed (96 parked fingerprints).
    n == 0 && a.0 - b.0 >= 1 && a.2 - b.2 <= 1
}

pub unsafe fn ctl_foreign_main() -> i32 {
    let ok = ctl_foreign_twice();
    let msg: &[u8] = if ok { b"stat_ctl_foreign_keyed: PASS\n" } else { b"stat_ctl_foreign_keyed: FAIL\n" };
    write(1, msg.as_ptr(), msg.len());
    if ok { 0 } else { 1 }
}
