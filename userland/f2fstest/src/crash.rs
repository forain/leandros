//! `f2fstest crash DIR SEED` / `f2fstest crashscan DIR` — crash consistency
//! (lane f2fsync). Driven by `.claude/skills/run-leandros/f2fscrashtest.py`.
//!
//! `crash` creates DIR and runs until it is killed (or an fsync fails), mixing
//! appends, in-place block rewrites, new files, rename-replace and directory
//! churn, and prints one line for every operation whose durability it may
//! claim, *after* the fsync returned:
//!
//!   A k size        a<k> (append-only) is durable up to `size`
//!   W k g b n       blocks [b, b+n) of o<k> are being rewritten with gen g
//!   O k g b n       ... and that rewrite was fsynced
//!   N id size       new file n<id> was created, written and fsynced
//!   U id            n<id> was unlinked (not durable)
//!   T k g size      tmp t<k> written with gen g (then fsynced, renamed to r<k>)
//!   R k g           the rename of gen g to r<k> returned (not durable)
//!
//! `crashscan` prints what survived (`SA`, `SO`, `SN`, `SR` lines; `bad`
//! marks content that is not the pattern it should be) and the harness
//! compares it with those claims.
//!
//! Content: append files are `fill(key k)` by offset; every other file
//! carries a header word (`MARK | gen`) at the start of each 4 KiB block
//! (o<k>) or of the file (n, r) followed by `fill(key, gen)` words.

use leandros_libc::*;
use crate::churn::{fill, word, out, out_dec, errno, path, Rng};

const NA: usize = 6;
const NO: usize = 3;
const NR: usize = 3;
const OBLK: u64 = 64;
const MARK: u64 = 0xC0DE_0000_0000_0000;

static mut CBUF: [u8; 65536] = [0; 65536];

fn okey(k: usize, g: u64) -> u64 { 0x100_0000_0000 + ((k as u64) << 32) + g }
fn nkey(id: u64) -> u64 { 0x200_0000_0000 + id }
fn rkey(k: usize, g: u64) -> u64 { 0x300_0000_0000 + ((k as u64) << 32) + g }

#[cfg(target_arch = "aarch64")]
const SYS_FSYNC: usize = 82;
#[cfg(target_arch = "x86_64")]
const SYS_FSYNC: usize = 74;

unsafe fn fsync(fd: i32) -> bool {
    leandros_libc::syscall::syscall1(SYS_FSYNC, fd as usize) == 0
}

unsafe fn line(tag: &[u8], v: &[u64]) {
    let mut b = [0u8; 128];
    let mut n = 0;
    for &c in tag { b[n] = c; n += 1; }
    for &x in v {
        b[n] = b' '; n += 1;
        let mut d = [0u8; 20];
        let mut i = d.len();
        let mut y = x;
        loop { i -= 1; d[i] = b'0' + (y % 10) as u8; y /= 10; if y == 0 { break; } }
        for &c in &d[i..] { b[n] = c; n += 1; }
    }
    b[n] = b'\n'; n += 1;
    out(&b[..n]);
}

/// A whole marked file/block: `MARK|gen`, then the pattern of `key` from
/// byte 8 (`off` = the block's byte offset within its file).
fn marked(buf: &mut [u8], key: u64, gen: u64, off: u64) {
    fill(buf, key, off);
    buf[..8].copy_from_slice(&(MARK | gen).to_le_bytes());
}

unsafe fn write_all(fd: i32, b: &[u8]) -> bool {
    write(fd, b.as_ptr(), b.len()) == b.len() as isize
}

unsafe fn fail(what: &[u8]) -> i32 {
    out(what); out(b" errno="); out_dec(errno()); out(b"\n");
    4
}

