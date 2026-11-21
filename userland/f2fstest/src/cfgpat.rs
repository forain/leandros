//! `f2fstest cfgpat DIR N cfg|plain`, `cfgcut DIR cfg|plain STAGE`, `cfgscan DIR`
//! (lane f2fsckpt3): the atomic small write, as applications do it.
//!
//!   cfg    cosmic-config / atomicwrites: mkdir a temp dir, create + write +
//!          fsync a file in it, rename it to DIR/c0, fsync the temp dir and DIR,
//!          rmdir the temp dir.
//!   plain  write a temp file in DIR, fsync it, rename it to DIR/c0, fsync DIR.
//!
//! `cfgpat` runs N of them and prints `CFGPAT mode= n= total_us=`; the caller
//! reads /proc/f2fsstat cp_total around it. `cfgcut` makes generation 1 durable
//! (sync), then writes generation 2 and stops forever at STAGE (1: after the
//! file's fsync, 2: after the rename, 3: after the last fsync returned), printing
//! `CUTAT <stage>` first; the harness kills the machine. `cfgscan` prints
//! `CFGSCAN gen=<g> ok=<0|1>` for c0 after the recovery boot.
use leandros_libc::*;
use crate::churn::{out, out_dec};

#[cfg(target_arch = "aarch64")]
const SYS_FSYNC: usize = 82;
#[cfg(target_arch = "x86_64")]
const SYS_FSYNC: usize = 74;
#[cfg(target_arch = "aarch64")]
const SYS_SYNC: usize = 81;
#[cfg(target_arch = "x86_64")]
const SYS_SYNC: usize = 162;
#[cfg(target_arch = "aarch64")]
const SYS_CLOCK_GETTIME: usize = 113;
#[cfg(target_arch = "x86_64")]
const SYS_CLOCK_GETTIME: usize = 228;
#[cfg(target_arch = "aarch64")]
const SYS_NANOSLEEP: usize = 101;
#[cfg(target_arch = "x86_64")]
const SYS_NANOSLEEP: usize = 35;

const LEN: usize = 96;

unsafe fn now_us() -> u64 {
    let mut ts = [0i64; 2];
    leandros_libc::syscall::syscall2(SYS_CLOCK_GETTIME, 1, ts.as_mut_ptr() as usize);
    ts[0] as u64 * 1_000_000 + ts[1] as u64 / 1000
}

unsafe fn fsync(fd: i32) -> bool { leandros_libc::syscall::syscall1(SYS_FSYNC, fd as usize) == 0 }

unsafe fn stop_here(stage: u64) -> ! {
    out(b"CUTAT "); out_dec(stage as _); out(b"\n");
    loop {
        let ts = [3600i64, 0];
        leandros_libc::syscall::syscall2(SYS_NANOSLEEP, ts.as_ptr() as usize, 0);
    }
}

fn content(g: u64) -> [u8; LEN] {
    let mut b = [0u8; LEN];
    for i in 8..LEN { b[i] = b'a' + ((g + i as u64) % 26) as u8; }
    b[..8].copy_from_slice(&g.to_le_bytes());
    b
}

