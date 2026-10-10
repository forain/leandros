//! fsbench — filesystem / mmap / responsiveness micro-benchmarks.
//!
//!     fsbench read <path> [bufsize_kib=1024]
//!     fsbench mmapdir <dir> <nproc> [maxfiles=400]
//!     fsbench walk <dir>
//!     fsbench tick <seconds> [period_ms=20]
//!     fsbench statlat <path> <seconds> [period_ms=50]
//!     fsbench dbwrite <file> <rate_per_s> [seconds=60] [file_kib=8192]
//!     fsbench statfs <n> [path=/]
//!
//! statlat: stat(path) every period, prints p50/p99/max latency (the UI
//! thread's view of a filesystem convoy). dbwrite: a database-like writer,
//! random 4 KiB pwrite + fsync at about `rate` per second, prints the fsync
//! latencies. statfs: n statfs calls, prints avg/max.
//!
//! Every output line starts with "FSBENCH "; times are CLOCK_MONOTONIC microseconds.

#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::syscall::{syscall2, syscall3, syscall4};
use leandros_libc::{
    clock_gettime, close, exit, fork, mmap, munmap, nanosleep, open, read, timespec, wait4, write,
    CLOCK_MONOTONIC, STDOUT_FILENO,
};

#[cfg(target_arch = "x86_64")]
mod nr {
    pub const GETDENTS64: usize = 217;
    pub const FSTAT: usize = 5;
    pub const NEWFSTATAT: usize = 262;
    pub const PWRITE64: usize = 18;
    pub const FSYNC: usize = 74;
    pub const STATFS: usize = 137;
    pub const FTRUNCATE: usize = 77;
}
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const GETDENTS64: usize = 61;
    pub const FSTAT: usize = 80;
    pub const NEWFSTATAT: usize = 79;
    pub const PWRITE64: usize = 68;
    pub const FSYNC: usize = 82;
    pub const STATFS: usize = 43;
    pub const FTRUNCATE: usize = 46;
}

const O_RDONLY: i32 = 0;
// O_DIRECTORY differs per arch in Linux's ABI.
#[cfg(target_arch = "aarch64")]
const O_DIRECTORY: i32 = 0o40000;
#[cfg(target_arch = "x86_64")]
const O_DIRECTORY: i32 = 0o200000;
const AT_FDCWD: isize = -100;
const DT_DIR: u8 = 4;
const DT_REG: u8 = 8;
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const MAX_DEPTH: usize = 6;
const PATH_MAX: usize = 512;
const PAGE: usize = 4096;

const MAX_BUF: usize = 4 << 20;
static mut BIG: [u8; MAX_BUF] = [0; MAX_BUF];

#[repr(C, align(8))]
struct Stat([u8; 144]);

impl Stat {
    fn size(&self) -> u64 {
        u64::from_ne_bytes(self.0[48..56].try_into().unwrap())
    }
    fn mode(&self) -> u32 {
        // st_mode: x86_64 offset 24, aarch64 offset 16 (both u32).
        #[cfg(target_arch = "x86_64")]
        let o = 24;
        #[cfg(target_arch = "aarch64")]
        let o = 16;
        u32::from_ne_bytes(self.0[o..o + 4].try_into().unwrap())
    }
}

struct Out {
    buf: [u8; 256],
    len: usize,
}

impl Out {
    fn new() -> Self {
        Out { buf: [0; 256], len: 0 }
    }
    fn push(&mut self, b: u8) {
        if self.len < self.buf.len() {
            self.buf[self.len] = b;
            self.len += 1;
        }
    }
    fn s(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.push(b);
        }
    }
    fn u(&mut self, mut n: u64) {
        let mut d = [0u8; 20];
        let mut i = 20usize;
        if n == 0 {
            self.push(b'0');
            return;
        }
        while n > 0 {
            i -= 1;
            d[i] = b'0' + (n % 10) as u8;
            n /= 10;
        }
        for k in i..20 {
            self.push(d[k]);
        }
    }
    fn kv(&mut self, k: &[u8], v: u64) {
        self.s(b" ");
        self.s(k);
        self.s(b"=");
        self.u(v);
    }
    unsafe fn flush(&mut self) {
        self.push(b'\n');
        write(STDOUT_FILENO, self.buf.as_ptr(), self.len);
        self.len = 0;
    }
}

unsafe fn err(what: &[u8], path: &[u8]) {
    let mut o = Out::new();
    o.s(b"FSBENCH error ");
    o.s(what);
    o.s(b" ");
    o.s(&path[..path.len().min(160)]);
    o.flush();
}

