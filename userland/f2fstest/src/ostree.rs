//! `f2fstest ostree DIR [ROUNDS] [NPROC]` (lane homenotdir): a network-free
//! stand-in for `ostree pull` into a repo on f2fs. Workers write temp files in
//! a per-worker staging dir, fsync some, and renameat them into 256 fan-out
//! dirs `objects/<xx>/<62 hex>.file`, with hardlink churn and replace-by-rename;
//! the parent verifies the tree after each round and "uninstalls" it (all but
//! the last round) so node ids get reused. Any unexpected errno (ENOTDIR!) or
//! type mismatch prints `ostree: FAIL <what> <path> errno=<n>` and exits 1.
//!
//! Object content: word 0 is the object key, the rest is `fill(key)`; the
//! name is 64 hex chars derived from the key and the size from the key.

use leandros_libc::*;
use leandros_libc::syscall::{nr, syscall0, syscall1, syscall2, syscall3, syscall4, syscall5};
use crate::churn::{errno, fill, mix, out, out_dec, Rng};

#[cfg(target_arch = "aarch64")]
mod nrs { pub const FSYNC: usize = 82; pub const LINKAT: usize = 37; pub const FALLOC: usize = 47; pub const FTRUNC: usize = 46; pub const MODE_OFF: usize = 16; }
#[cfg(target_arch = "x86_64")]
mod nrs { pub const FSYNC: usize = 74; pub const LINKAT: usize = 265; pub const FALLOC: usize = 285; pub const FTRUNC: usize = 77; pub const MODE_OFF: usize = 24; }

const OBJECTS: u64 = 6000;
const AT_FDCWD: isize = -100;
const AT_REMOVEDIR: usize = 0x200;
const O_EXCL_: i32 = 0o200;
const MAXSZ: usize = 131072;
const MAXENT: usize = 4096;

const BUFSZ: usize = 1 << 20;
const MIB: usize = 1 << 20;
static mut BUF: [u8; BUFSZ] = [0; BUFSZ];
static mut RBUF: [u8; BUFSZ] = [0; BUFSZ];
static mut NAMES: [[u8; 80]; MAXENT] = [[0; 80]; MAXENT];
static mut DBUF: [u8; 8192] = [0; 8192];

unsafe fn fail(what: &[u8], path: &[u8], e: u64) -> ! {
    out(b"ostree: FAIL "); out(what); out(b" "); out(path);
    out(b" errno="); out_dec(e); out(b"\n");
    exit(1)
}

fn cat(buf: &mut [u8; 300], parts: &[&[u8]]) -> usize {
    let mut k = 0;
    for p in parts { for &c in p.iter() { buf[k] = c; k += 1; } }
    buf[k] = 0;
    k
}

unsafe fn nowms() -> u64 {
    let mut t = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(1, &mut t);
    t.tv_sec as u64 * 1000 + t.tv_nsec as u64 / 1_000_000
}

fn hex(v: u64, o: &mut [u8]) {
    for i in 0..16 { o[i] = b"0123456789abcdef"[((v >> (60 - 4 * i)) & 15) as usize]; }
}

/// (dir "xx", rest "<62 hex>.file") as one relative path "xx/<62>.file".
fn rel_of(key: u64, r: &mut [u8; 80]) -> usize {
    let mut h = [0u8; 64];
    for i in 0..4 { hex(mix(key.wrapping_add(i as u64 * 0x9E3779B97F4A7C15)), &mut h[i * 16..i * 16 + 16]); }
    r[0] = h[0]; r[1] = h[1]; r[2] = b'/';
    for i in 2..64 { r[i + 1] = h[i]; }
    for (j, &c) in b".file".iter().enumerate() { r[65 + j] = c; }
    r[70] = 0;
    70
}

fn is_big(key: u64) -> bool { key & 0xff == 0xB1 }

fn size_of(key: u64) -> usize {
    if is_big(key) { return (12 * MIB + (mix(key ^ 0x7777) as usize % (24 * MIB))) & !7; }
    let r = mix(key ^ 0x5151);
    let s = match r % 10 {
        0 => 16384 + ((r >> 8) as usize % (112 * 1024)),
        1..=5 => (r >> 8) as usize % 200,
        _ => (r >> 8) as usize % 4096,
    };
    ((s + 7) & !7).max(8)
}

