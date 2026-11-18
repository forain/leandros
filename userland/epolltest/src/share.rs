//! Lane epref: an epoll instance is an open file description. fork shares it,
//! and a child's close / close-on-exec / exit drops only the child's
//! reference; dup keeps it alive across close of the original, and a nested
//! registration of it in an outer instance keeps reporting (Linux keys that
//! registration by the file, not by the number). cosmic-panel lost its
//! applet-compositor epoll (nested in calloop's) on x86_64 virgl sessions.

use crate::*;

extern "C" {
    fn fork() -> c_int;
    fn waitpid(pid: c_int, stat_loc: *mut c_int, options: c_int) -> c_int;
    fn execve(path: *const u8, argv: *const *const u8, envp: *const *const u8) -> c_int;
    fn _exit(code: c_int) -> !;
    fn dup(fd: c_int) -> c_int;
}

const EPOLL_CLOEXEC: c_int = 0x8_0000;
const AF_UNIX: c_int = 1;
const SOCK_STREAM: c_int = 1;
const SOCK_CLOEXEC: c_int = 0x8_0000;
const CHILDREN: u32 = 20;

unsafe fn add(ep: c_int, fd: c_int) -> bool {
    let mut ev = epoll_event { events: EPOLLIN, data: epoll_data { u64: fd as u64 } };
    epoll_ctl(ep, EPOLL_CTL_ADD, fd, &mut ev) == 0
}

unsafe fn ready(ep: c_int, tmo: c_int) -> c_int {
    let mut evs = [epoll_event { events: 0, data: epoll_data { u64: 0 } }; 4];
    epoll_wait(ep, evs.as_mut_ptr(), 4, tmo)
}

/// What each forked child does with the inherited epoll fds.
unsafe fn child(mode: u32, inner: c_int, outer: c_int) -> ! {
    match mode {
        0 | 3 => {
            let argv = [b"/bin/epolltest\0".as_ptr(), b"churnx\0".as_ptr(), core::ptr::null()];
            let envp = [core::ptr::null::<u8>()];
            execve(argv[0], argv.as_ptr(), envp.as_ptr());
            _exit(127)
        }
        1 => { close(inner); close(outer); _exit(0) }
        _ => _exit(0),
    }
}

unsafe fn fork_children(mode: u32, inner: c_int, outer: c_int) -> bool {
    let mut ok = true;
    for _ in 0..CHILDREN {
        let pid = fork();
        if pid == 0 { child(mode, inner, outer); }
        if pid < 0 { ok = false; continue; }
        let mut st = 0;
        if waitpid(pid, &mut st, 0) != pid || st != 0 { ok = false; }
    }
    ok
}

struct ThreadFork { mode: u32, inner: c_int, outer: c_int, ok: bool }

extern "C" fn thread_fork(a: *mut c_void) -> *mut c_void {
    unsafe {
        let t = &mut *(a as *mut ThreadFork);
        t.ok = fork_children(t.mode, t.inner, t.outer);
    }
    core::ptr::null_mut()
}

/// inner (nested in outer) must still work after the children are gone.
unsafe fn share_case(name: &[u8], mode: u32) -> bool {
    let inner = epoll_create1(EPOLL_CLOEXEC);
    let outer = epoll_create1(EPOLL_CLOEXEC);
    let mut ok = inner >= 0 && outer >= 0 && add(outer, inner);
    if mode == 3 {
        // Fork from a non-leader thread (cosmic-panel spawns its applets so).
        let mut t = ThreadFork { mode, inner, outer, ok: false };
        let mut th: pthread_t = core::ptr::null_mut();
        if pthread_create(&mut th, core::ptr::null(), thread_fork, &mut t as *mut _ as *mut c_void) == 0 {
            pthread_join(th, core::ptr::null_mut());
            ok &= t.ok;
        } else { ok = false; }
    } else {
        ok &= fork_children(mode, inner, outer);
    }
    let mut sv = [-1; 2];
    ok &= socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv.as_mut_ptr()) == 0;
    ok &= add(inner, sv[0]);
    ok &= write(sv[1], b"x".as_ptr(), 1) == 1;
    ok &= ready(outer, 2000) == 1;
    ok &= ready(inner, 0) == 1;
    close(sv[0]); close(sv[1]); close(inner); close(outer);
    report(name, ok)
}

/// dup an inner epoll fd nested in outer, close the original: the dup still
/// works, and outer still reports it (the file is alive).
unsafe fn dup_case() -> bool {
    let inner = epoll_create1(EPOLL_CLOEXEC);
    let outer = epoll_create1(EPOLL_CLOEXEC);
    let mut ok = inner >= 0 && outer >= 0 && add(outer, inner);
    let d = dup(inner);
    ok &= d >= 0 && close(inner) == 0;
    let mut sv = [-1; 2];
    ok &= socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sv.as_mut_ptr()) == 0;
    ok &= add(d, sv[0]);
    ok &= write(sv[1], b"x".as_ptr(), 1) == 1;
    ok &= ready(d, 0) == 1;
    ok &= ready(outer, 2000) == 1;
    close(sv[0]); close(sv[1]); close(d); close(outer);
    report(b"epoll_share_dup_close_original\0", ok)
}

/// Number of checks `share_main` reports.
pub const SHARE_CHECKS: i32 = 5;

/// Returns the number of failed checks.
pub unsafe fn share_main() -> i32 {
    let mut f = 0;
    if !share_case(b"epoll_share_fork_exec_cloexec\0", 0) { f += 1; }
    if !share_case(b"epoll_share_fork_close_exit\0", 1) { f += 1; }
    if !share_case(b"epoll_share_fork_exit\0", 2) { f += 1; }
    if !share_case(b"epoll_share_thread_fork_exec\0", 3) { f += 1; }
    if !dup_case() { f += 1; }
    f
}
