//! pcachetest — coherency of the unified page cache (lane pagecache).
//!
//! Every read below may be served from cached pages, so each case checks
//! that what read(2)/pread(2)/a mapping returns is what the file holds after
//! every kind of change:
//!
//!   write_read      write, read (fills the cache), pwrite into the middle,
//!                   read again; plus a cold re-open
//!   mmap_coherent   MAP_SHARED + pwrite → the mapping sees it; a store
//!                   through the mapping → pread sees it; msync(MS_SYNC),
//!                   munmap, re-open: the store is in the file
//!   mmap_fork       a forked child stores through its MAP_SHARED copy; the
//!                   parent's mapping and pread see it
//!   truncate        shrink (cached pages past EOF go, the new tail reads
//!                   zero, also through a mapping) and grow (the gap reads
//!                   zero)
//!   unlink_open     unlink while open: the descriptor keeps reading and
//!                   writing the data; the name is gone
//!   two_procs       parent and child alternate pwrite/pread of one page
//!                   through separate descriptors, 32 versions each way
//!   append          O_APPEND writes seen by a reader descriptor, lseek(END)
//!   big_evict       file larger than the cache cap (when /proc/pcache says
//!                   the cap is small) or than free memory pressure allows:
//!                   read twice, compare checksums, eviction counter moved
//!   fsync_durable   write + fsync, msync(MS_SYNC) of a mapping: both return
//!                   0 and the bytes read back (the reboot half of the
//!                   crash check is host-driven)
//!
//! Prints `<case>: PASS|FAIL` lines, exit status = number of failures.

#![no_std]
#![no_main]

extern crate leandros_libc;
use leandros_libc::*;
use leandros_libc::syscall::{syscall2, syscall3, syscall4};

#[cfg(target_arch = "aarch64")]
mod nr {
    pub const PREAD64: usize = 67;
    pub const PWRITE64: usize = 68;
    pub const FTRUNCATE: usize = 46;
    pub const FSYNC: usize = 82;
    pub const MSYNC: usize = 227;
}
#[cfg(not(target_arch = "aarch64"))]
mod nr {
    pub const PREAD64: usize = 17;
    pub const PWRITE64: usize = 18;
    pub const FTRUNCATE: usize = 77;
    pub const FSYNC: usize = 74;
    pub const MSYNC: usize = 26;
}

const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MS_SYNC: usize = 4;
const PS: usize = 4096;

static mut BUF: [u8; 1 << 20] = [0; 1 << 20];
static mut BUF2: [u8; 1 << 20] = [0; 1 << 20];

unsafe fn buf() -> &'static mut [u8] { &mut *core::ptr::addr_of_mut!(BUF) }
unsafe fn buf2() -> &'static mut [u8] { &mut *core::ptr::addr_of_mut!(BUF2) }

unsafe fn pread(fd: i32, b: *mut u8, n: usize, off: usize) -> isize {
    syscall4(nr::PREAD64, fd as usize, b as usize, n, off)
}
unsafe fn pwrite(fd: i32, b: *const u8, n: usize, off: usize) -> isize {
    syscall4(nr::PWRITE64, fd as usize, b as usize, n, off)
}
unsafe fn ftruncate(fd: i32, len: usize) -> isize { syscall2(nr::FTRUNCATE, fd as usize, len) }
unsafe fn fsync(fd: i32) -> isize { syscall2(nr::FSYNC, fd as usize, 0) }
unsafe fn msync(p: *mut u8, len: usize, flags: usize) -> isize {
    syscall3(nr::MSYNC, p as usize, len, flags)
}

/// The byte at offset `off` of version `v` of a test file.
fn pat(v: u32, off: usize) -> u8 {
    let x = (off as u32).wrapping_mul(2654435761).wrapping_add(v.wrapping_mul(40503));
    (x >> 13) as u8 ^ (v as u8)
}

unsafe fn fill(b: &mut [u8], v: u32, base: usize) {
    for (i, c) in b.iter_mut().enumerate() { *c = pat(v, base + i); }
}

