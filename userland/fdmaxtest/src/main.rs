//! fdmaxtest — the per-process descriptor table past 512 (lane fdmax).
//!
//! The fd table used to be a fixed 512 entries per process, with sockets and
//! epoll fds in their own number ranges above it. It is now one growable,
//! per-process namespace bounded by RLIMIT_NOFILE (soft 1024, hard 524288,
//! /proc/sys/fs/nr_open 1048576). Every case here runs on raw syscalls so it
//! tests the kernel, not a libc's emulation of it — except `relibc_poll_*`,
//! which goes through relibc's poll() (built on epoll) on purpose.
//!
//! `fdmaxtest` runs every case and exits with the number of failures.
//! `fdmaxtest bench` prints per-syscall costs for a few hot fd paths, with a
//! small table and again with 5000 extra descriptors open.

#![no_std]
#![no_main]

use core::ffi::c_void;

type c_int = i32;

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;
    pub fn fork() -> c_int;
    pub fn waitpid(pid: c_int, stat_loc: *mut c_int, options: c_int) -> c_int;
    pub fn _exit(status: c_int) -> !;
    pub fn exit(status: c_int) -> !;
    pub fn poll(fds: *mut PollFd, nfds: u64, timeout: c_int) -> c_int;
    pub fn getdtablesize() -> c_int;
    pub fn sysconf(name: c_int) -> i64;
}

// ── Raw syscalls ────────────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const READ: usize = 0;
    pub const WRITE: usize = 1;
    pub const CLOSE: usize = 3;
    pub const DUP: usize = 32;
    pub const GETPID: usize = 39;
    pub const SENDMSG: usize = 46;
    pub const RECVMSG: usize = 47;
    pub const SOCKETPAIR: usize = 53;
    pub const FCNTL: usize = 72;
    pub const GETDENTS64: usize = 217;
    pub const EPOLL_CTL: usize = 233;
    pub const OPENAT: usize = 257;
    pub const PSELECT6: usize = 270;
    pub const PPOLL: usize = 271;
    pub const EPOLL_PWAIT: usize = 281;
    pub const EVENTFD2: usize = 290;
    pub const EPOLL_CREATE1: usize = 291;
    pub const DUP3: usize = 292;
    pub const PIPE2: usize = 293;
    pub const PRLIMIT64: usize = 302;
    pub const CLOSE_RANGE: usize = 436;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const READ: usize = 63;
    pub const WRITE: usize = 64;
    pub const CLOSE: usize = 57;
    pub const DUP: usize = 23;
    pub const GETPID: usize = 172;
    pub const SENDMSG: usize = 211;
    pub const RECVMSG: usize = 212;
    pub const SOCKETPAIR: usize = 199;
    pub const FCNTL: usize = 25;
    pub const GETDENTS64: usize = 61;
    pub const EPOLL_CTL: usize = 21;
    pub const OPENAT: usize = 56;
    pub const PSELECT6: usize = 72;
    pub const PPOLL: usize = 73;
    pub const EPOLL_PWAIT: usize = 22;
    pub const EVENTFD2: usize = 19;
    pub const EPOLL_CREATE1: usize = 20;
    pub const DUP3: usize = 24;
    pub const PIPE2: usize = 59;
    pub const PRLIMIT64: usize = 261;
    pub const CLOSE_RANGE: usize = 436;
}

