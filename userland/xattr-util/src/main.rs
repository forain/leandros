//! xattr — command-line front-end for the setxattr(2)/getxattr(2)/
//! listxattr(2)/removexattr(2) family.
//!
//! ```text
//! xattr FILE                 list attribute names, one per line
//! xattr -p NAME FILE         print an attribute's raw value + newline
//! xattr -w NAME VALUE FILE   set an attribute (VALUE taken as-is, no escaping)
//! xattr -d NAME FILE         remove an attribute
//! ```
//!
//! Only the plain path-based syscalls are needed here (no fd/symlink
//! variants), so this tool never touches an fd — every operation goes
//! straight from a path argument to the matching raw syscall.

#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::*;
use leandros_libc::syscall::{syscall2, syscall3, syscall4, syscall5};

// setxattr/getxattr/listxattr/removexattr are not (yet) wrapped by
// leandros-libc, so — following the raw_chroot/raw_symlink pattern used by
// userland/vfstest — this tool makes the raw syscalls directly. Numbers
// match the kernel's `nr::SETXATTR`/`GETXATTR`/`LISTXATTR`/`REMOVEXATTR`
// entries in kernel/src/syscall.rs (AArch64 5/8/11/14, x86-64 188/191/
// 194/197 — the l*/f* variants sit at the numbers in between but this tool
// has no use for them).
#[cfg(target_arch = "aarch64")] const SYS_SETXATTR:    usize = 5;
#[cfg(target_arch = "aarch64")] const SYS_GETXATTR:    usize = 8;
#[cfg(target_arch = "aarch64")] const SYS_LISTXATTR:   usize = 11;
#[cfg(target_arch = "aarch64")] const SYS_REMOVEXATTR: usize = 14;

#[cfg(target_arch = "x86_64")] const SYS_SETXATTR:    usize = 188;
#[cfg(target_arch = "x86_64")] const SYS_GETXATTR:    usize = 191;
#[cfg(target_arch = "x86_64")] const SYS_LISTXATTR:   usize = 194;
#[cfg(target_arch = "x86_64")] const SYS_REMOVEXATTR: usize = 197;

fn xret(r: isize) -> isize {
    if r < 0 { set_errno(-r as i32); -1 } else { r }
}

unsafe fn raw_setxattr(path: *const u8, name: *const u8, value: *const u8, size: usize) -> isize {
    xret(syscall5(SYS_SETXATTR, path as usize, name as usize, value as usize, size, 0))
}
unsafe fn raw_getxattr(path: *const u8, name: *const u8, buf: *mut u8, size: usize) -> isize {
    xret(syscall4(SYS_GETXATTR, path as usize, name as usize, buf as usize, size))
}
unsafe fn raw_listxattr(path: *const u8, buf: *mut u8, size: usize) -> isize {
    xret(syscall3(SYS_LISTXATTR, path as usize, buf as usize, size))
}
unsafe fn raw_removexattr(path: *const u8, name: *const u8) -> isize {
    xret(syscall2(SYS_REMOVEXATTR, path as usize, name as usize))
}

unsafe fn out(s: &[u8]) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

unsafe fn errw(s: &[u8]) {
    write(STDERR_FILENO, s.as_ptr(), s.len());
}

unsafe fn out_dec(fd: i32, n: i32) {
    if n == 0 { write(fd, b"0".as_ptr(), 1); return; }
    let neg = n < 0;
    let mut u: u32 = if neg { n.unsigned_abs() } else { n as u32 };
    let mut buf = [0u8; 11];
    let mut i = buf.len();
    while u > 0 { i -= 1; buf[i] = b'0' + (u % 10) as u8; u /= 10; }
    if neg { i -= 1; buf[i] = b'-'; }
    write(fd, buf[i..].as_ptr(), buf.len() - i);
}

