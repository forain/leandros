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
use leandros_libc::syscall::{nr, syscall3};
use core::sync::atomic::{AtomicU32, Ordering};

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
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc > 1 && arg(argv, 1) == b"--argv-child" { return argv_child(argc as usize, argv); }
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

    // procps selectors: -u/-U (user), -C (command); a uid-1000 sleeper
    let mut hp = [0i32; 2];
    l::pipe(hp.as_mut_ptr());
    let u = l::fork();
    if u == 0 {
        let _ = l::setresgid(1000, 1000, 1000);
        let _ = l::setresuid(1000, 1000, 1000);
        l::close(hp[1]);
        let mut x = [0u8; 1];
        let _ = sys_read(hp[0], &mut x);
        l::exit(0);
    }
    l::close(hp[0]);
    msleep(50);
    let n = run_ps(&[b"-u\0", b"root\0", b"-o\0", b"pid=\0"], &mut out);
    check(has_pid_line(&out[..n], 1) && has_pid_line(&out[..n], me as u32) && !has_pid_line(&out[..n], u as u32),
          "ps -u root: root's processes only");
    let n = run_ps(&[b"-u\0", b"1000\0", b"-o\0", b"pid=,user=\0"], &mut out);
    check(has_pid_line(&out[..n], u as u32) && !has_pid_line(&out[..n], me as u32) && !has_pid_line(&out[..n], 1),
          "ps -u 1000: that user's processes only");
    let n = run_ps(&[b"-U1000\0", b"-o\0", b"pid=\0"], &mut out);
    check(has_pid_line(&out[..n], u as u32) && !has_pid_line(&out[..n], me as u32), "ps -U1000 (real uid)");
    let n = run_ps(&[b"-C\0", b"pstest\0", b"-o\0", b"pid=,comm=\0"], &mut out);
    check(has_pid_line(&out[..n], me as u32) && has_pid_line(&out[..n], u as u32) && !has_pid_line(&out[..n], 1)
          && out[..n].split(|&c| c == b'\n').filter(|l| !l.is_empty()).all(|l| l.ends_with(b" pstest")),
          "ps -C pstest: by command name");
    let n = run_ps(&[b"-C\0", b"pstest\0", b"-u\0", b"1000\0", b"-o\0", b"pid=\0"], &mut out);
    check(has_pid_line(&out[..n], me as u32) && has_pid_line(&out[..n], u as u32), "selectors combine as a union");
    let n = run_ps(&[b"-u\0", b"no-such-user\0"], &mut out);
    check(n == 0, "ps -u <unknown user> fails without output");
    let n = run_ps(&[b"aux\0"], &mut out);
    check(out[..n].starts_with(b"USER "), "BSD `u` still means the user format");
    l::close(hp[1]);
    l::wait4(u, &mut st, 0, core::ptr::null_mut());

    // 7. lane procps2: threads (task dirs, ps -T), argv (cmdline, environ,
    // setproctitle, kernel threads), the controlling tty (tty_nr, default ps)
    test_threads(me);
    test_argv();
    test_tty(me);
    // 8. lane procfix
    test_accounting();
    test_set_mm();
    test_sysstat(me);
    test_ps_simple(me);

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

// ── lane procps2: threads, argv, tty ───────────────────────────────────────

#[cfg(target_arch = "aarch64")] const SYS_GETTID: usize = 178;
#[cfg(target_arch = "x86_64")]  const SYS_GETTID: usize = 186;
const CLONE_THREAD_FLAGS: usize = 0x100 | 0x200 | 0x400 | 0x800 | 0x10000 | 0x40000;
const NTHREADS: usize = 3;
const THREAD_NAMES: [&[u8]; NTHREADS] = [b"pst-w0", b"pst-w1", b"pst-busy"];

#[repr(C, align(16))]
struct Stack([u8; 65536]);
static mut STACKS: [Stack; NTHREADS] = [const { Stack([0; 65536]) }; NTHREADS];
static TIDS: [AtomicU32; NTHREADS] = [const { AtomicU32::new(0) }; NTHREADS];
static READY: AtomicU32 = AtomicU32::new(0);
static STOP: AtomicU32 = AtomicU32::new(0);
static BUSY_DONE: AtomicU32 = AtomicU32::new(0);

fn msleep(ms: u64) {
    let ts = [(ms / 1000) as i64, ((ms % 1000) * 1_000_000) as i64];
    unsafe { syscall3(nr::NANOSLEEP, ts.as_ptr() as usize, 0, 0); }
}

fn mono_ms() -> u64 {
    let mut ts = [0i64; 2];
    unsafe { syscall3(nr::CLOCK_GETTIME, 1, ts.as_mut_ptr() as usize, 0); }
    ts[0] as u64 * 1000 + ts[1] as u64 / 1_000_000
}

/// A raw thread: no TLS, only raw syscalls. Names itself, spins (the busy
/// one) or sleeps until STOP, then exits just this thread.
extern "C" fn thread_main(i: usize) -> ! {
    let tid = unsafe { syscall3(SYS_GETTID, 0, 0, 0) } as u32;
    TIDS[i].store(tid, Ordering::SeqCst);
    let mut b = [0u8; 16];
    b[..THREAD_NAMES[i].len()].copy_from_slice(THREAD_NAMES[i]);
    prctl(15, b.as_ptr() as usize);
    READY.fetch_add(1, Ordering::SeqCst);
    if i == NTHREADS - 1 {
        // ~400 ms of spinning: per-thread utime must show it.
        let t0 = mono_ms();
        let mut x = 0u64;
        while mono_ms() - t0 < 400 {
            for k in 0..10000u64 { x = x.wrapping_mul(6364136223846793005).wrapping_add(k); }
            core::hint::black_box(x);
        }
        BUSY_DONE.store(1, Ordering::SeqCst);
    }
    while STOP.load(Ordering::SeqCst) == 0 { msleep(10); }
    loop { unsafe { syscall3(nr::EXIT, 0, 0, 0); } }
}