#[cfg(target_arch = "x86_64")]
unsafe fn sc6(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> isize {
    let r: isize;
    core::arch::asm!("syscall", inlateout("rax") n as isize => r, in("rdi") a, in("rsi") b,
        in("rdx") c, in("r10") d, in("r8") e, in("r9") f, out("rcx") _, out("r11") _,
        options(nostack));
    r
}
#[cfg(target_arch = "aarch64")]
unsafe fn sc6(n: usize, a: usize, b: usize, c: usize, d: usize, e: usize, f: usize) -> isize {
    let r: isize;
    core::arch::asm!("svc #0", in("x8") n, inlateout("x0") a as isize => r, in("x1") b,
        in("x2") c, in("x3") d, in("x4") e, in("x5") f, options(nostack));
    r
}
unsafe fn sc(n: usize, a: usize, b: usize, c: usize) -> isize { sc6(n, a, b, c, 0, 0, 0) }

const AT_FDCWD: usize = -100isize as usize;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_CLOEXEC: usize = 0x8_0000;
const O_DIRECTORY: usize = 0o200000;
const F_DUPFD: usize = 0;
const F_GETFD: usize = 1;
const F_DUPFD_CLOEXEC: usize = 1030;
const RLIMIT_NOFILE: usize = 7;
const EBADF: isize = -9;
const EINVAL: isize = -22;
const EMFILE: isize = -24;
const EPERM: isize = -1;
const AF_UNIX: usize = 1;
const SOCK_STREAM: usize = 1;
const EPOLLIN: u32 = 1;
const EPOLL_CTL_ADD: usize = 1;
const POLLIN: i16 = 1;
const POLLNVAL: i16 = 0x20;

unsafe fn open_null() -> isize { sc(nr::OPENAT, AT_FDCWD, b"/dev/null\0".as_ptr() as usize, O_RDWR) }
unsafe fn close(fd: isize) -> isize { sc(nr::CLOSE, fd as usize, 0, 0) }
unsafe fn is_open(fd: isize) -> bool { sc(nr::FCNTL, fd as usize, F_GETFD, 0) >= 0 }
unsafe fn dup2(a: isize, b: isize) -> isize { sc(nr::DUP3, a as usize, b as usize, 0) }
unsafe fn pipe() -> (isize, isize) {
    let mut p = [0i32; 2];
    let r = sc(nr::PIPE2, p.as_mut_ptr() as usize, 0, 0);
    if r < 0 { return (r, r); }
    (p[0] as isize, p[1] as isize)
}
unsafe fn sockpair() -> (isize, isize) {
    let mut p = [0i32; 2];
    let r = sc6(nr::SOCKETPAIR, AF_UNIX, SOCK_STREAM, 0, p.as_mut_ptr() as usize, 0, 0);
    if r < 0 { return (r, r); }
    (p[0] as isize, p[1] as isize)
}
unsafe fn epoll() -> isize { sc(nr::EPOLL_CREATE1, 0, 0, 0) }
unsafe fn eventfd() -> isize { sc(nr::EVENTFD2, 0, 0, 0) }
unsafe fn wr(fd: isize, b: &[u8]) -> isize { sc(nr::WRITE, fd as usize, b.as_ptr() as usize, b.len()) }
unsafe fn rd(fd: isize, b: &mut [u8]) -> isize { sc(nr::READ, fd as usize, b.as_mut_ptr() as usize, b.len()) }

unsafe fn getrlimit() -> (u64, u64) {
    let mut r = [0u64; 2];
    let e = sc6(nr::PRLIMIT64, 0, RLIMIT_NOFILE, 0, r.as_mut_ptr() as usize, 0, 0);
    if e < 0 { return (0, 0); }
    (r[0], r[1])
}
unsafe fn setrlimit(cur: u64, max: u64) -> isize {
    let r = [cur, max];
    sc6(nr::PRLIMIT64, 0, RLIMIT_NOFILE, r.as_ptr() as usize, 0, 0, 0)
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct PollFd { fd: i32, events: i16, revents: i16 }

#[cfg(target_arch = "x86_64")]
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent { events: u32, data: u64 }
#[cfg(not(target_arch = "x86_64"))]
#[repr(C)]
#[derive(Clone, Copy)]
struct EpollEvent { events: u32, data: u64 }

unsafe fn ep_add(ep: isize, fd: isize, data: u64) -> isize {
    let mut ev = EpollEvent { events: EPOLLIN, data };
    sc6(nr::EPOLL_CTL, ep as usize, EPOLL_CTL_ADD, fd as usize, &mut ev as *mut _ as usize, 0, 0)
}
unsafe fn ep_wait(ep: isize, out: &mut [EpollEvent]) -> isize {
    sc6(nr::EPOLL_PWAIT, ep as usize, out.as_mut_ptr() as usize, out.len(), 0, 0, 0)
}

// ── Output ──────────────────────────────────────────────────────────────────

unsafe fn puts(s: &[u8]) { let _ = wr(1, s); }
unsafe fn putn(mut n: u64) {
    let mut b = [0u8; 24];
    let mut i = b.len();
    if n == 0 { i -= 1; b[i] = b'0'; }
    while n > 0 { i -= 1; b[i] = b'0' + (n % 10) as u8; n /= 10; }
    puts(&b[i..]);
}
unsafe fn puti(n: isize) { if n < 0 { puts(b"-"); putn((-n) as u64) } else { putn(n as u64) } }

static mut FAILS: i32 = 0;
unsafe fn report(name: &[u8], ok: bool) {
    puts(name);
    puts(if ok { b": PASS\n" } else { b": FAIL\n" });
    if !ok { FAILS += 1; }
}
/// One diagnostic line for a failed expectation.
unsafe fn expect(what: &[u8], got: isize, want: isize) -> bool {
    if got == want { return true; }
    puts(b"  "); puts(what); puts(b": got "); puti(got); puts(b" want "); puti(want); puts(b"\n");
    false
}

unsafe fn closefrom(fd: isize) { let _ = sc(nr::CLOSE_RANGE, fd as usize, !0u32 as usize, 0); }

// ── Cases ───────────────────────────────────────────────────────────────────

unsafe fn t_limits() {
    let (cur, max) = getrlimit();
    let mut ok = expect(b"RLIMIT_NOFILE cur", cur as isize, 1024)
        & expect(b"RLIMIT_NOFILE max", max as isize, 524288);
    // cur > max is EINVAL; max above nr_open is EPERM even for root.
    ok &= expect(b"setrlimit cur>max", setrlimit(4096, 2048), EINVAL);
    ok &= expect(b"setrlimit max>nr_open", setrlimit(1024, (1 << 20) + 1), EPERM);
    // /proc/sys/fs/nr_open
    let fd = sc(nr::OPENAT, AT_FDCWD, b"/proc/sys/fs/nr_open\0".as_ptr() as usize, O_RDONLY);
    let mut b = [0u8; 32];
    let n = if fd >= 0 { rd(fd, &mut b) } else { fd };
    if fd >= 0 { close(fd); }
    let txt_ok = n > 0 && b[..n as usize].starts_with(b"1048576");
    if !txt_ok { puts(b"  /proc/sys/fs/nr_open: read "); puti(n); puts(b"\n"); }
    ok &= txt_ok;
    // libc's view of the same limit (relibc `leandros/fdlimits` branch).
    ok &= expect(b"getdtablesize()", getdtablesize() as isize, cur as isize);
    ok &= expect(b"sysconf(_SC_OPEN_MAX)", sysconf(4 /* _SC_OPEN_MAX */) as isize, cur as isize);
    report(b"limits_default_and_nr_open", ok);
}

/// At the default soft limit (1024) the 1025th descriptor is EMFILE, and the
/// numbers come out lowest-first with no gaps.
unsafe fn t_emfile_at_soft_limit() {
    closefrom(3);
    let mut ok = true;
    let mut last = 2isize;
    loop {
        let fd = open_null();
        if fd < 0 {
            ok &= expect(b"error at limit", fd, EMFILE);
            break;
        }
        if fd != last + 1 { ok &= expect(b"lowest-free fd", fd, last + 1); break; }
        last = fd;
        if last > 5000 { ok = false; break; }
    }
    ok &= expect(b"highest fd under soft 1024", last, 1023);
    // A pipe needs two: EMFILE too; dup with no room: EMFILE.
    ok &= expect(b"dup at limit", sc(nr::DUP, 3, 0, 0), EMFILE);
    // Close one in the middle; the next open takes exactly it.
    close(500);
    ok &= expect(b"reuse of the hole", open_null(), 500);
    // dup2 to a number at/over the soft limit is EBADF (Linux).
    ok &= expect(b"dup2 to fd >= RLIMIT_NOFILE", dup2(3, 1024), EBADF);
    // F_DUPFD with a minimum at/over the limit is EINVAL.
    ok &= expect(b"F_DUPFD min >= limit", sc(nr::FCNTL, 3, F_DUPFD, 1024), EINVAL);
    closefrom(3);
    report(b"emfile_at_soft_limit", ok);
}

unsafe fn t_open_5000() {
    closefrom(3);
    let mut ok = expect(b"raise soft limit", setrlimit(8192, 524288), 0);
    let mut n = 0;
    for i in 0..5000isize {
        let fd = open_null();
        if fd != 3 + i { ok &= expect(b"fd", fd, 3 + i); break; }
        n += 1;
    }
    ok &= expect(b"opened", n, 5000);
    // Every one of them works.
    ok &= expect(b"write fd 5002", wr(5002, b"x"), 1);
    ok &= expect(b"write fd 3", wr(3, b"x"), 1);
    // /proc/self/fd lists them all (plus 0-2 when tracked; count >= 5000).
    let d = sc(nr::OPENAT, AT_FDCWD, b"/proc/self/fd\0".as_ptr() as usize, O_RDONLY | O_DIRECTORY);
    let mut entries = 0isize;
    let mut saw_5002 = false;
    if d >= 0 {
        let mut buf = [0u8; 4096];
        loop {
            let r = sc(nr::GETDENTS64, d as usize, buf.as_mut_ptr() as usize, buf.len());
            if r <= 0 { break; }
            let mut off = 0usize;
            while off < r as usize {
                let reclen = u16::from_ne_bytes([buf[off + 16], buf[off + 17]]) as usize;
                let name = &buf[off + 19..off + reclen];
                let name = &name[..name.iter().position(|&c| c == 0).unwrap_or(name.len())];
                if name != b"." && name != b".." { entries += 1; }
                if name == b"5002" { saw_5002 = true; }
                off += reclen;
            }
        }
        close(d);
    }
    if !(entries >= 5000 && saw_5002) {
        puts(b"  /proc/self/fd entries "); puti(entries); puts(if saw_5002 { b" (5002 seen)\n" } else { b" (5002 missing)\n" });
        ok = false;
    }
    closefrom(3);
    ok &= expect(b"fd 4000 closed by close_range", is_open(4000) as isize, 0);
    report(b"open_5000_files", ok);
}

/// Sockets, epoll fds, pipes, eventfds and files share one number space:
/// each new descriptor of any kind takes the lowest free number.
unsafe fn t_mixed_kinds_lowest() {
    closefrom(3);
    let mut ok = true;
    let (a, b) = sockpair();
    ok &= expect(b"socketpair a", a, 3) & expect(b"socketpair b", b, 4);
    ok &= expect(b"epoll", epoll(), 5);
    let (r, w) = pipe();
    ok &= expect(b"pipe r", r, 6) & expect(b"pipe w", w, 7);
    ok &= expect(b"eventfd", eventfd(), 8);
    ok &= expect(b"file", open_null(), 9);
    // Holes of every kind are refilled lowest-first by any kind.
    close(3); close(5); close(8);
    let (r2, w2) = pipe();
    ok &= expect(b"pipe r into socket hole", r2, 3) & expect(b"pipe w into epoll hole", w2, 5);
    let (s1, s2) = sockpair();
    ok &= expect(b"socket into eventfd hole", s1, 8) & expect(b"socket peer", s2, 10);
    // dup of a socket and of an epoll fd: lowest free too.
    let low = sc(nr::DUP, s1 as usize, 0, 0);
    ok &= expect(b"dup(socket)", low, 11);
    let ep2 = sc(nr::FCNTL, 11, F_DUPFD, 0);
    ok &= expect(b"F_DUPFD(socket, 0)", ep2, 12);
    close(12);
    // Sockets work through every number they get.
    ok &= expect(b"send via dup", wr(low, b"hi"), 2);
    let mut buf = [0u8; 8];
    ok &= expect(b"recv on peer", rd(s2, &mut buf), 2);
    ok &= expect(b"close dup", close(low), 0);
    ok &= expect(b"original still works", wr(s1, b"k"), 1);
    closefrom(3);
    report(b"mixed_kinds_lowest_free", ok);
}

/// High numbers: dup2 a pipe, a socket and an epoll fd far up, use them, and
/// let a fork child use them too.
unsafe fn t_high_numbers_and_fork() {
    closefrom(3);
    let mut ok = expect(b"raise soft limit", setrlimit(200_000, 524288), 0);
    let (r, w) = pipe();
    let (sa, sb) = sockpair();
    let ep = epoll();
    ok &= expect(b"dup2 pipe w -> 100000", dup2(w, 100_000), 100_000);
    ok &= expect(b"dup2 sock -> 100001", dup2(sa, 100_001), 100_001);
    ok &= expect(b"dup2 epoll -> 100002", dup2(ep, 100_002), 100_002);
    ok &= expect(b"F_DUPFD sock >= 150000", sc(nr::FCNTL, sb as usize, F_DUPFD_CLOEXEC, 150_000), 150_000);
    close(w); close(sa); close(ep); close(sb);
    ok &= expect(b"write via 100000", wr(100_000, b"p"), 1);
    let mut buf = [0u8; 4];
    ok &= expect(b"read pipe", rd(r, &mut buf), 1);
    ok &= expect(b"epoll add on 100002", ep_add(100_002, r, 7), 0);
    ok &= expect(b"epoll add sock 150000", ep_add(100_002, 150_000, 9), 0);
    let pid = fork();
    if pid == 0 {
        // Child: every inherited high fd works.
        let mut good = wr(100_000, b"c") == 1 && wr(100_001, b"s") == 1;
        let mut ev = [EpollEvent { events: 0, data: 0 }; 4];
        let mut seen = 0;
        for _ in 0..50 {
            let n = ep_wait(100_002, &mut ev);
            if n > 0 { seen = n; break; }
        }
        good &= seen >= 1;
        _exit(if good { 0 } else { 1 });
    }
    let mut st = 0;
    waitpid(pid, &mut st, 0);
    ok &= expect(b"child status", st as isize, 0);
    ok &= expect(b"child's pipe byte", rd(r, &mut buf), 1);
    ok &= expect(b"child's socket byte", rd(150_000, &mut buf), 1);
    closefrom(3);
    ok &= expect(b"high fds closed", is_open(100_001) as isize, 0);
    ok &= expect(b"restore soft limit", setrlimit(8192, 524288), 0);
    report(b"high_numbers_and_fork", ok);
}

/// close_range over a mix of kinds closes them all; CLOEXEC marks them all.
unsafe fn t_close_range() {
    closefrom(3);
    let mut ok = true;
    let (a, _b) = sockpair();
    let ep = epoll();
    let (r, _w) = pipe();
    let hi = dup2(a, 3000);
    ok &= expect(b"dup2 to 3000", hi, 3000);
    ok &= expect(b"cloexec range", sc(nr::CLOSE_RANGE, 3, 4000, 4), 0);
    ok &= expect(b"socket cloexec", sc(nr::FCNTL, a as usize, F_GETFD, 0), 1);
    ok &= expect(b"epoll cloexec", sc(nr::FCNTL, ep as usize, F_GETFD, 0), 1);
    ok &= expect(b"pipe cloexec", sc(nr::FCNTL, r as usize, F_GETFD, 0), 1);
    ok &= expect(b"close_range 4..", sc(nr::CLOSE_RANGE, 4, !0u32 as usize, 0), 0);
    ok &= expect(b"3 still open", is_open(3) as isize, 1);
    ok &= expect(b"epoll gone", is_open(ep) as isize, 0);
    ok &= expect(b"3000 gone", is_open(3000) as isize, 0);
    ok &= expect(b"next is 4", open_null(), 4);
    closefrom(3);
    report(b"close_range_all_kinds", ok);
}

/// 500 pipes (1000 fds): readiness through ppoll, relibc poll(), epoll and
/// select reports exactly the pipes that hold data.
unsafe fn t_500_pipes_poll() {
    closefrom(3);
    let mut ok = true;
    let mut rfd = [0i32; 500];
    for i in 0..500 {
        let (r, w) = pipe();
        if r < 0 { ok &= expect(b"pipe", r, 0); break; }
        rfd[i] = r as i32;
        if i % 7 == 0 { wr(w, b"!"); }
    }
    let want = (0..500).filter(|i| i % 7 == 0).count() as isize;
    let mut p = [PollFd { fd: 0, events: POLLIN, revents: 0 }; 500];
    for i in 0..500 { p[i].fd = rfd[i]; }
    let ts = [0i64; 2];
    let n = sc6(nr::PPOLL, p.as_mut_ptr() as usize, 500, ts.as_ptr() as usize, 0, 0, 0);
    ok &= expect(b"ppoll ready count", n, want);
    for i in 0..500 {
        let ready = p[i].revents & POLLIN != 0;
        if ready != (i % 7 == 0) { ok &= expect(b"ppoll pipe index", i as isize, -1); break; }
    }
    for e in p.iter_mut() { e.revents = 0; }
    // relibc's poll() over all 500 pollfds reports every ready one (needs the
    // relibc `leandros/fdlimits` branch: poll used to stop at 32 events).
    let n2 = poll(p.as_mut_ptr(), 500, 0) as isize;
    ok &= expect(b"relibc poll ready count", n2, want);
    for i in 0..500 {
        let ready = p[i].revents & POLLIN != 0;
        if ready != (i % 7 == 0) { ok &= expect(b"relibc poll pipe index", i as isize, -1); break; }
    }
    // POLLNVAL for a closed number in the middle of a big set.
    let mut bad = [PollFd { fd: 999_999, events: POLLIN, revents: 0 }];
    sc6(nr::PPOLL, bad.as_mut_ptr() as usize, 1, ts.as_ptr() as usize, 0, 0, 0);
    ok &= expect(b"POLLNVAL", (bad[0].revents & POLLNVAL) as isize, POLLNVAL as isize);
    // epoll with 500 interests.
    let ep = epoll();
    for i in 0..500 { if ep_add(ep, rfd[i] as isize, i as u64) != 0 { ok &= expect(b"epoll add", i as isize, -1); break; } }
    let mut evs = [EpollEvent { events: 0, data: 0 }; 128];
    let n3 = ep_wait(ep, &mut evs);
    ok &= expect(b"epoll ready count", n3, want);
    for e in evs[..n3.max(0) as usize].iter() {
        let d = e.data;
        if d % 7 != 0 { ok &= expect(b"epoll data", d as isize, -1); break; }
    }
    // select over the fds below FD_SETSIZE (1024).
    let mut set = [0u64; 16];
    let mut maxfd = 0;
    for i in 0..500 { let fd = rfd[i] as usize; if fd < 1024 { set[fd / 64] |= 1 << (fd % 64); maxfd = maxfd.max(fd); } }
    let want_sel = (0..500).filter(|&i| i % 7 == 0 && (rfd[i] as usize) < 1024).count() as isize;
    let n4 = sc6(nr::PSELECT6, maxfd + 1, set.as_mut_ptr() as usize, 0, 0, ts.as_ptr() as usize, 0);
    ok &= expect(b"select ready count", n4, want_sel);
    closefrom(3);
    report(b"pipes_500_poll_epoll_select", ok);
}

/// SCM_RIGHTS carries a high-numbered descriptor; the receiver gets its own
/// lowest free number for it.
unsafe fn t_scm_rights_high() {
    closefrom(3);
    let mut ok = true;
    let (a, b) = sockpair();          // 3, 4
    let (r, w) = pipe();              // 5, 6
    ok &= expect(b"dup2 w -> 5000", dup2(w, 5000), 5000);
    close(w);
    #[repr(C)] struct Iov { base: *mut u8, len: usize }
    #[repr(C)] struct Msg { name: *mut u8, namelen: u32, iov: *mut Iov, iovlen: usize, ctl: *mut u8, ctllen: usize, flags: i32 }
    let mut byte = [b'm'];
    let mut iov = Iov { base: byte.as_mut_ptr(), len: 1 };
    let mut ctl = [0u64; 3]; // cmsghdr(16) + int + pad
    let cl = 16 + 4;
    let c = ctl.as_mut_ptr() as *mut u8;
    *(c as *mut usize) = cl;
    *(c.add(8) as *mut i32) = 1; // SOL_SOCKET
    *(c.add(12) as *mut i32) = 1; // SCM_RIGHTS
    *(c.add(16) as *mut i32) = 5000;
    let msg = Msg { name: core::ptr::null_mut(), namelen: 0, iov: &mut iov, iovlen: 1, ctl: c, ctllen: 24, flags: 0 };
    let sent = sc(nr::SENDMSG, a as usize, &msg as *const _ as usize, 0);
    ok &= expect(b"sendmsg", sent, 1);
    close(5000);
    if sent != 1 {
        // Nothing queued: a recvmsg would block forever.
        closefrom(3);
        report(b"scm_rights_high_fd", false);
        return;
    }
    let mut rbyte = [0u8];
    let mut riov = Iov { base: rbyte.as_mut_ptr(), len: 1 };
    let mut rctl = [0u64; 4];
    let mut rmsg = Msg { name: core::ptr::null_mut(), namelen: 0, iov: &mut riov, iovlen: 1, ctl: rctl.as_mut_ptr() as *mut u8, ctllen: 32, flags: 0 };
    ok &= expect(b"recvmsg", sc(nr::RECVMSG, b as usize, &mut rmsg as *mut _ as usize, 0), 1);
    let got = *((rctl.as_ptr() as *const u8).add(16) as *const i32) as isize;
    ok &= expect(b"received fd is lowest free", got, 6);
    ok &= expect(b"write via received", wr(got, b"z"), 1);
    let mut buf = [0u8; 2];
    ok &= expect(b"read it back", rd(r, &mut buf), 1);
    closefrom(3);
    report(b"scm_rights_high_fd", ok);
}

/// The net server does not copy inet sockets into a fork child; the child's
/// descriptor for one must not be left naming a free socket slot, or the
/// child's next socket lands in that slot and closing the stale number kills
/// it (Firefox's content processes lost their IPC channel exactly so).
unsafe fn t_fork_inet_socket() {
    closefrom(3);
    let mut ok = true;
    const AF_INET: usize = 2;
    let ti = sc(nr_socket(), AF_INET, SOCK_STREAM, 0);
    ok &= expect(b"inet socket", ti, 3);
    let pid = fork();
    if pid == 0 {
        let (s0, s1) = sockpair();
        if s0 < 0 { _exit(10); }
        // Whatever the child's fd 3 is, closing it must not touch s0/s1.
        let _ = close(3);
        if wr(s0, b"q") != 1 { _exit(11); }
        let mut b = [0u8; 2];
        if rd(s1, &mut b) != 1 { _exit(12); }
        _exit(0);
    }
    let mut st = 0;
    waitpid(pid, &mut st, 0);
    ok &= expect(b"child status", st as isize, 0);
    closefrom(3);
    report(b"fork_inet_socket", ok);
}

#[cfg(target_arch = "x86_64")] fn nr_socket() -> usize { 41 }
#[cfg(target_arch = "aarch64")] fn nr_socket() -> usize { 198 }

/// A socket survives execve on the number it was given — both when the child
/// clears FD_CLOEXEC on a SOCK_CLOEXEC socket in place (a launcher whose fd
/// mapping is src == dst) and when it dup2s one onto a fixed number — and the
/// exec'd image can use it as a socket.
unsafe fn t_exec_keeps_sockets() {
    closefrom(3);
    let mut ok = true;
    const SOCK_CLOEXEC: usize = 0x80000;
    let mut p = [0i32; 2];
    let r = sc6(nr::SOCKETPAIR, AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, p.as_mut_ptr() as usize, 0, 0);
    ok &= expect(b"socketpair", r, 0);
    let (a, b) = (p[0] as isize, p[1] as isize);
    let (c, d) = sockpair();
    let pid = fork();
    if pid == 0 {
        // In place: clear close-on-exec on `b`; and `d` dup2'd onto 40.
        if sc(nr::FCNTL, b as usize, 2 /* F_SETFD */, 0) != 0 { _exit(90); }
        if dup2(d, 40) != 40 { _exit(91); }
        let mut arg1 = [0u8; 8];
        let mut n = b as usize; let mut k = 0;
        let mut tmp = [0u8; 8]; loop { tmp[k] = b'0' + (n % 10) as u8; k += 1; n /= 10; if n == 0 { break; } }
        for i in 0..k { arg1[i] = tmp[k - 1 - i]; }
        let argv = [b"fdmaxtest\0".as_ptr(), b"x\0".as_ptr(), arg1.as_ptr(), core::ptr::null()];
        let envp = [core::ptr::null::<u8>()];
        #[cfg(target_arch = "x86_64")] let n_exec = 59;
        #[cfg(target_arch = "aarch64")] let n_exec = 221;
        sc(n_exec, b"/bin/fdmaxtest\0".as_ptr() as usize, argv.as_ptr() as usize, envp.as_ptr() as usize);
        _exit(92);
    }
    let mut st = 0;
    waitpid(pid, &mut st, 0);
    ok &= expect(b"exec'd child status", st as isize, 0);
    let mut buf = [0u8; 4];
    ok &= expect(b"byte through in-place socket", rd(a, &mut buf), 1);
    ok &= expect(b"byte through dup2'd socket", rd(c, &mut buf), 1);
    closefrom(3);
    report(b"exec_keeps_sockets", ok);
}

/// The exec'd half of `t_exec_keeps_sockets`: `fd` (argv[2]) and 40 must be
/// sockets; write a byte to each.
unsafe fn exec_child(fd: isize) -> i32 {
    let mut v = 0i32; let mut l = 4u32;
    // SOL_SOCKET=1, SO_SNDBUF=7
    #[cfg(target_arch = "x86_64")] let n_gso = 55;
    #[cfg(target_arch = "aarch64")] let n_gso = 209;
    if sc6(n_gso, fd as usize, 1, 7, &mut v as *mut i32 as usize, &mut l as *mut u32 as usize, 0) < 0 { return 3; }
    if wr(fd, b"e") != 1 { return 4; }
    if wr(40, b"f") != 1 { return 5; }
    0
}

// ── Benchmark ───────────────────────────────────────────────────────────────

unsafe fn now_ns() -> u64 {
    let mut ts = [0i64; 2];
    #[cfg(target_arch = "x86_64")] let n = 228;
    #[cfg(target_arch = "aarch64")] let n = 113;
    sc(n, 1, ts.as_mut_ptr() as usize, 0);
    ts[0] as u64 * 1_000_000_000 + ts[1] as u64
}

unsafe fn bench_round(label: &[u8]) {
    const N: u64 = 20_000;
    let (r, w) = pipe();
    let (sa, sb) = sockpair();
    let mut buf = [0u8; 8];
    let t0 = now_ns();
    for _ in 0..N { sc(nr::GETPID, 0, 0, 0); }
    let t1 = now_ns();
    for _ in 0..N { wr(w, b"x"); rd(r, &mut buf); }
    let t2 = now_ns();
    let mut p = [PollFd { fd: r as i32, events: POLLIN, revents: 0 }];
    let ts = [0i64; 2];
    for _ in 0..N { sc6(nr::PPOLL, p.as_mut_ptr() as usize, 1, ts.as_ptr() as usize, 0, 0, 0); }
    let t3 = now_ns();
    for _ in 0..N { wr(sa, b"x"); rd(sb, &mut buf); }
    let t4 = now_ns();
    for _ in 0..N { sc(nr::FCNTL, r as usize, F_GETFD, 0); }
    let t5 = now_ns();
    puts(b"[fdbench] "); puts(label);
    puts(b" getpid_ns="); putn((t1 - t0) / N);
    puts(b" pipe_wr_rd_ns="); putn((t2 - t1) / N);
    puts(b" ppoll1_ns="); putn((t3 - t2) / N);
    puts(b" sock_wr_rd_ns="); putn((t4 - t3) / N);
    puts(b" fcntl_ns="); putn((t5 - t4) / N);
    puts(b"\n");
    close(r); close(w); close(sa); close(sb);
}

unsafe fn bench() {
    closefrom(3);
    bench_round(b"small-table");
    bench_round(b"small-table");
    if setrlimit(8192, 524288) == 0 {
        let mut k = 0;
        for _ in 0..5000 { if open_null() >= 0 { k += 1; } }
        puts(b"[fdbench] opened "); putn(k); puts(b"\n");
        bench_round(b"5000-fds");
        closefrom(3);
    } else {
        puts(b"[fdbench] setrlimit failed; skipping 5000-fd round\n");
    }
}

// ── Entry ───────────────────────────────────────────────────────────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   xor rbp, rbp", "   mov rdi, rsp", "   mov rsi, offset fdmax_main",
    "   and rsp, -16", "   call relibc_start_v1", "   ud2"
);
#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start", ".global _start", "_start:",
    "   mov x29, #0", "   mov x30, #0", "   mov x0, sp",
    "   adrp x1, fdmax_main", "   add x1, x1, :lo12:fdmax_main",
    "   and sp, x0, #-16", "   bl relibc_start_v1", "   brk #0"
);

#[no_mangle]
pub unsafe extern "C" fn fdmax_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    if argc > 1 {
        let a = *argv.add(1);
        if *a == b'b' { bench(); return 0; }
        if *a == b'x' && argc > 2 {
            let s = *argv.add(2);
            let mut n = 0isize; let mut i = 0;
            while *s.add(i) != 0 { n = n * 10 + (*s.add(i) - b'0') as isize; i += 1; }
            return exec_child(n);
        }
    }
    t_limits();
    t_emfile_at_soft_limit();
    t_open_5000();
    t_mixed_kinds_lowest();
    t_high_numbers_and_fork();
    t_close_range();
    t_500_pipes_poll();
    t_scm_rights_high();
    t_exec_keeps_sockets();
    t_fork_inet_socket();
    puts(b"--- fdmaxtest done: "); putn(FAILS as u64); puts(b" failed ---\n");
    FAILS
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}
