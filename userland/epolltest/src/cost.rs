//! `epolltest cost` / `epolltest costbench` (lane epollcost): the cost of an
//! epoll_wait whose set nests other epoll instances, the shape of
//! cosmic-comp's calloop loop (and cosmic-panel's): an outer instance holding
//! eventfd pings, a frame timerfd, a signalfd, the Wayland listening socket,
//! a "DRM" fd, wayland-server's own epoll instance (one AF_UNIX end per
//! client) and libinput's (one fd per input device).
//!
//! On the laptop during YouTube playback cosmic-comp spent 21 % of a core in
//! epoll_pwait at 368 µs per call, and cosmic-panel ~470 µs per stray wake.
//! The ready list re-probed a nested registration on EVERY pass, and a
//! nested probe asks every registration of the inner instance twice (its
//! readiness, then its tag), plus a third time whenever the listener filter
//! is rebuilt — which any epoll_ctl anywhere in the system forced, since the
//! nested answer was judged stale against the global ctl generation.
//!
//! `cost` runs the nested ready-list correctness cases (LT, ET, ONESHOT
//! re-arm, an inner fd closed, a dup'd inner fd, an inner ADD while parked)
//! and then the bench with a loose performance assertion (probes per call,
//! not time, so it holds under TCG). `costbench` prints the bench only.

use crate::*;

extern "C" {
    pub fn fork() -> c_int;
    pub fn waitpid(pid: c_int, stat_loc: *mut c_int, options: c_int) -> c_int;
    pub fn dup(fd: c_int) -> c_int;
    pub fn unlink(path: *const u8) -> c_int;
}

const AF_UNIX: c_int = 1;
const SOCK_STREAM: c_int = 1;
const SIGKILL: c_int = 9;
const SIGUSR2: c_int = 12;

#[repr(C)]
struct Itimerspec { interval: timespec, value: timespec }

unsafe fn ep_add_u64(ep: c_int, fd: c_int, events: c_uint, data: u64) -> bool {
    let mut ev = epoll_event { events, data: epoll_data { u64: data } };
    epoll_ctl(ep, EPOLL_CTL_ADD, fd, &mut ev) == 0
}

unsafe fn ep_mod(ep: c_int, fd: c_int, events: c_uint) -> bool {
    let mut ev = epoll_event { events, data: epoll_data { fd } };
    epoll_ctl(ep, EPOLL_CTL_MOD, fd, &mut ev) == 0
}

unsafe fn pair() -> [c_int; 2] {
    let mut p = [-1 as c_int; 2];
    if socketpair(AF_UNIX, SOCK_STREAM, 0, p.as_mut_ptr()) != 0 { return [-1, -1]; }
    p
}

unsafe fn drain(fd: c_int) {
    let mut b = [0u8; 64];
    read(fd, b.as_mut_ptr(), 64);
}

// ── /proc/wakestat counters ──────────────────────────────────────────────────

#[derive(Clone, Copy, Default)]
struct Ws { probes: i64, inner: i64, passes: i64, useless: i64, probe_ns: i64, gencancel: i64 }

/// The value after `key` on the wakestat line starting with `prefix`.
fn ws_field(buf: &[u8], prefix: &[u8], key: &[u8]) -> i64 {
    for line in buf.split(|&c| c == b'\n') {
        if !line.starts_with(prefix) { continue; }
        let mut it = line.split(|&c| c == b' ').filter(|f| !f.is_empty());
        while let Some(f) = it.next() {
            if f == key {
                let mut x = 0i64;
                for &c in it.next().unwrap_or(b"") { if c.is_ascii_digit() { x = x * 10 + (c - b'0') as i64; } }
                return x;
            }
        }
    }
    0
}