#[cfg(target_arch = "x86_64")]
unsafe fn spawn_thread(i: usize) -> isize {
    let top = (core::ptr::addr_of_mut!(STACKS[i]) as usize) + 65536;
    let sp = top - 16;
    *(sp as *mut usize) = thread_main as *const () as usize;
    *((sp + 8) as *mut usize) = i;
    let ret: isize;
    core::arch::asm!(
        "syscall",
        "test rax, rax",
        "jnz 2f",
        "pop rax",
        "pop rdi",
        "call rax",
        "ud2",
        "2:",
        inlateout("rax") nr::CLONE as isize => ret,
        in("rdi") CLONE_THREAD_FLAGS, in("rsi") sp, in("rdx") 0usize, in("r10") 0usize, in("r8") 0usize,
        lateout("rcx") _, lateout("r11") _,
        options(nostack)
    );
    ret
}

#[cfg(target_arch = "aarch64")]
unsafe fn spawn_thread(i: usize) -> isize {
    let top = (core::ptr::addr_of_mut!(STACKS[i]) as usize) + 65536;
    let sp = top - 16;
    *(sp as *mut usize) = thread_main as *const () as usize;
    *((sp + 8) as *mut usize) = i;
    let ret: isize;
    core::arch::asm!(
        "svc #0",
        "cbnz x0, 2f",
        "ldp x9, x0, [sp], #16",
        "blr x9",
        "brk #0",
        "2:",
        inlateout("x0") CLONE_THREAD_FLAGS as isize => ret,
        in("x1") sp, in("x2") 0usize, in("x3") 0usize, in("x4") 0usize, in("x8") nr::CLONE,
        out("x9") _,
        options(nostack)
    );
    ret
}

/// Every number in directory `path`.
fn dir_numbers(path: &[u8], out: &mut [u32]) -> usize {
    let mut n = 0;
    dir_each(path, |name| {
        if let Some(v) = parse_u64(name) { if n < out.len() { out[n] = v as u32; n += 1; } }
    });
    n
}

/// Does directory `path` hold an entry named `want`?
fn dir_has(path: &[u8], want: &[u8]) -> bool {
    let mut found = false;
    dir_each(path, |name| if name == want { found = true; });
    found
}

fn dir_each(path: &[u8], mut f: impl FnMut(&[u8])) {
    let Ok(fd) = sys_open(path, O_RDONLY | O_DIRECTORY) else { return };
    let mut buf = [0u8; 2048];
    loop {
        let got = match sys_getdents(fd, &mut buf) { Ok(0) | Err(_) => break, Ok(k) => k };
        let mut off = 0;
        while off + 19 <= got {
            let reclen = u16::from_le_bytes([buf[off + 16], buf[off + 17]]) as usize;
            if reclen == 0 { break; }
            let name = &buf[off + 19..off + reclen];
            let e = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            f(&name[..e]);
            off += reclen;
        }
    }
    sys_close(fd);
}

/// Stat fields after the `(comm)`: index 0 is field 3 (state).
fn stat_fields<'a>(s: &'a [u8], out: &mut [&'a [u8]; 24]) -> usize {
    let Some(rp) = s.iter().rposition(|&c| c == b')') else { return 0 };
    let mut nt = 0;
    for w in s[rp + 1..].split(|&c| c == b' ' || c == b'\n') {
        if w.is_empty() { continue; }
        if nt < 24 { out[nt] = w; nt += 1; }
    }
    nt
}

fn line_with<'a>(text: &'a [u8], needle: &[u8]) -> Option<&'a [u8]> {
    text.split(|&c| c == b'\n').find(|l| contains(l, needle))
}

