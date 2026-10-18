//! Shared by `ps` and `pstest`: raw syscalls and a fixed-buffer formatter.
//! No allocator is used (the libc allocator never frees).

#![allow(dead_code)]

use core::fmt::{self, Write};
use leandros_libc::syscall::{nr, syscall3, syscall4};

pub const AT_FDCWD: isize = -100;
pub const O_RDONLY: i32 = 0;
pub const O_WRONLY: i32 = 1;
pub const O_RDWR: i32 = 2;
pub const O_DIRECTORY: i32 = 0o200000;

#[cfg(target_arch = "aarch64")] pub const SYS_PRCTL: usize = 167;
#[cfg(target_arch = "x86_64")]  pub const SYS_PRCTL: usize = 157;

fn res(r: isize) -> Result<usize, i32> { if r < 0 { Err((-r) as i32) } else { Ok(r as usize) } }

/// `path` need not be NUL-terminated.
pub fn sys_open(path: &[u8], flags: i32) -> Result<i32, i32> {
    let mut b = [0u8; 256];
    if path.len() >= b.len() { return Err(36); }
    b[..path.len()].copy_from_slice(path);
    res(unsafe { syscall4(nr::OPENAT, AT_FDCWD as usize, b.as_ptr() as usize, flags as usize, 0) }).map(|v| v as i32)
}
pub fn sys_close(fd: i32) { unsafe { syscall3(nr::CLOSE, fd as usize, 0, 0); } }
pub fn sys_read(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    res(unsafe { syscall3(nr::READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) })
}
pub fn sys_write(fd: i32, buf: &[u8]) -> Result<usize, i32> {
    res(unsafe { syscall3(nr::WRITE, fd as usize, buf.as_ptr() as usize, buf.len()) })
}
pub fn sys_getdents(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    res(unsafe { syscall3(nr::GETDENTS64, fd as usize, buf.as_mut_ptr() as usize, buf.len()) })
}

/// Read a whole (small) file into `buf`; returns the length, or the errno.
pub fn read_file(path: &[u8], buf: &mut [u8]) -> Result<usize, i32> {
    let fd = sys_open(path, O_RDONLY)?;
    let mut n = 0;
    while n < buf.len() {
        match sys_read(fd, &mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => { sys_close(fd); return Err(e); }
        }
    }
    sys_close(fd);
    Ok(n)
}

pub unsafe fn arg<'a>(argv: *const *const u8, i: usize) -> &'a [u8] {
    let p = *argv.add(i);
    let mut n = 0;
    while *p.add(n) != 0 { n += 1; }
    core::slice::from_raw_parts(p, n)
}

pub fn parse_u64(s: &[u8]) -> Option<u64> {
    if s.is_empty() { return None; }
    let mut v: u64 = 0;
    for &c in s {
        if !c.is_ascii_digit() { return None; }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(v)
}

// ── output ─────────────────────────────────────────────────────────────────

/// A fixed buffer that implements `fmt::Write` (silently truncating).
pub struct Buf<const N: usize> { pub b: [u8; N], pub n: usize }
impl<const N: usize> Buf<N> {
    pub const fn new() -> Self { Buf { b: [0; N], n: 0 } }
    pub fn clear(&mut self) { self.n = 0; }
    pub fn bytes(&self) -> &[u8] { &self.b[..self.n] }
    pub fn push(&mut self, s: &[u8]) {
        let k = s.len().min(N - self.n);
        self.b[self.n..self.n + k].copy_from_slice(&s[..k]);
        self.n += k;
    }
}
impl<const N: usize> Write for Buf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result { self.push(s.as_bytes()); Ok(()) }
}

pub fn write_all(fd: i32, mut b: &[u8]) {
    while !b.is_empty() {
        match sys_write(fd, b) { Ok(n) if n > 0 => b = &b[n..], _ => return }
    }
}

pub struct Out;
impl Write for Out {
    fn write_str(&mut self, s: &str) -> fmt::Result { write_all(1, s.as_bytes()); Ok(()) }
}
pub struct Err2;
impl Write for Err2{
    fn write_str(&mut self, s: &str) -> fmt::Result { write_all(2, s.as_bytes()); Ok(()) }
}

#[macro_export]
macro_rules! say { ($($t:tt)*) => {{ use core::fmt::Write; let _ = write!($crate::common::Out, $($t)*); }} }
#[macro_export]
macro_rules! sayln { ($($t:tt)*) => {{ use core::fmt::Write; let _ = writeln!($crate::common::Out, $($t)*); }} }
#[macro_export]
macro_rules! eprintln { ($($t:tt)*) => {{ use core::fmt::Write; let _ = writeln!($crate::common::Err2, $($t)*); }} }