unsafe fn say(s: &[u8]) { write(STDOUT_FILENO, s.as_ptr(), s.len()); }
unsafe fn say_num(n: u64) {
    let mut d = [0u8; 20]; let mut i = d.len(); let mut x = n;
    loop { i -= 1; d[i] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
    say(&d[i..]);
}

unsafe fn report(name: &[u8], ok: bool) -> bool {
    say(name);
    say(if ok { b": PASS\n" } else { b": FAIL\n" });
    ok
}

unsafe fn why(name: &[u8], what: &[u8], v: u64) -> bool {
    say(name); say(b": FAIL ("); say(what); say(b" "); say_num(v); say(b")\n");
    false
}

/// MAP_SHARED of a file goes through the page cache only with the cache
/// on; with `pcache=0` a writable shared mapping is the old private copy,
/// so the shared-mapping cases are skipped.
unsafe fn shared_maps_cached(name: &[u8]) -> bool {
    if pcache_val(b"enabled") == Some(0) {
        say(name); say(b": SKIP (pcache=0)\n");
        return false;
    }
    true
}

/// Read the whole of `path` (at most `cap` bytes) into `b`.
unsafe fn slurp(fd: i32, b: &mut [u8]) -> usize {
    let mut got = 0usize;
    while got < b.len() {
        let n = read(fd, b.as_mut_ptr().add(got), b.len() - got);
        if n <= 0 { break; }
        got += n as usize;
    }
    got
}

unsafe fn check_file(fd: i32, len: usize, model: &[u8]) -> Option<usize> {
    let b = buf2();
    let n = pread(fd, b.as_mut_ptr(), len + 100, 0);
    if n != len as isize { return Some(usize::MAX - 1); }
    (0..len).find(|&i| b[i] != model[i])
}

// ── write_read ────────────────────────────────────────────────────────────────
unsafe fn t_write_read() -> bool {
    let name = b"write_read";
    let path = b"/root/pc_wr.bin\0";
    unlink(path.as_ptr());
    let fd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return why(name, b"open", 0); }
    let len = 16 * PS + 777;
    let m = buf();
    fill(&mut m[..len], 1, 0);
    if write(fd, m.as_ptr(), len) != len as isize { close(fd); return why(name, b"write", 0); }
    // First read fills the cache, second is a hit; both must match.
    for round in 0..2 {
        if let Some(i) = check_file(fd, len, m) { close(fd); return why(name, b"read mismatch at", i as u64 + round * 1_000_000_000); }
    }
    // Overwrite across a page boundary and into the partial last page.
    for &(off, n, v) in &[(5000usize, 3000usize, 2u32), (len - 500, 400, 3), (PS * 3, PS * 2, 4)] {
        let mut tmp = [0u8; 2 * PS];
        fill(&mut tmp[..n], v, off);
        if pwrite(fd, tmp.as_ptr(), n, off) != n as isize { close(fd); return why(name, b"pwrite", off as u64); }
        m[off..off + n].copy_from_slice(&tmp[..n]);
        if let Some(i) = check_file(fd, len, m) { close(fd); return why(name, b"after pwrite mismatch at", i as u64); }
    }
    // read(2) with the descriptor offset: seek, read a span crossing pages.
    lseek(fd, 4000, SEEK_SET);
    let mut tmp = [0u8; 9000];
    if read(fd, tmp.as_mut_ptr(), 9000) != 9000 { close(fd); return why(name, b"read at offset", 0); }
    if tmp[..] != m[4000..13000] { close(fd); return why(name, b"read(2) span mismatch", 0); }
    if lseek(fd, 0, SEEK_CUR) as i64 != 13000 { close(fd); return why(name, b"offset after read", 0); }
    close(fd);
    // A fresh descriptor sees the same (cached) bytes.
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    let b = buf2();
    let n = slurp(fd, &mut b[..len + 50]);
    close(fd);
    unlink(path.as_ptr());
    if n != len || b[..len] != m[..len] { return why(name, b"reopen mismatch", n as u64); }
    report(name, true)
}

