//! errno — single-threaded storage for Stage 1.
//!
//! Stage 2 will replace this with a per-thread TLS slot once TPIDR_EL0 is
//! initialised by `__libc_start_main`.

use core::sync::atomic::{AtomicI32, Ordering};

static ERRNO_VAL: AtomicI32 = AtomicI32::new(0);

#[inline]
pub fn set_errno(e: i32) {
    ERRNO_VAL.store(e, Ordering::Relaxed);
}

#[inline]
pub fn get_errno() -> i32 {
    ERRNO_VAL.load(Ordering::Relaxed)
}

/// C ABI: `int *__errno_location(void)` — called by `errno` macro in C headers.
///
/// Returning a pointer to a global atomic is safe for single-threaded Stage 1.
#[no_mangle]
pub unsafe extern "C" fn __errno_location() -> *mut i32 {
    ERRNO_VAL.as_ptr()
}

// POSIX errno constants.
pub const EPERM:   i32 = 1;
pub const ENOENT:  i32 = 2;
pub const ESRCH:   i32 = 3;
pub const EINTR:   i32 = 4;
pub const EIO:     i32 = 5;
pub const EBADF:   i32 = 9;
pub const ECHILD:  i32 = 10;
pub const EAGAIN:  i32 = 11;
pub const ENOMEM:  i32 = 12;
pub const EACCES:  i32 = 13;
pub const EFAULT:  i32 = 14;
pub const EBUSY:   i32 = 16;
pub const EEXIST:  i32 = 17;
pub const EXDEV:   i32 = 18;
pub const ENOTDIR: i32 = 20;
pub const EISDIR:  i32 = 21;
pub const EINVAL:  i32 = 22;
pub const ENOSPC:  i32 = 28;
pub const EPIPE:   i32 = 32;
pub const ENOLCK:  i32 = 37;
pub const ENOSYS:  i32 = 38;
pub const ENOTEMPTY: i32 = 39;
