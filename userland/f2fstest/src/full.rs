//! `f2fstest fullchurn DIR SEED [ITERS]` / `f2fstest fullscan DIR KMAX` (lane f2fsfull).
//!
//! fullchurn: fill the volume holding DIR to ~98% with 1 MiB files, then churn
//! small files (create+fsync, rename, unlink) until ENOSPC has been seen many
//! times. Every failure must be ENOSPC (never EIO/other). Prints `ACK k` after
//! an fsync returned, `DEL k` / `REN k` before unlinking / renaming it, and
//! `FULL_DONE enospc=N ops=M` at the end. fullscan prints `P <name> OK|BAD`
//! for every s<k>/t<k> (k < KMAX) that exists, so a host can compare with the
//! claims after a crash.

use leandros_libc::*;
use crate::churn::{errno, out, out_dec, path};

#[cfg(target_arch = "aarch64")]
const SYS_FSYNC: usize = 82;
#[cfg(target_arch = "x86_64")]
const SYS_FSYNC: usize = 74;
const ENOSPC: u64 = 28;

/// 0 on success, else the errno (the raw syscall returns -errno).
unsafe fn fsync(fd: i32) -> u64 {
    let r = leandros_libc::syscall::syscall1(SYS_FSYNC, fd as usize) as isize;
    if r >= 0 { 0 } else { (-r) as u64 }
}

fn fill_block(b: &mut [u8; 4096], k: u64, idx: u64) {
    b[0..8].copy_from_slice(&k.to_le_bytes());
    b[8..16].copy_from_slice(&idx.to_le_bytes());
    let v = (k.wrapping_mul(7).wrapping_add(idx)) as u8;
    for x in b[16..].iter_mut() { *x = v; }
}

fn rng(s: &mut u64) -> u64 {
    *s ^= *s << 13; *s ^= *s >> 7; *s ^= *s << 17; *s
}

/// Create `p` with `nb` blocks for key `k`, fsync. Err(errno) on failure
/// (the partial file is unlinked).
unsafe fn make(p: *const u8, k: u64, nb: u64) -> Result<(), u64> {
    let fd = open(p, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return Err(errno()); }
    let mut b = [0u8; 4096];
    for i in 0..nb {
        fill_block(&mut b, k, i);
        let w = write(fd, b.as_ptr(), 4096);
        if w != 4096 { let e = if w >= 0 { ENOSPC } else { errno() }; close(fd); unlink(p); return Err(e); }
    }
    let fe = fsync(fd);
    if fe != 0 { let e = fe; close(fd); unlink(p); return Err(e); }
    close(fd);
    Ok(())
}

