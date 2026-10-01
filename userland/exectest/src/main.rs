//! exectest — `#!` (shebang) scripts through `execve`, Linux `binfmt_script`
//! semantics, against fixtures staged in /bin by mkfs-f2fs-populated.py from
//! `userland/exectest/scripts/`:
//!
//!  1. `#!/bin/brush` script: argv becomes `[/bin/brush, /bin/<script>, args]`,
//!     so `$0` is the script and `$1` the first caller argument.
//!  2. `#!/usr/bin/env brush`: the one optional interpreter argument.
//!  3. A script whose interpreter is itself a script nests: the outer line's
//!     argument and path are inserted after the inner interpreter.
//!  4. Leading/trailing blanks on the `#!` line are ignored.
//!  5. A `#!` script without any execute bit → EACCES.
//!  6. A non-ELF, non-`#!` file → ENOEXEC.
//!  7. A `#!` naming a missing interpreter → ENOENT.
//!  8. A script that names itself as interpreter → ELOOP after the recursion
//!     limit.
//!  9. `/bin/start-cosmic-leandros` — the one production script — carries a
//!     `#!` line (the file is checked, not run).
//! 10. execve from a multi-threaded process: no sibling thread may observe the
//!     close-on-exec sweep. Linux kills the siblings (de_thread) *before*
//!     closing cloexec fds; closing first let a tokio worker blocked on its
//!     signal self-pipe (a SOCK_CLOEXEC socketpair) wake to EBADF and panic
//!     "Bad read on self-pipe" whenever brush ran `exec <cmd>`.
//! 11. stdio redirection through fork + dup2 + execve, the way a shell sets
//!     it up: `2>FILE`, `>&2` inside a child whose stderr was redirected by
//!     its parent (`sh -c 'cmd >&2' 2>FILE`), `>FILE 2>&1`, `2>&1 >FILE`
//!     against both the raw console and a pipe, `/dev/stdout` opened before a
//!     redirect, `/dev/console` while stdin is redirected, and the same
//!     shapes feeding a pipeline. Each on tmpfs (/tmp) and f2fs (/data).
//!
//! Shape as sigtest2: relibc_start_v1 entry, "<name>: PASS"/"<name>: FAIL at
//! step N" per check, "EXECTEST: PASS"/"EXECTEST: FAIL <n>" summary, exit
//! code = failure count.

#![no_std]
#![no_main]
#![allow(non_camel_case_types)]

use core::ffi::c_void;

type c_int = i32;
type c_long = i64;
type time_t = i64;
type pid_t = c_int;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct timespec {
    pub tv_sec: time_t,
    pub tv_nsec: c_long,
}

const SIGKILL: c_int = 9;
const WNOHANG: c_int = 1;

const O_RDONLY: c_int = 0;
const O_WRONLY: c_int = 1;
const O_CREAT:  c_int = 0o100;
const O_TRUNC:  c_int = 0o1000;

const ENOENT:  c_int = 2;
const ENOEXEC: c_int = 8;
const EACCES:  c_int = 13;
const ELOOP:   c_int = 40;

const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const O_NONBLOCK: c_int = 0o4000;

fn wifexited(s: c_int) -> bool    { s & 0x7f == 0 }
fn wexitstatus(s: c_int) -> c_int { (s >> 8) & 0xff }

