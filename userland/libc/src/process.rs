//! Process lifecycle: exit, abort, fork, exec, getpid, wait.

use crate::syscall::{nr, syscall0, syscall1, syscall3, syscall4};

pub type pid_t = i32;

/// Wait for a child process to change state.
#[no_mangle]
pub unsafe extern "C" fn wait4(
    pid: pid_t,
    wstatus: *mut i32,
    options: i32,
    rusage: *mut u8,
) -> pid_t {
    let r = syscall4(
        nr::WAIT4, pid as usize,
        wstatus as usize, options as usize, rusage as usize,
    );
    if r < 0 { crate::errno::set_errno(-r as i32); -1 } else { r as pid_t }
}

/// Terminate the process with `status`.  Never returns.
#[no_mangle]
pub extern "C" fn exit(status: i32) -> ! {
    unsafe { syscall1(nr::EXIT_GROUP, status as usize); }
    loop { core::hint::spin_loop(); }
}

/// Abnormal termination (SIGABRT equivalent for Stage 1).
#[no_mangle]
pub extern "C" fn abort() -> ! {
    exit(134) // 128 + SIGABRT(6)
}

/// Return the PID of the calling process.
#[no_mangle]
pub unsafe extern "C" fn getpid() -> pid_t {
    syscall0(nr::GETPID) as pid_t
}

/// Return the PID of the parent process.
#[no_mangle]
pub unsafe extern "C" fn getppid() -> pid_t {
    syscall0(nr::GETPPID) as pid_t
}

/// Fork the current process.  Returns 0 in child, child PID in parent, -1 on error.
#[no_mangle]
pub unsafe extern "C" fn fork() -> pid_t {
    // clone(SIGCHLD, 0)
    const SIGCHLD: usize = 17;
    let r = syscall1(nr::CLONE, SIGCHLD);
    r as pid_t
}

/// Execute a program. On success this does not return.
#[no_mangle]
pub unsafe extern "C" fn execve(
    path: *const u8,
    argv: *const *const u8,
    envp: *const *const u8,
) -> i32 {
    let r = syscall3(nr::EXECVE, path as usize, argv as usize, envp as usize);
    crate::errno::set_errno(-r as i32);
    -1
}

/// Yield the CPU to the scheduler.
#[no_mangle]
pub unsafe extern "C" fn sched_yield() -> i32 {
    syscall0(nr::SCHED_YIELD) as i32
}

/// Return the real user ID (always 0 on Cyanos for now).
#[no_mangle]
pub unsafe extern "C" fn getuid() -> u32 {
    syscall0(nr::GETUID) as u32
}

/// Return the real group ID.
#[no_mangle]
pub unsafe extern "C" fn getgid() -> u32 {
    syscall0(nr::GETGID) as u32
}

/// Return the effective user ID.
#[no_mangle]
pub unsafe extern "C" fn geteuid() -> u32 {
    syscall0(nr::GETEUID) as u32
}

/// Return the effective group ID.
#[no_mangle]
pub unsafe extern "C" fn getegid() -> u32 {
    syscall0(nr::GETEGID) as u32
}