// ── mmap_coherent ─────────────────────────────────────────────────────────────
unsafe fn t_mmap_coherent() -> bool {
    let name = b"mmap_coherent";
    if !shared_maps_cached(name) { return true; }
    let path = b"/root/pc_mm.bin\0";
    unlink(path.as_ptr());
    let fd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return why(name, b"open", 0); }
    let len = 8 * PS + 1234;
    let m = buf();
    fill(&mut m[..len], 10, 0);
    if write(fd, m.as_ptr(), len) != len as isize { close(fd); return why(name, b"write", 0); }
    let map = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if map as isize == -1 { close(fd); return why(name, b"mmap errno", get_errno() as u64); }
    // Mapping reads what was written.
    for i in 0..len { if *map.add(i) != m[i] { close(fd); return why(name, b"map read mismatch at", i as u64); } }
    // write(2) → mapping.
    let mut tmp = [0u8; PS];
    fill(&mut tmp[..300], 11, 2 * PS + 100);
    pwrite(fd, tmp.as_ptr(), 300, 2 * PS + 100);
    for i in 0..300 {
        if *map.add(2 * PS + 100 + i) != tmp[i] { close(fd); return why(name, b"pwrite not seen by mapping at", i as u64); }
    }
    m[2 * PS + 100..2 * PS + 400].copy_from_slice(&tmp[..300]);
    // Mapping store → pread.
    for i in 0..500 { let b = pat(12, 5 * PS + 7 + i); *map.add(5 * PS + 7 + i) = b; m[5 * PS + 7 + i] = b; }
    // Store into the partial last page, inside EOF.
    *map.add(len - 1) = 0x5a; m[len - 1] = 0x5a;
    if let Some(i) = check_file(fd, len, m) { close(fd); return why(name, b"store not seen by pread at", i as u64); }
    if msync(map, len, MS_SYNC) != 0 { close(fd); return why(name, b"msync", 0); }
    munmap(map, len);
    close(fd);
    // Re-open: the stores are in the file (through the cache, and after
    // write-back through the filesystem).
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    let ok = check_file(fd, len, m);
    close(fd);
    if let Some(i) = ok { return why(name, b"reopen mismatch at", i as u64); }
    // A writable MAP_SHARED of an O_RDONLY descriptor is EACCES.
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    let bad = mmap(core::ptr::null_mut(), PS, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    close(fd);
    unlink(path.as_ptr());
    if bad as isize != -1 { return why(name, b"rw MAP_SHARED of O_RDONLY fd succeeded", 0); }
    report(name, true)
}

// ── mmap_fork ─────────────────────────────────────────────────────────────────
unsafe fn t_mmap_fork() -> bool {
    let name = b"mmap_fork";
    if !shared_maps_cached(name) { return true; }
    let path = b"/root/pc_mf.bin\0";
    unlink(path.as_ptr());
    let fd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return why(name, b"open", 0); }
    let len = 4 * PS;
    let m = buf();
    fill(&mut m[..len], 20, 0);
    write(fd, m.as_ptr(), len);
    let map = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if map as isize == -1 { close(fd); return why(name, b"mmap", 0); }
    let _ = *map; // fault page 0 in before the fork (shared, not CoW)
    let pid = fork();
    if pid == 0 {
        for i in 0..len { *map.add(i) = pat(21, i); }
        exit(0);
    }
    let mut st = 0i32;
    wait4(pid, &mut st, 0, core::ptr::null_mut());
    fill(&mut m[..len], 21, 0);
    for i in 0..len { if *map.add(i) != m[i] { munmap(map, len); close(fd); return why(name, b"child store not in parent mapping at", i as u64); } }
    let r = check_file(fd, len, m);
    munmap(map, len);
    close(fd);
    unlink(path.as_ptr());
    if let Some(i) = r { return why(name, b"child store not seen by pread at", i as u64); }
    report(name, true)
}

// ── truncate ──────────────────────────────────────────────────────────────────
unsafe fn t_truncate() -> bool {
    let name = b"truncate";
    let path = b"/root/pc_tr.bin\0";
    unlink(path.as_ptr());
    let fd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return why(name, b"open", 0); }
    let len = 3 * PS;
    let m = buf();
    fill(&mut m[..len], 30, 0);
    write(fd, m.as_ptr(), len);
    if check_file(fd, len, m).is_some() { close(fd); return why(name, b"initial", 0); }
    let map = mmap(core::ptr::null_mut(), 2 * PS, PROT_READ, MAP_SHARED, fd, 0);
    if map as isize == -1 { close(fd); return why(name, b"mmap", 0); }
    let _ = *map.add(PS + 10);
    // Shrink into page 1.
    let cut = PS + 904;
    if ftruncate(fd, cut) != 0 { close(fd); return why(name, b"ftruncate shrink", 0); }
    if check_file(fd, cut, m).is_some() { close(fd); return why(name, b"after shrink", 0); }
    // The mapping's page 1 past the new EOF reads zeros.
    for i in cut..2 * PS { if *map.add(i) != 0 { close(fd); return why(name, b"mapping tail not zero at", i as u64); } }
    // Grow: [cut, grown) must read zeros.
    let grown = 5 * PS + 17;
    if ftruncate(fd, grown) != 0 { close(fd); return why(name, b"ftruncate grow", 0); }
    for i in cut..grown { m[i] = 0; }
    if let Some(i) = check_file(fd, grown, m) { close(fd); return why(name, b"after grow mismatch at", i as u64); }
    // Write past EOF leaves a hole that reads zeros.
    let far = 7 * PS + 100;
    let mut tmp = [0u8; 50];
    fill(&mut tmp, 31, far);
    pwrite(fd, tmp.as_ptr(), 50, far);
    for i in grown..far { m[i] = 0; }
    m[far..far + 50].copy_from_slice(&tmp);
    if let Some(i) = check_file(fd, far + 50, m) { close(fd); return why(name, b"after write past EOF mismatch at", i as u64); }
    // Truncate to zero via O_TRUNC.
    munmap(map, 2 * PS);
    close(fd);
    let fd = open(path.as_ptr(), O_RDWR | O_TRUNC, 0);
    let mut one = [0u8; 16];
    let n = read(fd, one.as_mut_ptr(), 16);
    close(fd);
    unlink(path.as_ptr());
    if n != 0 { return why(name, b"O_TRUNC read", n as u64); }
    report(name, true)
}

