//! permtest — filesystem permission enforcement beyond open(2).
//!
//! Until 2026-09 the only permission check in the tree was open(2) of an
//! inode that already existed: path traversal read no mode on any component,
//! creating or removing an entry checked only that the parent existed, and
//! AF_UNIX connect discarded the caller (TODO.md, "Filesystem permissions are
//! enforced on ONE operation only"). This test pins the POSIX rules the VFS
//! (tmpfs) and the f2fs server now share through `xattr::may_access`:
//!
//!   * traversal: search (x) on every directory component — EACCES, and never
//!     ENOENT, through a 0700 root directory, whether or not the leaf exists;
//!   * creation (O_CREAT, mkdir, symlink, link, mknod, AF_UNIX bind): write +
//!     search on the parent;
//!   * removal (unlink, rmdir, rename): write + search on the parent, plus the
//!     sticky-bit rule (only the entry's owner, the directory's owner or root
//!     may remove an entry from a 1777 directory — EPERM);
//!   * AF_UNIX connect: write on the socket inode;
//!   * chmod/chown: owner or root, uid changes root-only, gid only to the
//!     caller's own group — EPERM;
//!   * a stored POSIX ACL is honoured on the same checks (a named-user x entry
//!     opens a 0700 directory to that user);
//!   * root (euid 0) bypasses all of it.
//!
//! Runs as ROOT. Each negative case runs in a forked child that drops to
//! uid/gid 1000 (`leandro`) with setresgid + setresuid; the parent reads the
//! child's exit status as the verdict. The matrix runs against the f2fs data
//! mount (/data) and against tmpfs (/tmp); the socket cases only on tmpfs,
//! which is the only backend that hosts S_IFSOCK nodes.
//!
//! Output follows the vfstest convention — "<name>: PASS" / "<name>: FAIL" per
//! case, failure count as the exit code — so a harness can grep it. A failing
//! child also prints the step and errno that failed, indented, for diagnosis.
//!
//! Note: this kernel's `wait4()` reports a child's raw `exit()` argument as
//! `wstatus` (not the shifted Linux encoding), so `status == 0` is the check.

#![no_std]
#![no_main]

extern crate leandros_libc;
use leandros_libc::*;
use leandros_libc::syscall::{syscall2, syscall3, syscall4, syscall5};

// Raw syscalls for what leandros-libc does not wrap. Numbers match the kernel's
// own `mod nr` tables in kernel/src/syscall.rs (AArch64 first, x86-64 second).
#[cfg(target_arch = "aarch64")] const SYS_SYMLINKAT:  usize = 36;
#[cfg(target_arch = "x86_64")]  const SYS_SYMLINKAT:  usize = 266;
#[cfg(target_arch = "aarch64")] const SYS_LINKAT:     usize = 37;
#[cfg(target_arch = "x86_64")]  const SYS_LINKAT:     usize = 265;
#[cfg(target_arch = "aarch64")] const SYS_MKNODAT:    usize = 33;
#[cfg(target_arch = "x86_64")]  const SYS_MKNODAT:    usize = 259;
#[cfg(target_arch = "aarch64")] const SYS_NEWFSTATAT: usize = 79;
#[cfg(target_arch = "x86_64")]  const SYS_NEWFSTATAT: usize = 262;
#[cfg(target_arch = "aarch64")] const SYS_FACCESSAT:  usize = 48;
#[cfg(target_arch = "x86_64")]  const SYS_FACCESSAT:  usize = 269;
#[cfg(target_arch = "aarch64")] const SYS_UMASK:      usize = 166;
#[cfg(target_arch = "x86_64")]  const SYS_UMASK:      usize = 95;
#[cfg(target_arch = "aarch64")] const SYS_SETXATTR:   usize = 5;
#[cfg(target_arch = "x86_64")]  const SYS_SETXATTR:   usize = 188;
#[cfg(target_arch = "aarch64")] const SYS_SOCKET:     usize = 198;
#[cfg(target_arch = "x86_64")]  const SYS_SOCKET:     usize = 41;
#[cfg(target_arch = "aarch64")] const SYS_BIND:       usize = 200;
#[cfg(target_arch = "x86_64")]  const SYS_BIND:       usize = 49;
#[cfg(target_arch = "aarch64")] const SYS_LISTEN:     usize = 201;
#[cfg(target_arch = "x86_64")]  const SYS_LISTEN:     usize = 50;
#[cfg(target_arch = "aarch64")] const SYS_CONNECT:    usize = 203;
#[cfg(target_arch = "x86_64")]  const SYS_CONNECT:    usize = 42;

// `struct stat`: the 128-byte asm-generic layout on AArch64 (st_uid at 24),
// x86-64's native 144-byte layout (st_uid at 28) — see servers/vfs
// write_stat_full_rdev.
#[cfg(target_arch = "aarch64")] const STAT_SIZE: usize = 128;
#[cfg(target_arch = "x86_64")]  const STAT_SIZE: usize = 144;
#[cfg(target_arch = "aarch64")] const STAT_UID_OFF: usize = 24;
#[cfg(target_arch = "x86_64")]  const STAT_UID_OFF: usize = 28;