unsafe fn wakestat() -> Ws {
    let fd = open(b"/proc/wakestat\0".as_ptr(), 0);
    if fd < 0 { return Ws::default(); }
    static mut BUF: [u8; 65536] = [0; 65536];
    let buf = &mut *core::ptr::addr_of_mut!(BUF);
    let mut len = 0usize;
    while len < buf.len() {
        let n = read(fd, buf.as_mut_ptr().add(len), buf.len() - len);
        if n <= 0 { break; }
        len += n as usize;
    }
    close(fd);
    let b = &buf[..len];
    Ws {
        probes: ws_field(b, b"# epoll calls", b"probes"),
        inner: ws_field(b, b"# epollx", b"inner"),
        passes: ws_field(b, b"# epollx", b"passes"),
        useless: ws_field(b, b"# epollx", b"useless"),
        probe_ns: ws_field(b, b"# epollx", b"probe_ns"),
        gencancel: ws_field(b, b"# epollx", b"gencancel"),
    }
}

// ── the bench ────────────────────────────────────────────────────────────────

const NW: usize = 32; // wayland clients (inner instance W)
const NL: usize = 4;  // input devices (inner instance L)

const T_PING: u64 = 1 << 32;
const T_TIMER: u64 = 2 << 32;
const T_SIG: u64 = 3 << 32;
const T_LISTEN: u64 = 4 << 32;
const T_DRM: u64 = 5 << 32;
const T_W: u64 = 6 << 32;
const T_L: u64 = 7 << 32;

#[repr(C)]
struct Client { cl: [c_int; NW], rounds: usize, done: usize, pace_us: u32 }

/// A Wayland client storm: one request on client k, then wait for the reply
/// (a commit and its frame callback), round-robin over the clients.
extern "C" fn client_thread(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &mut *(arg as *mut Client);
        let mut b = [0u8; 8];
        for r in 0..a.rounds {
            let fd = a.cl[r % NW];
            if write(fd, b"c".as_ptr(), 1) != 1 { break; }
            if read(fd, b.as_mut_ptr(), 1) != 1 { break; }
            if a.pace_us != 0 { usleep(a.pace_us); }
            a.done += 1;
        }
    }
    core::ptr::null_mut()
}

/// Unrelated traffic from another process: AF_UNIX ping-pong on sockets
/// nobody here watches, and epoll_ctl re-arms on a private instance (what
/// calloop's ONESHOT re-arms and relibc's poll() do all over the desktop;
/// each one moves the kernel's global epoll_ctl generation).
unsafe fn noise_child() -> ! {
    let mut t = [[-1 as c_int; 2]; 8];
    for p in t.iter_mut() { *p = pair(); }
    let ep = epoll_create1(0);
    let mut e = [-1 as c_int; 4];
    for f in e.iter_mut() { *f = efd_new(); ep_add(ep, *f, EPOLLIN); }
    let mut b = [0u8; 1];
    let mut i = 0usize;
    loop {
        let s = t[i % 8];
        write(s[0], b"n".as_ptr(), 1);
        read(s[1], b.as_mut_ptr(), 1);
        ep_mod(ep, e[i % 4], EPOLLIN | EPOLLONESHOT);
        i += 1;
        if i % 64 == 0 { usleep(1000); }
    }
}

struct Comp { e: c_int, w: c_int, l: c_int, pings: [c_int; 3], tfd: c_int, sfd: c_int, lis: c_int,
              drm: [c_int; 2], srv: [c_int; NW], cl: [c_int; NW], inp: [[c_int; 2]; NL] }

