//! utmpx / wtmp / lastlog records, shared by every login path.
//!
//! Self-contained (`core` only, raw Linux syscalls) so the same file is
//! `#[path]`-included by no_std crates (init, login, utmpd) and std crates
//! (sshd, pam-leandros). The record layout is the one Alpine's `utmps` (and
//! therefore Alpine's busybox `who`/`last`) uses: `struct utmpx` is 400 bytes.
//!
//! Files:
//!   /run/utmps/utmp    current logins (truncated at boot by init), served by `utmpd`
//!   /var/log/wtmp      append-only history, read directly by `last`
//!   /var/log/lastlog   `struct lastlog` (292 bytes) at uid*292
#![allow(dead_code)]

pub const EMPTY: i16 = 0;
pub const RUN_LVL: i16 = 1;
pub const BOOT_TIME: i16 = 2;
pub const INIT_PROCESS: i16 = 5;
pub const LOGIN_PROCESS: i16 = 6;
pub const USER_PROCESS: i16 = 7;
pub const DEAD_PROCESS: i16 = 8;

pub const UTMP_DIR: &[u8] = b"/run/utmps\0";
pub const UTMP_PATH: &[u8] = b"/run/utmps/utmp\0";
pub const WTMP_PATH: &[u8] = b"/var/log/wtmp\0";
pub const LASTLOG_PATH: &[u8] = b"/var/log/lastlog\0";
pub const UTMP_SOCK: &[u8] = b"/run/utmps/.utmpd-socket\0";
pub const REC: usize = 400;
pub const LL_REC: usize = 292;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Utmpx {
    pub ut_type: i16,
    pub ut_pid: i32,
    pub ut_line: [u8; 32],
    pub ut_id: [u8; 4],
    pub ut_user: [u8; 32],
    pub ut_host: [u8; 256],
    pub e_termination: i16,
    pub e_exit: i16,
    pub ut_session: i32,
    pub tv_sec: i64,
    pub tv_usec: i64,
    pub ut_addr_v6: [u32; 4],
    pub dummy: [u8; 20],
}
const _: () = assert!(core::mem::size_of::<Utmpx>() == REC);

impl Utmpx {
    pub fn zeroed() -> Utmpx { unsafe { core::mem::zeroed() } }
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { core::slice::from_raw_parts(self as *const Utmpx as *const u8, REC) }
    }
    pub fn from_bytes(b: &[u8; REC]) -> Utmpx {
        unsafe { core::ptr::read_unaligned(b.as_ptr() as *const Utmpx) }
    }
}

fn put(dst: &mut [u8], s: &[u8]) {
    let n = s.len().min(dst.len());
    dst[..n].copy_from_slice(&s[..n]);
}

fn cstr_eq(a: &[u8], b: &[u8]) -> bool {
    let la = a.iter().position(|&c| c == 0).unwrap_or(a.len());
    let lb = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    a[..la] == b[..lb]
}