unsafe fn test_threads(me: i32) {
    msleep(30); // the threads' starttime must differ from the leader's
    for i in 0..NTHREADS {
        if spawn_thread(i) <= 0 { check(false, "clone a thread"); return; }
    }
    let t0 = mono_ms();
    while (READY.load(Ordering::SeqCst) as usize) < NTHREADS && mono_ms() - t0 < 5000 { msleep(5); }
    check(READY.load(Ordering::SeqCst) as usize == NTHREADS, "3 threads started and named themselves");
    let mut tids = [0u32; NTHREADS];
    for i in 0..NTHREADS { tids[i] = TIDS[i].load(Ordering::SeqCst); }

    // /proc/<pid>/task lists the leader and the three threads
    let mut tp = Buf::<48>::new();
    let _ = write!(tp, "/proc/{}/task", me);
    let mut got = [0u32; 16];
    let n = dir_numbers(tp.bytes(), &mut got);
    let all_in = tids.iter().all(|t| got[..n].contains(t)) && got[..n].contains(&(me as u32));
    check(n == NTHREADS + 1 && all_in, "/proc/<pid>/task lists every thread");
    let n2 = dir_numbers(b"/proc/self/task", &mut got);
    check(n2 == NTHREADS + 1, "/proc/self/task agrees");

    // names through task/<tid>/comm, and /proc/<tid>/comm of a non-leader
    let mut names_ok = true;
    let mut b = [0u8; 32];
    for i in 0..NTHREADS {
        let mut p = Buf::<64>::new();
        let _ = write!(p, "/proc/{}/task/{}/comm", me, tids[i]);
        let k = read_file(p.bytes(), &mut b).unwrap_or(0);
        if k != THREAD_NAMES[i].len() + 1 || &b[..k - 1] != THREAD_NAMES[i] { names_ok = false; }
    }
    check(names_ok, "task/<tid>/comm shows each thread's name");
    let mut p = Buf::<64>::new();
    let _ = write!(p, "/proc/{}/task/{}/comm", me, me);
    let k = read_file(p.bytes(), &mut b).unwrap_or(0);
    check(&b[..k] == b"pstest\n", "task/<leader>/comm is the leader's name");
    let tc = comm_path(tids[0] as i32);
    let k = read_file(tc.bytes(), &mut b).unwrap_or(0);
    check(&b[..k] == b"pst-w0\n", "/proc/<tid>/comm of a non-leader thread");

    // a thread's directory lists the files; the process's lists `task`
    let mut d = Buf::<64>::new();
    let _ = write!(d, "/proc/{}/task/{}", me, tids[1]);
    check(dir_has(d.bytes(), b"stat") && dir_has(d.bytes(), b"comm") && dir_has(d.bytes(), b"cmdline")
          && !dir_has(d.bytes(), b"task"), "task/<tid>/ lists stat, comm, cmdline");
    let mut pd = Buf::<48>::new();
    let _ = write!(pd, "/proc/{}", me);
    check(dir_has(pd.bytes(), b"task"), "/proc/<pid>/ lists task");

    // writing a thread's comm through the task path
    let mut wp = Buf::<64>::new();
    let _ = write!(wp, "/proc/{}/task/{}/comm", me, tids[1]);
    check(write_comm(wp.bytes(), b"pst-renamed\n").is_ok(), "write task/<tid>/comm");
    let k = read_file(wp.bytes(), &mut b).unwrap_or(0);
    check(&b[..k] == b"pst-renamed\n", "the thread rename reads back");
    let k = read_file(comm_path(me).bytes(), &mut b).unwrap_or(0);
    check(&b[..k] == b"pstest\n", "renaming a thread leaves the leader's name");

    // per-thread stat: pid field, name, state; status Pid/Tgid
    let mut sp = Buf::<64>::new();
    let _ = write!(sp, "/proc/{}/task/{}/stat", me, tids[0]);
    let mut sb = [0u8; 512];
    let sn = read_file(sp.bytes(), &mut sb).unwrap_or(0);
    let mut want = Buf::<32>::new();
    let _ = write!(want, "{} (pst-w0) ", tids[0]);
    check(sb[..sn].starts_with(want.bytes()), "task/<tid>/stat: tid and thread name");
    let mut f: [&[u8]; 24] = [&[]; 24];
    let nf = stat_fields(&sb[..sn], &mut f);
    check(nf > 17 && (f[0] == b"S" || f[0] == b"R"), "task/<tid>/stat: the thread's state");
    let mut st = Buf::<64>::new();
    let _ = write!(st, "/proc/{}/task/{}/status", me, tids[0]);
    let mut stb = [0u8; 1536];
    let stn = read_file(st.bytes(), &mut stb).unwrap_or(0);
    let mut pidl = Buf::<32>::new();
    let _ = write!(pidl, "\nPid:\t{}\n", tids[0]);
    let mut tgl = Buf::<32>::new();
    let _ = write!(tgl, "\nTgid:\t{}\n", me);
    check(contains(&stb[..stn], b"Name:\tpst-w0\n") && contains(&stb[..stn], pidl.bytes())
          && contains(&stb[..stn], tgl.bytes()), "task/<tid>/status: Name, Pid, Tgid");
    // field 4 / PPid: the process's parent for every thread, not the thread
    // that created it (Linux: CLONE_THREAD shares the leader's real_parent)
    let pp = l::getppid() as u64;
    let mut ppl = Buf::<32>::new();
    let _ = write!(ppl, "\nPPid:\t{}\n", pp);
    let mut tstat = Buf::<48>::new();
    let _ = write!(tstat, "/proc/{}/stat", tids[0]);
    check(nf > 2 && parse_u64(f[1]) == Some(pp) && stat_u64(tstat.bytes(), 4) == Some(pp),
          "task/<tid>/stat and /proc/<tid>/stat: ppid is the process's parent");
    check(contains(&stb[..stn], ppl.bytes()), "task/<tid>/status: PPid is the process's parent");
    // starttime (field 22): the thread's own, after its leader's
    let mut lst = Buf::<64>::new();
    let _ = write!(lst, "/proc/{}/task/{}/stat", me, me);
    let lead_start = stat_u64(lst.bytes(), 22).unwrap_or(0);
    let proc_start = stat_u64(b"/proc/self/stat", 22).unwrap_or(0);
    let thr_start = if nf > 19 { parse_u64(f[19]).unwrap_or(0) } else { 0 };
    sayln!("  starttime: process {} leader {} thread {}", proc_start, lead_start, thr_start);
    check(lead_start == proc_start && thr_start > lead_start, "task/<tid>/stat starttime is the thread's own");

    // per-thread CPU time: the spinning thread, not the sleepers
    let t0 = mono_ms();
    while BUSY_DONE.load(Ordering::SeqCst) == 0 && mono_ms() - t0 < 10000 { msleep(10); }
    let cpu_of = |tid: u32| -> u64 {
        let mut p = Buf::<64>::new();
        let _ = write!(p, "/proc/{}/task/{}/stat", me, tid);
        let mut sb = [0u8; 512];
        let sn = read_file(p.bytes(), &mut sb).unwrap_or(0);
        let mut f: [&[u8]; 24] = [&[]; 24];
        if stat_fields(&sb[..sn], &mut f) < 13 { return 0; }
        parse_u64(f[11]).unwrap_or(0) + parse_u64(f[12]).unwrap_or(0)
    };
    let busy = cpu_of(tids[2]);
    let idle = cpu_of(tids[0]);
    sayln!("  busy thread {} ticks, idle thread {} ticks", busy, idle);
    check(busy >= 5 && busy > idle, "task/<tid>/stat carries per-thread utime+stime");

    // /proc lists only thread-group leaders ...
    let mut pids = [0u32; 1024];
    let np = dir_numbers(b"/proc", &mut pids);
    check(pids[..np].contains(&(me as u32)) && !tids.iter().any(|t| pids[..np].contains(t)),
          "/proc lists tgids only, no thread ids");
    // ... yet /proc/<tid> of a non-leader resolves
    let mut ts = Buf::<48>::new();
    let _ = write!(ts, "/proc/{}/stat", tids[0]);
    check(read_file(ts.bytes(), &mut sb).is_ok(), "/proc/<tid> of a non-leader works");
    let mut bad = Buf::<48>::new();
    let _ = write!(bad, "/proc/{}/task/1/stat", me);
    check(sys_open(bad.bytes(), O_RDONLY) == Err(2), "task/<foreign tid> is ENOENT");

    // ps -T / -L show the threads by name
    let mut out = [0u8; 16384];
    let mut pa = Buf::<16>::new();
    let _ = write!(pa, "{}\0", me);
    let n = run_ps(&[b"-T\0", b"-p\0", pa.bytes()], &mut out);
    let o = &out[..n];
    check(contains(o, b"pst-w0") && contains(o, b"pst-renamed") && contains(o, b"pst-busy")
          && o.iter().filter(|&&c| c == b'\n').count() == NTHREADS + 2, "ps -T -p <pid>: one row per thread, named");
    let n = run_ps(&[b"-L\0", b"-o\0", b"lwp=,comm=\0", b"-p\0", pa.bytes()], &mut out);
    let mut row = Buf::<32>::new();
    let _ = write!(row, "{:>7} pst-w0\n", tids[0]);
    check(contains(&out[..n], row.bytes()), "ps -L -o lwp,comm");

    let cpu_before = stat_u64(b"/proc/self/stat", 14).unwrap_or(0) + stat_u64(b"/proc/self/stat", 15).unwrap_or(0);
    STOP.store(1, Ordering::SeqCst);
    let t0 = mono_ms();
    while dir_numbers(b"/proc/self/task", &mut got) > 1 && mono_ms() - t0 < 5000 { msleep(10); }
    check(dir_numbers(b"/proc/self/task", &mut got) == 1, "exited threads leave the task dir");
    msleep(50); // the scheduler reaps the exited threads' slots
    let cpu_after = stat_u64(b"/proc/self/stat", 14).unwrap_or(0) + stat_u64(b"/proc/self/stat", 15).unwrap_or(0);
    sayln!("  process utime+stime {} -> {} ticks across the threads' exit", cpu_before, cpu_after);
    check(cpu_before >= busy && cpu_after >= cpu_before, "exited threads' CPU time stays in the process's utime/stime");
}

