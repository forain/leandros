//! `f2fstest orphanreuse DIR` — stale orphan ids vs nid reuse (lane homenotdir).
//!
//! Files unlinked while open and then closed are reclaimed on the close path,
//! but their ids used to stay in the in-memory orphan list. After a checkpoint
//! the freed nids are reused (e.g. for the indirect node of a big file) and the
//! periodic orphan reaper then "reclaimed" the stale id, freeing a live node.
//! This creates stale ids, checkpoints, builds files whose indirect node holds a
//! single dnode nid, waits for the reaper, writes more, and verifies everything.
//! Prints `orphanreuse: PASS` or `orphanreuse: FAIL <what> errno=<n>`.

use leandros_libc::*;
use leandros_libc::syscall::{nr, syscall0, syscall1, syscall4};
use crate::churn::{errno, fill, out, out_dec, path};

#[cfg(target_arch = "aarch64")]
mod nrs { pub const FSYNC: usize = 82; pub const SYNC: usize = 81; pub const MODE_OFF: usize = 16; }
#[cfg(target_arch = "x86_64")]
mod nrs { pub const FSYNC: usize = 74; pub const SYNC: usize = 162; pub const MODE_OFF: usize = 24; }

const MIB: usize = 1 << 20;
const BIG: usize = 12 * MIB + 256 * 1024;
const BUFSZ: usize = 256 * 1024;
static mut BUF: [u8; BUFSZ] = [0; BUFSZ];
static mut RBUF: [u8; BUFSZ] = [0; BUFSZ];

unsafe fn fail(what: &[u8], p: &[u8], e: u64) -> i32 {
    out(b"orphanreuse: FAIL "); out(what); out(b" "); out(p); out(b" errno="); out_dec(e); out(b"\n");
    1
}

unsafe fn sync_() { syscall0(nrs::SYNC); }

/// Write [from, to) of key's pattern at the current offset.
unsafe fn put(fd: i32, key: u64, from: usize, to: usize) -> bool {
    let mut off = from;
    while off < to {
        let n = BUFSZ.min(to - off);
        fill(&mut BUF[..n], key, off as u64);
        let mut o = 0;
        while o < n {
            let w = write(fd, BUF.as_ptr().add(o), n - o);
            if w <= 0 { return false; }
            o += w as usize;
        }
        off += n;
    }
    true
}

/// Check [from, to) of key's pattern (8-aligned bounds).
unsafe fn chkrange(fd: i32, key: u64, from: usize, to: usize) -> Result<(), u64> {
    lseek(fd, from as i64, 0);
    let mut off = from;
    while off < to {
        let n = BUFSZ.min(to - off);
        let r = read(fd, RBUF.as_mut_ptr(), n);
        if r < 0 { return Err(errno()); }
        if r == 0 { return Err(5); }
        let r = r as usize;
        let mut want = [0u8; 4096];
        let mut i = 0;
        while i < r {
            let m = 4096.min(r - i);
            fill(&mut want[..(m + 7) & !7], key, (off + i) as u64);
            if RBUF[i..i + m] != want[..m] { return Err(1000 + ((off + i) / 4096) as u64); }
            i += m;
        }
        off += r;
    }
    Ok(())
}

unsafe fn stat_mode_size(p: *const u8) -> Option<(u32, u64)> {
    let mut st = [0u64; 20];
    let r = syscall4(nr::NEWFSTATAT, (-100isize) as usize, p as usize, st.as_mut_ptr() as usize, 0);
    if r < 0 { return None; }
    let m = *((st.as_ptr() as *const u8).add(nrs::MODE_OFF) as *const u32);
    Some((m, st[6]))
}

unsafe fn sleep_s(s: i64) { let ts = timespec { tv_sec: s, tv_nsec: 0 }; nanosleep(&ts, core::ptr::null_mut()); }