unsafe fn chk(r: isize, what: &[u8], path: &[u8]) -> isize {
    if r < 0 { fail(what, path, (-r) as u64); }
    r
}

unsafe fn is_mode(r: &[u64; 20], fmt: u32) -> bool {
    let m = *((r.as_ptr() as *const u8).add(nrs::MODE_OFF) as *const u32);
    m & 0o170000 == fmt
}
unsafe fn st_size(r: &[u64; 20]) -> u64 { r[6] }

/// fstatat that must succeed and have the given type (0o100000 file, 0o040000 dir).
unsafe fn stat_chk(dfd: isize, p: &[u8], disp: &[u8], fmt: u32) -> [u64; 20] {
    let mut st = [0u64; 20];
    let r = syscall4(nr::NEWFSTATAT, dfd as usize, p.as_ptr() as usize, st.as_mut_ptr() as usize, 0);
    chk(r, b"fstatat", disp);
    if !is_mode(&st, fmt) { fail(b"type-mismatch", disp, 0); }
    st
}

unsafe fn write_all(fd: i32, n: usize, disp: &[u8]) {
    let mut o = 0;
    while o < n {
        let w = write(fd, BUF.as_ptr().add(o), n - o);
        if w <= 0 { fail(b"write", disp, errno()); }
        o += w as usize;
    }
}

/// Write bytes [from, to) of the object's content at the current offset.
unsafe fn put_range(fd: i32, key: u64, from: usize, to: usize, rng: &mut Rng, disp: &[u8]) {
    let mut off = from;
    while off < to {
        let want = (256 * 1024 * (1 + rng.below(4) as usize)).min(to - off);
        fill(&mut BUF[..want], key, off as u64);
        if off == 0 { BUF[..8].copy_from_slice(&key.to_le_bytes()); }
        write_all(fd, want, disp);
        off += want;
    }
}

unsafe fn write_big(dfd: i32, tmp: &[u8], key: u64, rng: &mut Rng) {
    let size = size_of(key);
    let fd = openat(dfd, tmp.as_ptr(), O_WRONLY | O_CREAT | O_EXCL_, 0o600);
    if fd < 0 { fail(b"open-excl-big", &tmp[..tmp.len() - 1], errno()); }
    let r = syscall4(nrs::FALLOC, fd as usize, 0, 0, size);
    if r < 0 && r != -95 && r != -38 { fail(b"fallocate", tmp, (-r) as u64); }
    if rng.below(2) == 0 {
        put_range(fd, key, 0, size + 8 * MIB, rng, tmp);
        chk(syscall2(nrs::FTRUNC, fd as usize, size), b"ftruncate", tmp);
        lseek(fd, (12 * MIB) as i64, 0);
        put_range(fd, key, 12 * MIB, (14 * MIB).min(size), rng, tmp);
    } else {
        put_range(fd, key, 0, size, rng, tmp);
    }
    if fchmod(fd, 0o644) < 0 { fail(b"fchmod", tmp, errno()); }
    chk(syscall1(nrs::FSYNC, fd as usize), b"fsync-big", tmp);
    close(fd);
}

unsafe fn write_obj(dfd: i32, tmp: &[u8], key: u64, rng: &mut Rng) {
    if is_big(key) { return write_big(dfd, tmp, key, rng); }
    let size = size_of(key);
    let fd = openat(dfd, tmp.as_ptr(), O_WRONLY | O_CREAT | O_EXCL_, 0o600);
    if fd < 0 { fail(b"open-excl", &tmp[..tmp.len() - 1], errno()); }
    fill(&mut BUF[..size], key, 0);
    BUF[..8].copy_from_slice(&key.to_le_bytes());
    write_all(fd, size, tmp);
    if fchmod(fd, 0o644) < 0 { fail(b"fchmod", tmp, errno()); }
    if rng.below(4) == 0 { chk(syscall1(nrs::FSYNC, fd as usize), b"fsync", tmp); }
    close(fd);
}

unsafe fn open_dir(dfd: isize, p: &[u8], disp: &[u8]) -> i32 {
    let fd = openat(dfd as i32, p.as_ptr(), O_RDONLY, 0);
    if fd < 0 { fail(b"opendir", disp, errno()); }
    fd
}