// ── unlink_open ───────────────────────────────────────────────────────────────
unsafe fn t_unlink_open() -> bool {
    let name = b"unlink_open";
    let path = b"/root/pc_ul.bin\0";
    unlink(path.as_ptr());
    let fd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return why(name, b"open", 0); }
    let len = 6 * PS + 5;
    let m = buf();
    fill(&mut m[..len], 40, 0);
    write(fd, m.as_ptr(), len);
    if check_file(fd, len, m).is_some() { close(fd); return why(name, b"initial", 0); }
    if unlink(path.as_ptr()) != 0 { close(fd); return why(name, b"unlink", 0); }
    if open(path.as_ptr(), O_RDONLY, 0) >= 0 { close(fd); return why(name, b"name still there", 0); }
    if let Some(i) = check_file(fd, len, m) { close(fd); return why(name, b"after unlink mismatch at", i as u64); }
    let mut tmp = [0u8; 100];
    fill(&mut tmp, 41, 2 * PS);
    pwrite(fd, tmp.as_ptr(), 100, 2 * PS);
    m[2 * PS..2 * PS + 100].copy_from_slice(&tmp);
    let r = check_file(fd, len, m);
    close(fd);
    if let Some(i) = r { return why(name, b"write after unlink mismatch at", i as u64); }
    report(name, true)
}

// ── two_procs ─────────────────────────────────────────────────────────────────
unsafe fn t_two_procs() -> bool {
    let name = b"two_procs";
    let path = b"/root/pc_2p.bin\0";
    unlink(path.as_ptr());
    let fd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return why(name, b"open", 0); }
    let len = 2 * PS;
    let mut page = [0u8; 2 * PS];
    fill(&mut page, 0, 0);
    write(fd, page.as_ptr(), len);
    close(fd);
    let mut p2c = [0i32; 2];
    let mut c2p = [0i32; 2];
    if pipe(p2c.as_mut_ptr()) != 0 || pipe(c2p.as_mut_ptr()) != 0 { return why(name, b"pipe", 0); }
    let pid = fork();
    if pid == 0 {
        let fd = open(path.as_ptr(), O_RDWR, 0);
        let mut bad = 0u8;
        let mut v = [0u8; 1];
        for k in 1..=32u32 {
            // Parent wrote version k: verify, then write k+100 for it.
            if read(p2c[0], v.as_mut_ptr(), 1) != 1 { bad = 1; break; }
            let mut b = [0u8; 2 * PS];
            pread(fd, b.as_mut_ptr(), len, 0);
            if (0..len).any(|i| b[i] != pat(k, i)) { bad = 2; }
            fill(&mut b, k + 100, 0);
            pwrite(fd, b.as_ptr(), len, 0);
            write(c2p[1], v.as_ptr(), 1);
        }
        close(fd);
        exit(bad as i32);
    }
    let fd = open(path.as_ptr(), O_RDWR, 0);
    let mut ok = true;
    let mut v = [0u8; 1];
    for k in 1..=32u32 {
        fill(&mut page, k, 0);
        pwrite(fd, page.as_ptr(), len, 0);
        write(p2c[1], v.as_ptr(), 1);
        if read(c2p[0], v.as_mut_ptr(), 1) != 1 { ok = false; break; }
        let mut b = [0u8; 2 * PS];
        // Through the descriptor's offset this time (read(2)).
        lseek(fd, 0, SEEK_SET);
        read(fd, b.as_mut_ptr(), len);
        if (0..len).any(|i| b[i] != pat(k + 100, i)) { ok = false; }
    }
    let mut st = 0i32;
    wait4(pid, &mut st, 0, core::ptr::null_mut());
    close(fd);
    unlink(path.as_ptr());
    for f in [p2c[0], p2c[1], c2p[0], c2p[1]] { close(f); }
    if !ok { return why(name, b"parent saw a stale version", 0); }
    if st != 0 { return why(name, b"child status", st as u64); }
    report(name, true)
}

