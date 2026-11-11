//! fusetest -- FUSE end to end, raw protocol (no libfuse).
//!
//! A forked daemon serves a tiny in-memory filesystem ("hello" plus whatever
//! gets created) over /dev/fuse; the parent mounts it on /tmp/fusemnt and
//! drives ls / cat / write / create / stat / unlink through it, then kills the
//! daemon and expects ENOTCONN. Prints "<name>: PASS|FAIL".
#![no_std]
#![no_main]

use leandros_libc::*;
use leandros_libc::syscall::{syscall2, syscall4};

#[cfg(target_arch = "aarch64")] const SYS_NEWFSTATAT: usize = 79;
#[cfg(target_arch = "x86_64")]  const SYS_NEWFSTATAT: usize = 262;
#[cfg(target_arch = "aarch64")] const SYS_KILL: usize = 129;
#[cfg(target_arch = "x86_64")]  const SYS_KILL: usize = 62;
#[cfg(target_arch = "aarch64")] const MODE_OFF: usize = 16;
#[cfg(target_arch = "x86_64")]  const MODE_OFF: usize = 24;
const AT_FDCWD: isize = -100;
const MNT: &[u8] = b"/tmp/fusemnt\0";

fn say(s: &[u8]) { unsafe { write(1, s.as_ptr(), s.len()); } }
fn verdict(name: &[u8], ok: bool) {
    say(name); say(if ok { b": PASS\n" } else { b": FAIL\n" });
}
fn num(mut n: i64) {
    let mut b = [0u8; 24]; let mut i = 24; let neg = n < 0; if neg { n = -n; }
    loop { i -= 1; b[i] = b'0' + (n % 10) as u8; n /= 10; if n == 0 { break; } }
    if neg { i -= 1; b[i] = b'-'; }
    say(&b[i..]);
}

// ── the daemon ────────────────────────────────────────────────────────────────

const NF: usize = 8;
struct F { used: bool, name: [u8; 32], nlen: usize, data: [u8; 512], len: usize }
static mut FILES: [F; NF] = [const { F { used: false, name: [0; 32], nlen: 0, data: [0; 512], len: 0 } }; NF];

fn u32at(b: &[u8], o: usize) -> u32 { u32::from_le_bytes([b[o], b[o+1], b[o+2], b[o+3]]) }
fn u64at(b: &[u8], o: usize) -> u64 { let mut a = [0u8; 8]; a.copy_from_slice(&b[o..o+8]); u64::from_le_bytes(a) }
fn p32(b: &mut [u8], o: usize, v: u32) { b[o..o+4].copy_from_slice(&v.to_le_bytes()); }
fn p64(b: &mut [u8], o: usize, v: u64) { b[o..o+8].copy_from_slice(&v.to_le_bytes()); }

fn attr(b: &mut [u8], node: u64) {
    // fuse_attr at b[0..88]
    unsafe {
        p64(b, 0, node);
        if node == 1 { p32(b, 60, 0o040755); p32(b, 64, 2); }
        else {
            let f = &FILES[node as usize - 2];
            p64(b, 8, f.len as u64); p64(b, 16, 1); p32(b, 60, 0o100644); p32(b, 64, 1);
        }
        p32(b, 80, 4096);
    }
}

fn find(name: &[u8]) -> Option<usize> {
    unsafe { (0..NF).find(|&i| FILES[i].used && &FILES[i].name[..FILES[i].nlen] == name) }
}