extern "C" {
    pub fn relibc_start_v1(
        sp: *const c_void,
        main: unsafe extern "C" fn(argc: isize, argv: *mut *mut u8, envp: *mut *mut u8) -> i32,
    ) -> !;

    pub fn puts(s: *const u8) -> i32;
    pub fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    pub fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    pub fn open(path: *const u8, oflag: c_int, ...) -> c_int;
    pub fn close(fd: i32) -> i32;
    pub fn pipe(fds: *mut c_int) -> c_int;
    pub fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    pub fn dup2(oldfd: c_int, newfd: c_int) -> c_int;
    pub fn unlink(path: *const u8) -> c_int;
    pub fn exit(status: i32) -> !;
    pub fn _exit(status: i32) -> !;
    pub fn __errno_location() -> *mut c_int;

    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn kill(pid: pid_t, sig: c_int) -> c_int;
    pub fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> c_int;
    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
    pub fn socketpair(domain: c_int, kind: c_int, protocol: c_int, sv: *mut c_int) -> c_int;
    pub fn pthread_create(
        thread: *mut usize,
        attr: *const c_void,
        start_routine: extern "C" fn(*mut c_void) -> *mut c_void,
        arg: *mut c_void,
    ) -> c_int;
}

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   xor rbp, rbp",
    "   mov rdi, rsp",
    "   mov rsi, offset exec_main",
    "   and rsp, -16",
    "   call relibc_start_v1",
    "   ud2"
);

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(
    ".section .text._start",
    ".global _start",
    "_start:",
    "   mov x29, #0",
    "   mov x30, #0",
    "   mov x0, sp",
    "   adrp x1, exec_main",
    "   add x1, x1, :lo12:exec_main",
    "   and sp, x0, #-16",
    "   bl relibc_start_v1",
    "   brk #0"
);

const ENVP: [*const u8; 3] = [
    b"PATH=/usr/bin:/bin\0".as_ptr(),
    b"HOME=/root\0".as_ptr(),
    core::ptr::null(),
];

#[no_mangle]
pub unsafe extern "C" fn exec_main(argc: isize, argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
    // Re-exec target for test 10: exit at once, touching nothing.
    if argc > 1 && cstr_is(*argv.add(1), b"--exit0\0") { return 0; }
    // Re-exec targets for test 11.
    if argc > 1 { redir_stage(*argv.add(1)); }

    let mut failures = 0;

    if !test_script_argv() { failures += 1; }
    if !test_env_interpreter_arg() { failures += 1; }
    if !test_nested_interpreter() { failures += 1; }
    if !test_blank_trimming() { failures += 1; }
    if !test_noexec_bit_eacces() { failures += 1; }
    if !test_plain_file_enoexec() { failures += 1; }
    if !test_missing_interpreter_enoent() { failures += 1; }
    if !test_self_interpreter_eloop() { failures += 1; }
    if !test_launcher_has_shebang() { failures += 1; }
    if !test_exec_hides_cloexec_from_siblings() { failures += 1; }
    for base in [b"/tmp/.exectest-redir\0".as_slice(), b"/data/.exectest-redir\0".as_slice()] {
        for (i, c) in REDIR_CASES.iter().enumerate() {
            if !test_redir(base, c, i) { failures += 1; }
        }
    }

    puts(b"--- exectest done ---\0".as_ptr());
    if failures == 0 {
        puts(b"EXECTEST: PASS\0".as_ptr());
    } else {
        let mut line = *b"EXECTEST: FAIL 00\0";
        line[15] = b'0' + (failures as u8 / 10 % 10);
        line[16] = b'0' + (failures as u8 % 10);
        puts(line.as_ptr());
    }
    failures
}

#[panic_handler]
fn panic(_info: &core::panic::PanicInfo) -> ! {
    unsafe { exit(134); }
}

// ── Helpers ────────────────────────────────────────────────────────────────

unsafe fn report(name: &[u8], ok: bool) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    if ok { puts(b": PASS\0".as_ptr()); } else { puts(b": FAIL\0".as_ptr()); }
    ok
}

unsafe fn fail_at(name: &[u8], step: u8) -> bool {
    write(1, name.as_ptr(), name.len() - 1);
    let mut tail = *b": FAIL at step 00\0";
    tail[15] = b'0' + step / 10;
    tail[16] = b'0' + step % 10;
    puts(tail.as_ptr());
    false
}

unsafe fn nap() {
    let ts = timespec { tv_sec: 0, tv_nsec: 10_000_000 };
    nanosleep(&ts, core::ptr::null_mut());
}