unsafe fn comp_setup() -> Option<Comp> {
    let mut c = Comp { e: epoll_create1(0), w: epoll_create1(0), l: epoll_create1(0), pings: [-1; 3],
                       tfd: -1, sfd: -1, lis: -1, drm: [-1; 2], srv: [-1; NW], cl: [-1; NW], inp: [[-1; 2]; NL] };
    if c.e < 0 || c.w < 0 || c.l < 0 { return None; }
    for (i, p) in c.pings.iter_mut().enumerate() {
        *p = efd_new();
        if !ep_add_u64(c.e, *p, EPOLLIN, T_PING | i as u64) { return None; }
    }
    c.tfd = syscall(nr::TIMERFD_CREATE, CLOCK_MONOTONIC as i64, O_NONBLOCK as i64) as c_int;
    let its = Itimerspec { interval: timespec { tv_sec: 0, tv_nsec: 16_666_667 },
                           value: timespec { tv_sec: 0, tv_nsec: 16_666_667 } };
    if c.tfd < 0 || syscall(nr::TIMERFD_SETTIME, c.tfd as i64, 0i64, &its as *const _ as i64, 0i64) != 0
        || !ep_add_u64(c.e, c.tfd, EPOLLIN, T_TIMER) { return None; }
    let mask: sigset_t = 1 << (SIGUSR2 - 1);
    sigprocmask(SIG_BLOCK, &mask, core::ptr::null_mut());
    c.sfd = syscall(nr::SIGNALFD4, -1i64, &mask as *const _ as i64, 8i64, O_NONBLOCK as i64) as c_int;
    if c.sfd < 0 || !ep_add_u64(c.e, c.sfd, EPOLLIN, T_SIG) { return None; }
    c.lis = socket(AF_UNIX, SOCK_STREAM, 0);
    let mut sa = sockaddr_un { sun_family: AF_UNIX as u16, sun_path: [0; 108] };
    let path = b"/tmp/epollcost-wl\0";
    sa.sun_path[..path.len()].copy_from_slice(path);
    unlink(path.as_ptr());
    if c.lis < 0 || bind_un(c.lis, &sa, core::mem::size_of::<sockaddr_un>() as u32) != 0
        || listen(c.lis, 8) != 0 || !ep_add_u64(c.e, c.lis, EPOLLIN, T_LISTEN) { return None; }
    if pipe2(c.drm.as_mut_ptr(), O_NONBLOCK) != 0 || !ep_add_u64(c.e, c.drm[0], EPOLLIN, T_DRM) { return None; }
    for k in 0..NW {
        let p = pair();
        if p[0] < 0 { return None; }
        c.srv[k] = p[0]; c.cl[k] = p[1];
        if !ep_add_u64(c.w, p[0], EPOLLIN, k as u64) { return None; }
    }
    for k in 0..NL {
        if pipe2(c.inp[k].as_mut_ptr(), O_NONBLOCK) != 0 || !ep_add_u64(c.l, c.inp[k][0], EPOLLIN, k as u64) {
            return None;
        }
    }
    if !ep_add_u64(c.e, c.w, EPOLLIN, T_W) || !ep_add_u64(c.e, c.l, EPOLLIN, T_L) { return None; }
    Some(c)
}

unsafe fn comp_close(c: &Comp) {
    for &f in c.pings.iter().chain(c.srv.iter()).chain(c.cl.iter()).chain(c.drm.iter()) { close(f); }
    for p in c.inp.iter() { close(p[0]); close(p[1]); }
    for f in [c.tfd, c.sfd, c.lis, c.w, c.l, c.e] { close(f); }
    unlink(b"/tmp/epollcost-wl\0".as_ptr());
}

pub struct CostResult { pub rounds: i64, pub calls: i64, pub ns_per_call: i64, pub parks_x100: i64,
                        pub useless_x100: i64, pub probes_x10: i64, pub inner_x10: i64 }

