//! pstest — /proc/<pid>/comm and /bin/ps conformance (regress suite).
//! Runs as root; it forks unprivileged children for the permission cases.

#![no_std]
#![no_main]

extern crate leandros_libc;

#[macro_use]
#[path = "common.rs"]
mod common;

use common::*;
use core::fmt::Write;
use leandros_libc as l;
use leandros_libc::syscall::syscall3;

static mut FAILS: u32 = 0;
static mut TESTS: u32 = 0;

fn check(ok: bool, what: &str) {
    unsafe {
        TESTS += 1;
        if !ok { FAILS += 1; }
    }
    sayln!("{}: {}", if ok { "PASS" } else { "FAIL" }, what);
}

fn prctl(opt: usize, arg: usize) -> isize {
    unsafe { syscall3(SYS_PRCTL, opt, arg, 0) }
}

fn set_name_prctl(name: &[u8]) {
    let mut b = [0u8; 16];
    let n = name.len().min(15);
    b[..n].copy_from_slice(&name[..n]);
    prctl(15, b.as_ptr() as usize);
}

fn get_name_prctl(out: &mut [u8; 16]) {
    prctl(16, out.as_mut_ptr() as usize);
}

fn comm_path(pid: i32) -> Buf<48> {
    let mut p = Buf::<48>::new();
    let _ = write!(p, "/proc/{}/comm", pid);
    p
}

fn read_comm(path: &[u8], out: &mut [u8; 32]) -> usize {
    read_file(path, out).unwrap_or(0)
}