const F_OK: usize = 0;
const S_IFIFO: u32 = 0o010000;
const AF_UNIX: i32 = 1;
const SOCK_STREAM: i32 = 1;

const UID_USER: u32 = 1000;  // leandro
const UID_OTHER: u32 = 1001; // a second unprivileged owner, exists only as a number

fn xret(r: isize) -> isize {
    if r < 0 { set_errno(-r as i32); -1 } else { r }
}

unsafe fn raw_symlink(target: *const u8, linkpath: *const u8) -> isize {
    xret(syscall3(SYS_SYMLINKAT, target as usize, AT_FDCWD as usize, linkpath as usize))
}
unsafe fn raw_link(old: *const u8, new: *const u8) -> isize {
    xret(syscall5(SYS_LINKAT, AT_FDCWD as usize, old as usize, AT_FDCWD as usize, new as usize, 0))
}
unsafe fn raw_mknod(path: *const u8, mode: u32) -> isize {
    xret(syscall4(SYS_MKNODAT, AT_FDCWD as usize, path as usize, mode as usize, 0))
}
unsafe fn raw_stat(path: *const u8, buf: *mut u8) -> isize {
    xret(syscall4(SYS_NEWFSTATAT, AT_FDCWD as usize, path as usize, buf as usize, 0))
}
unsafe fn raw_access(path: *const u8, mode: usize) -> isize {
    xret(syscall4(SYS_FACCESSAT, AT_FDCWD as usize, path as usize, mode, 0))
}
unsafe fn raw_umask(mask: usize) -> isize {
    syscall2(SYS_UMASK, mask, 0)
}
unsafe fn raw_setxattr(path: *const u8, name: *const u8, value: *const u8, size: usize) -> isize {
    xret(syscall5(SYS_SETXATTR, path as usize, name as usize, value as usize, size, 0))
}
unsafe fn raw_socket() -> i32 {
    xret(syscall3(SYS_SOCKET, AF_UNIX as usize, SOCK_STREAM as usize, 0)) as i32
}

/// `sockaddr_un`: sun_family(2) + sun_path(108); addrlen = 2 + strlen + 1.
#[repr(C)]
struct SockaddrUn { sun_family: u16, sun_path: [u8; 108] }

unsafe fn sockaddr(path: *const u8) -> (SockaddrUn, usize) {
    let mut a = SockaddrUn { sun_family: AF_UNIX as u16, sun_path: [0u8; 108] };
    let mut n = 0usize;
    while n < 107 && *path.add(n) != 0 { a.sun_path[n] = *path.add(n); n += 1; }
    (a, 2 + n + 1)
}
unsafe fn raw_bind(fd: i32, path: *const u8) -> isize {
    let (a, len) = sockaddr(path);
    xret(syscall3(SYS_BIND, fd as usize, &a as *const SockaddrUn as usize, len))
}
unsafe fn raw_listen(fd: i32) -> isize {
    xret(syscall2(SYS_LISTEN, fd as usize, 8))
}
unsafe fn raw_connect(fd: i32, path: *const u8) -> isize {
    let (a, len) = sockaddr(path);
    xret(syscall3(SYS_CONNECT, fd as usize, &a as *const SockaddrUn as usize, len))
}

/// Owner uid reported by stat(), or u32::MAX when stat fails.
unsafe fn stat_uid(path: *const u8) -> u32 {
    let mut st = [0u8; STAT_SIZE];
    if raw_stat(path, st.as_mut_ptr()) != 0 { return u32::MAX; }
    u32::from_ne_bytes([st[STAT_UID_OFF], st[STAT_UID_OFF + 1], st[STAT_UID_OFF + 2], st[STAT_UID_OFF + 3]])
}

// ── paths ────────────────────────────────────────────────────────────────────

/// `root` + `suffix`, NUL-terminated, in `buf`.
fn mkpath<'a>(buf: &'a mut [u8; 128], root: &[u8], suffix: &[u8]) -> *const u8 {
    let n = root.len() + suffix.len();
    buf[..root.len()].copy_from_slice(root);
    buf[root.len()..n].copy_from_slice(suffix);
    buf[n] = 0;
    buf.as_ptr()
}

macro_rules! p {
    ($root:expr, $suffix:expr) => {{
        let mut b = [0u8; 128];
        mkpath(&mut b, $root, $suffix);
        b
    }};
}

// ── reporting ────────────────────────────────────────────────────────────────

unsafe fn out(s: &[u8]) { write(STDOUT_FILENO, s.as_ptr(), s.len()); }

unsafe fn out_dec(mut v: u32) {
    let mut buf = [0u8; 10];
    let mut n = 0;
    if v == 0 { out(b"0"); return; }
    while v > 0 { buf[n] = b'0' + (v % 10) as u8; v /= 10; n += 1; }
    while n > 0 { n -= 1; write(STDOUT_FILENO, &buf[n], 1); }
}