/// One run: `rounds` client round trips through the compositor-shaped loop,
/// with (`noise`) or without unrelated traffic from another process.
unsafe fn comp_bench(rounds: usize, noise: bool) -> Option<CostResult> {
    let c = comp_setup()?;
    let child = if noise {
        let pid = fork();
        if pid == 0 { noise_child(); }
        usleep(50_000);
        pid
    } else { -1 };
    // With noise the client paces its requests (one per ~300 µs, a busy
    // frame rate), so the waiter really parks between them, where stray
    // wakes would land.
    let mut cl = Client { cl: c.cl, rounds, done: 0, pace_us: if noise { 300 } else { 0 } };
    let mut out: [epoll_event; 16] = core::mem::zeroed();
    let mut inn: [epoll_event; 32] = core::mem::zeroed();
    // Warm the snapshots (first call on each instance is a full pass).
    epoll_wait(c.e, out.as_mut_ptr(), 16, 0);
    epoll_wait(c.w, inn.as_mut_ptr(), 32, 0);
    let (c0, n0) = epoll_sysstat();
    let p0 = epoll_own_parks();
    let w0 = wakestat();
    let t0 = now_ms();
    let mut th: pthread_t = core::ptr::null_mut();
    pthread_create(&mut th, core::ptr::null(), client_thread, &mut cl as *mut _ as *mut c_void);
    let mut served = 0usize;
    let mut b = [0u8; 64];
    while served < rounds {
        let n = epoll_wait(c.e, out.as_mut_ptr(), 16, 2000);
        if n <= 0 { break; }
        for i in 0..(n as usize).min(16) {
            let d = out[i].data.u64;
            match d & !0xffff_ffff {
                T_W => {
                    // wayland-server's dispatch_clients: a zero-timeout wait on
                    // its own instance, then every ready client.
                    let m = epoll_wait(c.w, inn.as_mut_ptr(), 32, 0);
                    for j in 0..(m.max(0) as usize).min(32) {
                        let k = inn[j].data.u64 as usize;
                        if k < NW && read(c.srv[k], b.as_mut_ptr(), 64) > 0 {
                            write(c.srv[k], b"r".as_ptr(), 1);
                            served += 1;
                        }
                    }
                }
                T_L => {
                    let m = epoll_wait(c.l, inn.as_mut_ptr(), 32, 0);
                    for j in 0..(m.max(0) as usize).min(32) {
                        let k = inn[j].data.u64 as usize;
                        if k < NL { drain(c.inp[k][0]); }
                    }
                }
                T_TIMER => drain(c.tfd),
                T_PING => drain(c.pings[(d & 3) as usize % 3]),
                T_DRM => drain(c.drm[0]),
                T_SIG => drain(c.sfd),
                _ => {}
            }
        }
    }
    let ms = now_ms() - t0;
    let (c1, n1) = epoll_sysstat();
    let p1 = epoll_own_parks();
    let w1 = wakestat();
    // A run cut short leaves the client blocked on its reply: hang up on it.
    let mut c = c;
    if served < rounds {
        for f in c.srv.iter_mut() { close(*f); *f = -1; }
    }
    pthread_join(th, core::ptr::null_mut());
    if child > 0 { kill(child, SIGKILL); let mut st = 0; waitpid(child, &mut st, 0); }
    comp_close(&c);
    let calls = (c1 - c0).max(1);
    let r = CostResult {
        rounds: served as i64, calls: c1 - c0, ns_per_call: (n1 - n0) / calls,
        parks_x100: (p1 - p0) * 100 / calls, useless_x100: (w1.useless - w0.useless) * 100 / calls,
        probes_x10: (w1.probes - w0.probes) * 10 / calls, inner_x10: (w1.inner - w0.inner) * 10 / calls,
    };
    let tag: &[u8] = if noise { b"cost comp+noise:" } else { b"cost comp:" };
    write(1, tag.as_ptr(), tag.len());
    write(1, b" rounds=".as_ptr(), 8); write_i64(1, r.rounds);
    write(1, b" calls=".as_ptr(), 7); write_i64(1, r.calls);
    write(1, b" kernel_us_per_call=".as_ptr(), 20); write_x(r.ns_per_call, 1000);
    write(1, b" parks_per_call=".as_ptr(), 16); write_x(r.parks_x100, 100);
    write(1, b" useless_wakes_per_call=".as_ptr(), 24); write_x(r.useless_x100, 100);
    write(1, b" probes_per_call=".as_ptr(), 17); write_x(r.probes_x10, 10);
    write(1, b" inner_probes_per_call=".as_ptr(), 23); write_x(r.inner_x10, 10);
    write(1, b" probe_us_per_call=".as_ptr(), 19); write_x((w1.probe_ns - w0.probe_ns) / calls, 1000);
    write(1, b" gencancel=".as_ptr(), 11); write_i64(1, w1.gencancel - w0.gencancel);
    write(1, b" passes=".as_ptr(), 8); write_i64(1, w1.passes - w0.passes);
    write(1, b" wall_us_per_round=".as_ptr(), 19); write_i64(1, ms * 1000 / (served.max(1) as i64));
    write(1, b"\n".as_ptr(), 1);
    Some(r)
}