/// What one `execve` attempt produced: the child's exit status and whatever
/// it wrote to stdout (an exec failure exits with `errno`).
struct Run {
    status: c_int,
    out: [u8; 256],
    out_len: usize,
}

/// fork; child: stdout → pipe, execve(path, argv, ENVP), `_exit(errno)` on
/// failure. Parent: collect stdout (nonblocking, ~5 s budget) and the status.
unsafe fn run(path: &[u8], argv: &[*const u8]) -> Option<Run> {
    let mut fds: [c_int; 2] = [0; 2];
    if pipe(fds.as_mut_ptr()) != 0 { return None; }
    let (rfd, wfd) = (fds[0], fds[1]);
    let fl = fcntl(rfd, F_GETFL, 0);
    if fl < 0 || fcntl(rfd, F_SETFL, fl | O_NONBLOCK) != 0 { return None; }

    let child = fork();
    if child < 0 { return None; }
    if child == 0 {
        close(rfd);
        dup2(wfd, 1);
        if wfd != 1 { close(wfd); }
        execve(path.as_ptr(), argv.as_ptr(), ENVP.as_ptr());
        _exit(*__errno_location());
    }
    close(wfd);

    let mut r = Run { status: 0, out: [0u8; 256], out_len: 0 };
    let mut reaped = false;
    let mut eof = false;
    for _ in 0..500 {
        if !eof && r.out_len < r.out.len() {
            let n = read(rfd, r.out.as_mut_ptr().add(r.out_len), r.out.len() - r.out_len);
            if n > 0 { r.out_len += n as usize; continue; }
            if n == 0 { eof = true; }
        }
        if !reaped {
            let mut st: c_int = 0;
            let w = waitpid(child, &mut st, WNOHANG);
            if w == child { r.status = st; reaped = true; }
            else if w < 0 { break; }
        }
        if reaped && (eof || r.out_len >= r.out.len()) { break; }
        nap();
    }
    close(rfd);
    if !reaped {
        kill(child, SIGKILL);
        let mut st: c_int = 0;
        for _ in 0..300 {
            if waitpid(child, &mut st, WNOHANG) == child { break; }
            nap();
        }
        return None;
    }
    Some(r)
}

fn out_is(r: &Run, want: &[u8]) -> bool {
    // brush's echo ends the line with '\n'; the pty is not involved here, so
    // no CR translation. Accept an optional trailing newline.
    let got = &r.out[..r.out_len];
    got == want || (got.len() == want.len() + 1 && &got[..want.len()] == want && got[want.len()] == b'\n')
}

unsafe fn exec_errno_is(name: &[u8], path: &[u8], want: c_int) -> bool {
    let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
    let r = match run(path, &argv) { Some(r) => r, None => return fail_at(name, 1) };
    if !wifexited(r.status) { return fail_at(name, 2); }
    if wexitstatus(r.status) != want {
        // Say what came back instead: "<name>: FAIL at step 03" then the errno.
        let got = wexitstatus(r.status) as u8;
        let mut line = *b"  execve errno 000\0";
        line[15] = b'0' + got / 100;
        line[16] = b'0' + (got / 10) % 10;
        line[17] = b'0' + got % 10;
        puts(line.as_ptr());
        return fail_at(name, 3);
    }
    report(name, true)
}

// ── 1. argv rewriting: interpreter, script path, caller args ───────────────

unsafe fn test_script_argv() -> bool {
    let name = b"script_argv\0";
    let path = b"/bin/exectest-echo.sh\0";
    // argv[0] as the caller names it is dropped; the script path replaces it.
    let argv: [*const u8; 4] = [b"whatever\0".as_ptr(), b"one\0".as_ptr(), b"two\0".as_ptr(), core::ptr::null()];
    let r = match run(path, &argv) { Some(r) => r, None => return fail_at(name, 1) };
    if !wifexited(r.status) || wexitstatus(r.status) != 0 { return fail_at(name, 2); }
    if !out_is(&r, b"ARGS:/bin/exectest-echo.sh:one:two") {
        write(1, b"  got: ".as_ptr(), 7);
        write(1, r.out.as_ptr(), r.out_len);
        return fail_at(name, 3);
    }
    report(name, true)
}