unsafe fn now_us() -> u64 {
    let mut ts = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(CLOCK_MONOTONIC, &mut ts);
    ts.tv_sec as u64 * 1_000_000 + ts.tv_nsec as u64 / 1000
}

unsafe fn cstr_bytes<'a>(p: *const u8) -> &'a [u8] {
    let mut n = 0;
    while *p.add(n) != 0 {
        n += 1;
    }
    core::slice::from_raw_parts(p, n)
}

fn parse_u(b: &[u8]) -> Option<u64> {
    if b.is_empty() {
        return None;
    }
    let mut v = 0u64;
    for &c in b {
        if !(b'0'..=b'9').contains(&c) {
            return None;
        }
        v = v.checked_mul(10)?.checked_add((c - b'0') as u64)?;
    }
    Some(v)
}

unsafe fn arg<'a>(argc: i32, argv: *const *const u8, i: i32) -> Option<&'a [u8]> {
    if i < argc {
        Some(cstr_bytes(*argv.add(i as usize)))
    } else {
        None
    }
}

fn usage() -> i32 {
    let m = b"FSBENCH usage: fsbench read <path> [kib] | mmapdir <dir> <nproc> [maxfiles] | walk <dir> | tick <sec> [period_ms] | statlat <path> <sec> [period_ms] | dbwrite <file> <rate> [sec] [file_kib] | statfs <n> [path]\n";
    unsafe { write(STDOUT_FILENO, m.as_ptr(), m.len()) };
    2
}

// ---------------------------------------------------------------- walker

#[derive(Copy, Clone, PartialEq)]
enum Mode {
    Mmap,
    Walk,
}

struct Walk {
    mode: Mode,
    maxfiles: u64,
    files: u64,
    bytes: u64,
}

/// Open+fstat+mmap+touch+munmap one file.
unsafe fn do_mmap(path: *const u8, w: &mut Walk) {
    let fd = open(path, O_RDONLY, 0);
    if fd < 0 {
        return;
    }
    let mut st = Stat([0; 144]);
    if syscall2(nr::FSTAT, fd as usize, st.0.as_mut_ptr() as usize) < 0 {
        close(fd);
        return;
    }
    let size = st.size() as usize;
    if size == 0 {
        close(fd);
        return;
    }
    let p = mmap(core::ptr::null_mut(), size, 1, 1, fd, 0);
    if p as isize == -1 {
        err(b"mmap", cstr_bytes(path));
        close(fd);
        return;
    }
    let mut sink = 0u8;
    sink ^= core::ptr::read_volatile(p);
    sink ^= core::ptr::read_volatile(p.add(size / 2));
    let last = (size - 1) & !(PAGE - 1);
    sink ^= core::ptr::read_volatile(p.add(last));
    core::hint::black_box(sink);
    munmap(p, size);
    close(fd);
    w.files += 1;
    w.bytes += size as u64;
}

unsafe fn do_read4k(path: *const u8, w: &mut Walk) {
    let fd = open(path, O_RDONLY, 0);
    if fd < 0 {
        return;
    }
    let mut b = [0u8; 4096];
    let n = read(fd, b.as_mut_ptr(), b.len());
    close(fd);
    if n >= 0 {
        w.files += 1;
        w.bytes += n as u64;
    }
}

#[repr(C, align(8))]
struct Dbuf([u8; 2048]);