// ── raw syscalls ────────────────────────────────────────────────────────────
#[cfg(target_arch = "aarch64")]
mod nr {
    pub const OPENAT: usize = 56; pub const CLOSE: usize = 57; pub const LSEEK: usize = 62;
    pub const READ: usize = 63; pub const WRITE: usize = 64; pub const MKDIRAT: usize = 34;
    pub const FLOCK: usize = 32; pub const CLOCK_GETTIME: usize = 113; pub const UNAME: usize = 160;
    pub const FTRUNCATE: usize = 46;
}
#[cfg(target_arch = "x86_64")]
mod nr {
    pub const OPENAT: usize = 257; pub const CLOSE: usize = 3; pub const LSEEK: usize = 8;
    pub const READ: usize = 0; pub const WRITE: usize = 1; pub const MKDIRAT: usize = 258;
    pub const FLOCK: usize = 73; pub const CLOCK_GETTIME: usize = 228; pub const UNAME: usize = 63;
    pub const FTRUNCATE: usize = 77;
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
pub unsafe fn sys(n: usize, a: [usize; 6]) -> isize {
    let r: isize;
    core::arch::asm!("svc #0", in("x8") n, inlateout("x0") a[0] => r, in("x1") a[1], in("x2") a[2],
        in("x3") a[3], in("x4") a[4], in("x5") a[5], options(nostack));
    r
}
#[cfg(target_arch = "x86_64")]
#[inline(always)]
pub unsafe fn sys(n: usize, a: [usize; 6]) -> isize {
    let r: isize;
    core::arch::asm!("syscall", inlateout("rax") n as isize => r, in("rdi") a[0], in("rsi") a[1],
        in("rdx") a[2], in("r10") a[3], in("r8") a[4], in("r9") a[5],
        lateout("rcx") _, lateout("r11") _, options(nostack));
    r
}

const AT_FDCWD: usize = -100isize as usize;
const O_RDONLY: usize = 0;
const O_RDWR: usize = 2;
const O_WRONLY: usize = 1;
const O_CREAT: usize = 0o100;
const O_TRUNC: usize = 0o1000;
const O_APPEND: usize = 0o2000;

pub unsafe fn open(path: &[u8], flags: usize, mode: usize) -> isize {
    sys(nr::OPENAT, [AT_FDCWD, path.as_ptr() as usize, flags | 0o2000000, mode, 0, 0])
}
pub unsafe fn close(fd: isize) { sys(nr::CLOSE, [fd as usize, 0, 0, 0, 0, 0]); }
pub unsafe fn read(fd: isize, b: &mut [u8]) -> isize { sys(nr::READ, [fd as usize, b.as_mut_ptr() as usize, b.len(), 0, 0, 0]) }
pub unsafe fn write(fd: isize, b: &[u8]) -> isize { sys(nr::WRITE, [fd as usize, b.as_ptr() as usize, b.len(), 0, 0, 0]) }
pub unsafe fn lseek(fd: isize, off: i64, wh: usize) -> isize { sys(nr::LSEEK, [fd as usize, off as usize, wh, 0, 0, 0]) }
unsafe fn flock(fd: isize, op: usize) { sys(nr::FLOCK, [fd as usize, op, 0, 0, 0, 0]); }

fn read_full(fd: isize, b: &mut [u8]) -> usize {
    let mut got = 0;
    while got < b.len() {
        let r = unsafe { read(fd, &mut b[got..]) };
        if r <= 0 { break; }
        got += r as usize;
    }
    got
}

pub fn now() -> (i64, i64) {
    let mut ts = [0i64; 2];
    unsafe { sys(nr::CLOCK_GETTIME, [0, ts.as_mut_ptr() as usize, 0, 0, 0, 0]); }
    (ts[0], ts[1] / 1000)
}
/// Wall clock (CLOCK_REALTIME = 0).
fn kernel_release(out: &mut [u8; 256]) {
    let mut u = [0u8; 65 * 6];
    unsafe { sys(nr::UNAME, [u.as_mut_ptr() as usize, 0, 0, 0, 0, 0]); }
    put(out, &u[65 * 2..65 * 3]);
}

fn ensure_dirs() {
    unsafe {
        sys(nr::MKDIRAT, [AT_FDCWD, UTMP_DIR.as_ptr() as usize, 0o755, 0, 0, 0]);
        sys(nr::MKDIRAT, [AT_FDCWD, b"/var\0".as_ptr() as usize, 0o755, 0, 0, 0]);
        sys(nr::MKDIRAT, [AT_FDCWD, b"/var/log\0".as_ptr() as usize, 0o755, 0, 0, 0]);
    }
}

/// `ut_id` is the last four characters of the line, as login(1) does.
pub fn line_id(line: &[u8]) -> [u8; 4] {
    let n = line.iter().position(|&c| c == 0).unwrap_or(line.len());
    let s = &line[..n];
    let mut id = [0u8; 4];
    let t = if s.len() > 4 { &s[s.len() - 4..] } else { s };
    id[..t.len()].copy_from_slice(t);
    id
}

pub fn make(ty: i16, pid: i32, line: &[u8], user: &[u8], host: &[u8]) -> Utmpx {
    let mut r = Utmpx::zeroed();
    r.ut_type = ty;
    r.ut_pid = pid;
    put(&mut r.ut_line, line);
    r.ut_id = line_id(line);
    put(&mut r.ut_user, user);
    put(&mut r.ut_host, host);
    let (s, us) = now();
    r.tv_sec = s;
    r.tv_usec = us;
    r
}

fn is_proc(t: i16) -> bool { matches!(t, INIT_PROCESS | LOGIN_PROCESS | USER_PROCESS | DEAD_PROCESS) }

/// pututxline semantics: replace the proc entry with the same id and line,
/// else append. Boot/runlevel records are appended.
pub fn utmp_put(rec: &Utmpx) {
    ensure_dirs();
    unsafe {
        let fd = open(UTMP_PATH, O_RDWR | O_CREAT, 0o644);
        if fd < 0 { return; }
        flock(fd, 2);
        let mut slot: i64 = -1;
        if is_proc(rec.ut_type) {
            let mut idx: i64 = 0;
            let mut b = [0u8; REC];
            while read_full(fd, &mut b) == REC {
                let o = Utmpx::from_bytes(&b);
                if is_proc(o.ut_type) && o.ut_id == rec.ut_id && cstr_eq(&o.ut_line, &rec.ut_line) {
                    slot = idx;
                    break;
                }
                idx += 1;
            }
        }
        if slot >= 0 { lseek(fd, slot * REC as i64, 0); } else { lseek(fd, 0, 2); }
        write(fd, rec.as_bytes());
        flock(fd, 8);
        close(fd);
    }
}

pub fn wtmp_append(rec: &Utmpx) {
    ensure_dirs();
    unsafe {
        let fd = open(WTMP_PATH, O_WRONLY | O_CREAT | O_APPEND, 0o664);
        if fd < 0 { return; }
        write(fd, rec.as_bytes());
        close(fd);
    }
}

pub fn lastlog_set(uid: u32, line: &[u8], host: &[u8]) {
    ensure_dirs();
    unsafe {
        let fd = open(LASTLOG_PATH, O_RDWR | O_CREAT, 0o644);
        if fd < 0 { return; }
        let mut b = [0u8; LL_REC];
        let (s, _) = now();
        b[..4].copy_from_slice(&(s as i32).to_le_bytes());
        put(&mut b[4..36], line);
        put(&mut b[36..], host);
        lseek(fd, uid as i64 * LL_REC as i64, 0);
        write(fd, &b);
        close(fd);
    }
}

/// A user logged in on `line` ("tty1", "ttyS0", "pts/0", "ssh"…) with `pid`
/// the session leader (the process whose exit ends the session).
pub fn login(pid: i32, line: &[u8], user: &[u8], host: &[u8], uid: u32) {
    let r = make(USER_PROCESS, pid, line, user, host);
    utmp_put(&r);
    wtmp_append(&r);
    lastlog_set(uid, line, host);
}

/// End the session led by `pid`, if any: the utmp entry becomes DEAD_PROCESS
/// and a DEAD_PROCESS record goes to wtmp. Cheap enough to call for every
/// reaped child; returns true if a session was found.
pub fn logout_pid(pid: i32) -> bool {
    if pid <= 0 { return false; }
    unsafe {
        let fd = open(UTMP_PATH, O_RDWR, 0);
        if fd < 0 { return false; }
        flock(fd, 2);
        let mut idx: i64 = 0;
        let mut b = [0u8; REC];
        let mut found = false;
        while read_full(fd, &mut b) == REC {
            let o = Utmpx::from_bytes(&b);
            if o.ut_type == USER_PROCESS && o.ut_pid == pid {
                let mut d = make(DEAD_PROCESS, pid, &o.ut_line, b"", b"");
                d.ut_id = o.ut_id;
                lseek(fd, idx * REC as i64, 0);
                write(fd, d.as_bytes());
                wtmp_append(&d);
                found = true;
                break;
            }
            idx += 1;
        }
        flock(fd, 8);
        close(fd);
        found
    }
}

/// Boot: empty utmp; BOOT_TIME + RUN_LVL into utmp and wtmp.
pub fn boot() {
    ensure_dirs();
    unsafe {
        let fd = open(UTMP_PATH, O_RDWR | O_CREAT | O_TRUNC, 0o644);
        if fd >= 0 { close(fd); }
    }
    let mut b = make(BOOT_TIME, 0, b"~", b"reboot", b"");
    kernel_release(&mut b.ut_host);
    let mut rl = make(RUN_LVL, (b'N' as i32) << 8 | b'3' as i32, b"~", b"runlevel", b"");
    kernel_release(&mut rl.ut_host);
    b.ut_id = *b"~~\0\0";
    rl.ut_id = *b"~~\0\0";
    utmp_put(&b);
    utmp_put(&rl);
    wtmp_append(&b);
    wtmp_append(&rl);
}

/// Name of the terminal on `fd` without "/dev/" ("tty1", "ttyS0", "pts/0"),
/// via /proc/self/fd. Returns the length written into `out`, 0 if unknown.
pub fn tty_line(fd: u32, out: &mut [u8; 32]) -> usize {
    #[cfg(target_arch = "aarch64")]
    const READLINKAT: usize = 78;
    #[cfg(target_arch = "x86_64")]
    const READLINKAT: usize = 267;
    let mut path = *b"/proc/self/fd/\0\0\0\0\0\0\0\0";
    let mut n = 14;
    let mut digits = [0u8; 8];
    let mut d = 0;
    let mut v = fd;
    loop { digits[d] = b'0' + (v % 10) as u8; d += 1; v /= 10; if v == 0 { break; } }
    while d > 0 { d -= 1; path[n] = digits[d]; n += 1; }
    path[n] = 0;
    let mut buf = [0u8; 64];
    let r = unsafe { sys(READLINKAT, [AT_FDCWD, path.as_ptr() as usize, buf.as_mut_ptr() as usize, 63, 0, 0]) };
    if r <= 0 { return 0; }
    let s = &buf[..r as usize];
    let s = s.strip_prefix(b"/dev/").unwrap_or(s);
    let m = s.len().min(31);
    out[..m].copy_from_slice(&s[..m]);
    m
}

pub fn getpid() -> i32 {
    #[cfg(target_arch = "aarch64")]
    const GETPID: usize = 172;
    #[cfg(target_arch = "x86_64")]
    const GETPID: usize = 39;
    unsafe { sys(GETPID, [0; 6]) as i32 }
}
