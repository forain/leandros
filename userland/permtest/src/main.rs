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
//!   * root (euid 0) bypasses all of it;
//!   * supplementary groups (setgroups/getgroups, inherited across fork and
//!     exec) count as group membership on every check, and chgrp may target
//!     any of them;
//!   * execve needs execute permission on an ELF, not just on a `#!` script
//!     (root included: at least one x bit);
//!   * utimensat: explicit times need ownership (EPERM), "now" needs write
//!     permission (EACCES), UTIME_OMIT leaves a timestamp alone, and
//!     AT_SYMLINK_NOFOLLOW stamps the link itself;
//!   * a directory's default ACL is inherited at creation: the child's mode
//!     comes from the ACL (the umask is ignored), a named entry becomes part
//!     of the child's access ACL, and a child directory carries the default
//!     ACL on.
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
//! Note: `wait4()` reports the Linux-encoded status (exit code in bits 8..16),
//! so `status == 0` is the check for a clean pass.

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
#[cfg(target_arch = "aarch64")] const SYS_UTIMENSAT:  usize = 88;
#[cfg(target_arch = "x86_64")]  const SYS_UTIMENSAT:  usize = 280;
#[cfg(target_arch = "aarch64")] const SYS_GETXATTR:   usize = 8;
#[cfg(target_arch = "x86_64")]  const SYS_GETXATTR:   usize = 191;
#[cfg(target_arch = "aarch64")] const SYS_SETGROUPS:  usize = 159;
#[cfg(target_arch = "x86_64")]  const SYS_SETGROUPS:  usize = 116;
#[cfg(target_arch = "aarch64")] const SYS_GETGROUPS:  usize = 158;
#[cfg(target_arch = "x86_64")]  const SYS_GETGROUPS:  usize = 115;
#[cfg(target_arch = "aarch64")] const SYS_FCHOWNAT:   usize = 54;
#[cfg(target_arch = "x86_64")]  const SYS_FCHOWNAT:   usize = 260;

const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const UTIME_NOW:  i64 = (1 << 30) - 1;
const UTIME_OMIT: i64 = (1 << 30) - 2;
// struct stat timestamps: st_atim at 72, st_mtim at 88 (sec, nsec as i64 each)
// on both ABIs — see servers/vfs write_stat_times.
const STAT_ATIME_OFF: usize = 72;
const STAT_MTIME_OFF: usize = 88;
#[cfg(target_arch = "aarch64")] const STAT_MODE_OFF: usize = 16;
#[cfg(target_arch = "x86_64")]  const STAT_MODE_OFF: usize = 24;
#[cfg(target_arch = "aarch64")] const STAT_GID_OFF: usize = 28;
#[cfg(target_arch = "x86_64")]  const STAT_GID_OFF: usize = 32;

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
const GID_VIDEO: u32 = 44;   // a supplementary group (leandro is a member per /etc/group)
const GID_NONE: u32 = 45;    // a group the user is NOT in

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
unsafe fn raw_utimensat(path: *const u8, times: *const i64, flags: usize) -> isize {
    xret(syscall4(SYS_UTIMENSAT, AT_FDCWD as usize, path as usize, times as usize, flags))
}
unsafe fn raw_futimens(fd: i32, times: *const i64) -> isize {
    xret(syscall4(SYS_UTIMENSAT, fd as usize, 0, times as usize, 0))
}
unsafe fn raw_getxattr(path: *const u8, name: *const u8, buf: *mut u8, size: usize) -> isize {
    xret(syscall4(SYS_GETXATTR, path as usize, name as usize, buf as usize, size))
}
unsafe fn raw_setgroups(groups: &[u32]) -> isize {
    xret(syscall2(SYS_SETGROUPS, groups.len(), groups.as_ptr() as usize))
}
unsafe fn raw_getgroups(out: &mut [u32; 32]) -> isize {
    xret(syscall2(SYS_GETGROUPS, out.len(), out.as_mut_ptr() as usize))
}
unsafe fn raw_lchown(path: *const u8, uid: u32, gid: u32) -> isize {
    xret(syscall5(SYS_FCHOWNAT, AT_FDCWD as usize, path as usize, uid as usize, gid as usize, AT_SYMLINK_NOFOLLOW))
}
unsafe fn raw_lstat(path: *const u8, buf: *mut u8) -> isize {
    xret(syscall4(SYS_NEWFSTATAT, AT_FDCWD as usize, path as usize, buf as usize, AT_SYMLINK_NOFOLLOW))
}

