//! `f2fstest commitunk DIR` / `f2fstest commitscan DIR` (lane f2fscommit).
//!
//! Driven by `.claude/skills/run-leandros/f2fscommittest.py` on a boot with
//! `f2fs.cpunk=K`: the K-th checkpoint commit lands on the medium but is
//! reported as failed (the unknown outcome of a commit write or its flush).
//! commitunk: create `a` and `sync` (that checkpoint is the one that reports
//! failure), then create `b`, write and fsync it. Prints `CLAIM b` once the
//! fsync returned success: that is a durability promise.
//! commitscan (next boot, after the machine was killed): `b` must be there
//! and whole; prints `SCAN b ok|missing|bad`.

use leandros_libc::*;
use crate::churn::{out, path};

#[cfg(target_arch = "aarch64")]
const SYS_FSYNC: usize = 82;
#[cfg(target_arch = "x86_64")]
const SYS_FSYNC: usize = 74;
#[cfg(target_arch = "aarch64")]
const SYS_SYNC: usize = 81;
#[cfg(target_arch = "x86_64")]
const SYS_SYNC: usize = 162;

const LEN: usize = 8192;

fn pat(i: usize) -> u8 { (i as u8).wrapping_mul(31).wrapping_add(7) }

unsafe fn make(dir: &[u8], name: &[u8]) -> Option<i32> {
    let mut pb = [0u8; 256];
    let p = path(&mut pb, dir, name, None);
    let fd = open(p, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return None; }
    let mut buf = [0u8; LEN];
    for i in 0..LEN { buf[i] = pat(i); }
    if write(fd, buf.as_ptr(), LEN) != LEN as isize { return None; }
    Some(fd)
}

pub unsafe fn commitunk(dir: &[u8]) -> i32 {
    let fa = match make(dir, b"ca") { Some(f) => f, None => { out(b"FAIL commitunk create a\n"); return 1; } };
    close(fa);
    let r = leandros_libc::syscall::syscall0(SYS_SYNC);
    out(if r == 0 { b"SYNC ok\n" } else { b"SYNC err\n" });
    let fb = match make(dir, b"cb") { Some(f) => f, None => { out(b"FAIL commitunk create b\n"); return 1; } };
    if leandros_libc::syscall::syscall1(SYS_FSYNC, fb as usize) != 0 { out(b"FSYNC b err\n"); return 0; }
    out(b"CLAIM b\n");
    0
}

pub unsafe fn commitscan(dir: &[u8]) -> i32 {
    let mut pb = [0u8; 256];
    let p = path(&mut pb, dir, b"cb", None);
    let fd = open(p, O_RDONLY, 0);
    if fd < 0 { out(b"SCAN b missing\n"); return 0; }
    let mut buf = [0u8; LEN];
    let n = read(fd, buf.as_mut_ptr(), LEN);
    close(fd);
    let good = n == LEN as isize && (0..LEN).all(|i| buf[i] == pat(i));
    out(if good { b"SCAN b ok\n" } else { b"SCAN b bad\n" });
    0
}
