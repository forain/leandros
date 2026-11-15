//! utmptest - /bin/w, /bin/users, /bin/lastlog and concurrent utmp/wtmp/
//! lastlog writers (regress suite). 20 children log in at once through the
//! shared writer (lib/utmpx), all stay logged in, then all log out; the files
//! must keep whole 400/292-byte records and every session must be present.
#![no_std]
#![no_main]
extern crate leandros_libc;
#[macro_use]
#[path = "../../ps/src/common.rs"]
mod common;
#[path = "ut.rs"]
mod ut;
use common::*;
use core::fmt::Write;
use leandros_libc as l;
use ut::*;

const N: usize = 20;
const UID0: u32 = 3000;
static mut FAILS: u32 = 0;

fn check(ok: bool, what: &str) {
    if !ok { unsafe { FAILS += 1; } }
    sayln!("{}: {}", if ok { "PASS" } else { "FAIL" }, what);
}

fn file_size(path: &[u8]) -> i64 {
    let fd = match sys_open(path, O_RDONLY) { Ok(f) => f, Err(_) => return -1 };
    let n = unsafe { utmpx::lseek(fd as isize, 0, 2) } as i64;
    sys_close(fd);
    n
}

/// Run `path` with args, return its stdout in `out`.
unsafe fn run(path: &[u8], args: &[&[u8]], out: &mut [u8]) -> usize {
    let mut fds = [0i32; 2];
    if l::pipe(fds.as_mut_ptr()) != 0 { return 0; }
    let pid = l::fork();
    if pid == 0 {
        l::dup3(fds[1], 1, 0);
        l::close(fds[0]);
        l::close(fds[1]);
        let mut argv: [*const u8; 6] = [core::ptr::null(); 6];
        argv[0] = path.as_ptr();
        for (i, a) in args.iter().enumerate() { argv[i + 1] = a.as_ptr(); }
        let envp: [*const u8; 1] = [core::ptr::null()];
        l::execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());
        l::exit(127);
    }
    l::close(fds[1]);
    let mut n = 0;
    while n < out.len() {
        match sys_read(fds[0], &mut out[n..]) { Ok(0) | Err(_) => break, Ok(k) => n += k }
    }
    l::close(fds[0]);
    let mut st = 0i32;
    l::wait4(pid, &mut st, 0, core::ptr::null_mut());
    n
}