/// `(atime, mtime)` seconds+nanoseconds of `path`, or None when stat fails.
unsafe fn stat_times(path: *const u8, follow: bool) -> Option<((i64, i64), (i64, i64))> {
    let mut st = [0u8; STAT_SIZE];
    let r = if follow { raw_stat(path, st.as_mut_ptr()) } else { raw_lstat(path, st.as_mut_ptr()) };
    if r != 0 { return None; }
    let rd = |o: usize| i64::from_ne_bytes(st[o..o + 8].try_into().unwrap());
    Some(((rd(STAT_ATIME_OFF), rd(STAT_ATIME_OFF + 8)), (rd(STAT_MTIME_OFF), rd(STAT_MTIME_OFF + 8))))
}
unsafe fn stat_mode(path: *const u8) -> u32 {
    let mut st = [0u8; STAT_SIZE];
    if raw_stat(path, st.as_mut_ptr()) != 0 { return u32::MAX; }
    u32::from_ne_bytes(st[STAT_MODE_OFF..STAT_MODE_OFF + 4].try_into().unwrap())
}
unsafe fn stat_gid(path: *const u8) -> u32 {
    let mut st = [0u8; STAT_SIZE];
    if raw_stat(path, st.as_mut_ptr()) != 0 { return u32::MAX; }
    u32::from_ne_bytes(st[STAT_GID_OFF..STAT_GID_OFF + 4].try_into().unwrap())
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
    run_as_groups(uid, gid, &[], f, root)
}