unsafe fn report(name: &[u8], passed: bool) -> bool {
    out(name);
    out(if passed { b": PASS\n" } else { b": FAIL\n" });
    passed
}

/// One step of a case. Prints the step and errno on failure so a FAIL line
/// can be attributed. Returns the condition so callers can `&=` it.
unsafe fn step(cond: bool, tag: &[u8]) -> bool {
    if !cond {
        out(b"    step failed: ");
        out(tag);
        out(b" (errno ");
        out_dec(get_errno() as u32);
        out(b")\n");
    }
    cond
}

/// `r == -1 && errno == e` (the wrappers set errno only on failure, so a
/// success can never be mistaken for the expected error).
unsafe fn fails_with(r: isize, e: i32) -> bool { r == -1 && get_errno() == e }

/// Run `f` in a forked child as `uid`/`gid` (0 = stay root). The child's exit
/// status is the verdict: 0 = every step passed.
unsafe fn run_as(uid: u32, gid: u32, f: unsafe fn(&[u8]) -> bool, root: &[u8]) -> bool {
    let pid = fork();
    if pid == 0 {
        if uid != 0 {
            if setresgid(gid, gid, gid) != 0 { out(b"    setresgid failed\n"); exit(2); }
            if setresuid(uid, uid, uid) != 0 { out(b"    setresuid failed\n"); exit(2); }
            if geteuid() != uid { out(b"    privilege drop did not take\n"); exit(2); }
        }
        exit(if f(root) { 0 } else { 1 });
    }
    if pid < 0 { return false; }
    let mut status: i32 = -1;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    status == 0
}

// ── fixture ──────────────────────────────────────────────────────────────────
//
//   <root>/rootonly/            0700 root   f (0644 root), sub/ (0755)
//   <root>/ro755/               0755 root   victim (0644 root), subdir/ (0755)
//   <root>/open777/             0777 root
//   <root>/sticky/              1777 root   rootfile (0666 root),
//                                            otherfile (0666 uid 1001),
//                                            mine, mine2 (0666 uid 1000)
//   <root>/home/                0700 1000
//   <root>/rootfile             0644 root
//   <root>/ownfile              0644 1000

unsafe fn create(path: *const u8, mode: u32) -> bool {
    set_errno(0);
    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, mode);
    if fd < 0 { return false; }
    close(fd);
    true
}

unsafe fn setup(root: &[u8]) -> bool {
    let mut ok = true;
    ok &= step(mkdir(p!(root, b"\0").as_ptr(), 0o755) == 0, b"setup mkdir root");
    ok &= step(mkdir(p!(root, b"/rootonly\0").as_ptr(), 0o700) == 0, b"setup rootonly");
    ok &= step(create(p!(root, b"/rootonly/f\0").as_ptr(), 0o644), b"setup rootonly/f");
    ok &= step(mkdir(p!(root, b"/rootonly/sub\0").as_ptr(), 0o755) == 0, b"setup rootonly/sub");
    ok &= step(mkdir(p!(root, b"/ro755\0").as_ptr(), 0o755) == 0, b"setup ro755");
    ok &= step(create(p!(root, b"/ro755/victim\0").as_ptr(), 0o644), b"setup ro755/victim");
    ok &= step(mkdir(p!(root, b"/ro755/subdir\0").as_ptr(), 0o755) == 0, b"setup ro755/subdir");
    ok &= step(mkdir(p!(root, b"/open777\0").as_ptr(), 0o777) == 0, b"setup open777");
    ok &= step(mkdir(p!(root, b"/sticky\0").as_ptr(), 0o1777) == 0, b"setup sticky");
    // mkdir(2) may strip the sticky bit (umask is 0 here, but be explicit).
    ok &= step(chmod(p!(root, b"/sticky\0").as_ptr(), 0o1777) == 0, b"setup chmod sticky");
    ok &= step(create(p!(root, b"/sticky/rootfile\0").as_ptr(), 0o666), b"setup sticky/rootfile");
    ok &= step(create(p!(root, b"/sticky/otherfile\0").as_ptr(), 0o666), b"setup sticky/otherfile");
    ok &= step(chown(p!(root, b"/sticky/otherfile\0").as_ptr(), UID_OTHER, UID_OTHER) == 0, b"setup chown otherfile");
    ok &= step(create(p!(root, b"/sticky/mine\0").as_ptr(), 0o666), b"setup sticky/mine");
    ok &= step(chown(p!(root, b"/sticky/mine\0").as_ptr(), UID_USER, UID_USER) == 0, b"setup chown mine");
    ok &= step(create(p!(root, b"/sticky/mine2\0").as_ptr(), 0o666), b"setup sticky/mine2");
    ok &= step(chown(p!(root, b"/sticky/mine2\0").as_ptr(), UID_USER, UID_USER) == 0, b"setup chown mine2");
    ok &= step(mkdir(p!(root, b"/home\0").as_ptr(), 0o700) == 0, b"setup home");
    ok &= step(chown(p!(root, b"/home\0").as_ptr(), UID_USER, UID_USER) == 0, b"setup chown home");
    ok &= step(create(p!(root, b"/rootfile\0").as_ptr(), 0o644), b"setup rootfile");
    ok &= step(create(p!(root, b"/ownfile\0").as_ptr(), 0o644), b"setup ownfile");
    ok &= step(chown(p!(root, b"/ownfile\0").as_ptr(), UID_USER, UID_USER) == 0, b"setup chown ownfile");
    ok
}

