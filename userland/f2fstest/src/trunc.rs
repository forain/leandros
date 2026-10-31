//! `f2fstest trunc_cold DIR` (lane f2fshang): truncating a file whose node
//! blocks are not in the f2fs block cache must take one round of fetches, not
//! one doomed rerun of the whole walk per 16 nodes.
//!
//! Builds a file with one 4 KiB block every 4 MiB (one dnode each), syncs,
//! drops the clean cache (`/proc/f2fsdropcache`), then `ftruncate(1 MiB)` must
//! take under 1 s and at most 8 reruns (16 for 16000 dnodes: one more level of indirect nodes to discover) (`/proc/f2fsstat`), leave 16 sectors (one data block + its dnode)
//! (`st_blocks`), and after unlink the free-block count must be back.

use leandros_libc::*;
use crate::churn::{out, out_dec, path};

#[cfg(target_arch = "aarch64")]
const SYS_FSYNC: usize = 82;
#[cfg(target_arch = "x86_64")]
const SYS_FSYNC: usize = 74;
#[cfg(target_arch = "aarch64")]
const SYS_SYNC: usize = 81;
#[cfg(target_arch = "x86_64")]
const SYS_SYNC: usize = 162;
#[cfg(target_arch = "aarch64")]
const SYS_FSTAT: usize = 80;
#[cfg(target_arch = "x86_64")]
const SYS_FSTAT: usize = 5;
const ST_BLOCKS_OFF: usize = 64;

unsafe fn now_ms() -> u64 {
    let mut t = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(1, &mut t);
    t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000
}

/// `/proc/f2fsstat` field `noio_rounds`.
unsafe fn rounds() -> u64 {
    let fd = open(b"/proc/f2fsstat\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 { return u64::MAX; }
    let mut b = [0u8; 128];
    let n = read(fd, b.as_mut_ptr(), b.len());
    close(fd);
    let mut v = 0u64;
    let mut i = b"noio_rounds ".len();
    while n > 0 && i < n as usize && b[i].is_ascii_digit() { v = v * 10 + (b[i] - b'0') as u64; i += 1; }
    v
}

unsafe fn drop_cache() {
    let fd = open(b"/proc/f2fsdropcache\0".as_ptr(), O_RDONLY, 0);
    if fd >= 0 { let mut b = [0u8; 8]; read(fd, b.as_mut_ptr(), 8); close(fd); }
}

unsafe fn one(dir: &[u8], dnodes: u64, max_rounds: u64) -> bool {
    let mut db = [0u8; 256];
    let mut pb = [0u8; 256];
    for (i, &c) in dir.iter().enumerate().take(200) { db[i] = c; }
    let p = path(&mut pb, dir, b"trunc_cold", None);
    unlink(p);
    leandros_libc::syscall::syscall1(SYS_SYNC, 0);
    let base = match crate::statfs_bfree(db.as_ptr()) { Some(v) => v, None => { out(b"FAIL trunc_cold statfs\n"); return false; } };
    let fd = open(p, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { out(b"FAIL trunc_cold open\n"); return false; }
    let buf = [0xA5u8; 4096];
    for i in 0..dnodes {
        if lseek(fd, (i * (4 << 20)) as off_t, SEEK_SET) < 0 || write(fd, buf.as_ptr(), 4096) != 4096 {
            out(b"FAIL trunc_cold write\n"); return false;
        }
    }
    leandros_libc::syscall::syscall1(SYS_FSYNC, fd as usize);
    close(fd);
    leandros_libc::syscall::syscall1(SYS_SYNC, 0);
    drop_cache();
    let fd = open(p, O_RDWR, 0);
    if fd < 0 { out(b"FAIL trunc_cold reopen\n"); return false; }
    let r0 = rounds();
    let t0 = now_ms();
    let ok = leandros_libc::syscall::syscall2(crate::SYS_FTRUNCATE, fd as usize, 1 << 20) == 0;
    let ms = now_ms() - t0;
    let dr = rounds().wrapping_sub(r0);
    let mut st = [0u8; 256];
    leandros_libc::syscall::syscall2(SYS_FSTAT, fd as usize, st.as_mut_ptr() as usize);
    let blocks = core::ptr::read_unaligned(st.as_ptr().add(ST_BLOCKS_OFF) as *const u64);
    close(fd);
    unlink(p);
    leandros_libc::syscall::syscall1(SYS_SYNC, 0);
    let end = crate::statfs_bfree(db.as_ptr()).unwrap_or(0);
    out(b"trunc_cold dnodes="); out_dec(dnodes);
    out(b" ms="); out_dec(ms);
    out(b" rounds="); out_dec(dr);
    out(b" blocks="); out_dec(blocks);
    out(b" bfree_delta="); out_dec(base.wrapping_sub(end));
    out(b"\n");
    let good = ok && ms < 1000 && dr <= max_rounds && blocks == 16 && end == base;
    if !good { out(b"FAIL trunc_cold\n"); }
    good
}

pub unsafe fn trunc_cold(dir: &[u8]) -> i32 {
    if !one(dir, 4000, 8) { return 1; }
    if !one(dir, 16000, 16) { return 1; }
    out(b"PASS trunc_cold\n");
    0
}
