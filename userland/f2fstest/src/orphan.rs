//! `f2fstest mmaporphan DIR` / `f2fstest mmapfault FILE [DELAY_S]` (lane f2fsfu).
//!
//! mmaporphan: an unlinked file held only by a mapping must give its blocks
//! back once the mapping goes (the f2fs-pcache thread reaps it).
//! mmapfault: map a file, sleep, then touch every page; the host test
//! `blkfaulttest.py` kills the virtio-blk device in between and expects SIGBUS.

use leandros_libc::*;
use crate::churn::{out, out_dec, path};

const PROT_READ: i32 = 1;
const MAP_SHARED: i32 = 1;
const LEN: usize = 1 << 20;

#[cfg(target_arch = "aarch64")]
const SYS_FSYNC: usize = 82;
#[cfg(target_arch = "x86_64")]
const SYS_FSYNC: usize = 74;

unsafe fn failed(m: *mut u8) -> bool { m.is_null() || m as isize == -1 }

pub unsafe fn mmaporphan(dir: &[u8]) -> i32 {
    let mut db = [0u8; 256];
    let mut pb = [0u8; 256];
    for (i, &c) in dir.iter().enumerate().take(200) { db[i] = c; }
    let d = db.as_ptr();
    let p = path(&mut pb, dir, b"mo", None);
    let base = match crate::statfs_bfree(d) { Some(v) => v, None => { out(b"FAIL mmaporphan statfs\n"); return 1; } };
    let fd = open(p, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { out(b"FAIL mmaporphan open\n"); return 1; }
    let buf = [0x5Au8; 4096];
    for _ in 0..256 {
        if write(fd, buf.as_ptr(), 4096) != 4096 { out(b"FAIL mmaporphan write\n"); return 1; }
    }
    leandros_libc::syscall::syscall1(SYS_FSYNC, fd as usize);
    let m = mmap(core::ptr::null_mut(), LEN, PROT_READ, MAP_SHARED, fd, 0);
    if failed(m) { out(b"FAIL mmaporphan mmap\n"); return 1; }
    close(fd);
    unlink(p);
    if core::ptr::read_volatile(m.add(LEN - 1)) != 0x5A { out(b"FAIL mmaporphan content\n"); return 1; }
    let held = crate::statfs_bfree(d).unwrap_or(0);
    if held + 200 > base { out(b"FAIL mmaporphan file took no space\n"); return 1; }
    munmap(m, LEN);
    let mut now = 0;
    for _ in 0..30 {
        now = crate::statfs_bfree(d).unwrap_or(0);
        if now + 8 >= base {
            out(b"PASS mmaporphan\n");
            return 0;
        }
        usleep(500_000);
    }
    out(b"FAIL mmaporphan base="); out_dec(base);
    out(b" held="); out_dec(held);
    out(b" now="); out_dec(now);
    out(b"\n");
    1
}

pub unsafe fn mmapfault(file: &[u8], delay: u64) -> i32 {
    let mut pb = [0u8; 256];
    for (i, &c) in file.iter().enumerate().take(250) { pb[i] = c; }
    let fd = open(pb.as_ptr(), O_RDONLY, 0);
    if fd < 0 { out(b"FAIL mmapfault open\n"); return 1; }
    let m = mmap(core::ptr::null_mut(), LEN, PROT_READ, MAP_SHARED, fd, 0);
    if failed(m) { out(b"FAIL mmapfault mmap\n"); return 1; }
    close(fd);
    out(b"MAPPED\n");
    for _ in 0..delay * 2 { usleep(500_000); }
    let mut sum = 0u64;
    for k in 0..256 { sum += core::ptr::read_volatile(m.add(k * 4096)) as u64; }
    out(b"READ_OK sum="); out_dec(sum); out(b"\n");
    0
}