/// `pstest --argv-child ...`: the argv/environ subject. fd 3 reports steps to
/// the parent, fd 4 waits for it to go on.
unsafe fn argv_child(argc: usize, argv: *const *const u8) -> i32 {
    let step = |c: u8| { let _ = sys_write(3, &[c]); let mut x = [0u8; 1]; let _ = sys_read(4, &mut x); };
    step(b'1');
    // setproctitle in place: the argv area, zero-filled, with a new title
    let start = *argv as *mut u8;
    let last = arg(argv, argc - 1);
    let end = last.as_ptr().add(last.len() + 1) as *mut u8;
    let len = end as usize - start as usize;
    core::ptr::write_bytes(start, 0, len);
    let t = b"pstest: retitled";
    core::ptr::copy_nonoverlapping(t.as_ptr(), start, t.len());
    step(b'2');
    // a title longer than the argv area runs on into the environment
    let long = b"pstest: a much longer title, into the environment";
    core::ptr::copy_nonoverlapping(long.as_ptr(), start, long.len());
    *start.add(long.len()) = 0;
    step(b'3');
    0
}

const ARGV_ENV: &[u8] = b"PSTEST_ENV=some value";

unsafe fn test_argv() {
    let mut up = [0i32; 2];
    let mut down = [0i32; 2];
    l::pipe(up.as_mut_ptr());
    l::pipe(down.as_mut_ptr());
    let child = l::fork();
    if child == 0 {
        // onto fds 3 and 4 by way of high numbers (the pipes may sit on 3/4)
        l::dup3(up[1], 20, 0);
        l::dup3(down[0], 21, 0);
        for fd in [up[0], up[1], down[0], down[1]] { l::close(fd); }
        l::dup3(20, 3, 0);
        l::dup3(21, 4, 0);
        l::close(20);
        l::close(21);
        let argv: [*const u8; 5] = [b"pstest\0".as_ptr(), b"--argv-child\0".as_ptr(),
                                    b"hello world\0".as_ptr(), b"x\0".as_ptr(), core::ptr::null()];
        let envp: [*const u8; 2] = [b"PSTEST_ENV=some value\0".as_ptr(), core::ptr::null()];
        l::execve(b"/bin/pstest\0".as_ptr(), argv.as_ptr(), envp.as_ptr());
        l::exit(127);
    }
    l::close(up[1]);
    l::close(down[0]);
    let wait_step = |want: u8| -> bool { let mut x = [0u8; 1]; sys_read(up[0], &mut x) == Ok(1) && x[0] == want };
    let go = || { let _ = sys_write(down[1], b"g"); };
    let mut cp = Buf::<48>::new();
    let _ = write!(cp, "/proc/{}/cmdline", child);
    let mut ep = Buf::<48>::new();
    let _ = write!(ep, "/proc/{}/environ", child);
    let mut pa = Buf::<16>::new();
    let _ = write!(pa, "{}\0", child);
    let mut b = [0u8; 4096];
    let mut out = [0u8; 4096];

    check(wait_step(b'1'), "argv child started");
    let n = read_file(cp.bytes(), &mut b).unwrap_or(0);
    check(&b[..n] == b"pstest\0--argv-child\0hello world\0x\0", "cmdline: argv, NUL-separated, spaces kept");
    let n = read_file(ep.bytes(), &mut b).unwrap_or(0);
    check(n == ARGV_ENV.len() + 1 && &b[..n - 1] == ARGV_ENV && b[n - 1] == 0, "environ: envp, NUL-terminated");
    let n = run_ps(&[b"-o\0", b"args=\0", b"-p\0", pa.bytes()], &mut out);
    check(&out[..n] == b"pstest --argv-child hello world x\n", "ps -o args: the full argument list");
    let n = run_ps(&[b"-o\0", b"comm=\0", b"-p\0", pa.bytes()], &mut out);
    check(&out[..n] == b"pstest\n", "ps -o comm: still the short name");
    // environ is private to the owner (and root); cmdline is not
    let kid = l::fork();
    if kid == 0 {
        let _ = l::setresgid(1000, 1000, 1000);
        let _ = l::setresuid(1000, 1000, 1000);
        let e = sys_open(ep.bytes(), O_RDONLY);
        let c = sys_open(cp.bytes(), O_RDONLY);
        l::exit(if e == Err(13) && c.is_ok() { 0 } else { 1 });
    }
    let mut st = 0i32;
    l::wait4(kid, &mut st, 0, core::ptr::null_mut());
    check(st == 0, "another user's environ is EACCES, cmdline readable");
    go();

    check(wait_step(b'2'), "argv child retitled");
    let n = read_file(cp.bytes(), &mut b).unwrap_or(0);
    check(b[..n].starts_with(b"pstest: retitled\0"), "cmdline shows a setproctitle rewrite");
    let n = run_ps(&[b"-o\0", b"args=\0", b"-p\0", pa.bytes()], &mut out);
    check(&out[..n] == b"pstest: retitled\n", "ps -o args shows the new title");
    go();

    check(wait_step(b'3'), "argv child retitled past its argv area");
    let n = read_file(cp.bytes(), &mut b).unwrap_or(0);
    check(&b[..n] == b"pstest: a much longer title, into the environment\0",
          "cmdline: a title running into the env area (Linux rule)");
    go();
    l::wait4(child, &mut st, 0, core::ptr::null_mut());
    l::close(up[0]);
    l::close(down[1]);

    let n = read_file(b"/proc/self/cmdline", &mut b).unwrap_or(0);
    check(n > 0 && b[n - 1] == 0 && contains(&b[..n], b"pstest"), "/proc/self/cmdline");

    // kernel threads: empty cmdline, ps shows [comm]; none is called "init"
    let mut pids = [0u32; 1024];
    let np = dir_numbers(b"/proc", &mut pids);
    let mut kthreads = 0;
    let mut bad_name = false;
    let mut kpid = 0u32;
    for &p in &pids[..np] {
        let mut c = Buf::<48>::new();
        let _ = write!(c, "/proc/{}/cmdline", p);
        if read_file(c.bytes(), &mut b) != Ok(0) { continue; }
        kthreads += 1;
        if kpid == 0 { kpid = p; }
        let mut cb = [0u8; 32];
        let k = read_file(comm_path(p as i32).bytes(), &mut cb).unwrap_or(0);
        if &cb[..k] == b"init\n" || cb[..k].starts_with(b"[") {
            bad_name = true;
            sayln!("  kernel thread {} named {}", p, core::str::from_utf8(&cb[..k]).unwrap_or("?").trim_end());
        }
    }
    sayln!("  {} kernel threads", kthreads);
    check(kthreads > 0 && !bad_name, "kernel threads: empty cmdline, a real comm (not init, no brackets)");
    if kpid != 0 {
        let mut kp = Buf::<16>::new();
        let _ = write!(kp, "{}\0", kpid);
        let n = run_ps(&[b"-o\0", b"args=\0", b"-p\0", kp.bytes()], &mut out);
        check(n > 3 && out[0] == b'[' && out[n - 2] == b']', "ps shows a kernel thread as [comm]");
    }
}