/// As `run_as`, with a supplementary group list installed before the drop.
unsafe fn run_as_groups(uid: u32, gid: u32, groups: &[u32], f: unsafe fn(&[u8]) -> bool, root: &[u8]) -> bool {
    let pid = fork();
    if pid == 0 {
        if uid != 0 {
            if raw_setgroups(groups) != 0 { out(b"    setgroups failed\n"); exit(2); }
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
    // Group-only objects: reachable through group 44 (video) alone.
    ok &= step(mkdir(p!(root, b"/grpdir\0").as_ptr(), 0o070) == 0, b"setup grpdir");
    ok &= step(chown(p!(root, b"/grpdir\0").as_ptr(), 0, GID_VIDEO) == 0, b"setup chown grpdir");
    ok &= step(create(p!(root, b"/grpfile\0").as_ptr(), 0o640), b"setup grpfile");
    ok &= step(chown(p!(root, b"/grpfile\0").as_ptr(), 0, GID_VIDEO) == 0, b"setup chown grpfile");
    // utimensat targets: a world-writable root file, and a symlink to ownfile.
    ok &= step(create(p!(root, b"/rw666\0").as_ptr(), 0o666), b"setup rw666");
    ok &= step(raw_symlink(p!(root, b"/ownfile\0").as_ptr(), p!(root, b"/ownlink\0").as_ptr()) == 0, b"setup ownlink");
    ok &= step(raw_lchown(p!(root, b"/ownlink\0").as_ptr(), UID_USER, UID_USER) == 0, b"setup lchown ownlink");
    // Default-ACL directory, open to everyone so creation itself is not the
    // question.
    ok &= step(mkdir(p!(root, b"/dacl\0").as_ptr(), 0o777) == 0, b"setup dacl");
    ok &= step(set_default_acl(p!(root, b"/dacl\0").as_ptr()), b"setup dacl default ACL");
    ok
}

/// Copy /bin/hello (a tiny static ELF that exits 0) to `<root>/exe*` in three
/// modes for the execve x-bit cases. f2fs only: the x86-64 binary is bigger
/// than a tmpfs file may be.
unsafe fn setup_exec(root: &[u8]) -> bool {
    let src = open(b"/bin/hello\0".as_ptr(), O_RDONLY, 0);
    if !step(src >= 0, b"setup open /bin/hello") { return false; }
    static mut BUF: [u8; 131072] = [0u8; 131072];
    let n = read(src, core::ptr::addr_of_mut!(BUF) as *mut u8, 131072);
    close(src);
    if !step(n > 0 && n < 131072, b"setup read /bin/hello") { return false; }
    let mut ok = true;
    for (name, mode) in [(&b"/exe644\0"[..], 0o644u32), (b"/exe755\0", 0o755), (b"/exe700\0", 0o700)] {
        let path = p!(root, name);
        let fd = open(path.as_ptr(), O_CREAT | O_WRONLY | O_TRUNC, mode);
        ok &= step(fd >= 0, b"setup create exe");
        if fd < 0 { continue; }
        let w = write(fd, core::ptr::addr_of!(BUF) as *const u8, n as usize);
        close(fd);
        ok &= step(w == n, b"setup write exe");
        ok &= step(chmod(path.as_ptr(), mode) == 0, b"setup chmod exe");
    }
    ok
}

/// Remove everything setup() and the cases may have left. Errors ignored:
/// this runs first (a previous run may have died half-way) and last.
unsafe fn teardown(root: &[u8]) {
    for s in [&b"/rootonly/f\0"[..], b"/rootonly/socklink\0", b"/rootonly/g\0", b"/ro755/victim\0", b"/ro755/rootnew\0",
              b"/sticky/rootfile\0", b"/sticky/otherfile\0", b"/sticky/mine\0", b"/sticky/mine2\0",
              b"/sticky/renamed\0", b"/home/f\0", b"/home/l\0", b"/home/g\0", b"/home/s\0",
              b"/open777/f\0", b"/open777/g\0", b"/open777/l\0", b"/open777/sock\0",
              b"/open777/sock600\0", b"/rootfile\0", b"/ownfile\0",
              b"/grpdir/f\0", b"/grpfile\0", b"/rw666\0", b"/ownlink\0",
              b"/exe644\0", b"/exe755\0", b"/exe700\0",
              b"/dacl/sub/subsub/f\0", b"/dacl/sub/f\0", b"/dacl/f\0"] {
        unlink(p!(root, s).as_ptr());
    }
    for s in [&b"/rootonly/sub\0"[..], b"/rootonly\0", b"/ro755/subdir\0", b"/ro755\0",
              b"/open777/d\0", b"/open777\0", b"/sticky\0", b"/home/d\0", b"/home\0",
              b"/grpdir\0", b"/dacl/sub/subsub\0", b"/dacl/sub\0", b"/dacl\0"] {
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

// ── supplementary groups ─────────────────────────────────────────────────────

/// Without group 44: a 0070 root:44 directory and a 0640 root:44 file are
/// closed to uid 1000 (primary gid 1000).
unsafe fn case_group_denied(root: &[u8]) -> bool {
    let mut ok = true;
    ok &= step(fails_with(open(p!(root, b"/grpfile\0").as_ptr(), O_RDONLY, 0) as isize, EACCES), b"0640 root:44 file closed without the group");
    ok &= step(fails_with(open(p!(root, b"/grpdir/f\0").as_ptr(), O_CREAT | O_WRONLY, 0o644) as isize, EACCES), b"0070 root:44 dir closed without the group");
    // Unprivileged setgroups is EPERM, and the list is empty.
    ok &= step(fails_with(raw_setgroups(&[GID_VIDEO]), EPERM), b"setgroups as user is EPERM");
    let mut g = [0u32; 32];
    ok &= step(raw_getgroups(&mut g) == 0, b"getgroups is empty");
    ok
}

/// With group 44 installed by setgroups before the drop: the same objects
/// open, getgroups reports the list, and a forked + exec'd child still has it.
unsafe fn case_group_allowed(root: &[u8]) -> bool {
    let mut ok = true;
    let mut g = [0u32; 32];
    let n = raw_getgroups(&mut g);
    ok &= step(n == 1 && g[0] == GID_VIDEO, b"getgroups reports the supplementary group");
    let fd = open(p!(root, b"/grpfile\0").as_ptr(), O_RDONLY, 0);
    ok &= step(fd >= 0, b"0640 root:44 file opens through the group");
    if fd >= 0 { close(fd); }
    let fd = open(p!(root, b"/grpdir/f\0").as_ptr(), O_CREAT | O_WRONLY, 0o644);
    ok &= step(fd >= 0, b"create in 0070 root:44 dir through the group");
    if fd >= 0 { close(fd); }
    ok &= step(unlink(p!(root, b"/grpdir/f\0").as_ptr()) == 0, b"unlink in 0070 root:44 dir through the group");
    // chgrp: the owner may hand a file to a supplementary group, not to another.
    ok &= step(chown(p!(root, b"/ownfile\0").as_ptr(), u32::MAX, GID_VIDEO) == 0, b"chgrp to a supplementary group");
    ok &= step(stat_gid(p!(root, b"/ownfile\0").as_ptr()) == GID_VIDEO, b"chgrp took");
    ok &= step(fails_with(chown(p!(root, b"/ownfile\0").as_ptr(), u32::MAX, GID_NONE) as isize, EPERM), b"chgrp to a foreign group is EPERM");
    ok &= step(chown(p!(root, b"/ownfile\0").as_ptr(), u32::MAX, UID_USER) == 0, b"chgrp back");
    // Inheritance: fork, then exec /bin/permtest --groups 44, which exits 0
    // only when getgroups returns exactly that list.
    let pid = fork();
    if pid == 0 {
        let argv: [*const u8; 4] = [b"/bin/permtest\0".as_ptr(), b"--groups\0".as_ptr(), b"44\0".as_ptr(), core::ptr::null()];
        let envp: [*const u8; 1] = [core::ptr::null()];
        execve(argv[0], argv.as_ptr(), envp.as_ptr());
        exit(3);
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    ok &= step(status == 0, b"groups survive fork + exec");
    ok
}

/// `permtest --groups <gid>`: the exec'd half of case_group_allowed.
unsafe fn groups_probe(want: u32) -> i32 {
    let mut g = [0u32; 32];
    let n = raw_getgroups(&mut g);
    if n == 1 && g[0] == want { 0 } else { 1 }
}

// ── execve x-bit ─────────────────────────────────────────────────────────────

/// Fork + exec `path`; returns the wait status, or -errno when execve failed
/// (the child exits with 100 + errno).
unsafe fn try_exec(path: *const u8) -> i32 {
    let pid = fork();
    if pid == 0 {
        let argv: [*const u8; 2] = [path, core::ptr::null()];
        let envp: [*const u8; 1] = [core::ptr::null()];
        execve(path, argv.as_ptr(), envp.as_ptr());
        exit(100 + get_errno());
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    // Linux wait-status encoding: exit code in bits 8..16 of a normal exit.
    let code = if status & 0x7f == 0 { (status >> 8) & 0xff } else { -1 };
    if code >= 100 { -(code - 100) } else { code }
}

/// As uid 1000: a 0644 ELF and a 0700 root ELF are EACCES; 0755 runs.
unsafe fn case_exec_xbit_user(root: &[u8]) -> bool {
    let mut ok = true;
    ok &= step(try_exec(p!(root, b"/exe644\0").as_ptr()) == -EACCES, b"exec 0644 ELF is EACCES");
    ok &= step(try_exec(p!(root, b"/exe700\0").as_ptr()) == -EACCES, b"exec 0700 root ELF is EACCES for others");
    ok &= step(try_exec(p!(root, b"/exe755\0").as_ptr()) == 0, b"exec 0755 ELF runs");
    ok
}

/// As root: 0700 runs (owner), 0755 runs, but a file with no x bit at all is
/// still EACCES — CAP_DAC_OVERRIDE does not manufacture execute permission.
unsafe fn case_exec_xbit_root(root: &[u8]) -> bool {
    let mut ok = true;
    ok &= step(try_exec(p!(root, b"/exe644\0").as_ptr()) == -EACCES, b"root exec of 0644 ELF is EACCES");
    ok &= step(try_exec(p!(root, b"/exe700\0").as_ptr()) == 0, b"root exec of 0700 ELF runs");
    ok &= step(try_exec(p!(root, b"/exe755\0").as_ptr()) == 0, b"root exec of 0755 ELF runs");
    ok
}

// ── utimensat ────────────────────────────────────────────────────────────────

/// As uid 1000, owner of ownfile: explicit times land exactly; UTIME_OMIT
/// leaves the other timestamp alone; NOFOLLOW stamps the link, not the
/// target; futimens works through an fd; a non-owned 0644 file is EPERM for
/// explicit times and EACCES for "now"; a non-owned 0666 file takes "now"
/// but not explicit times.
unsafe fn case_utimens(root: &[u8]) -> bool {
    let mut ok = true;
    let own = p!(root, b"/ownfile\0");
    let times: [i64; 4] = [1_000_000, 111, 2_000_000, 222];
    ok &= step(raw_utimensat(own.as_ptr(), times.as_ptr(), 0) == 0, b"utimensat explicit as owner");
    match stat_times(own.as_ptr(), true) {
        Some((a, m)) => ok &= step(a == (1_000_000, 111) && m == (2_000_000, 222), b"stat reads the explicit times back"),
        None => ok &= step(false, b"stat after utimensat"),
    }
    let omit_a: [i64; 4] = [0, UTIME_OMIT, 3_000_000, 333];
    ok &= step(raw_utimensat(own.as_ptr(), omit_a.as_ptr(), 0) == 0, b"utimensat UTIME_OMIT atime");
    match stat_times(own.as_ptr(), true) {
        Some((a, m)) => ok &= step(a == (1_000_000, 111) && m == (3_000_000, 333), b"UTIME_OMIT left atime alone"),
        None => ok &= step(false, b"stat after UTIME_OMIT"),
    }
    // Both omitted: a no-op that succeeds even where the caller has no rights.
    let omit_both: [i64; 4] = [0, UTIME_OMIT, 0, UTIME_OMIT];
    ok &= step(raw_utimensat(p!(root, b"/rootfile\0").as_ptr(), omit_both.as_ptr(), 0) == 0, b"both UTIME_OMIT is a no-op");
    // NULL times = now: must be later than the explicit stamp above and
    // never zero.
    ok &= step(raw_utimensat(own.as_ptr(), core::ptr::null(), 0) == 0, b"utimensat NULL as owner");
    match stat_times(own.as_ptr(), true) {
        Some((a, m)) => ok &= step(a.0 >= 0 && m.0 >= 0 && m != (3_000_000, 333) && a == m, b"NULL set both to now"),
        None => ok &= step(false, b"stat after NULL"),
    }
    // futimens through an fd.
    let fd = open(own.as_ptr(), O_RDWR, 0);
    ok &= step(fd >= 0, b"open ownfile for futimens");
    if fd >= 0 {
        let t: [i64; 4] = [4_000_000, 444, 5_000_000, 555];
        ok &= step(raw_futimens(fd, t.as_ptr()) == 0, b"futimens explicit as owner");
        close(fd);
        match stat_times(own.as_ptr(), true) {
            Some((a, m)) => ok &= step(a == (4_000_000, 444) && m == (5_000_000, 555), b"futimens landed"),
            None => ok &= step(false, b"stat after futimens"),
        }
    }
    // AT_SYMLINK_NOFOLLOW: the link's own timestamps change, the target's don't.
    let link = p!(root, b"/ownlink\0");
    let lt: [i64; 4] = [6_000_000, 666, 7_000_000, 777];
    ok &= step(raw_utimensat(link.as_ptr(), lt.as_ptr(), AT_SYMLINK_NOFOLLOW) == 0, b"utimensat NOFOLLOW on link");
    match (stat_times(link.as_ptr(), false), stat_times(own.as_ptr(), true)) {
        (Some((la, lm)), Some((_, m))) => {
            ok &= step(la == (6_000_000, 666) && lm == (7_000_000, 777), b"link carries its own times");
            ok &= step(m == (5_000_000, 555), b"target untouched by NOFOLLOW");
        }
        _ => ok &= step(false, b"lstat/stat after NOFOLLOW"),
    }
    // Following the link stamps the target.
    let ft: [i64; 4] = [8_000_000, 888, 9_000_000, 999];
    ok &= step(raw_utimensat(link.as_ptr(), ft.as_ptr(), 0) == 0, b"utimensat through link");
    match stat_times(own.as_ptr(), true) {
        Some((a, m)) => ok &= step(a == (8_000_000, 888) && m == (9_000_000, 999), b"target stamped through link"),
        None => ok &= step(false, b"stat after follow"),
    }
    // Not the owner: 0644 root file.
    let rf = p!(root, b"/rootfile\0");
    ok &= step(fails_with(raw_utimensat(rf.as_ptr(), times.as_ptr(), 0), EPERM), b"explicit times on foreign file is EPERM");
    ok &= step(fails_with(raw_utimensat(rf.as_ptr(), core::ptr::null(), 0), EACCES), b"NULL on unwritable foreign file is EACCES");
    // Not the owner, but writable: 0666 root file.
    let rw = p!(root, b"/rw666\0");
    ok &= step(raw_utimensat(rw.as_ptr(), core::ptr::null(), 0) == 0, b"NULL on writable foreign file is allowed");
    let now_both: [i64; 4] = [0, UTIME_NOW, 0, UTIME_NOW];
    ok &= step(raw_utimensat(rw.as_ptr(), now_both.as_ptr(), 0) == 0, b"UTIME_NOW pair on writable foreign file is allowed");
    ok &= step(fails_with(raw_utimensat(rw.as_ptr(), times.as_ptr(), 0), EPERM), b"explicit times on writable foreign file is still EPERM");
    // Bad nanoseconds.
    let bad: [i64; 4] = [0, 1_000_000_000, 0, 0];
    ok &= step(fails_with(raw_utimensat(own.as_ptr(), bad.as_ptr(), 0), EINVAL), b"nsec out of range is EINVAL");
    ok
}

/// Root may set explicit times on anything.
unsafe fn case_utimens_root(root: &[u8]) -> bool {
    let times: [i64; 4] = [10_000_000, 1, 11_000_000, 2];
    let own = p!(root, b"/ownfile\0");
    let mut ok = step(raw_utimensat(own.as_ptr(), times.as_ptr(), 0) == 0, b"root utimensat on a user file");
    match stat_times(own.as_ptr(), true) {
        Some((a, m)) => ok &= step(a == (10_000_000, 1) && m == (11_000_000, 2), b"root's times landed"),
        None => ok &= step(false, b"stat after root utimensat"),
    }
    ok
}

// ── default ACL inheritance ──────────────────────────────────────────────────

/// `system.posix_acl_default` on <root>/dacl: USER_OBJ rwx, USER(1001) rwx,
/// GROUP_OBJ r-x, MASK rwx, OTHER ---.
unsafe fn set_default_acl(dir: *const u8) -> bool {
    let mut v = [0u8; 4 + 5 * 8];
    v[..4].copy_from_slice(&2u32.to_le_bytes());
    let entries: [(u16, u16, u32); 5] = [(0x01, 7, u32::MAX), (0x02, 7, UID_OTHER), (0x04, 5, u32::MAX),
                                        (0x10, 7, u32::MAX), (0x20, 0, u32::MAX)];
    for (i, (tag, perm, id)) in entries.iter().enumerate() {
        let o = 4 + i * 8;
        v[o..o + 2].copy_from_slice(&tag.to_le_bytes());
        v[o + 2..o + 4].copy_from_slice(&perm.to_le_bytes());
        v[o + 4..o + 8].copy_from_slice(&id.to_le_bytes());
    }
    raw_setxattr(dir, b"system.posix_acl_default\0".as_ptr(), v.as_ptr(), v.len()) == 0
}

/// Length of the named ACL xattr on `path`, or -1 when absent.
unsafe fn acl_len(path: *const u8, name: &[u8]) -> isize {
    let mut buf = [0u8; 256];
    raw_getxattr(path, name.as_ptr(), buf.as_mut_ptr(), buf.len())
}

/// As uid 1000 with umask 022: a file created 0666 under dacl gets mode 0660
/// (the ACL's group/mask r-x∧rw- = rw-, other ---; the umask would have said
/// 0644) and an access ACL naming uid 1001; a directory created 0777 gets
/// 0770, the same access ACL, and the default ACL itself; and its own child
/// inherits again.
unsafe fn case_default_acl_inherit(root: &[u8]) -> bool {
    let mut ok = true;
    raw_umask(0o022);
    let f = p!(root, b"/dacl/f\0");
    ok &= step(create(f.as_ptr(), 0o666), b"create file under default-ACL dir");
    ok &= step(stat_mode(f.as_ptr()) & 0o777 == 0o660, b"file mode comes from the default ACL, not the umask");
    ok &= step(acl_len(f.as_ptr(), b"system.posix_acl_access\0") == 4 + 5 * 8, b"file inherited an access ACL");
    ok &= step(acl_len(f.as_ptr(), b"system.posix_acl_default\0") == -1, b"file has no default ACL");
    let d = p!(root, b"/dacl/sub\0");
    ok &= step(mkdir(d.as_ptr(), 0o777) == 0, b"mkdir under default-ACL dir");
    ok &= step(stat_mode(d.as_ptr()) & 0o777 == 0o770, b"dir mode comes from the default ACL");
    ok &= step(acl_len(d.as_ptr(), b"system.posix_acl_access\0") == 4 + 5 * 8, b"dir inherited an access ACL");
    ok &= step(acl_len(d.as_ptr(), b"system.posix_acl_default\0") == 4 + 5 * 8, b"dir inherited the default ACL");
    let d2 = p!(root, b"/dacl/sub/subsub\0");
    ok &= step(mkdir(d2.as_ptr(), 0o755) == 0, b"mkdir two levels down");
    ok &= step(stat_mode(d2.as_ptr()) & 0o777 == 0o750, b"grandchild dir mode from the inherited default ACL");
    ok &= step(acl_len(d2.as_ptr(), b"system.posix_acl_default\0") == 4 + 5 * 8, b"grandchild inherited the default ACL");
    let f2 = p!(root, b"/dacl/sub/subsub/f\0");
    ok &= step(create(f2.as_ptr(), 0o644), b"create file two levels down");
    ok &= step(stat_mode(f2.as_ptr()) & 0o777 == 0o640, b"grandchild file mode from the inherited default ACL");
    // A file outside the ACL'd tree still obeys the umask.
    let g = p!(root, b"/home/g\0");
    ok &= step(create(g.as_ptr(), 0o666), b"create control file");
    ok &= step(stat_mode(g.as_ptr()) & 0o777 == 0o644, b"control file obeys the umask");
    raw_umask(0);
    ok
}

/// As uid 1001, named in the inherited access ACL: the file created by uid
/// 1000 is readable and writable although its mode says other=---. As uid
/// 1002 it is not.
unsafe fn case_default_acl_named_user(root: &[u8]) -> bool {
    let f = p!(root, b"/dacl/f\0");
    let fd = open(f.as_ptr(), O_RDWR, 0);
    let ok = step(fd >= 0, b"named user opens the inherited-ACL file rw");
    if fd >= 0 { close(fd); }
    ok
}
unsafe fn case_default_acl_other_denied(root: &[u8]) -> bool {
    let f = p!(root, b"/dacl/f\0");
    step(fails_with(open(f.as_ptr(), O_RDONLY, 0) as isize, EACCES), b"unnamed user is denied by the inherited ACL")
}

// ── driver ───────────────────────────────────────────────────────────────────

unsafe fn run_matrix(root: &[u8], tag: &[u8], with_sockets: bool, with_exec: bool) -> u32 {
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

    case!(b"group_denied_without_membership", UID_USER, case_group_denied);
    {
        let n = named(b"group_allowed_supplementary");
        let nm = core::slice::from_raw_parts(n, 27 + tag.len());
        if !report(nm, run_as_groups(UID_USER, UID_USER, &[GID_VIDEO], case_group_allowed, root)) { failures += 1; }
    }

    if with_exec {
        if setup_exec(root) {
            case!(b"exec_xbit_user", UID_USER, case_exec_xbit_user);
            case!(b"exec_xbit_root", 0, case_exec_xbit_root);
        } else if !report(core::slice::from_raw_parts(named(b"exec_setup"), 10 + tag.len()), false) {
            failures += 1;
        }
    }

    case!(b"utimens_rules", UID_USER, case_utimens);
    case!(b"utimens_root", 0, case_utimens_root);

    case!(b"default_acl_inherit", UID_USER, case_default_acl_inherit);
    case!(b"default_acl_named_user", UID_OTHER, case_default_acl_named_user);
    case!(b"default_acl_other_denied", 1002, case_default_acl_other_denied);

    teardown(root);
    failures
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    // `permtest --groups <gid>`: the exec'd probe of case_group_allowed.
    if argc >= 3 {
        let a1 = *argv.add(1);
        let mut n = 0usize;
        while *a1.add(n) != 0 { n += 1; }
        if core::slice::from_raw_parts(a1, n) == b"--groups" {
            let a2 = *argv.add(2);
            let mut want = 0u32;
            let mut i = 0usize;
            while *a2.add(i) != 0 { want = want * 10 + (*a2.add(i) - b'0') as u32; i += 1; }
            return groups_probe(want);
        }
    }
    if geteuid() != 0 {
        puts(b"permtest: must run as root\0".as_ptr());
        return 1;
    }
    // Exact modes: the fixture's 0777/1777/0700 must land as written.
    raw_umask(0);

    let mut failures = 0u32;
    failures += run_matrix(b"/data/pt", b"_f2fs", false, true);
    failures += run_matrix(b"/tmp/pt", b"_tmpfs", true, false);

    out(b"--- permtest done: ");
    out_dec(failures);
    out(b" failure(s) ---\n");
    failures as i32
}