/// Remove everything setup() and the cases may have left. Errors ignored:
/// this runs first (a previous run may have died half-way) and last.
unsafe fn teardown(root: &[u8]) {
    for s in [&b"/rootonly/f\0"[..], b"/rootonly/socklink\0", b"/rootonly/g\0", b"/ro755/victim\0", b"/ro755/rootnew\0",
              b"/sticky/rootfile\0", b"/sticky/otherfile\0", b"/sticky/mine\0", b"/sticky/mine2\0",
              b"/sticky/renamed\0", b"/home/f\0", b"/home/l\0", b"/home/g\0", b"/home/s\0",
              b"/open777/f\0", b"/open777/g\0", b"/open777/l\0", b"/open777/sock\0",
              b"/open777/sock600\0", b"/rootfile\0", b"/ownfile\0"] {
        unlink(p!(root, s).as_ptr());
    }
    for s in [&b"/rootonly/sub\0"[..], b"/rootonly\0", b"/ro755/subdir\0", b"/ro755\0",
              b"/open777/d\0", b"/open777\0", b"/sticky\0", b"/home/d\0", b"/home\0"] {
        rmdir(p!(root, s).as_ptr());
    }
    rmdir(p!(root, b"\0").as_ptr());
}

// ── cases (each runs as uid 1000 unless stated) ──────────────────────────────

/// Traversal through a 0700 root directory is EACCES on every operation, and
/// a missing leaf inside it is still EACCES — existence must not leak.
unsafe fn case_traversal_denied(root: &[u8]) -> bool {
    let mut ok = true;
    let f = p!(root, b"/rootonly/f\0");
    let nx = p!(root, b"/rootonly/nonexistent\0");
    let deep = p!(root, b"/rootonly/sub/x\0");
    let mut st = [0u8; STAT_SIZE];
    ok &= step(fails_with(open(f.as_ptr(), O_RDONLY, 0) as isize, EACCES), b"open through 0700 dir");
    ok &= step(fails_with(raw_stat(f.as_ptr(), st.as_mut_ptr()), EACCES), b"stat through 0700 dir");
    ok &= step(fails_with(open(nx.as_ptr(), O_RDONLY, 0) as isize, EACCES), b"open missing leaf is EACCES not ENOENT");
    ok &= step(fails_with(raw_access(deep.as_ptr(), F_OK), EACCES), b"access F_OK two levels down");
    ok &= step(fails_with(open(nx.as_ptr(), O_CREAT | O_WRONLY, 0o644) as isize, EACCES), b"O_CREAT through 0700 dir");
    ok &= step(fails_with(mkdir(p!(root, b"/rootonly/sub/d\0").as_ptr(), 0o755) as isize, EACCES), b"mkdir under 0700 dir");
    // The directory itself: readable? No (0700 root) — the pre-existing check.
    ok &= step(fails_with(open(p!(root, b"/rootonly\0").as_ptr(), O_RDONLY, 0) as isize, EACCES), b"opendir 0700 dir");
    ok
}

/// Creating anything inside a 0755 root directory is EACCES.
unsafe fn case_create_denied(root: &[u8]) -> bool {
    let mut ok = true;
    ok &= step(fails_with(open(p!(root, b"/ro755/newf\0").as_ptr(), O_CREAT | O_WRONLY, 0o644) as isize, EACCES), b"O_CREAT in 0755 dir");
    ok &= step(fails_with(mkdir(p!(root, b"/ro755/newd\0").as_ptr(), 0o755) as isize, EACCES), b"mkdir in 0755 dir");
    ok &= step(fails_with(raw_symlink(b"victim\0".as_ptr(), p!(root, b"/ro755/newl\0").as_ptr()), EACCES), b"symlink in 0755 dir");
    ok &= step(fails_with(raw_link(p!(root, b"/ro755/victim\0").as_ptr(), p!(root, b"/ro755/victim2\0").as_ptr()), EACCES), b"link in 0755 dir");
    // An existing file stays readable: the check is on creation, not access.
    let fd = open(p!(root, b"/ro755/victim\0").as_ptr(), O_RDONLY, 0);
    ok &= step(fd >= 0, b"read existing file in 0755 dir");
    if fd >= 0 { close(fd); }
    // O_CREAT on an EXISTING file is a plain open: allowed for read, EACCES for
    // write on a 0644 root file — no parent write needed.
    let fd = open(p!(root, b"/ro755/victim\0").as_ptr(), O_CREAT | O_RDONLY, 0o644);
    ok &= step(fd >= 0, b"O_CREAT|O_RDONLY on existing file");
    if fd >= 0 { close(fd); }
    ok &= step(fails_with(open(p!(root, b"/ro755/victim\0").as_ptr(), O_RDONLY | O_TRUNC, 0) as isize, EACCES), b"O_TRUNC needs write");
    ok
}

