//! `f2fstest fsyncbench DIR N`: fsync-heavy microbenchmark (lane f2fslx).
//! N rounds of: append 4 KiB to one file + fsync, create a new small file +
//! write + fsync. Prints `BENCH ops=<n> total_us=<t> us_per_fsync=<u>`.
use leandros_libc::*;
use crate::churn::{out, out_dec, path};

#[cfg(target_arch = "aarch64")]
const SYS_FSYNC: usize = 82;
#[cfg(target_arch = "x86_64")]
const SYS_FSYNC: usize = 74;
#[cfg(target_arch = "aarch64")]
const SYS_CLOCK_GETTIME: usize = 113;
#[cfg(target_arch = "x86_64")]
const SYS_CLOCK_GETTIME: usize = 228;

static mut BUF: [u8; 4096] = [0x5A; 4096];

unsafe fn now_us() -> u64 {
    let mut ts = [0i64; 2];
    leandros_libc::syscall::syscall2(SYS_CLOCK_GETTIME, 1, ts.as_mut_ptr() as usize);
    ts[0] as u64 * 1_000_000 + ts[1] as u64 / 1000
}

pub unsafe fn fsyncbench(dir: &[u8], n: u64) -> i32 {
    let mut pb = [0u8; 256];
    mkdir(path(&mut pb, dir, b".", None), 0o755);
    let fd = open(path(&mut pb, dir, b"bench", None), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { out(b"BENCH_FAIL open\n"); return 1; }
    let mut ops = 0u64;
    let t0 = now_us();
    for i in 0..n {
        if write(fd, BUF.as_ptr(), 4096) != 4096
            || leandros_libc::syscall::syscall1(SYS_FSYNC, fd as usize) != 0 {
            out(b"BENCH_FAIL append\n"); return 1;
        }
        ops += 1;
        let f = open(path(&mut pb, dir, b"n", Some(i)), O_CREAT | O_RDWR | O_TRUNC, 0o644);
        if f < 0 { out(b"BENCH_FAIL create\n"); return 1; }
        if write(f, BUF.as_ptr(), 4096) != 4096
            || leandros_libc::syscall::syscall1(SYS_FSYNC, f as usize) != 0 {
            out(b"BENCH_FAIL newfile\n"); return 1;
        }
        close(f);
        ops += 1;
    }
    let t = now_us() - t0;
    close(fd);
    out(b"BENCH ops="); out_dec(ops); out(b" total_us="); out_dec(t);
    out(b" us_per_fsync="); out_dec(t / ops.max(1)); out(b"\n");
    0
}