/// `v / div` with one or two decimals.
unsafe fn write_x(v: i64, div: i64) {
    write_i64(1, v / div);
    write(1, b".".as_ptr(), 1);
    let frac = (v % div).abs();
    if div == 1000 { let f = frac / 10; if f < 10 { write(1, b"0".as_ptr(), 1); } write_i64(1, f); }
    else if div == 100 { if frac < 10 { write(1, b"0".as_ptr(), 1); } write_i64(1, frac); }
    else { write_i64(1, frac); }
}

// ── stray wakes into a nested set ────────────────────────────────────────────
//
// cosmic-panel on the laptop: 239 parks/s for 9 returns, ~470 µs of kernel
// time per park. A wake that concerns an inner registration but leaves
// nothing to report (here: a helper writes a byte into a pipe the inner
// instance watches and takes it back before the waiter looks) costs the
// waiter one re-probe pass and a re-park: a "useless wake".

#[repr(C)]
struct Stray { p: [c_int; 2], quit: c_int, pushes: usize }

extern "C" fn stray_thread(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &*(arg as *const Stray);
        let mut b = [0u8; 4];
        for _ in 0..a.pushes {
            write(a.p[1], b"s".as_ptr(), 1);
            read(a.p[0], b.as_mut_ptr(), 4);
            usleep(300);
        }
        efd_bump(a.quit);
    }
    core::ptr::null_mut()
}

pub struct StrayResult { pub useless: i64, pub ns_per_push: i64, pub inner_x10: i64 }

unsafe fn stray_bench(pushes: usize) -> Option<StrayResult> {
    let c = comp_setup()?;
    let mut st = Stray { p: [-1; 2], quit: efd_new(), pushes };
    if pipe2(st.p.as_mut_ptr(), O_NONBLOCK) != 0 || !ep_add_u64(c.w, st.p[0], EPOLLIN, NW as u64)
        || !ep_add_u64(c.e, st.quit, EPOLLIN, T_DRM | 1) { return None; }
    // The frame timer would be a real event: stop it.
    let off: Itimerspec = core::mem::zeroed();
    syscall(nr::TIMERFD_SETTIME, c.tfd as i64, 0i64, &off as *const _ as i64, 0i64);
    drain(c.tfd);
    let mut out: [epoll_event; 16] = core::mem::zeroed();
    epoll_wait(c.e, out.as_mut_ptr(), 16, 0);
    let (c0, n0) = epoll_sysstat();
    let w0 = wakestat();
    let mut th: pthread_t = core::ptr::null_mut();
    pthread_create(&mut th, core::ptr::null(), stray_thread, &st as *const _ as *mut c_void);
    let (mut other, mut quit) = (0i64, false);
    while !quit {
        let n = epoll_wait(c.e, out.as_mut_ptr(), 16, 5000);
        if n <= 0 { break; }
        for i in 0..(n as usize).min(16) {
            if out[i].data.u64 == T_DRM | 1 { quit = true; } else { other += 1; }
        }
    }
    pthread_join(th, core::ptr::null_mut());
    let (c1, n1) = epoll_sysstat();
    let w1 = wakestat();
    close(st.p[0]); close(st.p[1]); close(st.quit);
    comp_close(&c);
    let r = StrayResult { useless: w1.useless - w0.useless, ns_per_push: (n1 - n0) / pushes as i64,
                          inner_x10: (w1.inner - w0.inner) * 10 / pushes as i64 };
    write(1, b"cost stray: pushes=".as_ptr(), 19); write_i64(1, pushes as i64);
    write(1, b" calls=".as_ptr(), 7); write_i64(1, c1 - c0);
    write(1, b" reported=".as_ptr(), 10); write_i64(1, other);
    write(1, b" useless_wakes=".as_ptr(), 15); write_i64(1, r.useless);
    write(1, b" kernel_us_per_push=".as_ptr(), 20); write_x(r.ns_per_push, 1000);
    write(1, b" inner_probes_per_push=".as_ptr(), 23); write_x(r.inner_x10, 10);
    write(1, b"\n".as_ptr(), 1);
    Some(r)
}