// ── 2. `#!/usr/bin/env brush` — the optional interpreter argument ──────────

unsafe fn test_env_interpreter_arg() -> bool {
    let name = b"env_interpreter_arg\0";
    // env must exist for this to mean anything.
    let e = open(b"/usr/bin/env\0".as_ptr(), O_RDONLY);
    if e < 0 { return fail_at(name, 1); }
    close(e);
    let path = b"/bin/exectest-env.sh\0";
    let argv: [*const u8; 3] = [path.as_ptr(), b"arg\0".as_ptr(), core::ptr::null()];
    let r = match run(path, &argv) { Some(r) => r, None => return fail_at(name, 2) };
    if !wifexited(r.status) || wexitstatus(r.status) != 0 { return fail_at(name, 3); }
    if !out_is(&r, b"ENV:/bin/exectest-env.sh:arg") {
        write(1, b"  got: ".as_ptr(), 7);
        write(1, r.out.as_ptr(), r.out_len);
        return fail_at(name, 4);
    }
    report(name, true)
}

// ── 3. an interpreter that is itself a script ──────────────────────────────

unsafe fn test_nested_interpreter() -> bool {
    let name = b"nested_interpreter\0";
    // exectest-nested.sh: `#!/bin/exectest-echo.sh nestedarg`
    // → [/bin/exectest-echo.sh, nestedarg, /bin/exectest-nested.sh]
    // → [/bin/brush, /bin/exectest-echo.sh, nestedarg, /bin/exectest-nested.sh]
    let path = b"/bin/exectest-nested.sh\0";
    let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
    let r = match run(path, &argv) { Some(r) => r, None => return fail_at(name, 1) };
    if !wifexited(r.status) || wexitstatus(r.status) != 0 { return fail_at(name, 2); }
    if !out_is(&r, b"ARGS:/bin/exectest-echo.sh:nestedarg:/bin/exectest-nested.sh") {
        write(1, b"  got: ".as_ptr(), 7);
        write(1, r.out.as_ptr(), r.out_len);
        return fail_at(name, 3);
    }
    report(name, true)
}

// ── 4. blanks around the interpreter are not part of it ────────────────────

unsafe fn test_blank_trimming() -> bool {
    let name = b"blank_trimming\0";
    let path = b"/bin/exectest-trail.sh\0";
    let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
    let r = match run(path, &argv) { Some(r) => r, None => return fail_at(name, 1) };
    if !wifexited(r.status) || wexitstatus(r.status) != 0 { return fail_at(name, 2); }
    if !out_is(&r, b"TRAIL:/bin/exectest-trail.sh") {
        write(1, b"  got: ".as_ptr(), 7);
        write(1, r.out.as_ptr(), r.out_len);
        return fail_at(name, 3);
    }
    report(name, true)
}

// ── 5–8. failure modes ─────────────────────────────────────────────────────

unsafe fn test_noexec_bit_eacces() -> bool {
    exec_errno_is(b"noexec_bit_eacces\0", b"/bin/exectest-noexec.sh\0", EACCES)
}

unsafe fn test_plain_file_enoexec() -> bool {
    exec_errno_is(b"plain_file_enoexec\0", b"/bin/exectest-data.txt\0", ENOEXEC)
}

unsafe fn test_missing_interpreter_enoent() -> bool {
    exec_errno_is(b"missing_interpreter_enoent\0", b"/bin/exectest-missing.sh\0", ENOENT)
}

unsafe fn test_self_interpreter_eloop() -> bool {
    exec_errno_is(b"self_interpreter_eloop\0", b"/bin/exectest-loop.sh\0", ELOOP)
}