pub unsafe fn fullchurn(dir: &[u8], seed: u64, iters: u64) -> i32 {
    let mut db = [0u8; 256];
    for (i, &c) in dir.iter().enumerate().take(200) { db[i] = c; }
    mkdir(db.as_ptr(), 0o755);
    let b0 = match crate::statfs_bfree(db.as_ptr()) { Some(v) => v, None => { out(b"FAIL statfs\n"); return 1; } };
    let mut pb = [0u8; 256];
    let mut pb2 = [0u8; 256];
    let mut bad = 0;
    // Phase 1: fill to ~98%.
    let mut n = 0u64;
    loop {
        let free = crate::statfs_bfree(db.as_ptr()).unwrap_or(0);
        if free * 100 <= b0 * 2 { break; }
        let p = path(&mut pb, dir, b"f", Some(n));
        match make(p, 0x100000 + n, 256) {
            Ok(()) => { out(b"ACKF "); out_dec(n); out(b"\n"); n += 1; }
            Err(e) => {
                if e != ENOSPC { out(b"FAIL fill errno="); out_dec(e); out(b"\n"); bad += 1; }
                break;
            }
        }
    }
    out(b"FILLED "); out_dec(n); out(b" files, free="); out_dec(crate::statfs_bfree(db.as_ptr()).unwrap_or(0));
    out(b" of "); out_dec(b0); out(b"\n");
    // Phase 2: metadata churn.
    let mut s = seed | 1;
    let mut next = 0u64;     // next key to create
    let mut oldest = 0u64;   // oldest live key
    let mut enospc = 0u64;
    let mut ops = 0u64;
    while ops < iters || enospc < 20 {
        if ops > iters * 4 { break; }
        ops += 1;
        let r = rng(&mut s) % 10;
        if r < 6 || next - oldest < 8 {
            let nb = 1 + rng(&mut s) % 3;
            let p = path(&mut pb, dir, b"s", Some(next));
            match make(p, next, nb) {
                Ok(()) => { out(b"ACK "); out_dec(next); out(b"\n"); next += 1; }
                Err(ENOSPC) => {
                    enospc += 1;
                    // Make room again: drop the newest fill file.
                    if n > 0 { n -= 1; unlink(path(&mut pb, dir, b"f", Some(n))); }
                }
                Err(e) => { out(b"FAIL create errno="); out_dec(e); out(b"\n"); bad += 1; }
            }
        } else if r < 8 {
            // Rename a live s<k> to t<k> (a key renamed already is skipped by the open failing).
            let k = oldest + rng(&mut s) % (next - oldest);
            let a = path(&mut pb, dir, b"s", Some(k));
            let b = path(&mut pb2, dir, b"t", Some(k));
            out(b"REN "); out_dec(k); out(b"\n");
            if rename(a, b) != 0 {
                let e = errno();
                if e != ENOSPC && e != 2 { out(b"FAIL rename errno="); out_dec(e); out(b"\n"); bad += 1; }
                if e == ENOSPC { enospc += 1; }
            }
        } else {
            let k = oldest;
            oldest += 1;
            out(b"DEL "); out_dec(k); out(b"\n");
            let a = path(&mut pb, dir, b"s", Some(k));
            let b = path(&mut pb2, dir, b"t", Some(k));
            if unlink(a) != 0 { unlink(b); }
        }
    }
    // Verify everything live, then free it all and check the space comes back.
    let mut vb = [0u8; 4096];
    for k in oldest..next {
        for nm in [b"s", b"t"] {
            let p = path(&mut pb, dir, nm, Some(k));
            let fd = open(p, O_RDONLY, 0);
            if fd < 0 { continue; }
            let mut i = 0u64;
            let mut exp = [0u8; 4096];
            while read(fd, vb.as_mut_ptr(), 4096) == 4096 {
                fill_block(&mut exp, k, i);
                if vb != exp { out(b"FAIL content k="); out_dec(k); out(b"\n"); bad += 1; break; }
                i += 1;
            }
            close(fd);
        }
    }
    for k in oldest..next {
        unlink(path(&mut pb, dir, b"s", Some(k)));
        unlink(path(&mut pb, dir, b"t", Some(k)));
    }
    for i in 0..n { unlink(path(&mut pb, dir, b"f", Some(i))); }
    out(b"CHURNED live="); out_dec(next - oldest); out(b"\n");
    let after = crate::statfs_bfree(db.as_ptr()).unwrap_or(0);
    if after + b0 / 50 + 16 < b0 { out(b"FAIL space not recovered after="); out_dec(after); out(b"\n"); bad += 1; }
    out(b"FULL_DONE enospc="); out_dec(enospc); out(b" ops="); out_dec(ops);
    out(if bad == 0 { b" PASS\n" } else { b" FAIL\n" });
    bad
}

pub unsafe fn fullscan(dir: &[u8], kmax: u64) -> i32 {
    let mut pb = [0u8; 256];
    let mut vb = [0u8; 4096];
    let mut exp = [0u8; 4096];
    for k in 0..kmax {
        for nm in [b"s", b"t"] {
            let p = path(&mut pb, dir, nm, Some(k));
            let fd = open(p, O_RDONLY, 0);
            if fd < 0 { continue; }
            let (mut i, mut good) = (0u64, true);
            loop {
                let r = read(fd, vb.as_mut_ptr(), 4096);
                if r == 0 { break; }
                fill_block(&mut exp, k, i);
                if r != 4096 || vb != exp { good = false; break; }
                i += 1;
            }
            close(fd);
            out(b"P "); out(nm); out_dec(k);
            out(if good && i > 0 { b" OK\n" } else { b" BAD\n" });
        }
    }
    out(b"SCAN_DONE\n");
    0
}