unsafe fn worker(dir: &[u8], round: u64, seedbase: u64) -> ! {
    let pid = syscall0(nr::GETPID) as u64;
    let mut rng = Rng(seedbase ^ (pid << 20) ^ round);
    let mut pb = [0u8; 300];
    let n = cat(&mut pb, &[dir, b"/repo/objects"]);
    let objfd = open_dir(AT_FDCWD, &pb[..n + 1], &pb[..n]) as isize;
    let n = cat(&mut pb, &[dir, b"/repo/tmp"]);
    let tmpfd = open_dir(AT_FDCWD, &pb[..n + 1], &pb[..n]) as isize;
    let n = cat(&mut pb, &[dir, b"/repo"]);
    let repofd = open_dir(AT_FDCWD, &pb[..n + 1], &pb[..n]) as isize;
    let mut sn = [0u8; 80];
    let mut k = 0;
    for &c in b"staging-" { sn[k] = c; k += 1; }
    for v in [pid, round] {
        let mut d = [0u8; 20]; let mut i = 20; let mut x = v;
        loop { i -= 1; d[i] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
        for &c in &d[i..] { sn[k] = c; k += 1; }
        if v == pid { sn[k] = b'-'; k += 1; }
    }
    sn[k] = 0;
    let sname = &sn[..k];
    chk(syscall3(nr::MKDIRAT, tmpfd as usize, sn.as_ptr() as usize, 0o755), b"mkdirat-staging", sname);
    let stfd = open_dir(tmpfd, &sn[..k + 1], sname) as isize;
    let mut ring = [0u64; 64];
    let mut nring = 0usize;
    for i in 0..OBJECTS {
        let big = i == 500 || i == 1500 || rng.below(500) == 0;
        let mut key = rng.next();
        if big { key = (key & !0xff) | 0xB1; } else if key & 0xff == 0xB1 { key ^= 1; }
        let mut tmp = [0u8; 20];
        tmp[0] = b't'; hex(rng.next(), &mut tmp[1..17]); tmp[17] = 0;
        let mut rel = [0u8; 80];
        let rl = rel_of(key, &mut rel);
        let disp = &rel[..rl];
        write_obj(stfd as i32, &tmp[..18], key, &mut rng);
        let mut dn = [0u8; 4];
        dn[0] = rel[0]; dn[1] = rel[1];
        let r = syscall3(nr::MKDIRAT, objfd as usize, dn.as_ptr() as usize, 0o755);
        if r < 0 && r != -17 { fail(b"mkdirat-fanout", &dn[..2], (-r) as u64); }
        chk(syscall4(nr::RENAMEAT, stfd as usize, tmp.as_ptr() as usize, objfd as usize, rel.as_ptr() as usize),
            b"renameat", disp);
        let st = stat_chk(objfd, &rel[..rl + 1], disp, 0o100000);
        if st_size(&st) != size_of(key) as u64 { out(b"got="); out_dec(st_size(&st)); out(b" want="); out_dec(size_of(key) as u64); out(b" big="); out_dec(is_big(key) as u64); out(b"\n"); fail(b"size-mismatch", disp, 0); }
        stat_chk(objfd, &dn[..3], &dn[..2], 0o040000);
        if rng.below(8) == 0 {
            let mut ln = [0u8; 20];
            ln[0] = b'l'; hex(rng.next(), &mut ln[1..17]);
            chk(syscall5(nrs::LINKAT, objfd as usize, rel.as_ptr() as usize, stfd as usize, ln.as_ptr() as usize, 0),
                b"linkat", disp);
            chk(syscall3(nr::UNLINKAT, stfd as usize, ln.as_ptr() as usize, 0), b"unlinkat-link", &ln[..17]);
        }
        if nring > 0 && rng.below(16) == 0 {
            let pk = ring[rng.below(nring as u64) as usize];
            let mut prel = [0u8; 80];
            let pl = rel_of(pk, &mut prel);
            let mut t2 = [0u8; 20];
            t2[0] = b'r'; hex(rng.next(), &mut t2[1..17]); t2[17] = 0;
            write_obj(stfd as i32, &t2[..18], pk, &mut rng);
            chk(syscall4(nr::RENAMEAT, stfd as usize, t2.as_ptr() as usize, objfd as usize, prel.as_ptr() as usize),
                b"renameat-replace", &prel[..pl]);
            stat_chk(objfd, &prel[..pl + 1], &prel[..pl], 0o100000);
        }
        ring[nring % 64] = key; if nring < 64 { nring += 1; } else { nring = 64; }
        if i % 200 == 199 {
            let sets: [&[u8]; 4] = [b"", b"/repo", b"/repo/objects", b"/repo/tmp"];
            for s in sets {
                let n = cat(&mut pb, &[dir, s]);
                stat_chk(AT_FDCWD, &pb[..n + 1], &pb[..n], 0o040000);
            }
            chk(syscall1(nrs::FSYNC, objfd as usize), b"fsync-objects", b"objects");
            chk(syscall1(nrs::FSYNC, repofd as usize), b"fsync-repo", b"repo");
        }
    }
    chk(syscall3(nr::UNLINKAT, tmpfd as usize, sn.as_ptr() as usize, AT_REMOVEDIR), b"rmdir-staging", sname);
    exit(0)
}

/// Names (no . / ..) of directory `fd` into NAMES; returns count.
unsafe fn list(fd: i32, disp: &[u8]) -> usize {
    let mut n = 0;
    lseek(fd, 0, 0);
    loop {
        let r = getdents64(fd, DBUF.as_mut_ptr(), DBUF.len());
        if r < 0 { fail(b"getdents", disp, errno()); }
        if r == 0 { break; }
        let mut o = 0usize;
        while o < r as usize {
            let reclen = u16::from_le_bytes([DBUF[o + 16], DBUF[o + 17]]) as usize;
            let nm = &DBUF[o + 19..o + reclen];
            let l = nm.iter().position(|&c| c == 0).unwrap_or(nm.len());
            if !(l == 1 && nm[0] == b'.') && !(l == 2 && nm[0] == b'.' && nm[1] == b'.') {
                if n >= MAXENT || l > 78 { fail(b"too-many-entries", disp, 0); }
                NAMES[n][..l].copy_from_slice(&nm[..l]);
                NAMES[n][l] = 0;
                n += 1;
            }
            o += reclen;
        }
    }
    n
}

unsafe fn read_at(fd: i32, off: usize, n: usize, disp: &[u8]) {
    lseek(fd, off as i64, 0);
    let mut got = 0;
    while got < n {
        let r = read(fd, RBUF.as_mut_ptr().add(got), n - got);
        if r <= 0 { fail(b"read-big", disp, errno()); }
        got += r as usize;
    }
}

unsafe fn verify_big(fd: i32, sz: usize, disp: &[u8]) {
    read_at(fd, 0, 4096, disp);
    let key = u64::from_le_bytes([RBUF[0], RBUF[1], RBUF[2], RBUF[3], RBUF[4], RBUF[5], RBUF[6], RBUF[7]]);
    let mut rel = [0u8; 80];
    let rl = rel_of(key, &mut rel);
    if !is_big(key) || &rel[..rl] != disp || size_of(key) != sz { fail(b"name-key-mismatch-big", disp, 0); }
    let offs = [0usize, 4 * MIB, 12 * MIB - 4096, 12 * MIB, 13 * MIB + 8, 14 * MIB + 4096, sz / 2 & !7, (sz - 4096) & !7];
    for &o in offs.iter() {
        if o + 4096 > sz { continue; }
        read_at(fd, o, 4096, disp);
        fill(&mut BUF[..4096], key, o as u64);
        if o == 0 { BUF[..8].copy_from_slice(&key.to_le_bytes()); }
        if BUF[..4096] != RBUF[..4096] { fail(b"content-mismatch-big", disp, o as u64); }
    }
}

/// Walk objects/*/: verify (and optionally unlink + rmdir). Returns object count.
unsafe fn walk(dir: &[u8], remove: bool) -> u64 {
    let mut pb = [0u8; 300];
    let n = cat(&mut pb, &[dir, b"/repo/objects"]);
    let objfd = open_dir(AT_FDCWD, &pb[..n + 1], &pb[..n]);
    let mut tops = [[0u8; 4]; 256];
    let nt = list(objfd, &pb[..n]);
    if nt > 256 { fail(b"too-many-fanout", &pb[..n], 0); }
    for i in 0..nt { tops[i].copy_from_slice(&NAMES[i][..4]); }
    let mut total = 0u64;
    for t in 0..nt {
        let tn = &tops[t][..2];
        stat_chk(objfd as isize, &tops[t][..3], tn, 0o040000);
        let sfd = open_dir(objfd as isize, &tops[t][..3], tn);
        let ne = list(sfd, tn);
        for e in 0..ne {
            let nm = NAMES[e];
            let l = nm.iter().position(|&c| c == 0).unwrap_or(0);
            let mut full = [0u8; 300];
            let fl = cat(&mut full, &[tn, b"/", &nm[..l]]);
            let st = stat_chk(sfd as isize, &nm[..l + 1], &full[..fl], 0o100000);
            let sz = st_size(&st) as usize;
            if sz < 8 || sz > 48 * MIB { fail(b"bad-size", &full[..fl], 0); }
            let fd = openat(sfd, nm.as_ptr(), O_RDONLY, 0);
            if fd < 0 { fail(b"open-read", &full[..fl], errno()); }
            if sz > MAXSZ {
                verify_big(fd, sz, &full[..fl]);
                close(fd);
                total += 1;
                continue;
            }
            let mut got = 0;
            while got < sz {
                let r = read(fd, RBUF.as_mut_ptr().add(got), sz - got);
                if r <= 0 { fail(b"read", &full[..fl], errno()); }
                got += r as usize;
            }
            close(fd);
            let key = u64::from_le_bytes([RBUF[0], RBUF[1], RBUF[2], RBUF[3], RBUF[4], RBUF[5], RBUF[6], RBUF[7]]);
            let mut rel = [0u8; 80];
            let rl = rel_of(key, &mut rel);
            if &rel[..rl] != &full[..fl] || size_of(key) != sz { fail(b"name-key-mismatch", &full[..fl], 0); }
            if mix(key) % 4 == 0 {
                fill(&mut BUF[..sz], key, 0);
                BUF[..8].copy_from_slice(&key.to_le_bytes());
                if BUF[..sz] != RBUF[..sz] { fail(b"content-mismatch", &full[..fl], 0); }
            }
            total += 1;
        }
        if remove {
            // Re-list (NAMES was reused by nothing else) and unlink every entry.
            for e in 0..ne {
                let nm = NAMES[e];
                let l = nm.iter().position(|&c| c == 0).unwrap_or(0);
                let mut full = [0u8; 300];
                let fl = cat(&mut full, &[tn, b"/", &nm[..l]]);
                chk(syscall3(nr::UNLINKAT, sfd as usize, nm.as_ptr() as usize, 0), b"unlinkat-object", &full[..fl]);
            }
        }
        close(sfd);
        if remove {
            chk(syscall3(nr::UNLINKAT, objfd as usize, tops[t].as_ptr() as usize, AT_REMOVEDIR), b"rmdir-fanout", tn);
        }
    }
    close(objfd);
    total
}

pub(crate) unsafe fn ostree(dir: &[u8], rounds: u64, nproc: u64) -> i32 {
    let t0 = nowms();
    let mut pb = [0u8; 300];
    for sub in [&b""[..], b"/repo", b"/repo/objects", b"/repo/tmp"] {
        let n = cat(&mut pb, &[dir, sub]);
        let r = syscall3(nr::MKDIRAT, AT_FDCWD as usize, pb.as_ptr() as usize, 0o755);
        if r < 0 && r != -17 { fail(b"mkdir", &pb[..n], (-r) as u64); }
    }
    let mut total = 0u64;
    for round in 0..rounds {
        for w in 0..nproc {
            let p = fork();
            if p < 0 { fail(b"fork", dir, errno()); }
            if p == 0 { worker(dir, round, 0x1234 + w * 7919); }
        }
        let mut bad = false;
        for _ in 0..nproc {
            let mut st = 0i32;
            if wait4(-1, &mut st, 0, core::ptr::null_mut()) < 0 || st != 0 { bad = true; }
        }
        if bad { out(b"ostree: FAIL worker-failed "); out(dir); out(b" errno=0\n"); return 1; }
        total += walk(dir, round + 1 < rounds);
        out(b"ostree: round "); out_dec(round); out(b" ok\n");
    }
    walk(dir, true);
    for sub in [&b"/repo/objects"[..], b"/repo/tmp", b"/repo", b""] {
        let n = cat(&mut pb, &[dir, sub]);
        let _ = n;
        syscall3(nr::UNLINKAT, AT_FDCWD as usize, pb.as_ptr() as usize, AT_REMOVEDIR);
    }
    out(b"ostree: PASS rounds="); out_dec(rounds);
    out(b" objects="); out_dec(total);
    out(b" secs="); out_dec((nowms() - t0) / 1000); out(b"\n");
    0
}
