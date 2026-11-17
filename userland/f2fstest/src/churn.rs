//! `f2fstest churn DIR [FACTOR]` / `f2fstest verify DIR` — space reuse under
//! fragmentation (lane f2fsgc).
//!
//! `churn` writes and deletes files on the (small, otherwise empty) volume
//! holding DIR until FACTOR (default 8) times the volume's capacity has been
//! written, while the live data stays below ~65 % of it. New files are written
//! two or three at a time in interleaved 8 KiB pieces, so every segment ends
//! up holding blocks of several files and a delete leaves it partly valid;
//! some files are later shrunk with ftruncate or rewritten in place. Before
//! the allocator reused the free blocks of partly valid segments this ran into
//! ENOSPC after about one capacity's worth of writes. Every write must succeed;
//! every file's content is checked (some during the run, all at the end).
//! The surviving set is recorded in DIR/MANIFEST (`<slot> <gen> <size>` per
//! line) and the volume is synced.
//!
//! `verify` re-reads MANIFEST and checks every file's size and content: run it
//! after a remount (and the host-side checker reads the same manifest).
//!
//! Content: little-endian 64-bit words, word `w` of the file with key
//! `slot << 32 | gen` is `mix(key * 0x9E3779B97F4A7C15 + w)` (splitmix64's
//! finalizer), so any block landing in the wrong file or offset is caught.

use leandros_libc::*;

const PIECE: usize = 8192;
const NSLOTS: usize = 24;
const MAXFILES_ROUND: usize = 3;

static mut BUF: [u8; PIECE] = [0; PIECE];
static mut RBUF: [u8; PIECE] = [0; PIECE];

#[derive(Clone, Copy)]
struct Slot { live: bool, gen: u32, size: u64 }

pub(crate) fn mix(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

pub(crate) fn word(key: u64, w: u64) -> u64 { mix(key.wrapping_mul(0x9E3779B97F4A7C15).wrapping_add(w)) }

pub(crate) fn fill(buf: &mut [u8], key: u64, off: u64) {
    // `off` and `buf.len()` are multiples of 8.
    for (i, c) in buf.chunks_exact_mut(8).enumerate() {
        c.copy_from_slice(&word(key, off / 8 + i as u64).to_le_bytes());
    }
}

pub(crate) struct Rng(pub(crate) u64);
impl Rng {
    pub(crate) fn next(&mut self) -> u64 { self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15); mix(self.0) }
    pub(crate) fn below(&mut self, n: u64) -> u64 { if n == 0 { 0 } else { self.next() % n } }
}