/// Creating a name that ALREADY EXISTS in a 0755 root dir is EEXIST, not
/// EACCES — Linux's order (lookup, then may_create): `mkdir /usr` as a user
/// says "File exists". Rust's `create_dir_all` depends on it: it only asks
/// "is it a directory already?" after an error that is not NotFound, and on
/// EACCES it has no reason to — so every `create_dir_all` over a chain some
/// other uid made (`/run/cosmic-greeter/...` in the user session) failed
/// with EACCES before this was fixed.
unsafe fn case_create_existing_eexist(root: &[u8]) -> bool {
    let mut ok = true;
    ok &= step(fails_with(mkdir(p!(root, b"/ro755/subdir\0").as_ptr(), 0o755) as isize, EEXIST), b"mkdir existing dir in 0755 dir is EEXIST");
    ok &= step(fails_with(mkdir(p!(root, b"/ro755/victim\0").as_ptr(), 0o755) as isize, EEXIST), b"mkdir over existing file is EEXIST");
    ok &= step(fails_with(raw_symlink(b"x\0".as_ptr(), p!(root, b"/ro755/victim\0").as_ptr()), EEXIST), b"symlink over existing name is EEXIST");
    ok &= step(fails_with(raw_link(p!(root, b"/ro755/victim\0").as_ptr(), p!(root, b"/ro755/subdir\0").as_ptr()), EEXIST), b"link over existing name is EEXIST");
    ok
}

/// mknod (FIFO) in a 0755 root dir is EACCES — tmpfs only, f2fs has no mknod.
unsafe fn case_mknod_denied(root: &[u8]) -> bool {
    step(fails_with(raw_mknod(p!(root, b"/ro755/fifo\0").as_ptr(), S_IFIFO | 0o644), EACCES), b"mknod in 0755 dir")
}

/// Removing or renaming inside a 0755 root directory is EACCES, including a
/// rename whose destination parent is the protected one.
unsafe fn case_remove_denied(root: &[u8]) -> bool {
    let mut ok = true;
    ok &= step(fails_with(unlink(p!(root, b"/ro755/victim\0").as_ptr()) as isize, EACCES), b"unlink in 0755 dir");
    ok &= step(fails_with(rename(p!(root, b"/ro755/victim\0").as_ptr(), p!(root, b"/ro755/v2\0").as_ptr()) as isize, EACCES), b"rename within 0755 dir");
    ok &= step(fails_with(rmdir(p!(root, b"/ro755/subdir\0").as_ptr()) as isize, EACCES), b"rmdir in 0755 dir");
    // Source parent writable (0777), destination parent not.
    ok &= step(create(p!(root, b"/open777/g\0").as_ptr(), 0o644), b"create in 0777 dir for rename");
    ok &= step(fails_with(rename(p!(root, b"/open777/g\0").as_ptr(), p!(root, b"/ro755/g\0").as_ptr()) as isize, EACCES), b"rename into 0755 dir");
    ok &= step(unlink(p!(root, b"/open777/g\0").as_ptr()) == 0, b"cleanup open777/g");
    // The victim is still there.
    ok &= step(raw_access(p!(root, b"/ro755/victim\0").as_ptr(), F_OK) == 0, b"victim survived");
    ok
}

/// Everything is allowed in a 0777 directory, and what is created belongs to
/// the creator.
unsafe fn case_allowed_0777(root: &[u8]) -> bool {
    let mut ok = true;
    let f = p!(root, b"/open777/f\0");
    let g = p!(root, b"/open777/g\0");
    let l = p!(root, b"/open777/l\0");
    let d = p!(root, b"/open777/d\0");
    ok &= step(create(f.as_ptr(), 0o644), b"create in 0777 dir");
    ok &= step(stat_uid(f.as_ptr()) == UID_USER, b"created file owned by creator");
    ok &= step(mkdir(d.as_ptr(), 0o755) == 0, b"mkdir in 0777 dir");
    ok &= step(raw_symlink(b"f\0".as_ptr(), l.as_ptr()) == 0, b"symlink in 0777 dir");
    let fd = open(l.as_ptr(), O_RDONLY, 0);
    ok &= step(fd >= 0, b"open through own symlink");
    if fd >= 0 { close(fd); }
    ok &= step(rename(f.as_ptr(), g.as_ptr()) == 0, b"rename in 0777 dir");
    ok &= step(unlink(l.as_ptr()) == 0, b"unlink symlink in 0777 dir");
    ok &= step(unlink(g.as_ptr()) == 0, b"unlink in 0777 dir");
    ok &= step(rmdir(d.as_ptr()) == 0, b"rmdir in 0777 dir");
    ok
}