pub unsafe fn costbench() -> i32 {
    comp_bench(2000, false);
    comp_bench(2000, true);
    stray_bench(1000);
    0
}

// ── correctness cases for nested sets under the ready list ───────────────────

/// Outer E ⊃ inner W ⊃ 16 socketpair ends, plus an eventfd directly in E.
struct Nest { e: c_int, w: c_int, q: c_int, s: [[c_int; 2]; 16] }

unsafe fn nest_new(events: c_uint) -> Option<Nest> {
    let mut n = Nest { e: epoll_create1(0), w: epoll_create1(0), q: efd_new(), s: [[-1; 2]; 16] };
    if n.e < 0 || n.w < 0 || n.q < 0 { return None; }
    for p in n.s.iter_mut() {
        *p = pair();
        if p[0] < 0 || !ep_add(n.w, p[0], events) { return None; }
    }
    if !ep_add(n.e, n.w, EPOLLIN) || !ep_add(n.e, n.q, EPOLLIN) { return None; }
    Some(n)
}

unsafe fn nest_close(n: &Nest) {
    for p in n.s.iter() { close(p[0]); close(p[1]); }
    close(n.q); close(n.w); close(n.e);
}

/// E's answer as a bit set over [W, q].
unsafe fn e_seen(n: &Nest, timeout: c_int) -> (i32, u64) { ep_seen(n.e, timeout, &[n.w, n.q]) }

#[repr(C)]
struct Later { fd: c_int, ms: u32 }

extern "C" fn later_write(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &*(arg as *const Later);
        usleep(a.ms * 1000);
        write(a.fd, b"x".as_ptr(), 1);
        // Linger past the verdict: a thread exit wakes broadcast, which would
        // rescue a lost wake and hide it.
        usleep(1_200_000);
    }
    core::ptr::null_mut()
}

/// Wait on E for up to 3 s while a thread writes to `fd` after 100 ms:
/// (E's answer, ms waited).
unsafe fn e_wait_for_write(n: &Nest, fd: c_int) -> ((i32, u64), i64) {
    let a = Later { fd, ms: 100 };
    let mut th: pthread_t = core::ptr::null_mut();
    pthread_create(&mut th, core::ptr::null(), later_write, &a as *const _ as *mut c_void);
    let t0 = now_ms();
    let r = e_seen(n, 3000);
    let ms = now_ms() - t0;
    pthread_join(th, core::ptr::null_mut());
    (r, ms)
}

// c1. LT inner registrations: an inner event is reported through E, again
// and again while undrained, and no more once drained; a write while parked
// wakes E within a second, on every client in turn.
unsafe fn cost_nested_lt() -> bool {
    let name = b"cost_nested_lt\0";
    let n = match nest_new(EPOLLIN) { Some(n) => n, None => return report(name, false) };
    let warm = e_seen(&n, 0);
    let mut ok = warm.0 == 0;
    let mut b = [0u8; 8];
    for k in 0..16 {
        write(n.s[k][1], b"x".as_ptr(), 1);
        let a = e_seen(&n, 1000);
        let again = e_seen(&n, 0);
        read(n.s[k][0], b.as_mut_ptr(), 8);
        let after = e_seen(&n, 0);
        if a != (1, 1) || again != (1, 1) || after.0 != 0 { ok = false; }
    }
    let (r, ms) = e_wait_for_write(&n, n.s[7][1]);
    read(n.s[7][0], b.as_mut_ptr(), 8);
    nest_close(&n);
    report(name, ok && r == (1, 1) && ms < 1000)
}

