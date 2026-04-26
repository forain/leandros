//! File I/O: open, read, write, close, lseek, dup, pipe, getcwd, chdir.

use crate::errno::set_errno;
use crate::syscall::{nr, syscall1, syscall2, syscall3, syscall4};

pub type c_int   = i32;
pub type ssize_t = isize;
pub type size_t  = usize;
pub type off_t   = i64;
pub type mode_t  = u32;

#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct linux_dirent64 {
    pub d_ino: u64,
    pub d_off: i64,
    pub d_reclen: u16,
    pub d_type: u8,
    pub d_name: [u8; 0], // Flexible array member
}

pub const AT_FDCWD: i32 = -100i32;
pub const STDIN_FILENO:  i32 = 0;
pub const STDOUT_FILENO: i32 = 1;
pub const STDERR_FILENO: i32 = 2;

// open(2) flags.
pub const O_RDONLY: i32 = 0;
pub const O_WRONLY: i32 = 1;
pub const O_RDWR:   i32 = 2;
pub const O_CREAT:  i32 = 0o100;
pub const O_TRUNC:  i32 = 0o1000;
pub const O_APPEND: i32 = 0o2000;
pub const O_CLOEXEC:i32 = 0o2000000;

// lseek(2) whence.
pub const SEEK_SET: i32 = 0;
pub const SEEK_CUR: i32 = 1;
pub const SEEK_END: i32 = 2;

fn ret_or_errno(r: isize) -> isize {
    if r < 0 { set_errno(-r as i32); -1 } else { r }
}

/// Open or create a file.
#[no_mangle]
pub unsafe extern "C" fn open(path: *const u8, flags: c_int, mode: mode_t) -> c_int {
    ret_or_errno(syscall4(
        nr::OPENAT, AT_FDCWD as usize,
        path as usize, flags as usize, mode as usize,
    )) as c_int
}

/// Like `open` but with a directory file descriptor.
#[no_mangle]
pub unsafe extern "C" fn openat(
    dirfd: c_int, path: *const u8, flags: c_int, mode: mode_t,
) -> c_int {
    ret_or_errno(syscall4(
        nr::OPENAT, dirfd as usize,
        path as usize, flags as usize, mode as usize,
    )) as c_int
}

/// Read up to `count` bytes from `fd` into `buf`.
#[no_mangle]
pub unsafe extern "C" fn read(fd: c_int, buf: *mut u8, count: size_t) -> ssize_t {
    ret_or_errno(syscall3(nr::READ, fd as usize, buf as usize, count))
}

/// Write `count` bytes from `buf` to `fd`.
#[no_mangle]
pub unsafe extern "C" fn write(fd: c_int, buf: *const u8, count: size_t) -> ssize_t {
    ret_or_errno(syscall3(nr::WRITE, fd as usize, buf as usize, count))
}

/// Close a file descriptor.
#[no_mangle]
pub unsafe extern "C" fn close(fd: c_int) -> c_int {
    let r = syscall1(nr::CLOSE, fd as usize);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

/// Reposition read/write file offset.
#[no_mangle]
pub unsafe extern "C" fn lseek(fd: c_int, offset: off_t, whence: c_int) -> off_t {
    let r = ret_or_errno(syscall3(nr::LSEEK, fd as usize, offset as usize, whence as usize));
    r as off_t
}

/// Duplicate a file descriptor.
#[no_mangle]
pub unsafe extern "C" fn dup(oldfd: c_int) -> c_int {
    ret_or_errno(syscall1(nr::DUP, oldfd as usize)) as c_int
}

/// Duplicate `oldfd` to `newfd` with flags.
#[no_mangle]
pub unsafe extern "C" fn dup3(oldfd: c_int, newfd: c_int, flags: c_int) -> c_int {
    ret_or_errno(syscall3(nr::DUP3, oldfd as usize, newfd as usize, flags as usize)) as c_int
}

/// Create a pipe. Writes read-fd and write-fd into `pipefd[0]` and `pipefd[1]`.
#[no_mangle]
pub unsafe extern "C" fn pipe(pipefd: *mut c_int) -> c_int {
    let r = syscall2(nr::PIPE2, pipefd as usize, 0);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

/// pipe2 with flags.
#[no_mangle]
pub unsafe extern "C" fn pipe2(pipefd: *mut c_int, flags: c_int) -> c_int {
    let r = syscall2(nr::PIPE2, pipefd as usize, flags as usize);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

/// Get current working directory.
#[no_mangle]
pub unsafe extern "C" fn getcwd(buf: *mut u8, size: size_t) -> *mut u8 {
    let r = syscall2(nr::GETCWD, buf as usize, size);
    if r < 0 { set_errno(-r as i32); core::ptr::null_mut() } else { buf }
}

/// Change working directory.
#[no_mangle]
pub unsafe extern "C" fn chdir(path: *const u8) -> c_int {
    let r = syscall1(nr::CHDIR, path as usize);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

/// Create a directory.
#[no_mangle]
pub unsafe extern "C" fn mkdir(path: *const u8, mode: mode_t) -> c_int {
    let r = syscall3(nr::MKDIRAT, AT_FDCWD as usize, path as usize, mode as usize);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

/// Remove a file.
#[no_mangle]
pub unsafe extern "C" fn unlink(path: *const u8) -> c_int {
    let r = syscall3(nr::UNLINKAT, AT_FDCWD as usize, path as usize, 0);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

/// Read directory entries.
#[no_mangle]
pub unsafe extern "C" fn getdents64(fd: c_int, buf: *mut u8, count: size_t) -> ssize_t {
    ret_or_errno(syscall3(nr::GETDENTS64, fd as usize, buf as usize, count))
}

/// I/O control.
#[no_mangle]
pub unsafe extern "C" fn ioctl(fd: c_int, cmd: usize, arg: usize) -> c_int {
    ret_or_errno(syscall3(nr::IOCTL, fd as usize, cmd, arg)) as c_int
}