fn write_comm(path: &[u8], data: &[u8]) -> Result<usize, i32> {
    let fd = sys_open(path, O_WRONLY)?;
    let r = sys_write(fd, data);
    sys_close(fd);
    r
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// Run /bin/ps with `args` (NUL-terminated byte strings), capture stdout.
fn run_ps(args: &[&[u8]], out: &mut [u8]) -> usize {
    unsafe {
        let mut fds = [0i32; 2];
        if l::pipe(fds.as_mut_ptr()) != 0 { return 0; }
        let pid = l::fork();
        if pid == 0 {
            l::dup3(fds[1], 1, 0);
            l::close(fds[0]);
            l::close(fds[1]);
            let mut argv: [*const u8; 12] = [core::ptr::null(); 12];
            argv[0] = b"/bin/ps\0".as_ptr();
            for (i, a) in args.iter().enumerate() { argv[i + 1] = a.as_ptr(); }
            let envp: [*const u8; 1] = [core::ptr::null()];
            l::execve(b"/bin/ps\0".as_ptr(), argv.as_ptr(), envp.as_ptr());
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
}

/// Does some line of `text` have `first` as its first whitespace token?
fn has_pid_line(text: &[u8], pid: u32) -> bool {
    let mut p = Buf::<16>::new();
    let _ = write!(p, "{}", pid);
    text.split(|&c| c == b'\n').any(|line| {
        line.split(|&c| c == b' ').find(|w| !w.is_empty()) == Some(p.bytes())
    })
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let me = l::getpid();
    let mine = comm_path(me);
    let mut b = [0u8; 32];

    // 1. default name = executable basename, plus newline
    let n = read_comm(mine.bytes(), &mut b);
    check(&b[..n] == b"pstest\n", "comm defaults to the executable name");
    let n = read_comm(b"/proc/self/comm", &mut b);
    check(&b[..n] == b"pstest\n", "/proc/self/comm agrees");

    // 2. write sets the name; readable via comm, stat, prctl
    check(write_comm(mine.bytes(), b"hello\n") == Ok(6), "write /proc/<pid>/comm");
    let n = read_comm(mine.bytes(), &mut b);
    check(&b[..n] == b"hello\n", "comm reads back the new name");
    let mut nm = [0u8; 16];
    get_name_prctl(&mut nm);
    check(&nm[..6] == b"hello\0", "PR_GET_NAME sees the written name");
    let mut sb = [0u8; 512];
    let sn = read_file(b"/proc/self/stat", &mut sb).unwrap_or(0);
    check(contains(&sb[..sn], b" (hello) "), "stat shows the new name");

    // 3. truncation to 15 bytes; write without newline
    check(write_comm(mine.bytes(), b"abcdefghijklmnopqrstuvwxyz").is_ok(), "long write accepted");
    let n = read_comm(mine.bytes(), &mut b);
    check(&b[..n] == b"abcdefghijklmno\n", "name truncated to 15 bytes + newline");
    check(write_comm(b"/proc/self/comm", b"viaself").is_ok(), "write via /proc/self/comm");
    let n = read_comm(mine.bytes(), &mut b);
    check(&b[..n] == b"viaself\n", "no-newline write stores the bare name");

    // 4. prctl(PR_SET_NAME) shares the storage
    set_name_prctl(b"a-very-long-thread-name");
    let n = read_comm(mine.bytes(), &mut b);
    check(&b[..n] == b"a-very-long-thr\n", "PR_SET_NAME truncates, shows in comm");
    set_name_prctl(b"pstest");

    // 5. permissions: an unprivileged child cannot rename root's processes
    let child = l::fork();
    if child == 0 {
        let _ = l::setresgid(1000, 1000, 1000);
        let _ = l::setresuid(1000, 1000, 1000);
        let parent = l::getppid();
        let pp = comm_path(parent);
        let mut code = 0;
        if sys_open(b"/proc/1/comm", O_WRONLY) != Err(13) { code = 1; }
        if sys_open(pp.bytes(), O_WRONLY) != Err(13) { code = 2; }
        if sys_open(pp.bytes(), O_RDWR) != Err(13) { code = 3; }
        let mut rb = [0u8; 32];
        if read_comm(b"/proc/1/comm", &mut rb) == 0 { code = 4; }
        let own = comm_path(l::getpid());
        if write_comm(own.bytes(), b"mine\n").is_err() { code = 5; }
        let n = read_comm(b"/proc/self/comm", &mut rb);
        if &rb[..n] != b"mine\n" { code = 6; }
        l::exit(code);
    }
    let mut st = 0i32;
    l::wait4(child, &mut st, 0, core::ptr::null_mut());
    let code = (st >> 8) & 0xff;
    check(st & 0x7f == 0 && code == 0, "non-owner write is EACCES, owner write works");
    if code != 0 { sayln!("  (child failed at step {})", code); }

    // root may rename another user's process
    let mut fds = [0i32; 2];
    l::pipe(fds.as_mut_ptr());
    let kid = l::fork();
    if kid == 0 {
        let _ = l::setresgid(1000, 1000, 1000);
        let _ = l::setresuid(1000, 1000, 1000);
        l::close(fds[1]);
        let mut x = [0u8; 1];
        let _ = sys_read(fds[0], &mut x);
        l::exit(0);
    }
    l::close(fds[0]);
    let kp = comm_path(kid);
    check(write_comm(kp.bytes(), b"renamed\n").is_ok(), "root renames another user's process");
    let n = read_comm(kp.bytes(), &mut b);
    check(&b[..n] == b"renamed\n", "the rename is visible");
    l::close(fds[1]);
    l::wait4(kid, &mut st, 0, core::ptr::null_mut());
    check(sys_open(kp.bytes(), O_RDONLY).is_err(), "comm of an exited process is gone");

    // 6. ps
    let mut out = [0u8; 16384];
    let n = run_ps(&[b"-e\0"], &mut out);
    check(has_pid_line(&out[..n], 1), "ps -e lists pid 1");
    check(has_pid_line(&out[..n], me as u32), "ps -e lists itself");
    check(out[..n].starts_with(b"    PID TTY"), "ps -e header");

    let n = run_ps(&[], &mut out);
    check(has_pid_line(&out[..n], me as u32), "ps (default) lists the caller");

    let mut pa = Buf::<16>::new();
    let _ = write!(pa, "{}\0", me);
    let n = run_ps(&[b"-o\0", b"pid,comm\0", b"-p\0", pa.bytes()], &mut out);
    let mut want = Buf::<64>::new();
    let _ = write!(want, "    PID COMMAND\n{:>7} pstest\n", me);
    check(&out[..n] == want.bytes(), "ps -o pid,comm -p <pid>");

    let n = run_ps(&[b"aux\0"], &mut out);
    check(out[..n].starts_with(b"USER ") && contains(&out[..n], b"%CPU %MEM"), "ps aux header");
    check(has_pid_line_after(&out[..n], 1), "ps aux lists pid 1");

    let n = run_ps(&[b"-ef\0"], &mut out);
    check(out[..n].starts_with(b"UID ") && contains(&out[..n], b"STIME"), "ps -ef header");

    let n = run_ps(&[b"-o\0", b"pid=,ppid=\0", b"-p\0", pa.bytes()], &mut out);
    check(n > 0 && !out[..n].starts_with(b"    PID"), "ps -o pid=,ppid= prints no header");

    let n = run_ps(&[b"-T\0", b"-p\0", pa.bytes()], &mut out);
    check(contains(&out[..n], b"SPID") && has_pid_line(&out[..n], me as u32), "ps -T -p <pid>");

    let n = run_ps(&[b"-p\0", b"999999\0"], &mut out);
    check(out[..n].iter().filter(|&&c| c == b'\n').count() == 1, "ps -p <missing> prints only the header");

    let (t, f) = (TESTS, FAILS);
    sayln!("pstest: {} checks, {} failed", t, f);
    if f == 0 { sayln!("pstest: ALL PASSED"); 0 } else { 1 }
}

/// Like `has_pid_line` but for `ps aux`, whose pid is the SECOND column.
fn has_pid_line_after(text: &[u8], pid: u32) -> bool {
    let mut p = Buf::<16>::new();
    let _ = write!(p, "{}", pid);
    text.split(|&c| c == b'\n').any(|line| {
        line.split(|&c| c == b' ').filter(|w| !w.is_empty()).nth(1) == Some(p.bytes())
    })
}