pub(crate) unsafe fn orphanreuse(dir: &[u8]) -> i32 {
    let mut p = [0u8; 256];
    // mkdir -p DIR (parents are the caller's job; DIR itself may exist)
    {
        let mut q = [0u8; 256];
        let n = dir.len();
        q[..n].copy_from_slice(dir);
        mkdir(q.as_ptr(), 0o755);
    }
    // 1. stale orphan ids
    for i in 0..200u64 {
        let pp = path(&mut p, dir, b"o", Some(i));
        let fd = open(pp, O_CREAT | O_RDWR, 0o644);
        if fd < 0 { return fail(b"create o", b"", errno()); }
        let b = [1u8];
        if write(fd, b.as_ptr(), 1) != 1 { return fail(b"write o", b"", errno()); }
        if unlink(pp) < 0 { return fail(b"unlink o", b"", errno()); }
        close(fd);
    }
    sync_(); sync_();
    // 2. big files (indirect node with a single dnode nid) + a directory
    for k in 0..8u64 {
        let pp = path(&mut p, dir, b"big", Some(k));
        let fd = open(pp, O_CREAT | O_RDWR, 0o644);
        if fd < 0 { return fail(b"create big", b"", errno()); }
        if !put(fd, 0xB16 + k, 0, BIG) { return fail(b"write big", b"", errno()); }
        if syscall1(nrs::FSYNC, fd as usize) as isize != 0 { return fail(b"fsync big", b"", errno()); }
        close(fd);
    }
    {
        let pp = path(&mut p, dir, b"d", None);
        if mkdir(pp, 0o755) < 0 { return fail(b"mkdir d", b"", errno()); }
        let mut dd = [0u8; 256];
        let mut k = 0;
        for &c in dir { dd[k] = c; k += 1; }
        for &c in b"/d" { dd[k] = c; k += 1; }
        for i in 0..20u64 {
            let pp = path(&mut p, &dd[..k], b"f", Some(i));
            let fd = open(pp, O_CREAT | O_RDWR, 0o644);
            if fd < 0 { return fail(b"create d/f", b"", errno()); }
            if !put(fd, 0xD0 + i, 0, 4096) { return fail(b"write d/f", b"", errno()); }
            close(fd);
        }
    }
    // 3. let the reaper run, then reuse whatever it freed
    sleep_s(4);
    sync_();
    for i in 0..100u64 {
        let pp = path(&mut p, dir, b"s", Some(i));
        let fd = open(pp, O_CREAT | O_RDWR, 0o644);
        if fd < 0 { return fail(b"create s", b"", errno()); }
        if !put(fd, 0x500 + i, 0, 65536) { return fail(b"write s", b"", errno()); }
        close(fd);
    }
    for k in 0..2u64 {
        let pp = path(&mut p, dir, b"more", Some(k));
        let fd = open(pp, O_CREAT | O_RDWR, 0o644);
        if fd < 0 { return fail(b"create more", b"", errno()); }
        if !put(fd, 0xE0 + k, 0, 12 * MIB + 512 * 1024) { return fail(b"write more", b"", errno()); }
        syscall1(nrs::FSYNC, fd as usize);
        close(fd);
    }
    sync_();
    // 4. verify
    {
        let mut q = [0u8; 256];
        q[..dir.len()].copy_from_slice(dir);
        match stat_mode_size(q.as_ptr()) {
            Some((m, _)) if m & 0o170000 == 0o040000 => {}
            _ => return fail(b"stat DIR not a directory", b"", errno()),
        }
        let pp = path(&mut p, dir, b"d", None);
        match stat_mode_size(pp) {
            Some((m, _)) if m & 0o170000 == 0o040000 => {}
            _ => return fail(b"stat DIR/d not a directory", b"", errno()),
        }
    }
    for k in 0..8u64 {
        let pp = path(&mut p, dir, b"big", Some(k));
        match stat_mode_size(pp) {
            Some((_, s)) if s as usize == BIG => {}
            _ => return fail(b"stat big size", b"", errno()),
        }
        let fd = open(pp, O_RDWR, 0);
        if fd < 0 { return fail(b"open big", b"", errno()); }
        let key = 0xB16 + k;
        for &(a, b) in &[(0usize, 8192usize), (4 * MIB, 4 * MIB + 8192), (12 * MIB - 4096, BIG)] {
            if let Err(e) = chkrange(fd, key, a, b) { close(fd); return fail(b"content big", b"", e); }
        }
        // extend by 4 MiB: needs ind0 node
        lseek(fd, BIG as i64, 0);
        if !put(fd, key, BIG, BIG + 4 * MIB) { let e = errno(); close(fd); return fail(b"extend big", b"", e); }
        syscall1(nrs::FSYNC, fd as usize);
        if let Err(e) = chkrange(fd, key, 12 * MIB, BIG + 4 * MIB) { close(fd); return fail(b"content big after extend", b"", e); }
        close(fd);
    }
    // 5. cleanup
    {
        let mut dd = [0u8; 256];
        let mut k = 0;
        for &c in dir { dd[k] = c; k += 1; }
        for &c in b"/d" { dd[k] = c; k += 1; }
        for i in 0..20u64 {
            if unlink(path(&mut p, &dd[..k], b"f", Some(i))) < 0 { return fail(b"unlink d/f", b"", errno()); }
        }
        if rmdir(path(&mut p, dir, b"d", None)) < 0 { return fail(b"rmdir d", b"", errno()); }
    }
    for k in 0..8u64 { if unlink(path(&mut p, dir, b"big", Some(k))) < 0 { return fail(b"unlink big", b"", errno()); } }
    for k in 0..2u64 { if unlink(path(&mut p, dir, b"more", Some(k))) < 0 { return fail(b"unlink more", b"", errno()); } }
    for i in 0..100u64 { if unlink(path(&mut p, dir, b"s", Some(i))) < 0 { return fail(b"unlink s", b"", errno()); } }
    sync_();
    out(b"orphanreuse: PASS\n");
    0
}