pub(crate) unsafe fn out(s: &[u8]) { write(STDOUT_FILENO, s.as_ptr(), s.len()); }
pub(crate) unsafe fn out_dec(mut v: u64) {
    let mut d = [0u8; 20];
    let mut i = d.len();
    loop { i -= 1; d[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    out(&d[i..]);
}
pub(crate) unsafe fn errno() -> u64 { *__errno_location() as u64 }

/// "DIR/f<slot>\0" into `p`.
pub(crate) fn path(p: &mut [u8; 256], dir: &[u8], name: &[u8], n: Option<u64>) -> *const u8 {
    let mut k = 0;
    for &c in dir.iter().chain(b"/".iter()).chain(name.iter()) { p[k] = c; k += 1; }
    if let Some(mut v) = n {
        let mut d = [0u8; 20];
        let mut i = d.len();
        loop { i -= 1; d[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
        for &c in &d[i..] { p[k] = c; k += 1; }
    }
    p[k] = 0;
    p.as_ptr()
}

const STATFS_SIZE: usize = 120;
/// (f_blocks, f_bfree) of the volume holding `p`.
unsafe fn statfs_blocks(p: *const u8) -> Option<(u64, u64)> {
    let mut buf = [0u8; STATFS_SIZE];
    if leandros_libc::syscall::syscall2(super::SYS_STATFS, p as usize, buf.as_mut_ptr() as usize) != 0 { return None; }
    let w = |i: usize| u64::from_le_bytes(buf[i * 8..i * 8 + 8].try_into().unwrap());
    Some((w(2), w(3)))
}

/// Check a whole file: size `size`, content of key `key`.
unsafe fn check_file(p: *const u8, key: u64, size: u64) -> bool {
    let fd = open(p, O_RDONLY, 0);
    if fd < 0 { out(b"churn: open for verify failed errno="); out_dec(errno()); out(b"\n"); return false; }
    let mut off = 0u64;
    let mut ok = true;
    loop {
        let n = read(fd, RBUF.as_mut_ptr(), PIECE);
        if n < 0 { ok = false; out(b"churn: read failed errno="); out_dec(errno()); out(b"\n"); break; }
        if n == 0 { break; }
        let n = n as usize;
        let mut want = [0u8; PIECE];
        fill(&mut want[..(n + 7) & !7], key, off);
        if RBUF[..n] != want[..n] {
            let bad = (0..n).find(|&i| RBUF[i] != want[i]).unwrap_or(0) as u64;
            out(b"churn: content mismatch at byte "); out_dec(off + bad); out(b"\n");
            ok = false;
            break;
        }
        off += n as u64;
    }
    close(fd);
    if ok && off != size {
        out(b"churn: size "); out_dec(off); out(b" != expected "); out_dec(size); out(b"\n");
        ok = false;
    }
    ok
}

fn key_of(slot: usize, gen: u32) -> u64 { (slot as u64) << 32 | gen as u64 }

pub unsafe fn churn(dir: &[u8], factor: u64) -> i32 {
    let mut pb = [0u8; 256];
    let dirp = path(&mut pb, dir, b".", None);
    let (blocks, bfree0) = match statfs_blocks(dirp) {
        Some(v) => v,
        None => { out(b"churn: statfs failed\n"); return 1; }
    };
    let _ = blocks;
    // The fillable capacity is what statfs reports free on the empty volume
    // (f_blocks includes metadata, overprovision and the allocator reserve).
    let cap = bfree0 * 4096;
    let budget = cap * 65 / 100;
    let goal = cap * factor;
    out(b"churn: capacity "); out_dec(cap >> 10); out(b" KiB, free "); out_dec(bfree0 * 4); out(b" KiB, writing ");
    out_dec(goal >> 20); out(b" MiB with live data <= "); out_dec(budget >> 10); out(b" KiB\n");

    let mut slots = [Slot { live: false, gen: 0, size: 0 }; NSLOTS];
    let mut rng = Rng(0x5EED_F2F5_0000_0001);
    let mut live = 0u64;
    let mut written = 0u64;
    let mut round = 0u64;
    let avg = budget / NSLOTS as u64 * 2;
    while written < goal {
        round += 1;
        // Pick 1-3 slots to replace.
        let nnew = 1 + rng.below(MAXFILES_ROUND as u64) as usize;
        let mut pick = [usize::MAX; MAXFILES_ROUND];
        let mut sizes = [0u64; MAXFILES_ROUND];
        let mut fds = [-1i32; MAXFILES_ROUND];
        for k in 0..nnew {
            let mut s = rng.below(NSLOTS as u64) as usize;
            while pick[..k].contains(&s) { s = (s + 1) % NSLOTS; }
            pick[k] = s;
            if slots[s].live {
                if unlink(path(&mut pb, dir, b"f", Some(s as u64))) != 0 {
                    out(b"churn: FAIL unlink errno="); out_dec(errno()); out(b"\n"); return 1;
                }
                live -= slots[s].size;
                slots[s].live = false;
            }
        }
        for k in 0..nnew {
            // One in four files is small (a few blocks), the rest up to 2x the
            // average, all capped by what the budget leaves.
            let want = if rng.below(4) == 0 { (1 + rng.below(4)) * 4096 } else { 4096 + rng.below(avg) };
            let room = budget.saturating_sub(live);
            let size = want.min(room) & !4095;
            sizes[k] = size;
            live += size;
            let s = pick[k];
            slots[s].gen = slots[s].gen.wrapping_add(1);
            slots[s].size = size;
            fds[k] = open(path(&mut pb, dir, b"f", Some(s as u64)), O_CREAT | O_RDWR | O_TRUNC, 0o644);
            if fds[k] < 0 {
                out(b"churn: FAIL create errno="); out_dec(errno()); out(b" round "); out_dec(round); out(b"\n");
                return 1;
            }
            slots[s].live = true;
        }
        // Interleaved writes.
        let mut off = 0u64;
        loop {
            let mut any = false;
            for k in 0..nnew {
                if off >= sizes[k] { continue; }
                any = true;
                let n = (sizes[k] - off).min(PIECE as u64) as usize;
                fill(&mut BUF[..n], key_of(pick[k], slots[pick[k]].gen), off);
                let r = write(fds[k], BUF.as_ptr(), n);
                if r != n as isize {
                    let e = errno();
                    let bfree = statfs_blocks(dirp).map_or(0, |v| v.1);
                    out(b"churn: FAIL write returned "); out_dec(r.max(0) as u64); out(b" errno="); out_dec(e);
                    out(b" after "); out_dec(written >> 10); out(b" KiB written (round "); out_dec(round);
                    out(b"), live "); out_dec(live >> 10); out(b" KiB of "); out_dec(cap >> 10);
                    out(b" KiB, statfs free "); out_dec(bfree * 4); out(b" KiB\n");
                    return 1;
                }
                written += n as u64;
            }
            if !any { break; }
            off += PIECE as u64;
        }
        for k in 0..nnew { close(fds[k]); }

        // Now and then: shrink a file (frees blocks in the middle of old
        // segments) or rewrite part of one in place.
        let s = rng.below(NSLOTS as u64) as usize;
        if slots[s].live && slots[s].size > 8192 && round % 3 == 0 {
            let p = path(&mut pb, dir, b"f", Some(s as u64));
            let fd = open(p, O_RDWR, 0);
            if fd < 0 { out(b"churn: FAIL open errno="); out_dec(errno()); out(b"\n"); return 1; }
            if round % 2 == 0 {
                let ns = (slots[s].size / 2) & !4095;
                if !super::ftruncate(fd, ns as off_t) { out(b"churn: FAIL ftruncate\n"); close(fd); return 1; }
                live -= slots[s].size - ns;
                slots[s].size = ns;
            } else {
                let at = (rng.below(slots[s].size / PIECE as u64)) * PIECE as u64;
                let n = (slots[s].size - at).min(PIECE as u64) as usize;
                fill(&mut BUF[..n], key_of(s, slots[s].gen), at);
                if lseek(fd, at as off_t, SEEK_SET) != at as off_t || write(fd, BUF.as_ptr(), n) != n as isize {
                    out(b"churn: FAIL in-place rewrite errno="); out_dec(errno()); out(b"\n"); close(fd); return 1;
                }
            }
            close(fd);
        }
        if round % 16 == 0 {
            let s = rng.below(NSLOTS as u64) as usize;
            if slots[s].live && !check_file(path(&mut pb, dir, b"f", Some(s as u64)), key_of(s, slots[s].gen), slots[s].size) {
                out(b"churn: FAIL verify of slot "); out_dec(s as u64); out(b" in round "); out_dec(round); out(b"\n");
                return 1;
            }
        }
    }
    // Verify everything, write the manifest, sync.
    for s in 0..NSLOTS {
        if slots[s].live && !check_file(path(&mut pb, dir, b"f", Some(s as u64)), key_of(s, slots[s].gen), slots[s].size) {
            out(b"churn: FAIL final verify of slot "); out_dec(s as u64); out(b"\n");
            return 1;
        }
    }
    let mut man = [0u8; 2048];
    let mut m = 0;
    for s in 0..NSLOTS {
        if !slots[s].live { continue; }
        for v in [s as u64, slots[s].gen as u64, slots[s].size] {
            let mut d = [0u8; 20];
            let mut i = d.len();
            let mut x = v;
            loop { i -= 1; d[i] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
            for &c in &d[i..] { man[m] = c; m += 1; }
            man[m] = b' '; m += 1;
        }
        man[m - 1] = b'\n';
    }
    let fd = open(path(&mut pb, dir, b"MANIFEST", None), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 || write(fd, man.as_ptr(), m) != m as isize {
        out(b"churn: FAIL manifest errno="); out_dec(errno()); out(b"\n");
        return 1;
    }
    close(fd);
    leandros_libc::syscall::syscall0(leandros_libc::syscall::nr::SYNC);
    let bfree = statfs_blocks(dirp).map_or(0, |v| v.1);
    out(b"churn: PASS "); out_dec(written >> 20); out(b" MiB written in "); out_dec(round);
    out(b" rounds ("); out_dec(written / cap.max(1)); out(b"x capacity), live "); out_dec(live >> 10);
    out(b" KiB, statfs free "); out_dec(bfree * 4); out(b" KiB\n");
    0
}

pub unsafe fn verify(dir: &[u8]) -> i32 {
    let mut pb = [0u8; 256];
    let fd = open(path(&mut pb, dir, b"MANIFEST", None), O_RDONLY, 0);
    if fd < 0 { out(b"verify: FAIL no MANIFEST\n"); return 1; }
    let mut man = [0u8; 2048];
    let n = read(fd, man.as_mut_ptr(), man.len());
    close(fd);
    if n <= 0 { out(b"verify: FAIL empty MANIFEST\n"); return 1; }
    let mut files = 0u64;
    let mut bytes = 0u64;
    for line in man[..n as usize].split(|&c| c == b'\n') {
        if line.is_empty() { continue; }
        let mut v = [0u64; 3];
        for (i, f) in line.split(|&c| c == b' ').take(3).enumerate() {
            v[i] = f.iter().fold(0u64, |a, &c| a * 10 + (c - b'0') as u64);
        }
        if !check_file(path(&mut pb, dir, b"f", Some(v[0])), key_of(v[0] as usize, v[1] as u32), v[2]) {
            out(b"verify: FAIL slot "); out_dec(v[0]); out(b"\n");
            return 1;
        }
        files += 1;
        bytes += v[2];
    }
    out(b"verify: PASS "); out_dec(files); out(b" files, "); out_dec(bytes >> 10); out(b" KiB\n");
    0
}