/// Concatenate parts (and optional decimal numbers) into a NUL-terminated path.
fn cat(buf: &mut [u8; 300], parts: &[&[u8]], num: Option<u64>, suffix: &[u8]) -> *const u8 {
    let mut n = 0;
    for p in parts { buf[n..n + p.len()].copy_from_slice(p); n += p.len(); }
    if let Some(mut v) = num {
        let mut d = [0u8; 20];
        let mut i = d.len();
        loop { i -= 1; d[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
        buf[n..n + d.len() - i].copy_from_slice(&d[i..]);
        n += d.len() - i;
    }
    buf[n..n + suffix.len()].copy_from_slice(suffix); n += suffix.len();
    buf[n] = 0;
    buf.as_ptr()
}

unsafe fn write_file(fd: i32, g: u64) -> bool {
    let c = content(g);
    write(fd, c.as_ptr(), LEN) == LEN as isize
}

/// One atomic write of generation `g` to DIR/c0; `stop`: stage to stop at (0 = none).
unsafe fn atomic(dir: &[u8], cfg: bool, g: u64, stop: u64) -> bool {
    let (mut a, mut b, mut c) = ([0u8; 300], [0u8; 300], [0u8; 300]);
    let target = cat(&mut c, &[dir, b"/c0"], None, b"");
    let dfd = open(cat(&mut a, &[dir], None, b""), O_RDONLY, 0);
    if dfd < 0 { return false; }
    if cfg {
        let td = cat(&mut a, &[dir, b"/.aw"], Some(g), b"");
        if mkdir(td, 0o700) != 0 { return false; }
        let tfd = open(td, O_RDONLY, 0);
        let tfp = cat(&mut b, &[dir, b"/.aw"], Some(g), b"/tmpfile.tmp");
        let f = open(tfp, O_CREAT | O_RDWR | O_TRUNC, 0o644);
        if f < 0 || tfd < 0 || !write_file(f, g) || !fsync(f) { return false; }
        close(f);
        if stop == 1 { stop_here(1); }
        if rename(tfp, target) != 0 { return false; }
        if stop == 2 { stop_here(2); }
        if !fsync(tfd) || !fsync(dfd) { return false; }
        close(tfd);
        if stop == 3 { stop_here(3); }
        if rmdir(td) != 0 { return false; }
    } else {
        let tp = cat(&mut b, &[dir, b"/.tmp"], Some(g), b"");
        let f = open(tp, O_CREAT | O_RDWR | O_TRUNC, 0o644);
        if f < 0 || !write_file(f, g) || !fsync(f) { return false; }
        close(f);
        if stop == 1 { stop_here(1); }
        if rename(tp, target) != 0 { return false; }
        if stop == 2 { stop_here(2); }
        if !fsync(dfd) { return false; }
        if stop == 3 { stop_here(3); }
    }
    close(dfd);
    true
}

fn num(s: &[u8]) -> u64 { s.iter().fold(0u64, |a, &c| a * 10 + c.wrapping_sub(b'0') as u64) }

pub unsafe fn cfgpat(dir: &[u8], n: u64, cfg: bool) -> i32 {
    let mut a = [0u8; 300];
    mkdir(cat(&mut a, &[dir], None, b""), 0o755);
    let t0 = now_us();
    for i in 0..n {
        if !atomic(dir, cfg, i + 1, 0) { out(b"CFGPAT_FAIL\n"); return 1; }
    }
    out(b"CFGPAT mode="); out(if cfg { b"cfg" } else { b"plain" });
    out(b" n="); out_dec(n as _); out(b" total_us="); out_dec((now_us() - t0) as _); out(b"\n");
    0
}

pub unsafe fn cfgcut(dir: &[u8], cfg: bool, stage: u64) -> i32 {
    let mut a = [0u8; 300];
    mkdir(cat(&mut a, &[dir], None, b""), 0o755);
    if !atomic(dir, cfg, 1, 0) { out(b"CFGCUT_FAIL base\n"); return 1; }
    leandros_libc::syscall::syscall0(SYS_SYNC);
    out(b"CUTBASE 1\n");
    if !atomic(dir, cfg, 2, stage) { out(b"CFGCUT_FAIL v2\n"); return 1; }
    stop_here(stage + 10)
}

pub unsafe fn cfgscan(dir: &[u8]) -> i32 {
    let mut a = [0u8; 300];
    let f = open(cat(&mut a, &[dir, b"/c0"], None, b""), O_RDONLY, 0);
    let mut buf = [0u8; 256];
    let n = if f >= 0 { read(f, buf.as_mut_ptr(), 256) } else { -1 };
    if n == LEN as isize {
        let g = u64::from_le_bytes([buf[0], buf[1], buf[2], buf[3], buf[4], buf[5], buf[6], buf[7]]);
        let ok = content(g)[..] == buf[..LEN];
        out(b"CFGSCAN gen="); out_dec(g as _); out(b" ok="); out(if ok { b"1" } else { b"0" }); out(b"\n");
    } else {
        out(b"CFGSCAN gen=none ok=0\n");
    }
    0
}

pub unsafe fn dispatch(argc: i32, argv: *const *const u8, cmd: &[u8], dir: &[u8]) -> Option<i32> {
    let arg = |i: usize| -> &'static [u8] { let p = *argv.add(i); core::slice::from_raw_parts(p, strlen(p)) };
    match cmd {
        b"cfgpat" if argc >= 5 => Some(cfgpat(dir, num(arg(3)).max(1), arg(4) == b"cfg")),
        b"cfgcut" if argc >= 5 => Some(cfgcut(dir, arg(3) == b"cfg", num(arg(4)))),
        b"cfgscan" => Some(cfgscan(dir)),
        _ => None,
    }
}