unsafe fn walk_dir(path: &mut [u8; PATH_MAX], plen: usize, depth: usize, w: &mut Walk) {
    if depth > MAX_DEPTH || w.files >= w.maxfiles {
        return;
    }
    path[plen] = 0;
    let fd = open(path.as_ptr(), O_RDONLY | O_DIRECTORY, 0);
    if fd < 0 {
        err(b"opendir", &path[..plen]);
        return;
    }
    let mut db = Dbuf([0; 2048]);
    loop {
        if w.files >= w.maxfiles {
            break;
        }
        let n = syscall3(nr::GETDENTS64, fd as usize, db.0.as_mut_ptr() as usize, db.0.len());
        if n <= 0 {
            break;
        }
        let n = n as usize;
        let mut off = 0usize;
        while off + 19 < n {
            if w.files >= w.maxfiles {
                break;
            }
            let reclen = u16::from_ne_bytes([db.0[off + 16], db.0[off + 17]]) as usize;
            if reclen == 0 || off + reclen > n {
                break;
            }
            let mut dtype = db.0[off + 18];
            let name_start = off + 19;
            let mut nl = 0;
            while name_start + nl < off + reclen && db.0[name_start + nl] != 0 {
                nl += 1;
            }
            let name = &db.0[name_start..name_start + nl];
            off += reclen;
            if name == b"." || name == b".." || nl == 0 {
                continue;
            }
            if plen + 1 + nl + 1 > PATH_MAX {
                continue;
            }
            let mut cl = plen;
            if cl == 0 || path[cl - 1] != b'/' {
                path[cl] = b'/';
                cl += 1;
            }
            path[cl..cl + nl].copy_from_slice(name);
            cl += nl;
            path[cl] = 0;

            if dtype == 0 {
                let mut st = Stat([0; 144]);
                if syscall4(
                    nr::NEWFSTATAT,
                    AT_FDCWD as usize,
                    path.as_ptr() as usize,
                    st.0.as_mut_ptr() as usize,
                    0,
                ) >= 0
                {
                    let m = st.mode() & S_IFMT;
                    if m == S_IFDIR {
                        dtype = DT_DIR;
                    } else if m == S_IFREG {
                        dtype = DT_REG;
                    }
                }
            }
            if dtype == DT_DIR {
                walk_dir(path, cl, depth + 1, w);
            } else if dtype == DT_REG {
                match w.mode {
                    Mode::Mmap => do_mmap(path.as_ptr(), w),
                    Mode::Walk => do_read4k(path.as_ptr(), w),
                }
            }
            path[plen] = 0;
        }
    }
    close(fd);
}

unsafe fn run_walk(dir: &[u8], mode: Mode, maxfiles: u64) -> (u64, u64) {
    let mut path = [0u8; PATH_MAX];
    let mut l = dir.len().min(PATH_MAX - 2);
    path[..l].copy_from_slice(&dir[..l]);
    while l > 1 && path[l - 1] == b'/' {
        l -= 1;
    }
    let mut w = Walk { mode, maxfiles, files: 0, bytes: 0 };
    walk_dir(&mut path, l, 0, &mut w);
    (w.files, w.bytes)
}

// ------------------------------------------------------------- commands

unsafe fn cmd_read(argc: i32, argv: *const *const u8) -> i32 {
    let Some(p) = arg(argc, argv, 2) else { return usage() };
    let kib = match arg(argc, argv, 3) {
        Some(b) => parse_u(b).unwrap_or(1024),
        None => 1024,
    };
    let bs = ((kib as usize) * 1024).clamp(1, MAX_BUF);
    let fd = open(*argv.add(2), O_RDONLY, 0);
    if fd < 0 {
        err(b"open", p);
        return 1;
    }
    let buf = core::ptr::addr_of_mut!(BIG) as *mut u8;
    let t0 = now_us();
    let mut total = 0u64;
    loop {
        let n = read(fd, buf, bs);
        if n < 0 {
            err(b"read", p);
            break;
        }
        if n == 0 {
            break;
        }
        total += n as u64;
    }
    let t = (now_us() - t0).max(1);
    close(fd);
    let mut o = Out::new();
    o.s(b"FSBENCH read path=");
    o.s(&p[..p.len().min(120)]);
    o.kv(b"bytes", total);
    o.kv(b"us", t);
    o.kv(b"kib_per_s", total * 1_000_000 / 1024 / t);
    o.flush();
    0
}

unsafe fn cmd_mmapdir(argc: i32, argv: *const *const u8) -> i32 {
    let (Some(dir), Some(np)) = (arg(argc, argv, 2), arg(argc, argv, 3)) else { return usage() };
    let Some(nproc) = parse_u(np) else { return usage() };
    let maxfiles = arg(argc, argv, 4).and_then(parse_u).unwrap_or(400);
    let t0 = now_us();
    let mut started = 0u64;
    for i in 0..nproc {
        let pid = fork();
        if pid < 0 {
            err(b"fork", dir);
            break;
        }
        if pid == 0 {
            let c0 = now_us();
            let (files, bytes) = run_walk(dir, Mode::Mmap, maxfiles);
            let mut o = Out::new();
            o.s(b"FSBENCH");
            o.kv(b"child", i);
            o.kv(b"files", files);
            o.kv(b"bytes_mapped", bytes);
            o.kv(b"us", now_us() - c0);
            o.flush();
            exit(0);
        }
        started += 1;
    }
    let mut status = 0i32;
    let mut bad = 0u64;
    for _ in 0..started {
        if wait4(-1, &mut status, 0, core::ptr::null_mut()) < 0 {
            break;
        }
        if status != 0 {
            bad += 1;
        }
    }
    let mut o = Out::new();
    o.s(b"FSBENCH mmapdir");
    o.kv(b"nproc", started);
    o.kv(b"failed", bad);
    o.kv(b"wall_us", now_us() - t0);
    o.flush();
    if bad != 0 { 1 } else { 0 }
}