// ── append ────────────────────────────────────────────────────────────────────
unsafe fn t_append() -> bool {
    let name = b"append";
    let path = b"/root/pc_ap.bin\0";
    unlink(path.as_ptr());
    let w = open(path.as_ptr(), O_CREAT | O_WRONLY | O_APPEND | O_TRUNC, 0o644);
    let r = open(path.as_ptr(), O_RDONLY, 0);
    if w < 0 || r < 0 { return why(name, b"open", 0); }
    let m = buf();
    let mut len = 0usize;
    for k in 0..24usize {
        let n = 700 + k * 97;
        let mut tmp = [0u8; 4096];
        fill(&mut tmp[..n], 50, len);
        // The descriptor's offset is irrelevant to O_APPEND.
        lseek(w, 0, SEEK_SET);
        if write(w, tmp.as_ptr(), n) != n as isize { close(w); close(r); return why(name, b"write", k as u64); }
        m[len..len + n].copy_from_slice(&tmp[..n]);
        len += n;
        if let Some(i) = check_file(r, len, m) { close(w); close(r); return why(name, b"reader mismatch at", i as u64); }
        if lseek(r, 0, SEEK_END) as i64 != len as i64 { close(w); close(r); return why(name, b"SEEK_END", k as u64); }
    }
    close(w); close(r);
    unlink(path.as_ptr());
    report(name, true)
}