/// Compare a NUL-terminated argv string against a literal that includes its
/// own trailing `\0` (e.g. `b"cols\0"`) — same helper as userland/tput and
/// userland/mount.
unsafe fn ceq(a: *const u8, lit: &[u8]) -> bool {
    let mut i = 0usize;
    loop {
        let ac = *a.add(i);
        let bc = lit[i];
        if ac != bc { return false; }
        if ac == 0 { return true; }
        i += 1;
    }
}

unsafe fn arg(argv: *const *const u8, argc: i32, i: i32) -> *const u8 {
    if i >= argc { core::ptr::null() } else { *argv.offset(i as isize) }
}

/// Print `xattr: <op>: errno <n>\n` to stderr and return the tool's uniform
/// failure exit code.
unsafe fn fail(op: &[u8]) -> i32 {
    errw(b"xattr: ");
    errw(op);
    errw(b": errno ");
    out_dec(STDERR_FILENO, get_errno());
    errw(b"\n");
    1
}

/// `xattr FILE`: list every attribute name, one per line. Queries the
/// required buffer size first (size == 0), then allocates and reads the
/// real NUL-joined name list.
unsafe fn cmd_list(path: *const u8) -> i32 {
    let need = raw_listxattr(path, core::ptr::null_mut(), 0);
    if need < 0 { return fail(b"listxattr"); }
    if need == 0 { return 0; }

    let buf = malloc(need as usize);
    if buf.is_null() { errw(b"xattr: out of memory\n"); return 1; }
    let got = raw_listxattr(path, buf, need as usize);
    if got < 0 { return fail(b"listxattr"); }

    let data = core::slice::from_raw_parts(buf, got as usize);
    let mut start = 0usize;
    for i in 0..data.len() {
        if data[i] == 0 {
            out(&data[start..i]);
            out(b"\n");
            start = i + 1;
        }
    }
    0
}

/// `xattr -p NAME FILE`: print the raw value bytes followed by a newline.
unsafe fn cmd_print(name: *const u8, path: *const u8) -> i32 {
    let need = raw_getxattr(path, name, core::ptr::null_mut(), 0);
    if need < 0 { return fail(b"getxattr"); }
    if need == 0 { out(b"\n"); return 0; }

    let buf = malloc(need as usize);
    if buf.is_null() { errw(b"xattr: out of memory\n"); return 1; }
    let got = raw_getxattr(path, name, buf, need as usize);
    if got < 0 { return fail(b"getxattr"); }

    out(core::slice::from_raw_parts(buf, got as usize));
    out(b"\n");
    0
}

/// `xattr -w NAME VALUE FILE`: set an attribute. VALUE is taken verbatim
/// from argv (its NUL-terminated string length), no escaping.
unsafe fn cmd_write(name: *const u8, value: *const u8, path: *const u8) -> i32 {
    let len = strlen(value);
    if raw_setxattr(path, name, value, len) != 0 { return fail(b"setxattr"); }
    0
}

/// `xattr -d NAME FILE`: remove an attribute.
unsafe fn cmd_delete(name: *const u8, path: *const u8) -> i32 {
    if raw_removexattr(path, name) != 0 { return fail(b"removexattr"); }
    0
}

const USAGE: &[u8] =
    b"usage: xattr FILE | xattr -p NAME FILE | xattr -w NAME VALUE FILE | xattr -d NAME FILE\n";

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc < 2 {
        errw(USAGE);
        return 1;
    }

    let a1 = arg(argv, argc, 1);

    if ceq(a1, b"-p\0") {
        if argc != 4 { errw(USAGE); return 1; }
        return cmd_print(arg(argv, argc, 2), arg(argv, argc, 3));
    }
    if ceq(a1, b"-w\0") {
        if argc != 5 { errw(USAGE); return 1; }
        return cmd_write(arg(argv, argc, 2), arg(argv, argc, 3), arg(argv, argc, 4));
    }
    if ceq(a1, b"-d\0") {
        if argc != 4 { errw(USAGE); return 1; }
        return cmd_delete(arg(argv, argc, 2), arg(argv, argc, 3));
    }

    if argc != 2 { errw(USAGE); return 1; }
    cmd_list(a1)
}