// ── 9. the session launcher is directly executable ─────────────────────────

unsafe fn test_launcher_has_shebang() -> bool {
    let name = b"launcher_has_shebang\0";
    let fd = open(b"/bin/start-cosmic-leandros\0".as_ptr(), O_RDONLY);
    if fd < 0 {
        // Not staged on this image (no artifacts tree): nothing to check.
        puts(b"launcher_has_shebang: SKIP (not staged)\0".as_ptr());
        return true;
    }
    let mut head = [0u8; 8];
    let n = read(fd, head.as_mut_ptr(), head.len());
    close(fd);
    if n < 2 { return fail_at(name, 1); }
    if &head[..2] != b"#!" { return fail_at(name, 2); }
    report(name, true)
}

// ── 10. execve with a live sibling thread: siblings die before cloexec ─────

const AF_UNIX: c_int = 1;
const SOCK_STREAM: c_int = 1;
const SOCK_CLOEXEC: c_int = 0o2000000;
const EINTR: c_int = 4;
const EAGAIN: c_int = 11;

unsafe fn cstr_is(p: *const u8, want: &[u8]) -> bool {
    if p.is_null() { return false; }
    for (i, &b) in want.iter().enumerate() {
        if *p.add(i) != b { return false; }
    }
    true
}

/// (socket the watcher blocks on, report pipe write end). Written by the
/// forked child before it creates the thread, so plain statics suffice.
static mut WATCH_FD: c_int = -1;
static mut REPORT_FD: c_int = -1;

/// Polls a nonblocking cloexec socket nobody writes to, the way a busy tokio
/// worker would find it: anything but EAGAIN means this thread was still
/// running after its process's execve had begun closing cloexec fds. It
/// reports what it saw ('0' = EOF, else the errno) and exits.
extern "C" fn exec_watcher(_: *mut c_void) -> *mut c_void {
    unsafe {
        let mut b = [0u8; 1];
        loop {
            let n = read(WATCH_FD, b.as_mut_ptr(), 1);
            if n < 0 {
                let e = *__errno_location();
                if e == EINTR || e == EAGAIN { continue; }
            }
            let tag = if n == 0 { b'0' } else if n < 0 { *__errno_location() as u8 } else { b'D' };
            write(REPORT_FD, &tag, 1);
            return core::ptr::null_mut();
        }
    }
}

unsafe fn test_exec_hides_cloexec_from_siblings() -> bool {
    let name = b"exec_hides_cloexec_from_siblings\0";
    const ROUNDS: usize = 30;
    for _ in 0..ROUNDS {
        let mut rp: [c_int; 2] = [0; 2];
        if pipe(rp.as_mut_ptr()) != 0 { return fail_at(name, 1); }
        let child = fork();
        if child < 0 { return fail_at(name, 2); }
        if child == 0 {
            close(rp[0]);
            let mut sv: [c_int; 2] = [0; 2];
            if socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv.as_mut_ptr()) != 0 { _exit(97); }
            let fl = fcntl(sv[0], F_GETFL, 0);
            if fl < 0 || fcntl(sv[0], F_SETFL, fl | O_NONBLOCK) != 0 { _exit(96); }
            WATCH_FD = sv[0];
            REPORT_FD = rp[1];
            let mut t: usize = 0;
            if pthread_create(&mut t, core::ptr::null(), exec_watcher, core::ptr::null_mut()) != 0 {
                _exit(98);
            }
            nap(); // let the watcher start polling
            let argv: [*const u8; 3] = [b"/bin/exectest\0".as_ptr(), b"--exit0\0".as_ptr(), core::ptr::null()];
            execve(b"/bin/exectest\0".as_ptr(), argv.as_ptr(), ENVP.as_ptr());
            _exit(99);
        }
        close(rp[1]);
        let mut st: c_int = 0;
        let mut reaped = false;
        for _ in 0..500 {
            if waitpid(child, &mut st, WNOHANG) == child { reaped = true; break; }
            nap();
        }
        if !reaped { kill(child, SIGKILL); close(rp[0]); return fail_at(name, 3); }
        if !wifexited(st) || wexitstatus(st) != 0 { close(rp[0]); return fail_at(name, 4); }
        // The exec'd image (and every holder of rp[1]) is gone: read to EOF.
        let mut tag = [0u8; 1];
        let n = read(rp[0], tag.as_mut_ptr(), 1);
        close(rp[0]);
        if n != 0 {
            let mut line = *b"  sibling saw the cloexec sweep: 000\0";
            let v = tag[0];
            line[33] = b'0' + v / 100;
            line[34] = b'0' + (v / 10) % 10;
            line[35] = b'0' + v % 10;
            puts(line.as_ptr());
            return fail_at(name, 5);
        }
    }
    report(name, true)
}

