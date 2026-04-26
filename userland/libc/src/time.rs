//! POSIX time functions: clock_gettime, nanosleep.

use crate::syscall::{nr, syscall2};
use crate::io::{c_int};

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct timespec {
    pub tv_sec:  i64,
    pub tv_nsec: i64,
}

pub const CLOCK_REALTIME:  i32 = 0;
pub const CLOCK_MONOTONIC: i32 = 1;

/// Get the current time of the specified clock.
#[no_mangle]
pub unsafe extern "C" fn clock_gettime(clk_id: i32, tp: *mut timespec) -> c_int {
    let r = syscall2(nr::CLOCK_GETTIME, clk_id as usize, tp as usize);
    if r < 0 { crate::errno::set_errno(-r as i32); -1 } else { 0 }
}

/// Sleep for the specified duration.
#[no_mangle]
pub unsafe extern "C" fn nanosleep(req: *const timespec, rem: *mut timespec) -> c_int {
    let r = syscall2(nr::NANOSLEEP, req as usize, rem as usize);
    if r < 0 { crate::errno::set_errno(-r as i32); -1 } else { 0 }
}

/// Sleep for `ms` milliseconds.
#[no_mangle]
pub unsafe extern "C" fn usleep(usec: u32) -> c_int {
    let req = timespec {
        tv_sec:  (usec / 1_000_000) as i64,
        tv_nsec: ((usec % 1_000_000) * 1000) as i64,
    };
    nanosleep(&req, core::ptr::null_mut())
}
