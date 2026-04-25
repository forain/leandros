//! CyanOS Init - userspace init program (PID 1)
//!
//! This is the first userspace program that runs and manages the system.

#![no_std]
#![no_main]

extern crate cyanos_libc;

use cyanos_libc::{write, STDOUT_FILENO, getpid, exit, fork, execve, sched_yield};

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
    
    // Call shell via its path in the initrd
    let path = b"/bin/shell\0";
    let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
    
    execve(path.as_ptr(), argv.as_ptr(), core::ptr::null());

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