const TIOCSCTTY: usize = 0x540E;
const TIOCGPTN: usize = 0x8004_5430;
const TIOCSPTLCK: usize = 0x4004_5431;

unsafe fn test_tty(me: i32) {
    let m = l::open(b"/dev/ptmx\0".as_ptr(), O_RDWR | 0o400 /* O_NOCTTY */, 0);
    if m < 0 { check(false, "open /dev/ptmx"); return; }
    let mut ptn: u32 = 0;
    let mut unlock: i32 = 0;
    if l::ioctl(m, TIOCGPTN, &mut ptn as *mut u32 as usize) != 0
        || l::ioctl(m, TIOCSPTLCK, &mut unlock as *mut i32 as usize) != 0 {
        check(false, "pty setup");
        l::close(m);
        return;
    }
    let mut slave = Buf::<24>::new();
    let _ = write!(slave, "/dev/pts/{}\0", ptn);
    let mut tname = Buf::<16>::new();
    let _ = write!(tname, "pts/{}", ptn);

    let mut outp = [0i32; 2];
    let mut hold = [0i32; 2];
    l::pipe(outp.as_mut_ptr());
    l::pipe(hold.as_mut_ptr());
    let a = l::fork();
    if a == 0 {
        // a session leader on the pty, a sleeper in its session, then ps
        l::setsid();
        let s = l::open(slave.bytes().as_ptr(), O_RDWR, 0);
        if s < 0 || l::ioctl(s, TIOCSCTTY, 0) != 0 { l::exit(2); }
        l::close(m);
        l::close(hold[1]);
        let b = l::fork();
        if b == 0 {
            l::close(outp[0]);
            l::close(outp[1]); // or the test never sees EOF on ps's output
            let mut x = [0u8; 1];
            let _ = sys_read(hold[0], &mut x); // until the test closes it
            l::exit(0);
        }
        l::close(hold[0]);
        l::close(outp[0]);
        l::dup3(outp[1], 1, 0);
        l::close(outp[1]);
        msleep(50);
        let argv: [*const u8; 2] = [b"/bin/ps\0".as_ptr(), core::ptr::null()];
        let envp: [*const u8; 1] = [core::ptr::null()];
        l::execve(b"/bin/ps\0".as_ptr(), argv.as_ptr(), envp.as_ptr());
        l::exit(127);
    }
    l::close(outp[1]);
    l::close(hold[0]);
    let mut out = [0u8; 4096];
    let mut n = 0;
    while n < out.len() {
        match sys_read(outp[0], &mut out[n..]) { Ok(0) | Err(_) => break, Ok(k) => n += k }
    }
    l::close(outp[0]);
    let o = &out[..n];
    sayln!("  ps under {}:", core::str::from_utf8(tname.bytes()).unwrap_or("?"));
    for line in o.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
        sayln!("    {}", core::str::from_utf8(line).unwrap_or("?"));
    }
    let rows = o.split(|&c| c == b'\n').skip(1).filter(|l| !l.is_empty()).count();
    let all_on_tty = o.split(|&c| c == b'\n').skip(1).filter(|l| !l.is_empty())
        .all(|l| contains(l, tname.bytes()));
    check(rows == 2 && all_on_tty, "default ps: the caller's tty only (ps and its sibling)");
    check(!has_pid_line(o, me as u32), "default ps: not the test itself (another tty)");
    // the sleeper's pid from its row, then its stat fields 7 and 8
    let bpid = line_with(o, b"pstest").and_then(|l| {
        l.split(|&c| c == b' ').find(|w| !w.is_empty()).and_then(parse_u64)
    }).unwrap_or(0) as u32;
    let mut sp = Buf::<48>::new();
    let _ = write!(sp, "/proc/{}/stat", bpid);
    let mut sb = [0u8; 512];
    let sn = read_file(sp.bytes(), &mut sb).unwrap_or(0);
    let mut f: [&[u8]; 24] = [&[]; 24];
    let nf = stat_fields(&sb[..sn], &mut f);
    let want_nr = (136u64 + ptn as u64 / 256) << 8 | (ptn as u64 % 256);
    check(bpid != 0 && nf > 6 && parse_u64(f[4]) == Some(want_nr), "stat tty_nr: pts major 136, minor N");
    check(nf > 6 && parse_u64(f[5]) == Some(a as u64), "stat tpgid: the terminal's foreground group");
    let mut bp = Buf::<16>::new();
    let _ = write!(bp, "{}\0", bpid);
    let n = run_ps(&[b"-o\0", b"tty=\0", b"-p\0", bp.bytes()], &mut out);
    let mut want = Buf::<24>::new();
    want.push(tname.bytes());
    check(trim_eq(&out[..n], want.bytes()), "ps -o tty names it pts/N");
    let mut tsel = Buf::<24>::new();
    tsel.push(tname.bytes());
    tsel.push(b"\0");
    let n = run_ps(&[b"-t\0", tsel.bytes(), b"-o\0", b"pid=\0"], &mut out);
    check(has_pid_line(&out[..n], bpid) && !has_pid_line(&out[..n], me as u32)
          && out[..n].iter().filter(|&&c| c == b'\n').count() == 1, "ps -t pts/N selects by terminal");

    l::close(hold[1]); // the sleeper exits
    let mut st = 0i32;
    l::wait4(a, &mut st, 0, core::ptr::null_mut());
    l::close(m);

    let mut sb2 = [0u8; 512];
    let n = read_file(b"/proc/self/stat", &mut sb2).unwrap_or(0);
    let mut f2: [&[u8]; 24] = [&[]; 24];
    let nf = stat_fields(&sb2[..n], &mut f2);
    sayln!("  own tty_nr {}", core::str::from_utf8(if nf > 4 { f2[4] } else { b"?" }).unwrap_or("?"));
}