// c2. ET inner registrations: E reports W while the inner edge is not
// consumed; once a wait on W consumes it, E is quiet although the data is
// still there; a new write is a new edge.
unsafe fn cost_nested_et() -> bool {
    let name = b"cost_nested_et\0";
    let n = match nest_new(EPOLLIN | EPOLLET) { Some(n) => n, None => return report(name, false) };
    e_seen(&n, 0);
    let mut ok = true;
    for k in [0usize, 5, 15] {
        write(n.s[k][1], b"x".as_ptr(), 1);
        let a = e_seen(&n, 1000);
        let a2 = e_seen(&n, 0);
        let inner = ep_seen(n.w, 0, &[n.s[k][0]]);
        let quiet = e_seen(&n, 0);
        write(n.s[k][1], b"y".as_ptr(), 1);
        let b2 = e_seen(&n, 1000);
        let inner2 = ep_seen(n.w, 0, &[n.s[k][0]]);
        let quiet2 = e_seen(&n, 0);
        if a != (1, 1) || a2 != (1, 1) || inner != (1, 1) || quiet.0 != 0 || b2 != (1, 1)
            || inner2 != (1, 1) || quiet2.0 != 0 { ok = false; }
        drain(n.s[k][0]);
    }
    nest_close(&n);
    report(name, ok)
}

// c3. ONESHOT inner registrations: after the inner wait fires one, E is quiet
// although its data is still queued; a MOD re-arm of the still-ready fd makes
// E ready at once; a MOD re-arm of a drained fd, made while E's waiter is
// away, is woken by its next write (the filter must learn the re-armed fd).
unsafe fn cost_nested_oneshot() -> bool {
    let name = b"cost_nested_oneshot\0";
    let n = match nest_new(EPOLLIN | EPOLLONESHOT) { Some(n) => n, None => return report(name, false) };
    e_seen(&n, 0);
    let k = 3usize;
    write(n.s[k][1], b"x".as_ptr(), 1);
    let a = e_seen(&n, 1000);
    let fired = ep_seen(n.w, 0, &[n.s[k][0]]);
    let quiet = e_seen(&n, 0);
    let rearm = ep_mod(n.w, n.s[k][0], EPOLLIN | EPOLLONESHOT);
    let ready = e_seen(&n, 1000);
    let fired2 = ep_seen(n.w, 0, &[n.s[k][0]]);
    drain(n.s[k][0]);
    let quiet2 = e_seen(&n, 0);
    let rearm2 = ep_mod(n.w, n.s[k][0], EPOLLIN | EPOLLONESHOT);
    let (late, ms) = e_wait_for_write(&n, n.s[k][1]);
    drain(n.s[k][0]);
    nest_close(&n);
    report(name, a == (1, 1) && fired == (1, 1) && quiet.0 == 0 && rearm && ready == (1, 1)
                 && fired2 == (1, 1) && quiet2.0 == 0 && rearm2 && late == (1, 1) && ms < 1000)
}

// c4. An inner fd closed while in the set (its last reference: the item
// goes): E never reports it, and the other clients still wake E.
unsafe fn cost_nested_close() -> bool {
    let name = b"cost_nested_close\0";
    let mut n = match nest_new(EPOLLIN) { Some(n) => n, None => return report(name, false) };
    e_seen(&n, 0);
    // Readable, then closed: its peer's data dies with it.
    write(n.s[2][1], b"x".as_ptr(), 1);
    let a = e_seen(&n, 1000);
    close(n.s[2][0]);
    let gone = e_seen(&n, 0);
    // The peer's hang-up is no event for anyone here.
    close(n.s[2][1]);
    n.s[2] = pair(); // keep nest_close's bookkeeping simple
    let gone2 = e_seen(&n, 50);
    let (r, ms) = e_wait_for_write(&n, n.s[9][1]);
    drain(n.s[9][0]);
    nest_close(&n);
    report(name, a == (1, 1) && gone.0 == 0 && gone2.0 == 0 && r == (1, 1) && ms < 1000)
}