/// The sticky bit: in a 1777 directory another user's entries (and root's)
/// are EPERM to remove, rename or replace; one's own are fine.
unsafe fn case_sticky(root: &[u8]) -> bool {
    let mut ok = true;
    let other = p!(root, b"/sticky/otherfile\0");
    let rootf = p!(root, b"/sticky/rootfile\0");
    let mine = p!(root, b"/sticky/mine\0");
    let mine2 = p!(root, b"/sticky/mine2\0");
    let renamed = p!(root, b"/sticky/renamed\0");
    ok &= step(fails_with(unlink(other.as_ptr()) as isize, EPERM), b"unlink another's file in 1777 dir");
    ok &= step(fails_with(rename(other.as_ptr(), renamed.as_ptr()) as isize, EPERM), b"rename another's file in 1777 dir");
    ok &= step(fails_with(unlink(rootf.as_ptr()) as isize, EPERM), b"unlink root's file in 1777 dir");
    ok &= step(fails_with(rename(mine2.as_ptr(), other.as_ptr()) as isize, EPERM), b"rename over another's file in 1777 dir");
    ok &= step(rename(mine2.as_ptr(), renamed.as_ptr()) == 0, b"rename own file in 1777 dir");
    ok &= step(unlink(renamed.as_ptr()) == 0, b"unlink own (renamed) file in 1777 dir");
    ok &= step(unlink(mine.as_ptr()) == 0, b"unlink own file in 1777 dir");
    // Creation is open to everyone there, as on /tmp.
    ok &= step(create(mine.as_ptr(), 0o644), b"create in 1777 dir");
    ok
}

/// chmod/chown ownership rules.
unsafe fn case_chmod_chown(root: &[u8]) -> bool {
    let mut ok = true;
    let rootf = p!(root, b"/rootfile\0");
    let own = p!(root, b"/ownfile\0");
    ok &= step(fails_with(chmod(rootf.as_ptr(), 0o600) as isize, EPERM), b"chmod another's file");
    ok &= step(fails_with(chown(rootf.as_ptr(), UID_USER, UID_USER) as isize, EPERM), b"chown another's file to self");
    ok &= step(fails_with(chown(own.as_ptr(), 0, u32::MAX) as isize, EPERM), b"give own file to root");
    ok &= step(fails_with(chown(own.as_ptr(), u32::MAX, UID_OTHER) as isize, EPERM), b"chgrp own file to a foreign group");
    ok &= step(chmod(own.as_ptr(), 0o600) == 0, b"chmod own file");
    ok &= step(chown(own.as_ptr(), UID_USER, UID_USER) == 0, b"chown own file to self");
    ok &= step(chmod(own.as_ptr(), 0o644) == 0, b"chmod own file back");
    ok
}

/// The positive path: a user can do all of it inside its own 0700 home.
unsafe fn case_own_home(root: &[u8]) -> bool {
    let mut ok = true;
    let f = p!(root, b"/home/f\0");
    let g = p!(root, b"/home/g\0");
    let l = p!(root, b"/home/l\0");
    let d = p!(root, b"/home/d\0");
    ok &= step(create(f.as_ptr(), 0o600), b"create in own home");
    ok &= step(mkdir(d.as_ptr(), 0o700) == 0, b"mkdir in own home");
    ok &= step(raw_symlink(b"f\0".as_ptr(), l.as_ptr()) == 0, b"symlink in own home");
    ok &= step(raw_link(f.as_ptr(), g.as_ptr()) == 0, b"link in own home");
    ok &= step(chmod(f.as_ptr(), 0o644) == 0, b"chmod in own home");
    ok &= step(rename(g.as_ptr(), p!(root, b"/home/d/g\0").as_ptr()) == 0, b"rename into own subdir");
    ok &= step(unlink(p!(root, b"/home/d/g\0").as_ptr()) == 0, b"unlink in own subdir");
    ok &= step(unlink(l.as_ptr()) == 0, b"unlink symlink in own home");
    ok &= step(unlink(f.as_ptr()) == 0, b"unlink in own home");
    ok &= step(rmdir(d.as_ptr()) == 0, b"rmdir in own home");
    ok
}

/// AF_UNIX bind is a creation: EACCES in a 0755 root dir, fine in a 0777 one
/// and in one's own home. tmpfs only.
unsafe fn case_unix_bind(root: &[u8]) -> bool {
    let mut ok = true;
    let s1 = raw_socket();
    ok &= step(s1 >= 0, b"socket");
    ok &= step(fails_with(raw_bind(s1, p!(root, b"/ro755/sock\0").as_ptr()), EACCES), b"bind in 0755 dir");
    close(s1);
    let s2 = raw_socket();
    ok &= step(raw_bind(s2, p!(root, b"/open777/sock\0").as_ptr()) == 0, b"bind in 0777 dir");
    ok &= step(stat_uid(p!(root, b"/open777/sock\0").as_ptr()) == UID_USER, b"socket node owned by binder");
    close(s2);
    ok &= step(unlink(p!(root, b"/open777/sock\0").as_ptr()) == 0, b"unlink own socket node");
    let s3 = raw_socket();
    ok &= step(raw_bind(s3, p!(root, b"/home/s\0").as_ptr()) == 0, b"bind in own home");
    close(s3);
    ok &= step(unlink(p!(root, b"/home/s\0").as_ptr()) == 0, b"unlink own socket node in home");
    ok
}