/// One line of output, trailing blanks ignored, equals `want`.
fn trim_eq(a: &[u8], want: &[u8]) -> bool {
    let mut a = a.strip_suffix(b"\n").unwrap_or(a);
    while let Some(r) = a.strip_suffix(b" ") { a = r; }
    a == want
}

// ── lane procfix: accounting, PR_SET_MM, st_rdev, sysstat ───────────────────

#[cfg(target_arch = "x86_64")] const SYS_EPOLL_CREATE1: usize = 291;
#[cfg(target_arch = "aarch64")] const SYS_EPOLL_CREATE1: usize = 20;
#[cfg(target_arch = "x86_64")] const SYS_EPOLL_PWAIT: usize = 281;
#[cfg(target_arch = "aarch64")] const SYS_EPOLL_PWAIT: usize = 22;
#[cfg(target_arch = "x86_64")] const SYS_TIMES: usize = 100;
#[cfg(target_arch = "aarch64")] const SYS_TIMES: usize = 153;
#[cfg(target_arch = "x86_64")] const SYS_GETRUSAGE: usize = 98;
#[cfg(target_arch = "aarch64")] const SYS_GETRUSAGE: usize = 165;
#[cfg(target_arch = "x86_64")] const ST_RDEV_OFF: usize = 40;
#[cfg(target_arch = "aarch64")] const ST_RDEV_OFF: usize = 32;

/// `st_rdev` of `path` (NUL-terminated), or of `fd` when `path` is empty.
fn rdev_of(path: &[u8], fd: i32) -> Option<u64> {
    let mut st = [0u8; 256];
    let r = unsafe {
        if path.is_empty() { syscall3(nr::FSTAT, fd as usize, st.as_mut_ptr() as usize, 0) }
        else { leandros_libc::syscall::syscall4(nr::NEWFSTATAT, AT_FDCWD as usize, path.as_ptr() as usize,
                                                st.as_mut_ptr() as usize, 0) }
    };
    if r != 0 { return None; }
    Some(u64::from_ne_bytes(st[ST_RDEV_OFF..ST_RDEV_OFF + 8].try_into().unwrap()))
}

fn makedev(maj: u64, min: u64) -> u64 { (maj << 8) | (min & 0xff) | ((min & !0xff) << 12) }

/// Fields of /proc/<path> stat: index 0 is field 3 (state).
fn stat_u64(path: &[u8], field: usize) -> Option<u64> {
    let mut sb = [0u8; 512];
    let n = read_file(path, &mut sb).ok()?;
    let mut f: [&[u8]; 24] = [&[]; 24];
    let nf = stat_fields(&sb[..n], &mut f);
    if field < 3 || field - 3 >= nf { return None; }
    parse_u64(f[field - 3])
}

fn burn_ms(ms: u64) {
    let t0 = mono_ms();
    let mut x = 0u64;
    while mono_ms() - t0 < ms {
        for k in 0..10000u64 { x = x.wrapping_mul(6364136223846793005).wrapping_add(k); }
        core::hint::black_box(x);
    }
}

static mut TOUCH: [u8; 64 * 4096] = [0; 64 * 4096];

