//! cyanos-libc — minimal C runtime for Cyanos user-space programs.
//!
//! Provides the symbols that relibc (Stage 2) will eventually supply.
//! Every function makes raw Linux-ABI syscalls (SVC #0 on AArch64) using
//! the same syscall numbers Cyanos's kernel already implements.
//!
//! # Layers
//! - `syscall`  — raw inline-asm wrappers + syscall number constants
//! - `start`    — `_start` entry stub + `__libc_start_main`
//! - `process`  — exit / abort / getpid / fork / execve
//! - `io`       — open / read / write / close / lseek / dup
//! - `mem`      — malloc / free / realloc (bump allocator for Stage 1)
//! - `string`   — memcpy / memset / memmove / strlen / strcmp / …
//! - `stdio`    — printf / puts / putchar / fprintf / sprintf / snprintf
//! - `errno`    — errno storage + __errno_location

#![no_std]
#![allow(non_camel_case_types, clippy::missing_safety_doc)]

pub mod syscall;
pub mod start;
pub mod process;
pub mod io;
pub mod mem;
pub mod string;
pub mod stdio;
pub mod errno;
pub mod time;

// Re-export every public symbol so dependents can do `use cyanos_libc::*`.
pub use io::*;
pub use mem::*;
pub use string::*;
pub use stdio::*;
pub use process::*;
pub use errno::*;
pub use time::*;

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    process::abort()
}