pub unsafe fn crash(dir: &[u8], seed: u64) -> i32 {
    let mut pb = [0u8; 256];
    let mut pc = [0u8; 256];
    let buf = &mut *core::ptr::addr_of_mut!(CBUF);
    let dirz = path(&mut pb, dir, b"", None);
    // `path` appends "/": strip it for mkdir.
    let dl = dir.len();
    pb[dl] = 0;
    if mkdir(dirz, 0o755) != 0 { return fail(b"CRASH mkdir"); }
    let mut asz = [0u64; NA];
    for k in 0..NA {
        let fd = open(path(&mut pb, dir, b"a", Some(k as u64)), O_CREAT | O_RDWR | O_TRUNC, 0o644);
        if fd < 0 { return fail(b"CRASH create"); }
        if !fsync(fd) { out(b"FSYNC_FAIL\n"); return 3; }
        close(fd);
    }
    for k in 0..NO {
        let fd = open(path(&mut pb, dir, b"o", Some(k as u64)), O_CREAT | O_RDWR | O_TRUNC, 0o644);
        if fd < 0 { return fail(b"CRASH create"); }
        for b in 0..OBLK {
            marked(&mut buf[..4096], okey(k, 0), 0, b * 4096);
            if !write_all(fd, &buf[..4096]) { return fail(b"WRITE_FAIL"); }
        }
        if !fsync(fd) { out(b"FSYNC_FAIL\n"); return 3; }
        close(fd);
    }
    out(b"SETUP\n");
    let mut rng = Rng(0x5EED_C0DE_0000_0000 ^ seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
    let mut gen = 0u64;
    let mut rgen = [0u64; NR];
    let mut nid = 0u64;
    let mut live: [u64; 32] = [u64::MAX; 32];
    let mut op = 0u64;
    loop {
        op += 1;
        let r = rng.below(100);
        if r < 40 {
            // Append.
            let k = rng.below(NA as u64) as usize;
            let len = 8 * (1 + rng.below(2500)) as usize;
            fill(&mut buf[..len], k as u64, asz[k]);
            let fd = open(path(&mut pb, dir, b"a", Some(k as u64)), O_WRONLY | O_APPEND, 0);
            if fd < 0 { return fail(b"CRASH open"); }
            if !write_all(fd, &buf[..len]) { return fail(b"WRITE_FAIL"); }
            asz[k] += len as u64;
            if rng.below(2) == 0 {
                if !fsync(fd) { out(b"FSYNC_FAIL\n"); return 3; }
                line(b"A", &[k as u64, asz[k]]);
            }
            close(fd);
        } else if r < 65 {
            // Rewrite blocks of o<k> in place.
            let k = rng.below(NO as u64) as usize;
            let b = rng.below(OBLK);
            let n = 1 + rng.below(16.min(OBLK - b));
            gen += 1;
            line(b"W", &[k as u64, gen, b, n]);
            let fd = open(path(&mut pb, dir, b"o", Some(k as u64)), O_RDWR, 0);
            if fd < 0 { return fail(b"CRASH open"); }
            for q in 0..n as usize {
                marked(&mut buf[q * 4096..(q + 1) * 4096], okey(k, gen), gen, (b + q as u64) * 4096);
            }
            if lseek(fd, (b * 4096) as off_t, SEEK_SET) != (b * 4096) as off_t
                || !write_all(fd, &buf[..n as usize * 4096]) { return fail(b"WRITE_FAIL"); }
            if rng.below(10) < 6 {
                if !fsync(fd) { out(b"FSYNC_FAIL\n"); return 3; }
                line(b"O", &[k as u64, gen, b, n]);
            }
            close(fd);
        } else if r < 80 {
            // A new file, fsynced; sometimes an older one goes.
            let id = nid;
            nid += 1;
            let size = 8 * (1 + rng.below(6000)) as usize;
            marked(&mut buf[..size], nkey(id), id, 0);
            let fd = open(path(&mut pb, dir, b"n", Some(id)), O_CREAT | O_RDWR | O_TRUNC, 0o644);
            if fd < 0 { return fail(b"CRASH create"); }
            if !write_all(fd, &buf[..size]) { return fail(b"WRITE_FAIL"); }
            if !fsync(fd) { out(b"FSYNC_FAIL\n"); return 3; }
            close(fd);
            line(b"N", &[id, size as u64]);
            let slot = (id % 32) as usize;
            let old = live[slot];
            live[slot] = id;
            if old != u64::MAX || rng.below(10) < 3 {
                let victim = if old != u64::MAX { old } else { live[rng.below(32) as usize] };
                if victim != u64::MAX && victim != id {
                    if unlink(path(&mut pb, dir, b"n", Some(victim))) == 0 { line(b"U", &[victim]); }
                    for v in live.iter_mut() { if *v == victim { *v = u64::MAX; } }
                }
            }
        } else if r < 90 {
            // Replace r<k> through a fsynced temporary.
            let k = rng.below(NR as u64) as usize;
            rgen[k] += 1;
            let g = rgen[k];
            let size = 8 * (1 + rng.below(3000)) as usize;
            line(b"T", &[k as u64, g, size as u64]);
            marked(&mut buf[..size], rkey(k, g), g, 0);
            let t = path(&mut pb, dir, b"t", Some(k as u64));
            let fd = open(t, O_CREAT | O_RDWR | O_TRUNC, 0o644);
            if fd < 0 { return fail(b"CRASH create"); }
            if !write_all(fd, &buf[..size]) { return fail(b"WRITE_FAIL"); }
            if !fsync(fd) { out(b"FSYNC_FAIL\n"); return 3; }
            close(fd);
            if rename(t, path(&mut pc, dir, b"r", Some(k as u64))) != 0 { return fail(b"CRASH rename"); }
            line(b"R", &[k as u64, g]);
        } else {
            // Directory churn (link counts).
            let j = rng.below(6);
            let d = path(&mut pb, dir, b"d", Some(j));
            if mkdir(d, 0o755) != 0 {
                let mut sub = [0u8; 256];
                let dl = strlen(d);
                sub[..dl].copy_from_slice(&pb[..dl]);
                sub[dl..dl + 3].copy_from_slice(b"/e\0");
                if rng.below(2) == 0 { let _ = mkdir(sub.as_ptr(), 0o755); } else {
                    let _ = rmdir(sub.as_ptr());
                    let _ = rmdir(d);
                }
            }
        }
        if op % 64 == 0 { line(b"TICK", &[op]); }
    }
}

/// Bytes of `p` into `buf` (whole file, up to its size); None if absent.
unsafe fn slurp(p: *const u8, buf: &mut [u8]) -> Option<usize> {
    let fd = open(p, O_RDONLY, 0);
    if fd < 0 { return None; }
    let mut n = 0usize;
    loop {
        if n == buf.len() { break; }
        let r = read(fd, buf[n..].as_mut_ptr(), buf.len() - n);
        if r <= 0 { break; }
        n += r as usize;
    }
    close(fd);
    Some(n)
}

/// Does `b` (a whole marked file or block) hold gen `g` of `key` at `off`?
fn check_marked(b: &[u8], key: u64, g: u64, off: u64) -> bool {
    for (i, c) in b.chunks(8).enumerate().skip(1) {
        let w = word(key, off / 8 + i as u64).to_le_bytes();
        if c != &w[..c.len()] { return false; }
    }
    true
}

fn gen_of(b: &[u8]) -> Option<u64> {
    if b.len() < 8 { return None; }
    let w = u64::from_le_bytes(b[..8].try_into().unwrap());
    if w & 0xFFFF_0000_0000_0000 != MARK { return None; }
    Some(w & 0x0000_FFFF_FFFF_FFFF)
}

pub unsafe fn crashscan(dir: &[u8]) -> i32 {
    static mut BIG: [u8; 1 << 20] = [0; 1 << 20];
    let big = &mut *core::ptr::addr_of_mut!(BIG);
    let mut pb = [0u8; 256];
    for k in 0..NA {
        let fd = open(path(&mut pb, dir, b"a", Some(k as u64)), O_RDONLY, 0);
        if fd < 0 { line(b"SA", &[k as u64, u64::MAX]); continue; }
        let mut want = [0u8; 4096];
        let mut off = 0usize;
        let mut ok = true;
        loop {
            let r = read(fd, big.as_mut_ptr(), big.len());
            if r <= 0 { break; }
            let n = r as usize;
            let mut q = 0;
            while q < n {
                let m = (n - q).min(4096);
                fill(&mut want[..(m + 7) & !7], k as u64, (off + q) as u64);
                if big[q..q + m] != want[..m] { ok = false; break; }
                q += m;
            }
            if !ok { off += q; break; }
            off += n;
        }
        close(fd);
        if ok { line(b"SA", &[k as u64, off as u64]); } else { line(b"SA_BAD", &[k as u64, 0, off as u64]); }
    }
    for k in 0..NO {
        match slurp(path(&mut pb, dir, b"o", Some(k as u64)), big) {
            None => line(b"SO_MISSING", &[k as u64]),
            Some(n) => {
                if n != (OBLK * 4096) as usize { line(b"SO_SIZE", &[k as u64, n as u64]); continue; }
                for b in 0..OBLK as usize {
                    let blk = &big[b * 4096..(b + 1) * 4096];
                    match gen_of(blk) {
                        Some(g) if check_marked(blk, okey(k, g), g, b as u64 * 4096) => line(b"SO", &[k as u64, b as u64, g]),
                        _ => line(b"SO_BAD", &[k as u64, b as u64]),
                    }
                }
            }
        }
    }
    // n<id>: every name in DIR.
    let fd = open(path(&mut pb, dir, b"", None), O_RDONLY | 0o200000, 0);
    if fd < 0 { out(b"SCAN no dir\n"); return 1; }
    let mut ents = [0u8; 8192];
    let mut names: alloc_free::Names = alloc_free::Names::new();
    loop {
        let n = getdents64(fd, ents.as_mut_ptr(), ents.len());
        if n <= 0 { break; }
        let mut o = 0usize;
        while o < n as usize {
            let reclen = u16::from_le_bytes([ents[o + 16], ents[o + 17]]) as usize;
            let name = &ents[o + 19..o + reclen];
            let nl = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            names.push(&name[..nl]);
            o += reclen;
        }
    }
    close(fd);
    for i in 0..names.len() {
        let nm = names.get(i);
        if nm.len() < 2 { continue; }
        let num = nm[1..].iter().try_fold(0u64, |a, &c| if c.is_ascii_digit() { Some(a * 10 + (c - b'0') as u64) } else { None });
        let Some(id) = num else { continue };
        if nm[0] == b'n' || nm[0] == b'r' {
            let n = slurp(path(&mut pb, dir, &nm[..1], Some(id)), big).unwrap_or(0);
            let g = gen_of(&big[..n]);
            let ok = match (nm[0], g) {
                (b'n', Some(g)) => g == id && check_marked(&big[..n], nkey(id), g, 0),
                (b'r', Some(g)) => check_marked(&big[..n], rkey(id as usize, g), g, 0),
                _ => false,
            };
            let tag: &[u8] = if nm[0] == b'n' { if ok { b"SN" } else { b"SN_BAD" } } else if ok { b"SR" } else { b"SR_BAD" };
            line(tag, &[id, n as u64, g.unwrap_or(u64::MAX)]);
        }
    }
    out(b"SCAN_DONE\n");
    0
}

/// A fixed-capacity list of names (no allocator in this test).
mod alloc_free {
    pub struct Names { buf: [u8; 32768], off: [u16; 1024], n: usize, used: usize }
    impl Names {
        pub fn new() -> Self { Names { buf: [0; 32768], off: [0; 1024], n: 0, used: 0 } }
        pub fn push(&mut self, s: &[u8]) {
            if self.n + 1 >= self.off.len() || self.used + s.len() > self.buf.len() { return; }
            self.buf[self.used..self.used + s.len()].copy_from_slice(s);
            self.off[self.n] = self.used as u16;
            self.used += s.len();
            self.n += 1;
            self.off[self.n] = self.used as u16;
        }
        pub fn len(&self) -> usize { self.n }
        pub fn get(&self, i: usize) -> &[u8] { &self.buf[self.off[i] as usize..self.off[i + 1] as usize] }
    }
}