// ── 11. stdio redirection across fork/exec ──────────────────────────────────
//
// Re-exec stages (argv[1]); each is one link of what a shell does:
//   --emit        write "<O>\n" to fd 1, then "<E>\n" to fd 2, exit 0
//
// Checks are by marker, not exact bytes: the exec'd image's startup writes
// loader diagnostics to fd 2, which lands wherever stderr was redirected.
//   --dup21-emit  dup2(2, 1), then exec --emit   (`cmd >&2`)
//   --dup12-emit  dup2(1, 2), then exec --emit   (`cmd 2>&1`)

unsafe fn exec_stage(stage: &[u8]) -> ! {
    let argv: [*const u8; 3] = [b"/bin/exectest\0".as_ptr(), stage.as_ptr(), core::ptr::null()];
    execve(b"/bin/exectest\0".as_ptr(), argv.as_ptr(), ENVP.as_ptr());
    _exit(99);
}

unsafe fn redir_stage(arg: *const u8) {
    if cstr_is(arg, b"--emit\0") {
        let a = write(1, b"<O>\n".as_ptr(), 4);
        let b = write(2, b"<E>\n".as_ptr(), 4);
        _exit(if a == 4 && b == 4 { 0 } else { 91 });
    }
    if cstr_is(arg, b"--dup21-emit\0") {
        if dup2(2, 1) != 1 { _exit(90); }
        exec_stage(b"--emit\0");
    }
    if cstr_is(arg, b"--dup12-emit\0") {
        if dup2(1, 2) != 2 { _exit(90); }
        exec_stage(b"--emit\0");
    }
}

/// One redirect scenario. `setup` runs in the forked child with `f` (FILE,
/// opened O_WRONLY|O_CREAT|O_TRUNC) and `w` (the write end of the parent's
/// capture pipe); it must exec or `_exit`. The parent then compares what
/// reached the pipe and the file: each of `<O>` (stdout), `<E>` (stderr),
/// `<S>` and `<C>` must appear exactly where listed and nowhere else.
struct RedirCase {
    name: &'static [u8],
    setup: unsafe fn(f: c_int, w: c_int) -> !,
    want_pipe: &'static [&'static [u8]],
    want_file: &'static [&'static [u8]],
}

const MARKERS: [&[u8]; 4] = [b"<O>", b"<E>", b"<S>", b"<C>"];

fn has(hay: &[u8], needle: &[u8]) -> bool {
    hay.len() >= needle.len() && hay.windows(needle.len()).any(|w| w == needle)
}

/// Every marker in `want` present in `got`, every other marker absent.
fn markers_match(got: &[u8], want: &[&[u8]]) -> bool {
    MARKERS.iter().all(|m| has(got, m) == want.contains(m))
}