// c5. A dup'd inner fd: closing the registered number keeps the item alive
// (the description lives on in the dup), so its events still reach E and W.
unsafe fn cost_nested_dup() -> bool {
    let name = b"cost_nested_dup\0";
    let mut n = match nest_new(EPOLLIN) { Some(n) => n, None => return report(name, false) };
    e_seen(&n, 0);
    let orig = n.s[4][0];
    let d = dup(orig);
    close(orig);
    n.s[4][0] = d;
    let quiet = e_seen(&n, 0);
    let (r, ms) = e_wait_for_write(&n, n.s[4][1]);
    // W still names the registration by its original number (the data word).
    let inner = ep_seen(n.w, 0, &[orig]);
    drain(d);
    let after = e_seen(&n, 0);
    nest_close(&n);
    report(name, d >= 0 && quiet.0 == 0 && r == (1, 1) && ms < 1000 && inner == (1, 1) && after.0 == 0)
}

// c6. An inner ADD while E's waiter is parked, of a fd that becomes ready
// only later: the waiter must wake for it. Also an outer ADD the same way.
#[repr(C)]
struct AddLater { ep: c_int, fd: c_int, wfd: c_int }

extern "C" fn add_then_write(arg: *mut c_void) -> *mut c_void {
    unsafe {
        let a = &*(arg as *const AddLater);
        usleep(100_000);
        ep_add(a.ep, a.fd, EPOLLIN);
        usleep(100_000);
        write(a.wfd, b"x".as_ptr(), 1);
        usleep(1_200_000);
    }
    core::ptr::null_mut()
}

unsafe fn cost_nested_add_while_parked() -> bool {
    let name = b"cost_nested_add_while_parked\0";
    let n = match nest_new(EPOLLIN) { Some(n) => n, None => return report(name, false) };
    e_seen(&n, 0);
    let mut ok = true;
    for (target, expect) in [(n.w, 1u64), (n.e, 0u64)] {
        let mut p = [-1 as c_int; 2];
        if pipe2(p.as_mut_ptr(), O_NONBLOCK) != 0 { ok = false; break; }
        let a = AddLater { ep: target, fd: p[0], wfd: p[1] };
        let mut th: pthread_t = core::ptr::null_mut();
        pthread_create(&mut th, core::ptr::null(), add_then_write, &a as *const _ as *mut c_void);
        let t0 = now_ms();
        let r = ep_seen(n.e, 3000, &[n.w, p[0]]);
        let ms = now_ms() - t0;
        pthread_join(th, core::ptr::null_mut());
        let want = if expect == 1 { (1, 1) } else { (1, 2) };
        if r != want || ms > 1000 { ok = false; }
        close(p[0]); close(p[1]);
        e_seen(&n, 0);
    }
    nest_close(&n);
    report(name, ok)
}

pub unsafe fn cost_main() -> i32 {
    let mut failures = 0;
    if !cost_nested_lt() { failures += 1; }
    if !cost_nested_et() { failures += 1; }
    if !cost_nested_oneshot() { failures += 1; }
    if !cost_nested_close() { failures += 1; }
    if !cost_nested_dup() { failures += 1; }
    if !cost_nested_add_while_parked() { failures += 1; }
    // The bench, judged by work per call (holds under TCG): a client round
    // trip through the nested set must cost a few probes, not a re-probe of
    // every inner registration (2-3 x 36 before lane epollcost), and must
    // not make the waiter take stray wakes.
    let name = b"cost_nested_bench\0";
    let ok = match comp_bench(1000, true) {
        Some(r) => r.rounds == 1000 && r.probes_x10 + r.inner_x10 <= 80 && r.useless_x100 <= 50,
        None => false,
    };
    if !report(name, ok) { failures += 1; }
    // A stray wake on one inner registration re-probes that one, not the
    // whole inner instance (~4 x 37 before).
    let name = b"cost_nested_stray\0";
    let ok = match stray_bench(500) { Some(r) => r.inner_x10 <= 40, None => false };
    if !report(name, ok) { failures += 1; }
    write_summary(8, failures);
    puts(b"--- epolltest cost done ---\n\0".as_ptr());
    failures
}
