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
        return Some((f[3]?, f[4]?, f[5]?, has));
    }
    None
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
    rep(b"stat_exit_wake_keyed", exit_wake_keyed());
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
unsafe fn exit_wake_keyed() -> bool {
    const NFD: usize = 96;
    const NKID: usize = 24;
    let ep = epoll_create1(0);
    if ep < 0 { return false; }
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
    let before = row(ep, -1);
    let mut ev = epoll_event { events: 0, data: epoll_data { fd: 0 } };
    // One wait spanning every exit: a useless wake re-parks in the kernel.
    let n = epoll_wait(ep, &mut ev, 1, 40 + NKID as c_int * 6 + 300);
    let after = row(ep, -1);
    for &k in &kids { let mut st = 0; crate::cost::waitpid(k, &mut st, 0); }
    for &f in &fds { close(f); }
    close(ep);
    let b = before.unwrap_or((0, 0, 0, false));
    let Some(a) = after else { return false };
    write(1, b"  exit_wake: useless=".as_ptr(), 21); write_i64(1, a.2 - b.2);
    write(1, b" parks=".as_ptr(), 7); write_i64(1, a.0 - b.0);
    write(1, b"\n".as_ptr(), 1);
    n == 0 && a.2 - b.2 == 0
}