unsafe fn test_accounting() {
    // Own page faults: pstest itself demand-paged its text from the file
    // (major) and its stack/bss (minor).
    let minflt = stat_u64(b"/proc/self/stat", 10).unwrap_or(0);
    let majflt = stat_u64(b"/proc/self/stat", 12).unwrap_or(0);
    sayln!("  own minflt {} majflt {}", minflt, majflt);
    check(minflt > 0 && majflt > 0, "stat minflt/majflt count this process's page faults");

    // A child that burns ~300 ms of CPU and touches 64 fresh pages: once
    // waited for, its time and faults are the parent's cutime/cminflt.
    let cut0 = stat_u64(b"/proc/self/stat", 16).unwrap_or(0) + stat_u64(b"/proc/self/stat", 17).unwrap_or(0);
    let cmin0 = stat_u64(b"/proc/self/stat", 11).unwrap_or(0);
    let c = l::fork();
    if c == 0 {
        let t = &mut *core::ptr::addr_of_mut!(TOUCH);
        for i in 0..64 { core::ptr::write_volatile(&mut t[i * 4096], 1); }
        burn_ms(300);
        l::exit(0);
    }
    let mut st = 0i32;
    // Before the wait the child's time is not ours yet.
    msleep(50);
    l::wait4(c, &mut st, 0, core::ptr::null_mut());
    let cut1 = stat_u64(b"/proc/self/stat", 16).unwrap_or(0) + stat_u64(b"/proc/self/stat", 17).unwrap_or(0);
    let cmin1 = stat_u64(b"/proc/self/stat", 11).unwrap_or(0);
    sayln!("  cutime+cstime {} -> {} ticks, cminflt {} -> {}", cut0, cut1, cmin0, cmin1);
    check(cut1 >= cut0 + 15, "stat cutime+cstime: a waited-for child's CPU time");
    check(cmin1 >= cmin0 + 64, "stat cminflt: a waited-for child's page faults");
    let mut tms = [0u64; 4];
    syscall3(SYS_TIMES, tms.as_mut_ptr() as usize, 0, 0);
    check(tms[2] + tms[3] >= 15 && tms[0] + tms[1] > 0, "times(2): tms_utime/stime and tms_cutime/cstime");
    let mut ru = [0i64; 18];
    syscall3(SYS_GETRUSAGE, (-1isize) as usize, ru.as_mut_ptr() as usize, 0);
    let ch_ms = ru[0] * 1000 + ru[1] / 1000 + ru[2] * 1000 + ru[3] / 1000;
    check(ch_ms >= 150 && ru[8] as u64 >= 64, "getrusage(RUSAGE_CHILDREN): time and ru_minflt");
    let mut ru = [0i64; 18];
    syscall3(SYS_GETRUSAGE, 0, ru.as_mut_ptr() as usize, 0);
    check(ru[8] > 0 && ru[9] > 0, "getrusage(RUSAGE_SELF): ru_minflt and ru_majflt");

    // A grandchild's time reaches us through the child that waited for it.
    let c = l::fork();
    if c == 0 {
        let g = l::fork();
        if g == 0 { burn_ms(200); l::exit(0); }
        let mut s = 0i32;
        l::wait4(g, &mut s, 0, core::ptr::null_mut());
        l::exit(0);
    }
    l::wait4(c, &mut st, 0, core::ptr::null_mut());
    let cut2 = stat_u64(b"/proc/self/stat", 16).unwrap_or(0) + stat_u64(b"/proc/self/stat", 17).unwrap_or(0);
    check(cut2 >= cut1 + 10, "cutime includes the children's own waited-for children");

    // An exited thread's time stays in the process's utime (see test_threads:
    // the busy thread has exited by now).
}

/// prctl(PR_SET_MM, PR_SET_MM_ARG_START/END...) in a forked child: the new
/// bounds are what /proc/self/cmdline reads; Linux's permission and argument
/// rules.
unsafe fn test_set_mm() {
    static mut TITLE: [u8; 32] = [0; 32];
    let c = l::fork();
    if c == 0 {
        let t = &mut *core::ptr::addr_of_mut!(TITLE);
        t[..18].copy_from_slice(b"new-title\0arg-two\0");
        let base = t.as_ptr() as usize;
        let mm = |opt: usize, addr: usize, a4: usize| -> isize {
            leandros_libc::syscall::syscall5(SYS_PRCTL, 35, opt, addr, a4, 0)
        };
        let mut code = 0;
        if mm(8, base, 1) != -22 { code |= 1; }          // arg4 != 0: EINVAL
        if mm(8, 100, 0) != -22 { code |= 2; }           // below mmap_min_addr: EINVAL
        if mm(8, base, 0) != 0 { code |= 4; }            // ARG_START (<= old end)
        if mm(9, base + 18, 0) != 0 { code |= 8; }       // ARG_END
        let mut b = [0u8; 64];
        let n = read_file(b"/proc/self/cmdline", &mut b).unwrap_or(0);
        if &b[..n] != b"new-title\0arg-two\0" { code |= 16; }
        // ARG_START past ARG_END: EINVAL, nothing changes
        if mm(8, base + 20, 0) != -22 { code |= 32; }
        // unprivileged: EPERM
        l::setuid(1000);
        if mm(9, base + 10, 0) != -1 { code |= 64; }
        l::exit(code);
    }
    let mut st = 0i32;
    l::wait4(c, &mut st, 0, core::ptr::null_mut());
    let code = (st >> 8) & 0xff;
    if code != 0 { sayln!("  PR_SET_MM child code {:#x}", code); }
    check(st & 0x7f == 0 && code & 3 == 0, "PR_SET_MM: arg4 != 0 and addr < mmap_min_addr are EINVAL");
    check(st & 0x7f == 0 && code & (4 | 8 | 16) == 0, "PR_SET_MM_ARG_START/END move what /proc/<pid>/cmdline reads");
    check(st & 0x7f == 0 && code & 32 == 0, "PR_SET_MM: start past end is EINVAL");
    check(st & 0x7f == 0 && code & 64 == 0, "PR_SET_MM: EPERM without CAP_SYS_RESOURCE (non-root)");
}

/// /proc/sysstat: `w` lines (where every task waits) and the per-call epoll
/// timing (`s` wall column, `e` lines).
unsafe fn test_sysstat(me: i32) {
    let ep = syscall3(SYS_EPOLL_CREATE1, 0, 0, 0);
    if ep < 0 { check(false, "epoll_create1"); return; }
    let mut evs = [0u8; 64];
    let t0 = mono_ms();
    for _ in 0..3 {
        leandros_libc::syscall::syscall6(SYS_EPOLL_PWAIT, ep as usize, evs.as_mut_ptr() as usize, 4, 40, 0, 8);
    }
    let waited = mono_ms() - t0;
    sys_close(ep as i32);
    static mut SS: [u8; 262144] = [0; 262144];
    let buf = &mut *core::ptr::addr_of_mut!(SS);
    let n = read_file(b"/proc/sysstat", buf).unwrap_or(0);
    let text = &buf[..n];
    let mut wl = Buf::<32>::new();
    let _ = write!(wl, "w {} {} ", me, me);
    check(text.split(|&c| c == b'\n').any(|l| l.starts_with(wl.bytes())), "/proc/sysstat: a `w` line for this task");
    let mut el = Buf::<32>::new();
    let _ = write!(el, "e {} ", me);
    let e = text.split(|&c| c == b'\n').find(|l| l.starts_with(el.bytes()));
    let mut v = [0u64; 6];
    if let Some(e) = e {
        for (i, w) in e.split(|&c| c == b' ').skip(1).take(6).enumerate() { v[i] = parse_u64(w).unwrap_or(0); }
        sayln!("  {}", core::str::from_utf8(e).unwrap_or("?"));
    }
    // e tgid calls oncpu_ns wall_ns blocked_ns zero
    check(v[1] >= 3 && v[5] >= 3, "/proc/sysstat `e`: epoll wait calls and timeouts counted");
    check(v[3] >= 100_000_000 && v[3] <= (waited + 50) * 1_000_000 && v[4] + v[2] == v[3] && v[4] > v[2],
          "/proc/sysstat `e`: wall = on-CPU + blocked, blocked time measured");
}