unsafe fn daemon(fd: i32) -> ! {
    static mut REQ: [u8; 8192] = [0; 8192];
    static mut OUT: [u8; 8192] = [0; 8192];
    // "hello" lives at slot 0.
    let h = &mut FILES[0];
    h.used = true; h.name[..5].copy_from_slice(b"hello"); h.nlen = 5;
    let msg = b"Hello, FUSE!\n"; h.data[..msg.len()].copy_from_slice(msg); h.len = msg.len();
    loop {
        let n = read(fd, REQ.as_mut_ptr(), REQ.len());
        if n < 40 { exit(0); }
        let rq = &REQ[..n as usize];
        let op = u32at(rq, 4); let uniq = u64at(rq, 8); let node = u64at(rq, 16);
        let body = &rq[40..];
        let out = &mut OUT;
        let mut err: i32 = 0; let mut blen = 0usize;
        match op {
            26 => { // INIT: major, minor, max_readahead, flags, ..., max_write at +20
                p32(&mut out[16..], 0, 7); p32(&mut out[16..], 4, 31); p32(&mut out[16..], 8, 65536);
                p32(&mut out[16..], 20, 65536); blen = 64;
            }
            2 => continue, // FORGET
            1 => { // LOOKUP
                let nm = &body[..body.len() - 1];
                if node != 1 { err = -2; }
                else if let Some(i) = find(nm) { p64(&mut out[16..], 0, i as u64 + 2); attr(&mut out[16 + 40..], i as u64 + 2); blen = 128; }
                else { err = -2; }
            }
            3 => { p64(&mut out[16..], 0, 3600); attr(&mut out[16 + 16..], node); blen = 104; } // GETATTR
            4 => { // SETATTR: size
                if node >= 2 { let valid = u32at(body, 0); if valid & 8 != 0 { let sz = u64at(body, 16) as usize; FILES[node as usize - 2].len = sz.min(512); } }
                attr(&mut out[16 + 16..], node); blen = 104;
            }
            14 | 27 => { p64(&mut out[16..], 0, node); blen = 16; } // OPEN / OPENDIR
            15 => { // READ
                let off = u64at(body, 8) as usize; let sz = u32at(body, 16) as usize;
                let f = &FILES[node as usize - 2];
                if off < f.len { let l = (f.len - off).min(sz); out[16..16 + l].copy_from_slice(&f.data[off..off + l]); blen = l; }
            }
            16 => { // WRITE
                let off = u64at(body, 8) as usize; let sz = u32at(body, 16) as usize;
                let f = &mut FILES[node as usize - 2];
                if off + sz > 512 { err = -27; } else {
                    f.data[off..off + sz].copy_from_slice(&body[40..40 + sz]);
                    if off + sz > f.len { f.len = off + sz; }
                    p32(&mut out[16..], 0, sz as u32); blen = 8;
                }
            }
            28 => { // READDIR from offset
                let off = u64at(body, 8) as usize; let mut o = 0usize; let mut idx = 0usize;
                let names: [&[u8]; 2] = [b".", b".."];
                for nm in names.iter() {
                    idx += 1;
                    if idx <= off { continue; }
                    p64(&mut out[16..], o, 1); p64(&mut out[16..], o + 8, idx as u64); p32(&mut out[16..], o + 16, nm.len() as u32); p32(&mut out[16..], o + 20, 4);
                    out[16 + o + 24..16 + o + 24 + nm.len()].copy_from_slice(nm); o += (24 + nm.len() + 7) & !7;
                }
                for i in 0..NF {
                    if !FILES[i].used { continue; }
                    idx += 1;
                    if idx <= off { continue; }
                    let nl = FILES[i].nlen;
                    p64(&mut out[16..], o, i as u64 + 2); p64(&mut out[16..], o + 8, idx as u64); p32(&mut out[16..], o + 16, nl as u32); p32(&mut out[16..], o + 20, 8);
                    out[16 + o + 24..16 + o + 24 + nl].copy_from_slice(&FILES[i].name[..nl]); o += (24 + nl + 7) & !7;
                }
                blen = o;
            }
            18 | 29 | 25 | 38 => {} // RELEASE / RELEASEDIR / FLUSH / DESTROY
            35 => { // CREATE: create_in 16 bytes then name
                let nm = &body[16..body.len() - 1];
                match (0..NF).find(|&i| !FILES[i].used) {
                    Some(i) if nm.len() < 32 => {
                        let f = &mut FILES[i]; f.used = true; f.nlen = nm.len(); f.name[..nm.len()].copy_from_slice(nm); f.len = 0;
                        p64(&mut out[16..], 0, i as u64 + 2); attr(&mut out[16 + 40..], i as u64 + 2);
                        p64(&mut out[16..], 128, 0); blen = 144;
                    }
                    _ => err = -28,
                }
            }
            10 => { // UNLINK
                match find(&body[..body.len() - 1]) { Some(i) => FILES[i].used = false, None => err = -2 }
            }
            12 => { // RENAME: newdir u64, oldname\0, newname\0
                let names = &body[8..];
                let z = names.iter().position(|&c| c == 0).unwrap_or(0);
                let (on, nn) = (&names[..z], &names[z + 1..names.len() - 1]);
                match find(on) {
                    Some(i) if nn.len() < 32 => { FILES[i].nlen = nn.len(); FILES[i].name[..nn.len()].copy_from_slice(nn); }
                    _ => err = -2,
                }
            }
            17 => { p64(&mut out[16..], 0, 100); p64(&mut out[16..], 8, 50); p64(&mut out[16..], 16, 50); p32(&mut out[16..], 40, 4096); p32(&mut out[16..], 44, 255); blen = 80; }
            _ => err = -38,
        }
        p32(out, 0, (16 + blen) as u32); p32(out, 4, err as u32); p64(out, 8, uniq);
        write(fd, out.as_ptr(), 16 + blen);
    }
}

