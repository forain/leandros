// `epolltest nest` (lane nestep): lost-wakeup stress for the COSMIC panel's
// shape. Thread A parks in epoll_wait(outer, -1); `outer` holds `inner`
// (EPOLLIN, level) and A drains `inner` with epoll_wait(inner, 0) on wake.
// B (another CPU) creates a socketpair per iteration, registers one end
// (EPOLLIN / EPOLLET / EPOLLIN|EPOLLRDHUP) and writes 24 bytes to the peer,
// alternating write-before-ADD and ADD-before-write, from a thread or from a
// forked child (applet process). Each iteration must be consumed by A within
// 2 s, else the state is dumped and the test fails. A single-level control
// (the fd is registered directly in the epoll A waits on) runs the same loop.

use crate::*;
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering::SeqCst};

extern "C" {
    fn fork() -> c_int;
    fn waitpid(pid: c_int, stat_loc: *mut c_int, options: c_int) -> c_int;
    fn _exit(code: c_int) -> !;
    fn poll(fds: *mut PollFd, n: u64, timeout: c_int) -> c_int;
}

#[repr(C)]
struct PollFd { fd: c_int, events: i16, revents: i16 }

const EPOLLRDHUP: c_uint = 0x2000;
const EPOLL_CTL_DEL: c_int = 2;
const QUIT: u64 = u64::MAX;

static OUTER: AtomicI32 = AtomicI32::new(-1);
static INNER: AtomicI32 = AtomicI32::new(-1);
static NESTED: AtomicI32 = AtomicI32::new(0);
static DONE: AtomicU64 = AtomicU64::new(0);

struct Buf { b: [u8; 256], n: usize }
impl Buf {
    fn new() -> Buf { Buf { b: [0; 256], n: 0 } }
    fn s(&mut self, s: &[u8]) { for &c in s { if self.n < 256 { self.b[self.n] = c; self.n += 1; } } }
    fn d(&mut self, v: i64) {
        if v < 0 { self.s(b"-"); }
        let mut x = if v < 0 { (-v) as u64 } else { v as u64 };
        let mut t = [0u8; 20]; let mut k = 0;
        loop { t[k] = b'0' + (x % 10) as u8; k += 1; x /= 10; if x == 0 { break; } }
        while k > 0 { k -= 1; let c = t[k]; self.s(&[c]); }
    }
    fn x(&mut self, v: u64) {
        self.s(b"0x");
        let mut started = false;
        for sh in (0..16).rev() {
            let nib = ((v >> (sh * 4)) & 15) as u8;
            if nib != 0 || started || sh == 0 { started = true; self.s(&[if nib < 10 { b'0' + nib } else { b'a' + nib - 10 }]); }
        }
    }
    fn flush(&self) { unsafe { write(1, self.b.as_ptr(), self.n); } }
}

unsafe fn now_ms() -> u64 {
    let mut t: timespec = core::mem::zeroed();
    clock_gettime(CLOCK_MONOTONIC, &mut t);
    t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000
}

unsafe fn handle(d: u64, target: c_int) {
    let fd = (d & 0xffff_ffff) as c_int;
    let it = d >> 32;
    let mut buf = [0u8; 64];
    let n = read(fd, buf.as_mut_ptr(), 64);
    if n > 0 {
        let mut ev = epoll_event { events: 0, data: epoll_data { u64: 0 } };
        epoll_ctl(target, EPOLL_CTL_DEL, fd, &mut ev);
        close(fd);
        DONE.store(it, SeqCst);
    }
}

extern "C" fn waiter(_a: *mut c_void) -> *mut c_void {
    unsafe {
        let outer = OUTER.load(SeqCst);
        let inner = INNER.load(SeqCst);
        let nested = NESTED.load(SeqCst) != 0;
        let mut evs = [epoll_event { events: 0, data: epoll_data { u64: 0 } }; 16];
        let mut ievs = [epoll_event { events: 0, data: epoll_data { u64: 0 } }; 16];
        loop {
            let n = epoll_wait(outer, evs.as_mut_ptr(), 16, -1);
            if n < 0 { continue; }
            for k in 0..n as usize {
                let d = evs[k].data;
                let v = d.u64;
                if v == QUIT { return core::ptr::null_mut(); }
                if !nested { handle(v, outer); }
            }
            if nested {
                loop {
                    let m = epoll_wait(inner, ievs.as_mut_ptr(), 16, 0);
                    if m <= 0 { break; }
                    for k in 0..m as usize { let d = ievs[k].data; handle(d.u64, inner); }
                }
            }
        }
    }
}

unsafe fn child_write(w: c_int) {
    let pid = fork();
    if pid == 0 {
        write(w, [7u8; 24].as_ptr(), 24);
        _exit(0);
    }
    if pid > 0 {
        let mut st: c_int = 0;
        let mut r = waitpid(pid, &mut st, 0);
        while r < 0 && *__errno_location() == 4 { r = waitpid(pid, &mut st, 0); }
    }
}