unsafe fn cmd_walk(argc: i32, argv: *const *const u8) -> i32 {
    let Some(dir) = arg(argc, argv, 2) else { return usage() };
    let t0 = now_us();
    let (files, bytes) = run_walk(dir, Mode::Walk, 2000);
    let mut o = Out::new();
    o.s(b"FSBENCH walk");
    o.kv(b"files", files);
    o.kv(b"bytes", bytes);
    o.kv(b"us", now_us() - t0);
    o.flush();
    0
}

unsafe fn cmd_tick(argc: i32, argv: *const *const u8) -> i32 {
    let Some(secs) = arg(argc, argv, 2).and_then(parse_u) else { return usage() };
    let period_ms = arg(argc, argv, 3).and_then(parse_u).unwrap_or(20).max(1);
    let period_us = period_ms * 1000;
    let end = now_us() + secs * 1_000_000;
    let (mut iters, mut max_late, mut l50, mut l200) = (0u64, 0u64, 0u64, 0u64);
    loop {
        let t0 = now_us();
        if t0 >= end {
            break;
        }
        let req = timespec {
            tv_sec: (period_us / 1_000_000) as i64,
            tv_nsec: ((period_us % 1_000_000) * 1000) as i64,
        };
        nanosleep(&req, core::ptr::null_mut());
        let el = now_us() - t0;
        let late = el.saturating_sub(period_us);
        iters += 1;
        if late > max_late {
            max_late = late;
        }
        if late > 50_000 {
            l50 += 1;
        }
        if late > 200_000 {
            l200 += 1;
        }
    }
    let mut o = Out::new();
    o.s(b"FSBENCH tick");
    o.kv(b"iters", iters);
    o.kv(b"max_late_us", max_late);
    o.kv(b"late50", l50);
    o.kv(b"late200", l200);
    o.flush();
    0
}


// ------------------------------------------------------- latency probes

const MAX_SAMPLES: usize = 65536;
static mut SAMPLES: [u32; MAX_SAMPLES] = [0; MAX_SAMPLES];

/// Sort the first `n` samples and print `p50 p99 max` (+ count over 100 ms).
unsafe fn report(label: &[u8], n: usize, extra: &[(&[u8], u64)]) {
    let s = &mut *core::ptr::addr_of_mut!(SAMPLES);
    let v = &mut s[..n];
    // Insertion sort is too slow for 64k; a simple heap sort.
    heap_sort(v);
    let pick = |q: usize| -> u64 { if n == 0 { 0 } else { v[((n - 1) * q) / 100] as u64 } };
    let over = v.iter().filter(|&&x| x > 100_000).count() as u64;
    let mut o = Out::new();
    o.s(b"FSBENCH ");
    o.s(label);
    o.kv(b"n", n as u64);
    o.kv(b"p50_us", pick(50));
    o.kv(b"p99_us", pick(99));
    o.kv(b"max_us", if n == 0 { 0 } else { v[n - 1] as u64 });
    o.kv(b"over100ms", over);
    for (k, x) in extra { o.kv(k, *x); }
    o.flush();
}

fn heap_sort(v: &mut [u32]) {
    let n = v.len();
    if n < 2 { return; }
    fn sift(v: &mut [u32], mut root: usize, end: usize) {
        loop {
            let mut c = 2 * root + 1;
            if c >= end { break; }
            if c + 1 < end && v[c + 1] > v[c] { c += 1; }
            if v[root] >= v[c] { break; }
            v.swap(root, c);
            root = c;
        }
    }
    for i in (0..n / 2).rev() { sift(v, i, n); }
    for end in (1..n).rev() { v.swap(0, end); sift(v, 0, end); }
}

unsafe fn sleep_us(us: u64) {
    if us == 0 { return; }
    let req = timespec { tv_sec: (us / 1_000_000) as i64, tv_nsec: ((us % 1_000_000) * 1000) as i64 };
    nanosleep(&req, core::ptr::null_mut());
}