/// `sh -c 'cmd' 2>FILE`, stdout captured.
unsafe fn rc_stderr_file(f: c_int, w: c_int) -> ! {
    dup2(f, 2); dup2(w, 1); close(f); close(w);
    exec_stage(b"--emit\0");
}
/// `sh -c 'cmd >&2' 2>FILE`: the redirect is made in the parent, survives
/// exec, and the exec'd image dups it again before exec'ing the command.
unsafe fn rc_nested_gt_amp2(f: c_int, w: c_int) -> ! {
    dup2(f, 2); dup2(w, 1); close(f); close(w);
    exec_stage(b"--dup21-emit\0");
}
/// `cmd >FILE 2>&1`.
unsafe fn rc_file_then_2to1(f: c_int, w: c_int) -> ! {
    dup2(f, 1); dup2(1, 2); close(f); close(w);
    exec_stage(b"--emit\0");
}
/// `cmd 2>&1 >FILE` with stdout on the raw console: stderr must stay on the
/// console (the "E" is printed there), FILE gets stdout only.
unsafe fn rc_2to1_then_file_console(f: c_int, w: c_int) -> ! {
    close(w);
    close(1); // fd 1 untracked = the raw console
    dup2(1, 2); dup2(f, 1); close(f);
    exec_stage(b"--emit\0");
}
/// `cmd 2>&1 >FILE` with stdout a pipe: stderr to the pipe, stdout to FILE.
unsafe fn rc_2to1_then_file_pipe(f: c_int, w: c_int) -> ! {
    dup2(w, 1); dup2(1, 2); dup2(f, 1); close(f); close(w);
    exec_stage(b"--emit\0");
}
/// `/dev/stdout` names the object fd 1 held at open time, not the slot.
unsafe fn rc_dev_stdout_snapshot(f: c_int, w: c_int) -> ! {
    dup2(f, 1); close(f);
    let g = open(b"/dev/stdout\0".as_ptr(), O_WRONLY);
    if g < 0 { _exit(92); }
    dup2(w, 1); close(w);
    _exit(if write(g, b"<S>\n".as_ptr(), 4) == 4 { 0 } else { 93 });
}
/// `/dev/console` is the console even while stdin is redirected from a file
/// (it used to write into whatever fd 0 named). The "C" lands on the console.
unsafe fn rc_dev_console_vs_stdin(f: c_int, w: c_int) -> ! {
    close(w);
    dup2(f, 0); close(f);
    let g = open(b"/dev/console\0".as_ptr(), O_WRONLY);
    if g < 0 { _exit(92); }
    _exit(if write(g, b"<C>\n".as_ptr(), 4) == 4 { 0 } else { 93 });
}
/// `sh -c 'cmd >&2' 2>&1 | reader`.
unsafe fn rc_pipeline_gt_amp2(f: c_int, w: c_int) -> ! {
    close(f);
    dup2(w, 2); close(w);
    close(1);
    exec_stage(b"--dup21-emit\0");
}
/// `sh -c 'cmd 2>&1' | reader`.
unsafe fn rc_pipeline_2to1(f: c_int, w: c_int) -> ! {
    close(f);
    dup2(w, 1); close(w);
    exec_stage(b"--dup12-emit\0");
}

static REDIR_CASES: [RedirCase; 9] = [
    RedirCase { name: b"redir_stderr_file", setup: rc_stderr_file, want_pipe: &[b"<O>"], want_file: &[b"<E>"] },
    RedirCase { name: b"redir_nested_gt_amp2", setup: rc_nested_gt_amp2, want_pipe: &[], want_file: &[b"<O>", b"<E>"] },
    RedirCase { name: b"redir_file_then_2to1", setup: rc_file_then_2to1, want_pipe: &[], want_file: &[b"<O>", b"<E>"] },
    RedirCase { name: b"redir_2to1_then_file_console", setup: rc_2to1_then_file_console, want_pipe: &[], want_file: &[b"<O>"] },
    RedirCase { name: b"redir_2to1_then_file_pipe", setup: rc_2to1_then_file_pipe, want_pipe: &[b"<E>"], want_file: &[b"<O>"] },
    RedirCase { name: b"redir_dev_stdout_snapshot", setup: rc_dev_stdout_snapshot, want_pipe: &[], want_file: &[b"<S>"] },
    RedirCase { name: b"redir_dev_console_vs_stdin", setup: rc_dev_console_vs_stdin, want_pipe: &[], want_file: &[] },
    RedirCase { name: b"redir_pipeline_gt_amp2", setup: rc_pipeline_gt_amp2, want_pipe: &[b"<O>", b"<E>"], want_file: &[] },
    RedirCase { name: b"redir_pipeline_2to1", setup: rc_pipeline_2to1, want_pipe: &[b"<O>", b"<E>"], want_file: &[] },
];