unsafe fn variant(nested: bool, flags: c_uint, proc_: bool, name: &[u8], iters: u64, budget_ms: u64) -> bool {
    let outer = epoll_create1(0);
    let inner = epoll_create1(0);
    let mut qp = [0i32; 2];
    pipe2(qp.as_mut_ptr(), 0);
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { u64: QUIT } };
    epoll_ctl(outer, EPOLL_CTL_ADD, qp[0], &mut ev);
    if nested {
        let mut e2 = epoll_event { events: EPOLLIN, data: epoll_data { u64: 0 } };
        epoll_ctl(outer, EPOLL_CTL_ADD, inner, &mut e2);
    }
    OUTER.store(outer, SeqCst); INNER.store(inner, SeqCst);
    NESTED.store(nested as i32, SeqCst); DONE.store(0, SeqCst);
    let target = if nested { inner } else { outer };
    let mut th: pthread_t = core::ptr::null_mut();
    pthread_create(&mut th, core::ptr::null(), waiter, core::ptr::null_mut());
    usleep(20_000);
    let start = now_ms();
    let mut i = 1u64;
    let mut ok = true;
    while i <= iters && now_ms() - start < budget_ms {
        let mut sv = [0i32; 2];
        if socketpair(1, 1, 0, sv.as_mut_ptr()) != 0 { ok = false; break; }
        let data = (i << 32) | sv[0] as u64;
        let mut e = epoll_event { events: flags, data: epoll_data { u64: data } };
        let add_first = i & 1 == 0;
        if add_first { epoll_ctl(target, EPOLL_CTL_ADD, sv[0], &mut e); }
        if proc_ { child_write(sv[1]); } else { write(sv[1], [7u8; 24].as_ptr(), 24); }
        if !add_first { epoll_ctl(target, EPOLL_CTL_ADD, sv[0], &mut e); }
        close(sv[1]);
        let t0 = now_ms();
        while DONE.load(SeqCst) < i {
            if now_ms() - t0 > 2000 {
                let mut pf = PollFd { fd: sv[0], events: 1, revents: 0 };
                let pr = poll(&mut pf, 1, 0);
                let mut tmp = [epoll_event { events: 0, data: epoll_data { u64: 0 } }; 4];
                let ri = epoll_wait(inner, tmp.as_mut_ptr(), 4, 0);
                let ro = epoll_wait(outer, tmp.as_mut_ptr(), 4, 0);
                let mut b = Buf::new();
                b.s(name); b.s(b": FAIL lost wakeup iter "); b.d(i as i64);
                b.s(if add_first { b" order add-first" } else { b" order write-first" });
                b.s(b" flags "); b.x(flags as u64);
                b.s(b" poll_rc "); b.d(pr as i64); b.s(b" revents "); b.x(pf.revents as u64);
                b.s(b" wait(inner,0) "); b.d(ri as i64);
                b.s(b" wait(outer,0) "); b.d(ro as i64); b.s(b"\n");
                b.flush();
                return false;
            }
            usleep(10);
        }
        i += 1;
    }
    write(qp[1], b"q".as_ptr(), 1);
    pthread_join(th, core::ptr::null_mut());
    for f in [qp[0], qp[1], inner, outer] { close(f); }
    let mut b = Buf::new();
    b.s(name); b.s(if ok { b": PASS iters=" } else { b": FAIL setup iters=" }); b.d(i as i64 - 1); b.s(b"\n");
    b.flush();
    ok
}

pub unsafe fn nest_main() -> i32 {
    let mut fails: i32 = 0;
    let names: [&[u8]; 12] = [
        b"nest_single_lt_thr", b"nest_single_et_thr", b"nest_single_rdhup_thr",
        b"nest_single_lt_proc", b"nest_single_et_proc", b"nest_single_rdhup_proc",
        b"nest_nested_lt_thr", b"nest_nested_et_thr", b"nest_nested_rdhup_thr",
        b"nest_nested_lt_proc", b"nest_nested_et_proc", b"nest_nested_rdhup_proc",
    ];
    let mut k = 0;
    for nested in [false, true] {
        for proc_ in [false, true] {
            for fl in [EPOLLIN, EPOLLIN | EPOLLET, EPOLLIN | EPOLLRDHUP] {
                // order in `names`: nested, proc, flag
                let idx = (nested as usize) * 6 + (proc_ as usize) * 3 + k % 3;
                k += 1;
                if !variant(nested, fl, proc_, names[idx], 5000, 5000) { fails += 1; return 1; }
            }
        }
    }
    let mut b = Buf::new();
    b.s(b"nest: failures "); b.d(fails as i64); b.s(b"\n"); b.flush();
    fails
}