/// connect() needs write on the socket inode: a 0600 root socket refuses uid
/// 1000 with EACCES, a 0666 one accepts. The listener is root's, in the parent.
unsafe fn connect_expect(root: &[u8], want_errno: i32) -> bool {
    let fd = raw_socket();
    if fd < 0 { return step(false, b"socket for connect"); }
    let r = raw_connect(fd, p!(root, b"/open777/sock600\0").as_ptr());
    close(fd);
    if want_errno == 0 { step(r == 0, b"connect to 0666 root socket") }
    else { step(fails_with(r, want_errno), b"connect to 0600 root socket") }
}
unsafe fn case_connect_denied(root: &[u8]) -> bool { connect_expect(root, EACCES) }
unsafe fn case_connect_allowed(root: &[u8]) -> bool { connect_expect(root, 0) }
/// Same socket reached through a 0700 root directory (via a symlink parked
/// there): traversal, not the socket mode, is what refuses.
unsafe fn case_connect_traversal(root: &[u8]) -> bool {
    let fd = raw_socket();
    if fd < 0 { return step(false, b"socket for connect"); }
    let r = raw_connect(fd, p!(root, b"/rootonly/socklink\0").as_ptr());
    close(fd);
    step(fails_with(r, EACCES), b"connect through 0700 dir")
}

/// Root bypasses every one of the checks above (runs with uid 0).
unsafe fn case_root_bypass(root: &[u8]) -> bool {
    let mut ok = true;
    let fd = open(p!(root, b"/rootonly/f\0").as_ptr(), O_RDONLY, 0);
    ok &= step(fd >= 0, b"root reads through 0700 dir");
    if fd >= 0 { close(fd); }
    ok &= step(create(p!(root, b"/ro755/rootnew\0").as_ptr(), 0o644), b"root creates in 0755 dir");
    ok &= step(unlink(p!(root, b"/ro755/rootnew\0").as_ptr()) == 0, b"root unlinks in 0755 dir");
    ok &= step(mkdir(p!(root, b"/rootonly/sub2\0").as_ptr(), 0o755) == 0, b"root mkdirs in 0700 dir");
    ok &= step(rmdir(p!(root, b"/rootonly/sub2\0").as_ptr()) == 0, b"root rmdirs in 0700 dir");
    // Sticky: root removes another user's entry.
    ok &= step(unlink(p!(root, b"/sticky/otherfile\0").as_ptr()) == 0, b"root unlinks another's file in 1777 dir");
    ok &= step(create(p!(root, b"/sticky/otherfile\0").as_ptr(), 0o666), b"root recreates it");
    ok &= step(chown(p!(root, b"/sticky/otherfile\0").as_ptr(), UID_OTHER, UID_OTHER) == 0, b"root chowns it");
    // Entering a directory owned by someone else.
    ok &= step(create(p!(root, b"/home/f\0").as_ptr(), 0o644), b"root creates in user's 0700 home");
    ok &= step(unlink(p!(root, b"/home/f\0").as_ptr()) == 0, b"root unlinks there");
    ok &= step(chmod(p!(root, b"/ownfile\0").as_ptr(), 0o644) == 0, b"root chmods another's file");
    ok
}

/// A stored POSIX ACL granting uid 1000 search on the 0700 directory opens the
/// traversal for that user alone (mode bits still say 0710 for everyone else).
unsafe fn case_acl_traversal(root: &[u8]) -> bool {
    let mut ok = true;
    let mut st = [0u8; STAT_SIZE];
    ok &= step(raw_stat(p!(root, b"/rootonly/f\0").as_ptr(), st.as_mut_ptr()) == 0, b"stat through ACL-opened dir");
    let fd = open(p!(root, b"/rootonly/f\0").as_ptr(), O_RDONLY, 0);
    ok &= step(fd >= 0, b"open through ACL-opened dir");
    if fd >= 0 { close(fd); }
    // Search, not write: creation there is still EACCES.
    ok &= step(fails_with(open(p!(root, b"/rootonly/g\0").as_ptr(), O_CREAT | O_WRONLY, 0o644) as isize, EACCES), b"ACL x does not grant create");
    ok
}