unsafe fn test_redir(base: &[u8], c: &RedirCase, _idx: usize) -> bool {
    // "<name>_tmp" / "<name>_f2fs"
    let mut name = [0u8; 64];
    let suffix: &[u8] = if base.starts_with(b"/tmp") { b"_tmp\0" } else { b"_f2fs\0" };
    name[..c.name.len()].copy_from_slice(c.name);
    name[c.name.len()..c.name.len() + suffix.len()].copy_from_slice(suffix);
    let name = &name[..c.name.len() + suffix.len()];

    unlink(base.as_ptr());
    let mut fds: [c_int; 2] = [0; 2];
    if pipe(fds.as_mut_ptr()) != 0 { return fail_at(name, 1); }
    let (rfd, wfd) = (fds[0], fds[1]);
    let fl = fcntl(rfd, F_GETFL, 0);
    if fl < 0 || fcntl(rfd, F_SETFL, fl | O_NONBLOCK) != 0 { return fail_at(name, 2); }

    let child = fork();
    if child < 0 { return fail_at(name, 3); }
    if child == 0 {
        close(rfd);
        let f = open(base.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC, 0o644);
        if f < 0 { _exit(94); }
        (c.setup)(f, wfd);
    }
    close(wfd);

    let mut out = [0u8; 4096];
    let mut out_len = 0usize;
    let mut st: c_int = 0;
    let mut reaped = false;
    let mut eof = false;
    for _ in 0..500 {
        if !eof && out_len < out.len() {
            let n = read(rfd, out.as_mut_ptr().add(out_len), out.len() - out_len);
            if n > 0 { out_len += n as usize; continue; }
            if n == 0 { eof = true; }
        }
        if !reaped {
            let w = waitpid(child, &mut st, WNOHANG);
            if w == child { reaped = true; } else if w < 0 { break; }
        }
        if reaped && eof { break; }
        nap();
    }
    close(rfd);
    if !reaped {
        kill(child, SIGKILL);
        for _ in 0..300 { if waitpid(child, &mut st, WNOHANG) == child { break; } nap(); }
        return fail_at(name, 4);
    }
    if !wifexited(st) || wexitstatus(st) != 0 {
        let got = wexitstatus(st) as u8;
        let mut line = *b"  child exit 000\0";
        line[13] = b'0' + got / 100;
        line[14] = b'0' + (got / 10) % 10;
        line[15] = b'0' + got % 10;
        puts(line.as_ptr());
        return fail_at(name, 5);
    }

    let mut file = [0u8; 4096];
    let mut file_len = 0usize;
    let fd = open(base.as_ptr(), O_RDONLY);
    if fd < 0 { return fail_at(name, 6); }
    loop {
        let n = read(fd, file.as_mut_ptr().add(file_len), file.len() - file_len);
        if n <= 0 || file_len + n as usize >= file.len() { if n > 0 { file_len += n as usize; } break; }
        file_len += n as usize;
    }
    close(fd);
    unlink(base.as_ptr());

    if !markers_match(&out[..out_len], c.want_pipe) {
        puts(b"  pipe got:\0".as_ptr());
        write(1, out.as_ptr(), out_len);
        return fail_at(name, 7);
    }
    if !markers_match(&file[..file_len], c.want_file) {
        puts(b"  file got:\0".as_ptr());
        write(1, file.as_ptr(), file_len);
        return fail_at(name, 8);
    }
    report(name, true)
}