fn contains(h: &[u8], n: &[u8]) -> bool { h.windows(n.len()).any(|w| w == n) }

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut up = [0i32; 2];
    let mut go = [0i32; 2];
    l::pipe(up.as_mut_ptr());
    l::pipe(go.as_mut_ptr());
    let wt0 = file_size(WTMP_PATH).max(0);
    let mut pids = [0i32; N];
    for i in 0..N {
        let pid = l::fork();
        if pid == 0 {
            l::close(up[0]);
            l::close(go[1]);
            let mut line = Buf::<16>::new();
            let mut user = Buf::<16>::new();
            let _ = write!(line, "ut{}", i);
            let _ = write!(user, "utu{}", i);
            let me = l::getpid();
            login(me, line.bytes(), user.bytes(), b"10.9.8.7", UID0 + i as u32);
            let _ = sys_write(up[1], b"x");
            let mut b = [0u8; 1];
            let _ = sys_read(go[0], &mut b); // EOF when the parent closes its end
            logout_pid(me);
            l::exit(0);
        }
        pids[i] = pid;
    }
    l::close(up[1]);
    l::close(go[0]);
    let mut ready = 0;
    let mut b = [0u8; 1];
    while ready < N {
        match sys_read(up[0], &mut b) { Ok(1) => ready += 1, _ => break }
    }
    check(ready == N, "20 concurrent sessions logged in");

    // all 20 present exactly once, file made of whole records
    let sz = file_size(UTMP_PATH);
    check(sz > 0 && sz % REC as i64 == 0, "utmp is a whole number of records");
    let mut seen = [0u32; N];
    let mut bad = 0;
    each_utmp(|r| {
        if r.ut_type > 8 || r.ut_type < 0 { bad += 1; }
        if r.ut_type == USER_PROCESS {
            for i in 0..N {
                let mut u = Buf::<16>::new();
                let _ = write!(u, "utu{}", i);
                if cstr(&r.ut_user) == u.bytes() { seen[i] += 1; }
            }
        }
    });
    check(bad == 0, "no corrupt utmp record types");
    check(seen.iter().all(|&c| c == 1), "every session present exactly once");

    // consumers
    let mut o = [0u8; 4096];
    let n = run(b"/bin/users\0", &[], &mut o);
    check(contains(&o[..n], b"utu0") && contains(&o[..n], b"utu19") && o[..n].ends_with(b"\n"), "users lists the sessions");
    let n = run(b"/bin/w\0", &[], &mut o);
    // other live sessions may exist, so only the plural form is asserted, not the exact count
    check(contains(&o[..n], b"load average:") && contains(&o[..n], b" up ") && contains(&o[..n], b" users,"), "w header: uptime, users, load");
    check(contains(&o[..n], b"USER") && contains(&o[..n], b"utu7") && contains(&o[..n], b"ut7") && contains(&o[..n], b"10.9.8.7"), "w rows: user, tty, from");
    let n = run(b"/bin/w\0", &[b"-h\0", b"utu3\0"], &mut o);
    check(!contains(&o[..n], b"load average") && contains(&o[..n], b"utu3") && !contains(&o[..n], b"utu4"), "w -h user filters");

    // lastlog for the 20 uids (written concurrently into one sparse file)
    let ll = file_size(LASTLOG_PATH);
    check(ll >= (UID0 as i64 + N as i64) * LL_REC as i64 && ll % LL_REC as i64 == 0, "lastlog is a whole number of records");
    let mut okll = 0;
    let fd = sys_open(LASTLOG_PATH, O_RDONLY).unwrap_or(-1);
    for i in 0..N {
        let mut rec = [0u8; LL_REC];
        let mut line = Buf::<16>::new();
        let _ = write!(line, "ut{}", i);
        let r = leandros_libc::syscall::syscall4(PREAD64, fd as usize, rec.as_mut_ptr() as usize, LL_REC, ((UID0 as usize + i) * LL_REC) as usize);
        if r == LL_REC as isize && cstr(&rec[4..36]) == line.bytes() && cstr(&rec[36..]) == b"10.9.8.7" { okll += 1; }
    }
    sys_close(fd);
    check(okll == N, "lastlog record of every uid intact");
    let n = run(b"/bin/lastlog\0", &[b"-u\0", b"root\0"], &mut o);
    check(contains(&o[..n], b"Username") && contains(&o[..n], b"root"), "lastlog -u root");
    let n = run(b"/bin/lastlog\0", &[b"-u\0", b"nosuchuser\0"], &mut o);
    check(n == 0, "lastlog unknown user prints nothing");

    l::close(go[1]);
    for i in 0..N { let mut st = 0i32; l::wait4(pids[i], &mut st, 0, core::ptr::null_mut()); }

    let mut dead = 0;
    let mut alive = 0;
    each_utmp(|r| {
        for i in 0..N {
            let mut l_ = Buf::<16>::new();
            let _ = write!(l_, "ut{}", i);
            if cstr(&r.ut_line) == l_.bytes() {
                if r.ut_type == DEAD_PROCESS { dead += 1 } else if r.ut_type == USER_PROCESS { alive += 1 }
            }
        }
    });
    check(dead == N && alive == 0, "all 20 sessions ended (DEAD_PROCESS)");
    let n = run(b"/bin/users\0", &[], &mut o);
    check(!contains(&o[..n], b"utu"), "users no longer lists them");

    // wtmp: 20 USER + 20 DEAD appended, whole records
    let wt1 = file_size(WTMP_PATH);
    check((wt1 - wt0) >= (2 * N * REC) as i64 && (wt1 - wt0) % REC as i64 == 0, "wtmp grew by at least 40 whole records");
    let (mut u, mut d) = (0, 0);
    let fd = sys_open(WTMP_PATH, O_RDONLY).unwrap_or(-1);
    unsafe { utmpx::lseek(fd as isize, wt0, 0); }
    let mut rb = [0u8; REC];
    loop {
        let mut k = 0;
        while k < REC { match sys_read(fd, &mut rb[k..]) { Ok(0) | Err(_) => break, Ok(x) => k += x } }
        if k < REC { break; }
        let r = Utmpx::from_bytes(&rb);
        if r.ut_type == USER_PROCESS && cstr(&r.ut_host) == b"10.9.8.7" { u += 1 }
        if r.ut_type == DEAD_PROCESS { d += 1 }
    }
    sys_close(fd);
    check(u == N && d == N, "wtmp has 20 logins and 20 logouts");

    let f = FAILS;
    sayln!("utmptest: {}", if f == 0 { "ALL PASSED" } else { "FAILED" });
    if f == 0 { 0 } else { 1 }
}

#[cfg(target_arch = "aarch64")] const PREAD64: usize = 67;
#[cfg(target_arch = "x86_64")] const PREAD64: usize = 17;