// ── big_evict ─────────────────────────────────────────────────────────────────
/// `/proc/pcache` value of `key`.
unsafe fn pcache_val(key: &[u8]) -> Option<u64> {
    let fd = open(b"/proc/pcache\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 { return None; }
    let mut b = [0u8; 2048];
    let n = slurp(fd, &mut b);
    close(fd);
    let t = &b[..n];
    let mut i = 0;
    while i < t.len() {
        let e = t[i..].iter().position(|&c| c == b'\n').map_or(t.len(), |p| i + p);
        let line = &t[i..e];
        if line.len() > key.len() && &line[..key.len()] == key && line[key.len()] == b' ' {
            let mut v = 0u64;
            for &c in &line[key.len() + 1..] { if c.is_ascii_digit() { v = v * 10 + (c - b'0') as u64; } }
            return Some(v);
        }
        i = e + 1;
    }
    None
}

unsafe fn file_sum(path: &[u8]) -> Option<(u64, u64)> {
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    if fd < 0 { return None; }
    let b = buf2();
    let mut sum = 0xcbf29ce484222325u64;
    let mut total = 0u64;
    loop {
        let n = read(fd, b.as_mut_ptr(), b.len());
        if n <= 0 { break; }
        for &c in &b[..n as usize] { sum = (sum ^ c as u64).wrapping_mul(0x100000001b3); }
        total += n as u64;
    }
    close(fd);
    Some((sum, total))
}

unsafe fn t_big_evict() -> bool {
    let name = b"big_evict";
    let (Some(cap), Some(ev0)) = (pcache_val(b"cap_pages"), pcache_val(b"evicted")) else {
        return why(name, b"no /proc/pcache", 0);
    };
    let em0 = pcache_val(b"emergency_evicted").unwrap_or(0);
    let cap_bytes = cap * PS as u64;
    // A file bigger than the cap when it is small (`pcache.max_mb=`);
    // otherwise big existing files under memory pressure from this process.
    let files: [&[u8]; 6] = [b"/usr/lib/firefox/libxul.so\0", b"/bin/cosmic-settings\0",
        b"/bin/cosmic-files-applet\0", b"/bin/cosmic-term\0", b"/bin/cosmic-greeter-login\0",
        b"/bin/cosmic-launcher\0"];
    let mut sums = [(0u64, 0u64); 6];
    let mut total = 0u64;
    for (i, f) in files.iter().enumerate() {
        match file_sum(f) { Some(s) => { sums[i] = s; total += s.1; } None => {} }
    }
    let mut pressure: *mut u8 = core::ptr::null_mut();
    let mut plen = 0usize;
    if total <= cap_bytes {
        // Make the cache give pages back: take most of what is free plus
        // what it could give back, touch it, and keep it while re-reading.
        let free = pcache_val(b"free_pages").unwrap_or(0) * PS as u64;
        let cached = pcache_val(b"cached_pages").unwrap_or(0) * PS as u64;
        let low = pcache_val(b"low_watermark_pages").unwrap_or(0) * PS as u64;
        let want = (free + cached / 2).saturating_sub(low + (64 << 20)) as usize & !(PS - 1);
        if want > 0 {
            pressure = mmap(core::ptr::null_mut(), want, PROT_READ | PROT_WRITE, 0x22, -1, 0);
            if pressure as isize != -1 {
                plen = want;
                let mut i = 0; while i < want { *pressure.add(i) = 1; i += PS; }
            } else { pressure = core::ptr::null_mut(); }
        }
    }
    let mut ok = true;
    for (i, f) in files.iter().enumerate() {
        if sums[i].1 == 0 { continue; }
        match file_sum(f) {
            Some(s) if s == sums[i] => {}
            _ => { ok = false; why(name, b"checksum changed for file", i as u64); }
        }
    }
    if !pressure.is_null() { munmap(pressure, plen); }
    let ev = pcache_val(b"evicted").unwrap_or(0) + pcache_val(b"emergency_evicted").unwrap_or(0);
    say(name); say(b": read "); say_num(total >> 20); say(b" MiB twice, cap "); say_num(cap_bytes >> 20);
    say(b" MiB, pressure "); say_num((plen >> 20) as u64); say(b" MiB, evicted "); say_num(ev - ev0 - em0);
    say(b" pages\n");
    if ev <= ev0 + em0 { ok = false; why(name, b"nothing was evicted", 0); }
    report(name, ok)
}

// ── fsync_durable ─────────────────────────────────────────────────────────────
unsafe fn t_fsync_durable() -> bool {
    let name = b"fsync_durable";
    if !shared_maps_cached(name) { return true; }
    let path = b"/root/pc_dur.bin\0";
    let fd = open(path.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return why(name, b"open", 0); }
    let len = 4 * PS;
    let m = buf();
    fill(&mut m[..len], 60, 0);
    write(fd, m.as_ptr(), len);
    if fsync(fd) != 0 { close(fd); return why(name, b"fsync", 0); }
    let map = mmap(core::ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if map as isize == -1 { close(fd); return why(name, b"mmap", 0); }
    for i in 0..PS { let b = pat(61, PS + i); *map.add(PS + i) = b; m[PS + i] = b; }
    if msync(map, len, MS_SYNC) != 0 { close(fd); return why(name, b"msync", 0); }
    munmap(map, len);
    let r = check_file(fd, len, m);
    close(fd);
    // The file stays for the host's reboot check (pcachetest verify).
    if let Some(i) = r { return why(name, b"mismatch at", i as u64); }
    report(name, true)
}

/// `pcachetest verify`: after a hard reset, the fsync'd/msync'd file of
/// `fsync_durable` must hold exactly what was committed.
unsafe fn verify_durable() -> i32 {
    let path = b"/root/pc_dur.bin\0";
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    if fd < 0 { say(b"verify: no file\n"); return 1; }
    let len = 4 * PS;
    let m = buf();
    fill(&mut m[..len], 60, 0);
    for i in 0..PS { m[PS + i] = pat(61, PS + i); }
    let r = check_file(fd, len, m);
    close(fd);
    match r {
        None => { say(b"verify_durable: PASS\n"); 0 }
        Some(i) => { why(b"verify_durable", b"mismatch at", i as u64); 1 }
    }
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc > 1 {
        let a = *argv.add(1);
        let mut n = 0; while *a.add(n) != 0 { n += 1; }
        let arg = core::slice::from_raw_parts(a, n);
        if arg == b"verify" { return verify_durable(); }
        if arg == b"quick" {
            let mut f = 0;
            for t in [t_write_read, t_mmap_coherent, t_mmap_fork, t_truncate, t_unlink_open,
                      t_two_procs, t_append, t_fsync_durable] {
                if !t() { f += 1; }
            }
            say(b"--- pcachetest done ---\n");
            return f;
        }
    }
    let mut failures = 0;
    for t in [t_write_read, t_mmap_coherent, t_mmap_fork, t_truncate, t_unlink_open,
              t_two_procs, t_append, t_fsync_durable, t_big_evict] {
        if !t() { failures += 1; }
    }
    say(b"--- pcachetest done ---\n");
    failures
}
