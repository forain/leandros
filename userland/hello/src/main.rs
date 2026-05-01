//! hello — minimal Leandros user-space program.
//!
//! Links against leandros-libc which provides `_start`, memory allocation,
//! and I/O.  Build with `scripts/build-userland.sh` to get a static ELF.

#![no_std]
#![no_main]

// Pull in leandros-libc so its `_start` / `__libc_start_main` are linked.
extern crate leandros_libc;

use leandros_libc::{write, STDOUT_FILENO, getpid};

/// Called by `__libc_start_main` after the C runtime is set up.
#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let pid = getpid();

    // Write a greeting using the raw write() syscall wrapper.
    let msg = b"Hello from Leandros userland!\n";
    write(STDOUT_FILENO, msg.as_ptr(), msg.len());

    // Print PID using puts-level formatting (no printf dependency for Stage 1).
    let pid_msg = b"PID: ";
    write(STDOUT_FILENO, pid_msg.as_ptr(), pid_msg.len());
    write_u32(pid as u32);
    write(STDOUT_FILENO, b"\n".as_ptr(), 1);

    0
}

/// Write a u32 as decimal digits to stdout.
unsafe fn write_u32(mut n: u32) {
    let mut buf = [0u8; 10];
    if n == 0 { write(STDOUT_FILENO, b"0".as_ptr(), 1); return; }
    let mut i = 10usize;
    while n > 0 { i -= 1; buf[i] = b'0' + (n % 10) as u8; n /= 10; }
    write(STDOUT_FILENO, buf.as_ptr().add(i), 10 - i);
}