/// `system.posix_acl_access` v2: USER_OBJ rwx, USER(1000) --x, GROUP_OBJ ---,
/// MASK --x, OTHER --- (canonical order, named entry needs a mask).
unsafe fn set_acl_x_for_user(dir: *const u8) -> bool {
    let mut v = [0u8; 4 + 5 * 8];
    v[..4].copy_from_slice(&2u32.to_le_bytes());
    let entries: [(u16, u16, u32); 5] = [(0x01, 7, u32::MAX), (0x02, 1, UID_USER), (0x04, 0, u32::MAX),
                                        (0x10, 1, u32::MAX), (0x20, 0, u32::MAX)];
    for (i, (tag, perm, id)) in entries.iter().enumerate() {
        let o = 4 + i * 8;
        v[o..o + 2].copy_from_slice(&tag.to_le_bytes());
        v[o + 2..o + 4].copy_from_slice(&perm.to_le_bytes());
        v[o + 4..o + 8].copy_from_slice(&id.to_le_bytes());
    }
    raw_setxattr(dir, b"system.posix_acl_access\0".as_ptr(), v.as_ptr(), v.len()) == 0
}

// ── driver ───────────────────────────────────────────────────────────────────

unsafe fn run_matrix(root: &[u8], tag: &[u8], with_sockets: bool) -> u32 {
    let mut failures = 0u32;
    let mut name = [0u8; 96];
    let mut named = |case: &[u8]| -> *const u8 {
        let n = case.len() + tag.len();
        name[..case.len()].copy_from_slice(case);
        name[case.len()..n].copy_from_slice(tag);
        name[n] = 0;
        name.as_ptr()
    };
    macro_rules! case {
        ($label:literal, $uid:expr, $f:ident) => {{
            let n = named($label);
            let nm = core::slice::from_raw_parts(n, $label.len() + tag.len());
            if !report(nm, run_as($uid, $uid, $f, root)) { failures += 1; }
        }};
    }

    teardown(root);
    if !setup(root) {
        report(core::slice::from_raw_parts(named(b"setup"), 5 + tag.len()), false);
        teardown(root);
        return 1;
    }

    case!(b"traversal_denied", UID_USER, case_traversal_denied);
    case!(b"create_denied_0755", UID_USER, case_create_denied);
    case!(b"create_existing_eexist", UID_USER, case_create_existing_eexist);
    if with_sockets { case!(b"mknod_denied_0755", UID_USER, case_mknod_denied); }
    case!(b"remove_denied_0755", UID_USER, case_remove_denied);
    case!(b"allowed_0777", UID_USER, case_allowed_0777);
    case!(b"sticky_protects_others", UID_USER, case_sticky);
    case!(b"chmod_chown_ownership", UID_USER, case_chmod_chown);
    case!(b"own_home_positive", UID_USER, case_own_home);

    if with_sockets {
        case!(b"unix_bind", UID_USER, case_unix_bind);
        // Root's listener at open777/sock600, first 0600 then 0666; plus a
        // symlink to it inside the 0700 directory for the traversal case.
        let sockpath = p!(root, b"/open777/sock600\0");
        let lfd = raw_socket();
        let mut lok = lfd >= 0;
        lok &= raw_bind(lfd, sockpath.as_ptr()) == 0;
        lok &= raw_listen(lfd) == 0;
        lok &= chmod(sockpath.as_ptr(), 0o600) == 0;
        lok &= raw_symlink(sockpath.as_ptr(), p!(root, b"/rootonly/socklink\0").as_ptr()) == 0;
        if !report(core::slice::from_raw_parts(named(b"unix_listener_setup"), 19 + tag.len()), lok) { failures += 1; }
        case!(b"unix_connect_0600_denied", UID_USER, case_connect_denied);
        case!(b"unix_connect_through_0700_denied", UID_USER, case_connect_traversal);
        case!(b"unix_connect_0600_root_bypass", 0, case_connect_allowed);
        chmod(sockpath.as_ptr(), 0o666);
        case!(b"unix_connect_0666_allowed", UID_USER, case_connect_allowed);
        close(lfd);
        unlink(p!(root, b"/rootonly/socklink\0").as_ptr());
        unlink(sockpath.as_ptr());
    }

    case!(b"root_bypass", 0, case_root_bypass);

    if set_acl_x_for_user(p!(root, b"/rootonly\0").as_ptr()) {
        case!(b"acl_grants_traversal", UID_USER, case_acl_traversal);
    } else {
        if !report(core::slice::from_raw_parts(named(b"acl_grants_traversal"), 20 + tag.len()), false) { failures += 1; }
    }

    teardown(root);
    failures
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if geteuid() != 0 {
        puts(b"permtest: must run as root\0".as_ptr());
        return 1;
    }
    // Exact modes: the fixture's 0777/1777/0700 must land as written.
    raw_umask(0);

    let mut failures = 0u32;
    failures += run_matrix(b"/data/pt", b"_f2fs", false);
    failures += run_matrix(b"/tmp/pt", b"_tmpfs", true);

    out(b"--- permtest done: ");
    out_dec(failures);
    out(b" failure(s) ---\n");
    failures as i32
}