/// ps's simple selections (procps-ng) and the terminals' st_rdev, around a
/// pty session of a live leader and a sleeper.
unsafe fn test_ps_simple(me: i32) {
    let m = l::open(b"/dev/ptmx\0".as_ptr(), O_RDWR | 0o400 /* O_NOCTTY */, 0);
    if m < 0 { check(false, "open /dev/ptmx"); return; }
    let mut ptn: u32 = 0;
    let mut unlock: i32 = 0;
    l::ioctl(m, TIOCGPTN, &mut ptn as *mut u32 as usize);
    l::ioctl(m, TIOCSPTLCK, &mut unlock as *mut i32 as usize);
    let mut slave = Buf::<24>::new();
    let _ = write!(slave, "/dev/pts/{}\0", ptn);
    let mut rep = [0i32; 2];
    let mut hold = [0i32; 2];
    l::pipe(rep.as_mut_ptr());
    l::pipe(hold.as_mut_ptr());
    let leader = l::fork();
    if leader == 0 {
        l::setsid();
        let s = l::open(slave.bytes().as_ptr(), O_RDWR, 0);
        if s < 0 || l::ioctl(s, TIOCSCTTY, 0) != 0 { l::exit(2); }
        l::close(m);
        l::close(hold[1]);
        l::close(rep[0]);
        // /dev/tty of a pty session: the pty's own device number
        let ok = rdev_of(b"/dev/tty\0", -1) == Some(makedev(136 + ptn as u64 / 256, ptn as u64 % 256));
        let sl = l::fork();
        if sl == 0 {
            let mut x = [0u8; 1];
            let _ = sys_read(hold[0], &mut x);
            l::exit(0);
        }
        let msg = [sl as u32, ok as u32];
        let _ = sys_write(rep[1], core::slice::from_raw_parts(msg.as_ptr() as *const u8, 8));
        let mut x = [0u8; 1];
        let _ = sys_read(hold[0], &mut x);
        let mut st = 0i32;
        l::wait4(sl, &mut st, 0, core::ptr::null_mut());
        l::exit(0);
    }
    l::close(rep[1]);
    l::close(hold[0]);
    let mut msg = [0u8; 8];
    let _ = sys_read(rep[0], &mut msg);
    l::close(rep[0]);
    let sleeper = u32::from_ne_bytes(msg[0..4].try_into().unwrap());
    let leader = leader as u32;
    msleep(50);

    // st_rdev: the device numbers tty_nr is made of (5:1 console, 4:N VT,
    // 136:N pts, 5:2 ptmx), by path and by fd.
    let pts = makedev(136 + ptn as u64 / 256, ptn as u64 % 256);
    let mut sp = Buf::<48>::new();
    let _ = write!(sp, "/proc/{}/stat", sleeper);
    let tty_nr = stat_u64(sp.bytes(), 7);
    check(rdev_of(slave.bytes(), -1) == Some(pts) && tty_nr == Some(pts),
          "st_rdev of /dev/pts/N is 136:N, the same number as tty_nr");
    check(u32::from_ne_bytes(msg[4..8].try_into().unwrap()) == 1, "st_rdev of /dev/tty in a pty session is the pty's");
    check(rdev_of(b"", m) == Some(makedev(5, 2)) && rdev_of(b"/dev/ptmx\0", -1) == Some(makedev(5, 2)),
          "st_rdev of the pty master and /dev/ptmx is 5:2");
    check(rdev_of(b"/dev/console\0", -1) == Some(makedev(5, 1)) && rdev_of(b"/dev/tty1\0", -1) == Some(makedev(4, 1))
          && rdev_of(b"/dev/null\0", -1) == Some(makedev(1, 3)),
          "st_rdev of /dev/console, /dev/tty1, /dev/null: 5:1, 4:1, 1:3");

    let mut out = [0u8; 16384];
    let my_tty = stat_u64(b"/proc/self/stat", 7).unwrap_or(0);
    let my_sid = stat_u64(b"/proc/self/stat", 6).unwrap_or(0) as u32;
    let n = run_ps(&[b"-a\0", b"-o\0", b"pid=\0"], &mut out);
    check(has_pid_line(&out[..n], sleeper) && !has_pid_line(&out[..n], leader) && !has_pid_line(&out[..n], 1)
          && (my_tty == 0 || has_pid_line(&out[..n], me as u32))
          && (my_sid == me as u32 || !has_pid_line(&out[..n], my_sid)),
          "ps -a: processes with a terminal, minus session leaders");
    let n = run_ps(&[b"a\0", b"-o\0", b"pid=\0"], &mut out);
    let rows_a = out[..n].iter().filter(|&&c| c == b'\n').count();
    let ok_a = has_pid_line(&out[..n], sleeper) && has_pid_line(&out[..n], leader);
    let n = run_ps(&[b"-e\0", b"-o\0", b"pid=\0"], &mut out);
    let rows_e = out[..n].iter().filter(|&&c| c == b'\n').count();
    check(ok_a && rows_a < rows_e, "ps a: every process with a terminal, session leaders too (not all)");
    let n = run_ps(&[b"x\0", b"-o\0", b"pid=\0"], &mut out);
    check(has_pid_line(&out[..n], 1) && has_pid_line(&out[..n], me as u32), "ps x: the caller's own processes, tty or not");
    let n = run_ps(&[b"-d\0", b"-o\0", b"pid=\0"], &mut out);
    check(has_pid_line(&out[..n], sleeper) && !has_pid_line(&out[..n], leader), "ps -d: all but session leaders");
    let n = run_ps(&[b"-ax\0", b"-o\0", b"pid=\0"], &mut out);
    check(has_pid_line(&out[..n], 1) && has_pid_line(&out[..n], sleeper), "ps -ax is BSD ax: every process");

    l::close(hold[1]);
    let mut st = 0i32;
    l::wait4(leader as i32, &mut st, 0, core::ptr::null_mut());
    l::close(m);
}
