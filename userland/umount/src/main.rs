#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{write, umount2, STDOUT_FILENO, STDERR_FILENO};

unsafe fn out(s: &[u8]) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

unsafe fn err(s: &[u8]) {
    write(STDERR_FILENO, s.as_ptr(), s.len());
}

unsafe fn arg(argv: *const *const u8, argc: i32, i: i32) -> *const u8 {
    if i >= argc { core::ptr::null() } else { *argv.offset(i as isize) }
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc < 2 {
        err(b"usage: umount <target>\n");
        return 2;
    }

    let target = arg(argv, argc, 1);
    let r = umount2(target, 0);
    if r < 0 {
        err(b"umount: failed (not mounted, or in use)\n");
        return 1;
    }
    out(b"unmounted\n");
    0
}
