//! CyanOS Init - userspace init program (PID 1)
//!
//! This is the first userspace program that runs and manages the system.

#![no_std]
#![no_main]

extern crate cyanos_libc;

use cyanos_libc::{write, STDOUT_FILENO, getpid, exit, fork, execve, sched_yield};

// Embedded shell binary for Phase 1 execve
#[cfg(target_arch = "aarch64")]
static SHELL_BINARY: &[u8] = include_bytes!("../../target/aarch64-unknown-none/release/shell");
#[cfg(target_arch = "x86_64")]
static SHELL_BINARY: &[u8] = include_bytes!("../../target/x86_64-unknown-none/release/shell");

/// Called by `__libc_start_main` after the C runtime is set up.
#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    write_str("CyanOS Init (PID 1) starting...\n");

    // Show our PID
    write_str("Init PID: ");
    write_u32(getpid() as u32);
    write_str("\n");

    write_str("Init process running successfully!\n");
    
    write_str("Launching shell via execve...\n");
    
    // Phase 1 backward-compat: if path points to ELF magic and argv is a length,
    // the kernel loads the ELF directly from that memory.
    let path_ptr = SHELL_BINARY.as_ptr() as usize;
    let len_as_argv = SHELL_BINARY.len();
    
    execve(path_ptr as *const u8, len_as_argv as *const *const u8, core::ptr::null());

    write_str("ERROR: execve failed!\n");
    loop {
        sched_yield();
    }
}

unsafe fn write_str(s: &str) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

unsafe fn write_u32(mut n: u32) {
    let mut buf = [0u8; 10];
    if n == 0 {
        write(STDOUT_FILENO, b"0".as_ptr(), 1);
        return;
    }
    let mut i = 10usize;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write(STDOUT_FILENO, buf.as_ptr().add(i), 10 - i);
}