unsafe fn cmd_statlat(argc: i32, argv: *const *const u8) -> i32 {
    let (Some(p), Some(secs)) = (arg(argc, argv, 2), arg(argc, argv, 3).and_then(parse_u)) else { return usage() };
    let period_us = arg(argc, argv, 4).and_then(parse_u).unwrap_or(50).max(1) * 1000;
    let path = *argv.add(2);
    let end = now_us() + secs * 1_000_000;
    let mut n = 0usize;
    let mut fails = 0u64;
    let mut next = now_us();
    while now_us() < end && n < MAX_SAMPLES {
        let mut st = Stat([0; 144]);
        let t0 = now_us();
        let r = syscall4(nr::NEWFSTATAT, AT_FDCWD as usize, path as usize, st.0.as_mut_ptr() as usize, 0);
        let dt = now_us() - t0;
        if r < 0 { fails += 1; }
        (*core::ptr::addr_of_mut!(SAMPLES))[n] = dt.min(u32::MAX as u64) as u32;
        n += 1;
        next += period_us;
        let now = now_us();
        if next > now { sleep_us(next - now); } else { next = now; }
    }
    let _ = p;
    report(b"statlat", n, &[(b"fails", fails)]);
    0
}

unsafe fn cmd_dbwrite(argc: i32, argv: *const *const u8) -> i32 {
    let (Some(p), Some(rate)) = (arg(argc, argv, 2), arg(argc, argv, 3).and_then(parse_u)) else { return usage() };
    let secs = arg(argc, argv, 4).and_then(parse_u).unwrap_or(60);
    let file_kib = arg(argc, argv, 5).and_then(parse_u).unwrap_or(8192).max(4);
    let period_us = 1_000_000 / rate.max(1);
    let fd = open(*argv.add(2), leandros_libc::io::O_RDWR | leandros_libc::io::O_CREAT, 0o644);
    if fd < 0 { err(b"open", p); return 1; }
    let blocks = file_kib / 4;
    let size = blocks * 4096;
    let _ = syscall2(nr::FTRUNCATE, fd as usize, size as usize);
    let mut seed = now_us() | 1;
    let mut page = [0u8; 4096];
    let end = now_us() + secs * 1_000_000;
    let mut n = 0usize;
    let (mut wmax, mut errs) = (0u64, 0u64);
    let mut next = now_us();
    while now_us() < end && n < MAX_SAMPLES {
        seed ^= seed << 13; seed ^= seed >> 7; seed ^= seed << 17;
        let blk = seed % blocks;
        for (i, b) in page.iter_mut().enumerate() { *b = (seed as u8).wrapping_add(i as u8); }
        let t0 = now_us();
        let w = syscall4(nr::PWRITE64, fd as usize, page.as_ptr() as usize, 4096, (blk * 4096) as usize);
        let t1 = now_us();
        let f = syscall2(nr::FSYNC, fd as usize, 0);
        let t2 = now_us();
        if w != 4096 || f != 0 { errs += 1; }
        if t1 - t0 > wmax { wmax = t1 - t0; }
        (*core::ptr::addr_of_mut!(SAMPLES))[n] = (t2 - t0).min(u32::MAX as u64) as u32;
        n += 1;
        next += period_us;
        let now = now_us();
        if next > now { sleep_us(next - now); } else { next = now; }
    }
    close(fd);
    report(b"dbwrite", n, &[(b"pwrite_max_us", wmax), (b"errors", errs)]);
    0
}

unsafe fn cmd_statfs(argc: i32, argv: *const *const u8) -> i32 {
    let Some(count) = arg(argc, argv, 2).and_then(parse_u) else { return usage() };
    let path: *const u8 = if argc > 3 { *argv.add(3) } else { b"/\0".as_ptr() };
    let mut buf = [0u64; 16];
    let mut n = 0usize;
    let mut fails = 0u64;
    let mut total = 0u64;
    for _ in 0..count.min(MAX_SAMPLES as u64) {
        let t0 = now_us();
        if syscall2(nr::STATFS, path as usize, buf.as_mut_ptr() as usize) < 0 { fails += 1; }
        let dt = now_us() - t0;
        total += dt;
        (*core::ptr::addr_of_mut!(SAMPLES))[n] = dt.min(u32::MAX as u64) as u32;
        n += 1;
    }
    report(b"statfs", n, &[(b"avg_us", total / (n.max(1) as u64)), (b"fails", fails)]);
    0
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argv.is_null() {
        return usage();
    }
    match arg(argc, argv, 1) {
        Some(b"read") => cmd_read(argc, argv),
        Some(b"mmapdir") => cmd_mmapdir(argc, argv),
        Some(b"walk") => cmd_walk(argc, argv),
        Some(b"tick") => cmd_tick(argc, argv),
        Some(b"statlat") => cmd_statlat(argc, argv),
        Some(b"dbwrite") => cmd_dbwrite(argc, argv),
        Some(b"statfs") => cmd_statfs(argc, argv),
        _ => usage(),
    }
}