// ── the tests ─────────────────────────────────────────────────────────────────

unsafe fn stat_mode_size(path: &[u8]) -> Option<(u32, u64)> {
    let mut st = [0u8; 256];
    let r = syscall4(SYS_NEWFSTATAT, AT_FDCWD as usize, path.as_ptr() as usize, st.as_mut_ptr() as usize, 0);
    if r < 0 { return None; }
    Some((u32::from_le_bytes([st[MODE_OFF], st[MODE_OFF+1], st[MODE_OFF+2], st[MODE_OFF+3]]), u64at(&st, 48)))
}

unsafe fn slurp(path: &[u8], buf: &mut [u8]) -> isize {
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    if fd < 0 { return -(get_errno() as isize); }
    let mut tot = 0isize;
    loop {
        let n = read(fd, buf.as_mut_ptr().add(tot as usize), buf.len() - tot as usize);
        if n <= 0 { if n < 0 && tot == 0 { tot = -(get_errno() as isize); } break; }
        tot += n;
    }
    close(fd);
    tot
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let fd = open(b"/dev/fuse\0".as_ptr(), O_RDWR, 0);
    verdict(b"open_dev_fuse", fd >= 0);
    if fd < 0 { return 1; }
    mkdir(MNT.as_ptr(), 0o755);
    let pid = fork();
    if pid == 0 { daemon(fd); }
    let mut opts = *b"fd=000,rootmode=40000,user_id=0,group_id=0\0";
    opts[3] = b'0' + (fd / 100 % 10) as u8; opts[4] = b'0' + (fd / 10 % 10) as u8; opts[5] = b'0' + (fd % 10) as u8;
    let r = mount(b"hellofs\0".as_ptr(), MNT.as_ptr(), b"fuse.hellofs\0".as_ptr(), 2, opts.as_ptr());
    verdict(b"mount_fuse_subtype", r == 0);
    if r != 0 { say(b"  mount errno="); num(get_errno() as i64); say(b"\n"); kill_child(pid); return 1; }
    close(fd);

    // ls
    let d = open(MNT.as_ptr(), O_RDONLY, 0);
    let mut db = [0u8; 1024];
    let mut seen_hello = false;
    let mut n = 0isize;
    loop {
        let k = if d >= 0 { getdents64(d, db.as_mut_ptr(), db.len()) } else { -1 };
        if k <= 0 { if n == 0 { n = k; } break; }
        n += k;
        let mut o = 0usize;
        while o < k as usize {
            let rl = u16::from_le_bytes([db[o + 16], db[o + 17]]) as usize;
            if &db[o + 19..o + 24] == b"hello" && db[o + 24] == 0 { seen_hello = true; }
            o += rl;
        }
    }
    if !seen_hello { say(b"  ls d="); num(d as i64); say(b" n="); num(n as i64); say(b" errno="); num(get_errno() as i64); say(b"\n"); }
    verdict(b"ls_lists_hello", seen_hello);
    if d >= 0 { close(d); }

    // cat
    let mut buf = [0u8; 512];
    let n = slurp(b"/tmp/fusemnt/hello\0", &mut buf);
    if n != 13 { say(b"  cat n="); num(n as i64); say(b"\n"); }
    verdict(b"cat_hello", n == 13 && &buf[..13] == b"Hello, FUSE!\n");

    // stat
    let st = stat_mode_size(b"/tmp/fusemnt/hello\0");
    verdict(b"stat_hello", st == Some((0o100644, 13)));
    let st = stat_mode_size(b"/tmp/fusemnt\0");
    verdict(b"stat_root_is_dir", matches!(st, Some((m, _)) if m & 0o170000 == 0o040000));
    verdict(b"stat_missing_enoent", stat_mode_size(b"/tmp/fusemnt/nope\0").is_none());

    // write (truncate + write), read back
    let w = open(b"/tmp/fusemnt/hello\0".as_ptr(), O_WRONLY | O_TRUNC, 0);
    let wn = if w >= 0 { write(w, b"rewritten".as_ptr(), 9) } else { -1 };
    if w >= 0 { close(w); }
    let n = slurp(b"/tmp/fusemnt/hello\0", &mut buf);
    if !(n == 9) { say(b"  wtc w="); num(w as i64); say(b" wn="); num(wn as i64); say(b" n="); num(n as i64); say(b"\n"); }
    verdict(b"write_then_cat", wn == 9 && n == 9 && &buf[..9] == b"rewritten");

    // create + write + read back
    let c = open(b"/tmp/fusemnt/new.txt\0".as_ptr(), O_WRONLY | O_CREAT, 0o644);
    let cn = if c >= 0 { write(c, b"created!".as_ptr(), 8) } else { -1 };
    if c >= 0 { close(c); }
    let n = slurp(b"/tmp/fusemnt/new.txt\0", &mut buf);
    verdict(b"create_write_read", cn == 8 && n == 8 && &buf[..8] == b"created!");

    // unlink
    let u = unlink(b"/tmp/fusemnt/new.txt\0".as_ptr());
    verdict(b"unlink", u == 0 && stat_mode_size(b"/tmp/fusemnt/new.txt\0").is_none());

    // An open file's /proc/self/fd/N link follows rename and unlink.
    let c = open(b"/tmp/fusemnt/mv.txt\0".as_ptr(), O_WRONLY | O_CREAT, 0o644);
    let mut lb = [0u8; 128];
    let l1 = fdlink(c, &mut lb);
    verdict(b"fd_link_opened_path", l1 == b"/tmp/fusemnt/mv.txt");
    let rr = rename(b"/tmp/fusemnt/mv.txt\0".as_ptr(), b"/tmp/fusemnt/mv2.txt\0".as_ptr());
    let l2 = fdlink(c, &mut lb);
    verdict(b"fd_link_follows_rename", rr == 0 && l2 == b"/tmp/fusemnt/mv2.txt");
    unlink(b"/tmp/fusemnt/mv2.txt\0".as_ptr());
    let l3 = fdlink(c, &mut lb);
    verdict(b"fd_link_deleted_suffix", l3 == b"/tmp/fusemnt/mv2.txt (deleted)");
    if c >= 0 { close(c); }

    // statfs-free sanity: mount shows up
    let mut listed = false;
    let cnt = devinfo::mounts_count().max(0) as usize;
    for i in 0..cnt { if let Some(m) = devinfo::mounts_info(i) { if m.mountpoint() == b"/tmp/fusemnt" { listed = true; } } }
    verdict(b"mount_listed", listed);

    // daemon death -> ENOTCONN
    kill_child(pid);
    let n = slurp(b"/tmp/fusemnt/hello\0", &mut buf);
    say(b"  kill n="); num(n as i64); say(b"\n");
    verdict(b"daemon_kill_read_enotconn", n == -107);
    verdict(b"daemon_kill_stat_fails", stat_mode_size(b"/tmp/fusemnt/hello\0").is_none());

    let r = umount2(MNT.as_ptr(), 0);
    verdict(b"umount", r == 0);
    verdict(b"umount_gone", stat_mode_size(b"/tmp/fusemnt/hello\0").is_none());
    0
}

#[cfg(target_arch = "aarch64")] const SYS_READLINKAT: usize = 78;
#[cfg(target_arch = "x86_64")]  const SYS_READLINKAT: usize = 267;
/// readlink("/proc/self/fd/<fd>") into `out`; the link text (empty on error).
unsafe fn fdlink(fd: i32, out: &mut [u8]) -> &[u8] {
    let mut p = *b"/proc/self/fd/\0\0\0\0\0\0\0\0";
    let mut n = 14; let mut d = [0u8; 8]; let mut di = 0; let mut v = fd.max(0) as usize;
    if v == 0 { d[0] = b'0'; di = 1; }
    while v > 0 { d[di] = b'0' + (v % 10) as u8; di += 1; v /= 10; }
    while di > 0 { di -= 1; p[n] = d[di]; n += 1; }
    let r = syscall4(SYS_READLINKAT, AT_FDCWD as usize, p.as_ptr() as usize, out.as_mut_ptr() as usize, out.len());
    if r < 0 { &out[..0] } else { &out[..r as usize] }
}

unsafe fn kill_child(pid: i32) {
    syscall2(SYS_KILL, pid as usize, 9);
    let mut st = 0i32;
    wait4(pid, &mut st, 0, core::ptr::null_mut());
}
