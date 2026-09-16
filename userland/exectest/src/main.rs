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
    pub fn exit(status: i32) -> !;
    pub fn _exit(status: i32) -> !;
    pub fn __errno_location() -> *mut c_int;

    pub fn fork() -> pid_t;
    pub fn waitpid(pid: pid_t, stat_loc: *mut c_int, options: c_int) -> pid_t;
    pub fn kill(pid: pid_t, sig: c_int) -> c_int;
    pub fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> c_int;
    pub fn nanosleep(rqtp: *const timespec, rmtp: *mut timespec) -> c_int;
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
pub unsafe extern "C" fn exec_main(_argc: isize, _argv: *mut *mut u8, _envp: *mut *mut u8) -> i32 {
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

    puts(b"--- exectest done ---\0".as_ptr());
    if failures == 0 {
        puts(b"EXECTEST: PASS\0".as_ptr());
    } else {
        let mut line = *b"EXECTEST: FAIL 0\0";
        line[15] = b'0' + (failures as u8 % 10);
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
