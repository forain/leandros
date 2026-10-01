//! vfstest — regression coverage for the VFS server: rmdir,
//! cross-mount-capable rename, advisory locking (flock + fcntl byte-range),
//! real file permissions/ownership (including setuid privilege drop), and
//! extended attributes / POSIX ACLs (setxattr/getxattr/listxattr/removexattr
//! and their l*/f* variants, plus ACL-driven access enforcement).
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); `main` returns the number of failures as the exit code.
//!
//! Note: this kernel's `wait4()` reports the Linux-encoded `wstatus` (exit
//! code in bits 8..16, `WEXITSTATUS(status) == status >> 8`), not a child's
//! raw `exit()` argument. Tests below only ever compare `wstatus` to 0, which
//! is 0 either way, so the encoding doesn't matter here — but don't compare
//! it to a bare 1 expecting `exit(1)` to show up as that.

#![no_std]
#![no_main]

extern crate leandros_libc;
use leandros_libc::*;
use leandros_libc::syscall::{syscall1, syscall2, syscall3, syscall4, syscall5};

// chroot(2) and symlink(2) are not (yet) wrapped by leandros-libc, so this
// test makes the raw syscalls directly, matching the style of
// `userland/libc/src/syscall.rs`'s per-arch `nr` table. Numbers verified
// against the kernel's own dispatch tables in `kernel/src/syscall.rs`
// (`nr::CHROOT` / `nr::SYMLINKAT` in the AArch64 and x86-64 `mod nr` blocks),
// and match the standard Linux syscall ABI these wrappers already assume.
#[cfg(target_arch = "aarch64")] const SYS_CHROOT: usize = 51;
#[cfg(target_arch = "x86_64")]  const SYS_CHROOT: usize = 161;
#[cfg(target_arch = "aarch64")] const SYS_SYMLINKAT: usize = 36;
#[cfg(target_arch = "x86_64")]  const SYS_SYMLINKAT: usize = 266;
#[cfg(target_arch = "aarch64")] const SYS_RENAMEAT2: usize = 276;
#[cfg(target_arch = "x86_64")]  const SYS_RENAMEAT2: usize = 316;
const RENAME_NOREPLACE: usize = 1;

/// Change the process's filesystem root. Irreversible for the calling
/// process, so callers that need to keep operating outside the jail must
/// confine the call to a forked child.
unsafe fn raw_chroot(path: *const u8) -> i32 {
    let r = syscall1(SYS_CHROOT, path as usize);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

/// Create a symlink at `linkpath` pointing to `target`. Argument order
/// mirrors `symlinkat(target, dirfd, linkpath)`, as dispatched by the
/// kernel's `sys_symlinkat`.
unsafe fn raw_symlink(target: *const u8, linkpath: *const u8) -> i32 {
    let r = syscall3(SYS_SYMLINKAT, target as usize, AT_FDCWD as usize, linkpath as usize);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

// ── xattr(2) family + POSIX ACLs (TODO.md item: extended attributes) ───────
//
// setxattr/getxattr/listxattr/removexattr and their l*/f* variants are not
// (yet) wrapped by leandros-libc, so — following the raw_chroot/raw_symlink
// pattern above — this test makes the raw syscalls directly. Numbers match
// the kernel's own `nr::SETXATTR`..`nr::FREMOVEXATTR` dispatch table in
// `kernel/src/syscall.rs` (AArch64 5-16, x86-64 188-199, both in the same
// setxattr/lsetxattr/fsetxattr/getxattr/lgetxattr/fgetxattr/listxattr/
// llistxattr/flistxattr/removexattr/lremovexattr/fremovexattr order Linux
// uses). `struct stat`/`faccessat` are needed too, to check ACL-driven group
// mode bits and ACL-enforced access denial; also not yet wrapped.
#[cfg(target_arch = "aarch64")] const SYS_SETXATTR:     usize = 5;
#[cfg(target_arch = "aarch64")] const SYS_LSETXATTR:    usize = 6;
#[cfg(target_arch = "aarch64")] const SYS_FSETXATTR:    usize = 7;
#[cfg(target_arch = "aarch64")] const SYS_GETXATTR:     usize = 8;
#[cfg(target_arch = "aarch64")] const SYS_LGETXATTR:    usize = 9;
#[cfg(target_arch = "aarch64")] const SYS_FGETXATTR:    usize = 10;
#[cfg(target_arch = "aarch64")] const SYS_LISTXATTR:    usize = 11;
#[cfg(target_arch = "aarch64")] const SYS_LLISTXATTR:   usize = 12;
#[cfg(target_arch = "aarch64")] const SYS_FLISTXATTR:   usize = 13;
#[cfg(target_arch = "aarch64")] const SYS_REMOVEXATTR:  usize = 14;
#[cfg(target_arch = "aarch64")] const SYS_LREMOVEXATTR: usize = 15;
#[cfg(target_arch = "aarch64")] const SYS_FREMOVEXATTR: usize = 16;

#[cfg(target_arch = "x86_64")] const SYS_SETXATTR:     usize = 188;
#[cfg(target_arch = "x86_64")] const SYS_LSETXATTR:    usize = 189;
#[cfg(target_arch = "x86_64")] const SYS_FSETXATTR:    usize = 190;
#[cfg(target_arch = "x86_64")] const SYS_GETXATTR:     usize = 191;
#[cfg(target_arch = "x86_64")] const SYS_LGETXATTR:    usize = 192;
#[cfg(target_arch = "x86_64")] const SYS_FGETXATTR:    usize = 193;
#[cfg(target_arch = "x86_64")] const SYS_LISTXATTR:    usize = 194;
#[cfg(target_arch = "x86_64")] const SYS_LLISTXATTR:   usize = 195;
#[cfg(target_arch = "x86_64")] const SYS_FLISTXATTR:   usize = 196;
#[cfg(target_arch = "x86_64")] const SYS_REMOVEXATTR:  usize = 197;
#[cfg(target_arch = "x86_64")] const SYS_LREMOVEXATTR: usize = 198;
#[cfg(target_arch = "x86_64")] const SYS_FREMOVEXATTR: usize = 199;

#[cfg(target_arch = "aarch64")] const SYS_NEWFSTATAT: usize = 79;
#[cfg(target_arch = "x86_64")]  const SYS_NEWFSTATAT: usize = 262;
#[cfg(target_arch = "aarch64")] const SYS_FACCESSAT:  usize = 48;
#[cfg(target_arch = "x86_64")]  const SYS_FACCESSAT:  usize = 269;

// `struct stat` layout (see servers/vfs/src/lib.rs's own comment above its
// `STAT_SIZE`/`st_mode`-offset constants, which this mirrors): the 128-byte
// asm-generic layout on AArch64, x86-64's native 144-byte layout elsewhere.
#[cfg(target_arch = "aarch64")] const STAT_SIZE: usize = 128;
#[cfg(target_arch = "x86_64")]  const STAT_SIZE: usize = 144;
#[cfg(target_arch = "aarch64")] const STAT_MODE_OFF: usize = 16;
#[cfg(target_arch = "x86_64")]  const STAT_MODE_OFF: usize = 24;

const XATTR_CREATE:  i32 = 1;
const XATTR_REPLACE: i32 = 2;

// errno values not yet in leandros-libc's errno module (kept local, same as
// the errno consts already re-exported from there follow POSIX numbering).
const ENODATA:    i32 = 61;
const EOPNOTSUPP: i32 = 95;
const ERANGE:     i32 = 34;

const R_OK: i32 = 4;

fn xret(r: isize) -> isize {
    if r < 0 { set_errno(-r as i32); -1 } else { r }
}

unsafe fn raw_setxattr(path: *const u8, name: *const u8, value: *const u8, size: usize, flags: i32) -> isize {
    xret(syscall5(SYS_SETXATTR, path as usize, name as usize, value as usize, size, flags as usize))
}
unsafe fn raw_lsetxattr(path: *const u8, name: *const u8, value: *const u8, size: usize, flags: i32) -> isize {
    xret(syscall5(SYS_LSETXATTR, path as usize, name as usize, value as usize, size, flags as usize))
}
unsafe fn raw_fsetxattr(fd: i32, name: *const u8, value: *const u8, size: usize, flags: i32) -> isize {
    xret(syscall5(SYS_FSETXATTR, fd as usize, name as usize, value as usize, size, flags as usize))
}
unsafe fn raw_getxattr(path: *const u8, name: *const u8, buf: *mut u8, size: usize) -> isize {
    xret(syscall4(SYS_GETXATTR, path as usize, name as usize, buf as usize, size))
}
unsafe fn raw_lgetxattr(path: *const u8, name: *const u8, buf: *mut u8, size: usize) -> isize {
    xret(syscall4(SYS_LGETXATTR, path as usize, name as usize, buf as usize, size))
}
unsafe fn raw_fgetxattr(fd: i32, name: *const u8, buf: *mut u8, size: usize) -> isize {
    xret(syscall4(SYS_FGETXATTR, fd as usize, name as usize, buf as usize, size))
}
unsafe fn raw_listxattr(path: *const u8, buf: *mut u8, size: usize) -> isize {
    xret(syscall3(SYS_LISTXATTR, path as usize, buf as usize, size))
}
unsafe fn raw_llistxattr(path: *const u8, buf: *mut u8, size: usize) -> isize {
    xret(syscall3(SYS_LLISTXATTR, path as usize, buf as usize, size))
}
unsafe fn raw_flistxattr(fd: i32, buf: *mut u8, size: usize) -> isize {
    xret(syscall3(SYS_FLISTXATTR, fd as usize, buf as usize, size))
}
unsafe fn raw_removexattr(path: *const u8, name: *const u8) -> isize {
    xret(syscall2(SYS_REMOVEXATTR, path as usize, name as usize))
}
unsafe fn raw_lremovexattr(path: *const u8, name: *const u8) -> isize {
    xret(syscall2(SYS_LREMOVEXATTR, path as usize, name as usize))
}
unsafe fn raw_fremovexattr(fd: i32, name: *const u8) -> isize {
    xret(syscall2(SYS_FREMOVEXATTR, fd as usize, name as usize))
}

/// Fetch `st_mode` (type + permission bits) for `path`, following symlinks.
unsafe fn raw_mode(path: *const u8) -> i32 {
    let mut buf = [0u8; STAT_SIZE];
    let r = syscall4(SYS_NEWFSTATAT, AT_FDCWD as usize, path as usize, buf.as_mut_ptr() as usize, 0);
    if r < 0 { set_errno(-r as i32); return -1; }
    let p = buf.as_ptr().add(STAT_MODE_OFF) as *const u32;
    core::ptr::read_unaligned(p) as i32
}

#[cfg(target_arch = "aarch64")] const SYS_UTIMENSAT: usize = 88;
#[cfg(target_arch = "x86_64")]  const SYS_UTIMENSAT: usize = 280;
// st_mtim (sec, nsec as i64) at 88 on both ABIs — servers/vfs write_stat_times.
const STAT_MTIME_OFF: usize = 88;
// st_atim precedes st_mtim by one (sec, nsec) pair — servers/vfs write_stat_times.
const STAT_ATIME_OFF: usize = 72;

/// `st_mtim` of `path` as (sec, nsec), or None when stat fails.
unsafe fn raw_mtime(path: *const u8) -> Option<(i64, i64)> {
    let mut buf = [0u8; STAT_SIZE];
    let r = syscall4(SYS_NEWFSTATAT, AT_FDCWD as usize, path as usize, buf.as_mut_ptr() as usize, 0);
    if r < 0 { set_errno(-r as i32); return None; }
    let sec  = core::ptr::read_unaligned(buf.as_ptr().add(STAT_MTIME_OFF) as *const i64);
    let nsec = core::ptr::read_unaligned(buf.as_ptr().add(STAT_MTIME_OFF + 8) as *const i64);
    Some((sec, nsec))
}

/// `st_atim` of `path` as (sec, nsec), or None when stat fails.
unsafe fn raw_atime(path: *const u8) -> Option<(i64, i64)> {
    let mut buf = [0u8; STAT_SIZE];
    let r = syscall4(SYS_NEWFSTATAT, AT_FDCWD as usize, path as usize, buf.as_mut_ptr() as usize, 0);
    if r < 0 { set_errno(-r as i32); return None; }
    let sec  = core::ptr::read_unaligned(buf.as_ptr().add(STAT_ATIME_OFF) as *const i64);
    let nsec = core::ptr::read_unaligned(buf.as_ptr().add(STAT_ATIME_OFF + 8) as *const i64);
    Some((sec, nsec))
}

unsafe fn raw_utimensat(path: *const u8, times: *const i64) -> i32 {
    let r = syscall4(SYS_UTIMENSAT, AT_FDCWD as usize, path as usize, times as usize, 0);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

/// Timestamps are real, on both backends: a new file's mtime is "now" (the
/// same clock `clock_gettime` reads — this kernel's CLOCK_REALTIME counts
/// from boot, so the two agree to within the call gap), a write moves it
/// forward, and utimensat sets it to the nanosecond. Before this, stat
/// reported every timestamp as 0 and utimensat was a kernel no-op.
unsafe fn test_timestamps(root: &[u8], name: &[u8]) -> bool {
    let mut b = [0u8; 96];
    let path = mkpath(&mut b, root, b"_ts");
    unlink(path);
    let mut t0 = timespec { tv_sec: 0, tv_nsec: 0 };
    clock_gettime(0, &mut t0); // CLOCK_REALTIME
    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    let m1 = match raw_mtime(path) { Some(m) => m, None => { close(fd); return report(name, false); } };
    let ns = |t: (i64, i64)| t.0 * 1_000_000_000 + t.1;
    let t0n = t0.tv_sec * 1_000_000_000 + t0.tv_nsec;
    // Created "now": not zero, not before the clock we read just before, and
    // not more than a second after it.
    if ns(m1) == 0 || ns(m1) < t0n - 20_000_000 || ns(m1) > t0n + 1_000_000_000 {
        close(fd); unlink(path); return report(name, false);
    }
    usleep(30_000); // well past the 10 ms tick either way
    if write(fd, b"x".as_ptr(), 1) != 1 { close(fd); unlink(path); return report(name, false); }
    close(fd);
    let m2 = match raw_mtime(path) { Some(m) => m, None => { unlink(path); return report(name, false); } };
    if ns(m2) <= ns(m1) { unlink(path); return report(name, false); }
    // Exact set.
    let times: [i64; 4] = [123, 456, 789_000, 12_345];
    if raw_utimensat(path, times.as_ptr()) != 0 { unlink(path); return report(name, false); }
    let m3 = raw_mtime(path);
    unlink(path);
    report(name, m3 == Some((789_000, 12_345)))
}

/// `faccessat(AT_FDCWD, path, mode, flags)`.
unsafe fn raw_faccessat_flags(path: *const u8, mode: i32, flags: i32) -> i32 {
    let r = syscall4(SYS_FACCESSAT, AT_FDCWD as usize, path as usize, mode as usize, flags as usize);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

/// `faccessat(AT_FDCWD, path, mode, 0)` — used to probe ACL-enforced access.
unsafe fn raw_faccessat(path: *const u8, mode: i32) -> i32 {
    raw_faccessat_flags(path, mode, 0)
}

/// Linux `AT_EACCESS`: check with the caller's effective ids instead of the
/// real ones. Plain `access()`/`faccessat()` without it (flags == 0) is the
/// POSIX default: real ids.
const AT_EACCESS: i32 = 0x200;

/// Build a NUL-terminated path by concatenating `root` and `suffix` (neither
/// includes its own terminator) into `buf`.
fn mkpath<'a>(buf: &'a mut [u8; 96], root: &[u8], suffix: &[u8]) -> *const u8 {
    let mut i = 0;
    for &b in root { buf[i] = b; i += 1; }
    for &b in suffix { buf[i] = b; i += 1; }
    buf[i] = 0;
    buf.as_ptr()
}

/// Build the *basename* of `root` concatenated with `suffix`: the relative
/// symlink body that names the same file `mkpath(root, suffix)` names as an
/// absolute path (both hang off the same parent directory). For root
/// "/tmp/xa" and suffix "_symtarget" this is "xa_symtarget".
fn rel_basename<'a>(buf: &'a mut [u8; 96], root: &[u8], suffix: &[u8]) -> *const u8 {
    let start = root.iter().rposition(|&b| b == b'/').map(|p| p + 1).unwrap_or(0);
    let mut i = 0;
    for &b in &root[start..] { buf[i] = b; i += 1; }
    for &b in suffix { buf[i] = b; i += 1; }
    buf[i] = 0;
    buf.as_ptr()
}

/// Whether NUL-separated `buf[..len]` (as returned by listxattr) contains
/// `name` as one of its entries — order-insensitive.
fn contains_name(buf: &[u8], len: usize, name: &[u8]) -> bool {
    let data = &buf[..len];
    let mut start = 0;
    for i in 0..data.len() {
        if data[i] == 0 {
            if &data[start..i] == name { return true; }
            start = i + 1;
        }
    }
    false
}

// POSIX ACL wire format (little-endian): u32 version, then 8-byte entries
// {u16 e_tag, u16 e_perm, u32 e_id}; e_id is ACL_UNDEFINED_ID for tags that
// aren't qualified by a uid/gid.
const ACL_USER_OBJ:  u16 = 1;
const ACL_USER:      u16 = 2;
const ACL_GROUP_OBJ: u16 = 4;
#[allow(dead_code)]
const ACL_GROUP:     u16 = 8;
const ACL_MASK:      u16 = 0x10;
const ACL_OTHER:     u16 = 0x20;
const ACL_UNDEFINED_ID: u32 = 0xFFFFFFFF;

/// Encode `entries` (already in canonical order: USER_OBJ, USER*, GROUP_OBJ,
/// GROUP*, MASK, OTHER) into the wire format above. Returns the byte length.
fn build_acl(buf: &mut [u8], version: u32, entries: &[(u16, u16, u32)]) -> usize {
    buf[0..4].copy_from_slice(&version.to_le_bytes());
    let mut off = 4;
    for &(tag, perm, id) in entries {
        buf[off..off + 2].copy_from_slice(&tag.to_le_bytes());
        buf[off + 2..off + 4].copy_from_slice(&perm.to_le_bytes());
        buf[off + 4..off + 8].copy_from_slice(&id.to_le_bytes());
        off += 8;
    }
    off
}

/// The non-trivial ACL shared by tests 8/9: root (USER_OBJ) gets rwx, uid
/// 1000 (a named USER entry) is explicitly denied all access, GROUP_OBJ/
/// OTHER get rx, capped (and mirrored into the group mode bits) by an rx
/// MASK.
fn build_enforcing_acl(buf: &mut [u8]) -> usize {
    build_acl(buf, 2, &[
        (ACL_USER_OBJ,  0o7, ACL_UNDEFINED_ID),
        (ACL_USER,      0o0, 1000),
        (ACL_GROUP_OBJ, 0o5, ACL_UNDEFINED_ID),
        (ACL_MASK,      0o5, ACL_UNDEFINED_ID),
        (ACL_OTHER,     0o5, ACL_UNDEFINED_ID),
    ])
}

/// (1) Basic user.* set/get round-trip on a plain file: exact-size read, a
/// zero-size length query, and a too-small buffer reporting ERANGE.
unsafe fn test_xattr_basic(root: &[u8], name: &[u8]) -> bool {
    let mut pb = [0u8; 96];
    let path = mkpath(&mut pb, root, b"_basic");

    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    close(fd);

    if raw_setxattr(path, b"user.test\0".as_ptr(), b"hello".as_ptr(), 5, 0) != 0 {
        return report(name, false);
    }

    let mut buf = [0u8; 32];
    let n = raw_getxattr(path, b"user.test\0".as_ptr(), buf.as_mut_ptr(), buf.len());
    if n != 5 || &buf[..5] != b"hello" { return report(name, false); }

    let n0 = raw_getxattr(path, b"user.test\0".as_ptr(), core::ptr::null_mut(), 0);
    if n0 != 5 { return report(name, false); }

    let mut small = [0u8; 2];
    let ns = raw_getxattr(path, b"user.test\0".as_ptr(), small.as_mut_ptr(), 2);
    report(name, ns == -1 && get_errno() == ERANGE)
}

/// (2) Getting a never-set attribute fails ENODATA; setting an attribute in
/// an unrecognised namespace fails EOPNOTSUPP.
unsafe fn test_xattr_missing_and_unsupported(root: &[u8], name: &[u8]) -> bool {
    let mut pb = [0u8; 96];
    let path = mkpath(&mut pb, root, b"_missing");

    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    close(fd);

    let mut buf = [0u8; 16];
    let g = raw_getxattr(path, b"user.missing\0".as_ptr(), buf.as_mut_ptr(), buf.len());
    if g != -1 || get_errno() != ENODATA { return report(name, false); }

    let s = raw_setxattr(path, b"foo.bar\0".as_ptr(), b"x".as_ptr(), 1, 0);
    report(name, s == -1 && get_errno() == EOPNOTSUPP)
}

/// (3) listxattr enumerates every set name (order-insensitive), a size==0
/// query reports the same total length as a real read, and a freshly
/// created file lists 0.
unsafe fn test_xattr_list(root: &[u8], name: &[u8]) -> bool {
    let mut pb = [0u8; 96];
    let path = mkpath(&mut pb, root, b"_list");

    // Idempotent for a re-run in the same boot: setxattr/removexattr leave
    // their marks on the *inode*, not the file's byte content, so
    // O_CREAT|O_TRUNC against a leftover inode from an earlier run reopens
    // that same inode with its xattrs intact (this matches real filesystem
    // semantics — truncating a file never clears its xattrs). The "starts
    // empty" assumption below only holds for a genuinely fresh inode, so
    // unlink the leftover first (best-effort; ENOENT on a first run) to
    // force a new one.
    unlink(path);
    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    close(fd);

    let empty = raw_listxattr(path, core::ptr::null_mut(), 0);
    if empty != 0 { return report(name, false); }

    if raw_setxattr(path, b"user.a\0".as_ptr(), b"1".as_ptr(), 1, 0) != 0 { return report(name, false); }
    if raw_setxattr(path, b"user.b\0".as_ptr(), b"22".as_ptr(), 2, 0) != 0 { return report(name, false); }

    let want_len = "user.a\0".len() + "user.b\0".len();
    let len0 = raw_listxattr(path, core::ptr::null_mut(), 0);
    if len0 < 0 || len0 as usize != want_len { return report(name, false); }

    let mut buf = [0u8; 64];
    let len = raw_listxattr(path, buf.as_mut_ptr(), buf.len());
    if len < 0 || len as usize != want_len { return report(name, false); }

    report(name,
        contains_name(&buf, len as usize, b"user.a")
        && contains_name(&buf, len as usize, b"user.b"))
}

/// (4) XATTR_CREATE refuses an already-existing attribute (EEXIST);
/// XATTR_REPLACE refuses a missing one (ENODATA).
unsafe fn test_xattr_create_replace(root: &[u8], name: &[u8]) -> bool {
    let mut pb = [0u8; 96];
    let path = mkpath(&mut pb, root, b"_cr");

    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    close(fd);

    if raw_setxattr(path, b"user.x\0".as_ptr(), b"1".as_ptr(), 1, 0) != 0 { return report(name, false); }

    let create_existing = raw_setxattr(path, b"user.x\0".as_ptr(), b"2".as_ptr(), 1, XATTR_CREATE);
    if create_existing != -1 || get_errno() != EEXIST { return report(name, false); }

    let replace_missing = raw_setxattr(path, b"user.y\0".as_ptr(), b"1".as_ptr(), 1, XATTR_REPLACE);
    report(name, replace_missing == -1 && get_errno() == ENODATA)
}

/// (5) removexattr deletes an attribute: a subsequent get reports ENODATA,
/// listxattr no longer includes it, and removing it again also fails
/// ENODATA.
unsafe fn test_xattr_remove(root: &[u8], name: &[u8]) -> bool {
    let mut pb = [0u8; 96];
    let path = mkpath(&mut pb, root, b"_rm");

    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    close(fd);

    if raw_setxattr(path, b"user.z\0".as_ptr(), b"v\0".as_ptr(), 1, 0) != 0 { return report(name, false); }
    if raw_removexattr(path, b"user.z\0".as_ptr()) != 0 { return report(name, false); }

    let mut buf = [0u8; 16];
    if raw_getxattr(path, b"user.z\0".as_ptr(), buf.as_mut_ptr(), buf.len()) != -1
        || get_errno() != ENODATA { return report(name, false); }

    let mut lbuf = [0u8; 32];
    let llen = raw_listxattr(path, lbuf.as_mut_ptr(), lbuf.len());
    if llen < 0 || contains_name(&lbuf, llen as usize, b"user.z") { return report(name, false); }

    report(name, raw_removexattr(path, b"user.z\0".as_ptr()) == -1 && get_errno() == ENODATA)
}

/// (6) user.* is forbidden on the symlink object itself (lsetxattr → EPERM,
/// lremovexattr also fails), but plain setxattr through the same path
/// follows the link and mutates the *target*'s attributes, leaving the link
/// object itself with none of its own.
/// `relative_link` picks the symlink-target form each backend can resolve
/// today: tmpfs follows absolute targets but misresolves relative ones,
/// f2fs follows relative targets but can't re-anchor absolute ones outside
/// the volume (both are pre-existing open()-path gaps, not xattr behavior —
/// see the open-issues notes).
unsafe fn test_xattr_symlink(root: &[u8], name: &[u8], relative_link: bool) -> bool {
    let mut tb = [0u8; 96];
    let target = mkpath(&mut tb, root, b"_symtarget");
    let mut lb = [0u8; 96];
    let link = mkpath(&mut lb, root, b"_symlink");

    // Idempotent: this runs once per (backend, body-form), so clear any link or
    // target a prior form left behind or the second symlink() would EEXIST.
    unlink(link);
    unlink(target);

    let fd = open(target, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    close(fd);

    // The relative body is the target's basename (both share the parent dir),
    // which for these roots is "<root-basename>_symtarget".
    let mut rb = [0u8; 96];
    let rel_body = rel_basename(&mut rb, root, b"_symtarget");
    let link_target: *const u8 = if relative_link { rel_body } else { target };
    if raw_symlink(link_target, link) != 0 { return report(name, false); }

    if raw_lsetxattr(link, b"user.a\0".as_ptr(), b"x".as_ptr(), 1, 0) != -1 || get_errno() != EPERM {
        return report(name, false);
    }
    if raw_lremovexattr(link, b"user.a\0".as_ptr()) != -1 { return report(name, false); }

    if raw_setxattr(link, b"user.a\0".as_ptr(), b"ok\0".as_ptr(), 2, 0) != 0 {
        return report(name, false);
    }

    let mut buf = [0u8; 8];
    let n = raw_getxattr(target, b"user.a\0".as_ptr(), buf.as_mut_ptr(), buf.len());
    if n != 2 || &buf[..2] != b"ok" { return report(name, false); }

    let lg = raw_lgetxattr(link, b"user.a\0".as_ptr(), buf.as_mut_ptr(), buf.len());
    if lg != -1 || get_errno() != ENODATA { return report(name, false); }

    let mut lbuf = [0u8; 16];
    let llen = raw_llistxattr(link, lbuf.as_mut_ptr(), lbuf.len());
    report(name, llen == 0)
}

/// Create `link -> body`, open it *following* the link, and check the bytes
/// read back equal `want`. The caller owns cleanup of `link`.
///
/// The read-back is the load-bearing assertion: a symlink that misresolves to
/// a wrong-but-existing empty node opens fine and returns 0 bytes at rc 0 — a
/// silent success. Comparing content, not just the open result, catches it.
unsafe fn symlink_reads_back(body: *const u8, link: *const u8, want: &[u8]) -> bool {
    if raw_symlink(body, link) != 0 { return false; }
    let fd = open(link, O_RDONLY, 0);
    if fd < 0 { return false; }
    let mut buf = [0u8; 64];
    let n = read(fd, buf.as_mut_ptr(), buf.len());
    close(fd);
    n == want.len() as isize && &buf[..want.len()] == want
}

/// A symlink must resolve to its target in BOTH body forms, and reading through
/// it must return the target's bytes:
///   * relative body — resolved against the link's own directory;
///   * absolute body — resolved from the process root, back through the mount
///     point (the f2fs case `ln -s /data/x l` inside /data that used to ENOENT).
/// Runs on whichever backend `root` names; reports the two forms separately.
unsafe fn test_symlink_read(root: &[u8], rel_name: &[u8], abs_name: &[u8]) -> bool {
    let mut tb = [0u8; 96];
    let target = mkpath(&mut tb, root, b"_rtgt");
    let mut lb = [0u8; 96];
    let link = mkpath(&mut lb, root, b"_rlink");
    let mut rb = [0u8; 96];
    let rel_body = rel_basename(&mut rb, root, b"_rtgt");

    unlink(link);
    unlink(target);

    let want = b"symlink-ok\n";
    let fd = open(target, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    let w = if fd < 0 { -1 } else { let r = write(fd, want.as_ptr(), want.len()); close(fd); r };
    if w != want.len() as isize {
        let a = report(rel_name, false);
        let b = report(abs_name, false);
        return a && b;
    }

    let rel_ok = symlink_reads_back(rel_body, link, want);
    unlink(link);
    let abs_ok = symlink_reads_back(target, link, want);
    unlink(link);
    unlink(target);

    let a = report(rel_name, rel_ok);
    let b = report(abs_name, abs_ok);
    a && b
}

/// A tmpfs symlink whose absolute body names a path on another mount (f2fs at
/// /data) resolves across the boundary — the VFS re-dispatches the resolved
/// path. LIMITATION: the reverse is unsupported. An f2fs symlink out to /tmp
/// resolves within the f2fs volume (its body does not strip to the volume) and
/// so ENOENTs; f2fs has no re-dispatch hook, so it is deliberately not tested.
unsafe fn test_symlink_cross_mount(name: &[u8]) -> bool {
    let target = b"/data/xsl_xtgt\0".as_ptr();
    let link   = b"/tmp/xsl_xlink\0".as_ptr();
    unlink(link);
    unlink(target);

    let want = b"cross-mount\n";
    let fd = open(target, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    let w = if fd < 0 { -1 } else { let r = write(fd, want.as_ptr(), want.len()); close(fd); r };
    if w != want.len() as isize { return report(name, false); }

    let ok = symlink_reads_back(target, link, want);
    unlink(link);
    unlink(target);
    report(name, ok)
}

/// (7) fsetxattr/fgetxattr/flistxattr/fremovexattr all operate on an
/// already-open fd, matching the path forms' behavior, on both backends.
unsafe fn test_xattr_fd(root: &[u8], name: &[u8]) -> bool {
    let mut pb = [0u8; 96];
    let path = mkpath(&mut pb, root, b"_fd");

    let fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }

    if raw_fsetxattr(fd, b"user.fd\0".as_ptr(), b"val".as_ptr(), 3, 0) != 0 {
        close(fd);
        return report(name, false);
    }

    let mut buf = [0u8; 8];
    let n = raw_fgetxattr(fd, b"user.fd\0".as_ptr(), buf.as_mut_ptr(), buf.len());
    if n != 3 || &buf[..3] != b"val" { close(fd); return report(name, false); }

    let mut lbuf = [0u8; 16];
    let llen = raw_flistxattr(fd, lbuf.as_mut_ptr(), lbuf.len());
    if llen < 0 || !contains_name(&lbuf, llen as usize, b"user.fd") {
        close(fd);
        return report(name, false);
    }

    if raw_fremovexattr(fd, b"user.fd\0".as_ptr()) != 0 { close(fd); return report(name, false); }
    let after = raw_fgetxattr(fd, b"user.fd\0".as_ptr(), buf.as_mut_ptr(), buf.len());
    close(fd);
    report(name, after == -1 && get_errno() == ENODATA)
}

/// (8) A non-trivial ACL (root full access, uid 1000 explicitly denied,
/// group/other rx via an rx MASK) is accepted, updates the file's group
/// mode bits to the MASK permissions, shows up in listxattr, and round-trips
/// byte-for-byte through getxattr.
unsafe fn test_xattr_acl_basic(root: &[u8], name: &[u8]) -> bool {
    let mut pb = [0u8; 96];
    let path = mkpath(&mut pb, root, b"_acl");

    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o755);
    if fd < 0 { return report(name, false); }
    close(fd);

    let mut acl = [0u8; 64];
    let acl_len = build_enforcing_acl(&mut acl);

    if raw_setxattr(path, b"system.posix_acl_access\0".as_ptr(), acl.as_ptr(), acl_len, 0) != 0 {
        return report(name, false);
    }

    let mode = raw_mode(path);
    if mode < 0 || (mode & 0o070) >> 3 != 0o5 { return report(name, false); }

    let mut lbuf = [0u8; 64];
    let llen = raw_listxattr(path, lbuf.as_mut_ptr(), lbuf.len());
    if llen < 0 || !contains_name(&lbuf, llen as usize, b"system.posix_acl_access") {
        return report(name, false);
    }

    let mut rbuf = [0u8; 64];
    let rlen = raw_getxattr(path, b"system.posix_acl_access\0".as_ptr(), rbuf.as_mut_ptr(), rbuf.len());
    report(name, rlen >= 0 && rlen as usize == acl_len && rbuf[..acl_len] == acl[..acl_len])
}

/// (9) The ACL from (8) actually gates access: an unprivileged uid named in
/// the ACL with perm 0 is denied both open(O_RDONLY) and faccessat(R_OK),
/// while the same uid opens a same-mode file *without* an ACL just fine.
unsafe fn test_xattr_acl_enforcement(root: &[u8], name: &[u8]) -> bool {
    let mut ab = [0u8; 96];
    let acl_path = mkpath(&mut ab, root, b"_aclenf");
    let mut nb = [0u8; 96];
    let noacl_path = mkpath(&mut nb, root, b"_noaclenf");

    let fd1 = open(acl_path, O_CREAT | O_WRONLY | O_TRUNC, 0o755);
    if fd1 < 0 { return report(name, false); }
    close(fd1);
    let fd2 = open(noacl_path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd2 < 0 { return report(name, false); }
    close(fd2);

    let mut acl = [0u8; 64];
    let acl_len = build_enforcing_acl(&mut acl);
    if raw_setxattr(acl_path, b"system.posix_acl_access\0".as_ptr(), acl.as_ptr(), acl_len, 0) != 0 {
        return report(name, false);
    }

    let pid = fork();
    if pid == 0 {
        if setuid(1000) != 0 { exit(1); }

        let denied_open = open(acl_path, O_RDONLY, 0) == -1 && get_errno() == EACCES;
        let denied_access = raw_faccessat(acl_path, R_OK) == -1 && get_errno() == EACCES;

        let allowed_fd = open(noacl_path, O_RDONLY, 0);
        let allowed_open = allowed_fd >= 0;
        if allowed_fd >= 0 { close(allowed_fd); }

        exit(if denied_open && denied_access && allowed_open { 0 } else { 1 });
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    report(name, status == 0)
}

/// (10) A malformed ACL (bad version, or a named USER entry with no MASK) is
/// rejected with EINVAL. A trivial ACL (only the three base entries) is
/// accepted but applied as a plain chmod: it is not stored, so it does not
/// show up in listxattr, and the mode bits change accordingly.
unsafe fn test_xattr_acl_malformed_trivial(root: &[u8], name: &[u8]) -> bool {
    let mut b1 = [0u8; 96];
    let bad_version_path = mkpath(&mut b1, root, b"_aclbadver");
    let mut b2 = [0u8; 96];
    let no_mask_path = mkpath(&mut b2, root, b"_aclnomask");
    let mut b3 = [0u8; 96];
    let trivial_path = mkpath(&mut b3, root, b"_acltrivial");

    for p in [bad_version_path, no_mask_path, trivial_path] {
        let fd = open(p, O_CREAT | O_WRONLY | O_TRUNC, 0o600);
        if fd < 0 { return report(name, false); }
        close(fd);
    }

    let mut bad = [0u8; 64];
    let bad_len = build_acl(&mut bad, 1, &[
        (ACL_USER_OBJ,  0o6, ACL_UNDEFINED_ID),
        (ACL_GROUP_OBJ, 0o4, ACL_UNDEFINED_ID),
        (ACL_OTHER,     0o4, ACL_UNDEFINED_ID),
    ]);
    let bad_ver = raw_setxattr(bad_version_path, b"system.posix_acl_access\0".as_ptr(), bad.as_ptr(), bad_len, 0);
    if bad_ver != -1 || get_errno() != EINVAL { return report(name, false); }

    let mut nomask = [0u8; 64];
    let nomask_len = build_acl(&mut nomask, 2, &[
        (ACL_USER_OBJ,  0o7, ACL_UNDEFINED_ID),
        (ACL_USER,      0o0, 1000),
        (ACL_GROUP_OBJ, 0o5, ACL_UNDEFINED_ID),
        (ACL_OTHER,     0o5, ACL_UNDEFINED_ID),
    ]);
    let nomask_r = raw_setxattr(no_mask_path, b"system.posix_acl_access\0".as_ptr(), nomask.as_ptr(), nomask_len, 0);
    if nomask_r != -1 || get_errno() != EINVAL { return report(name, false); }

    let mut triv = [0u8; 64];
    let triv_len = build_acl(&mut triv, 2, &[
        (ACL_USER_OBJ,  0o6, ACL_UNDEFINED_ID),
        (ACL_GROUP_OBJ, 0o4, ACL_UNDEFINED_ID),
        (ACL_OTHER,     0o4, ACL_UNDEFINED_ID),
    ]);
    if raw_setxattr(trivial_path, b"system.posix_acl_access\0".as_ptr(), triv.as_ptr(), triv_len, 0) != 0 {
        return report(name, false);
    }

    let mut lbuf = [0u8; 64];
    let llen = raw_listxattr(trivial_path, lbuf.as_mut_ptr(), lbuf.len());
    if llen < 0 || contains_name(&lbuf, llen as usize, b"system.posix_acl_access") {
        return report(name, false);
    }

    let mode = raw_mode(trivial_path);
    report(name, mode >= 0 && (mode & 0o777) == 0o644)
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut failures = 0;

    if !test_rmdir() { failures += 1; }
    if !test_rename() { failures += 1; }
    if !test_rename_replace(b"/tmp", b"rename_replace_tmpfs\0") { failures += 1; }
    if !test_rename_replace(b"/data", b"rename_replace_f2fs\0") { failures += 1; }
    if !test_flock_conflict() { failures += 1; }
    if !test_fcntl_byte_range_conflict() { failures += 1; }
    if !test_permission_enforced() { failures += 1; }
    if !test_f2fs_ownership_enforced() { failures += 1; }
    if !test_access_real_vs_effective(b"/tmp", b"access_real_vs_effective_tmpfs\0") { failures += 1; }
    if !test_access_real_vs_effective(b"/data", b"access_real_vs_effective_f2fs\0") { failures += 1; }
    if !test_chroot_confines_symlink_resolution() { failures += 1; }

    // O_APPEND, on both backends: the f2fs pair is the regression (appends
    // through the mount proxy wrote from offset 0), the tmpfs pair is the
    // control that the VFS-owned path still behaves the same way.
    if !test_append_across_opens(b"/data", b"append_across_opens_f2fs\0") { failures += 1; }
    if !test_append_across_opens(b"/tmp", b"append_across_opens_tmpfs\0") { failures += 1; }
    if !test_append_after_lseek(b"/data", b"append_after_lseek_f2fs\0") { failures += 1; }

    // Extended attributes / POSIX ACLs, each run against both the tmpfs
    // mount at /tmp and the f2fs mount at /data.
    if !test_xattr_basic(b"/tmp/xa", b"xattr_basic_tmpfs\0") { failures += 1; }
    if !test_xattr_basic(b"/data/xa", b"xattr_basic_f2fs\0") { failures += 1; }
    if !test_xattr_missing_and_unsupported(b"/tmp/xa", b"xattr_missing_unsupported_tmpfs\0") { failures += 1; }
    if !test_xattr_missing_and_unsupported(b"/data/xa", b"xattr_missing_unsupported_f2fs\0") { failures += 1; }
    if !test_xattr_list(b"/tmp/xa", b"xattr_list_tmpfs\0") { failures += 1; }
    if !test_xattr_list(b"/data/xa", b"xattr_list_f2fs\0") { failures += 1; }
    if !test_xattr_create_replace(b"/tmp/xa", b"xattr_create_replace_tmpfs\0") { failures += 1; }
    if !test_xattr_create_replace(b"/data/xa", b"xattr_create_replace_f2fs\0") { failures += 1; }
    if !test_xattr_remove(b"/tmp/xa", b"xattr_remove_tmpfs\0") { failures += 1; }
    if !test_xattr_remove(b"/data/xa", b"xattr_remove_f2fs\0") { failures += 1; }
    // Both symlink body forms on both backends (previously only each backend's
    // then-working form: absolute on tmpfs, relative on f2fs).
    if !test_xattr_symlink(b"/tmp/xa", b"xattr_symlink_tmpfs_abs\0", false) { failures += 1; }
    if !test_xattr_symlink(b"/tmp/xa", b"xattr_symlink_tmpfs_rel\0", true) { failures += 1; }
    if !test_xattr_symlink(b"/data/xa", b"xattr_symlink_f2fs_abs\0", false) { failures += 1; }
    if !test_xattr_symlink(b"/data/xa", b"xattr_symlink_f2fs_rel\0", true) { failures += 1; }
    if !test_xattr_fd(b"/tmp/xa", b"xattr_fd_tmpfs\0") { failures += 1; }
    if !test_xattr_fd(b"/data/xa", b"xattr_fd_f2fs\0") { failures += 1; }
    if !test_xattr_acl_basic(b"/tmp/xa", b"xattr_acl_basic_tmpfs\0") { failures += 1; }
    if !test_xattr_acl_basic(b"/data/xa", b"xattr_acl_basic_f2fs\0") { failures += 1; }
    if !test_xattr_acl_enforcement(b"/tmp/xa", b"xattr_acl_enforcement_tmpfs\0") { failures += 1; }
    if !test_xattr_acl_enforcement(b"/data/xa", b"xattr_acl_enforcement_f2fs\0") { failures += 1; }
    if !test_xattr_acl_malformed_trivial(b"/tmp/xa", b"xattr_acl_malformed_trivial_tmpfs\0") { failures += 1; }
    if !test_xattr_acl_malformed_trivial(b"/data/xa", b"xattr_acl_malformed_trivial_f2fs\0") { failures += 1; }

    // Symlink target resolution: both body forms on both backends, verified by
    // reading the target's bytes through the link (a silent-empty misresolve
    // would pass an open-only check but fail this one), plus a tmpfs->f2fs
    // cross-mount body.
    if !test_symlink_read(b"/tmp/xa", b"symlink_read_relative_tmpfs\0", b"symlink_read_absolute_tmpfs\0") { failures += 1; }
    if !test_symlink_read(b"/data/xa", b"symlink_read_relative_f2fs\0", b"symlink_read_absolute_f2fs\0") { failures += 1; }
    if !test_symlink_cross_mount(b"symlink_cross_mount_tmpfs_to_f2fs\0") { failures += 1; }

    if !test_fd_layout() { failures += 1; }
    if !test_timestamps(b"/tmp/xa", b"timestamps_tmpfs\0") { failures += 1; }
    if !test_timestamps(b"/data/xa", b"timestamps_f2fs\0") { failures += 1; }
    if !test_atime_relatime(b"/tmp/xa", b"atime_relatime_tmpfs\0") { failures += 1; }
    if !test_atime_relatime(b"/data/xa", b"atime_relatime_f2fs\0") { failures += 1; }

    // tmpfs file size (lane tmpcap): files grow page by page, bounded by the
    // tmpfs page budget, instead of a fixed 32 KiB in-struct array.
    if !test_tmpfs_large_rw() { failures += 1; }
    if !test_tmpfs_sparse_hole() { failures += 1; }
    if !test_tmpfs_truncate() { failures += 1; }
    if !test_tmpfs_append_big() { failures += 1; }
    if !test_tmpfs_cp_multi_mb() { failures += 1; }
    if !test_tmpfs_unlink_frees() { failures += 1; }
    if !test_tmpfs_mmap_sparse() { failures += 1; }
    if !test_tmpfs_shared_offset() { failures += 1; }
    if !test_tmpfs_enospc() { failures += 1; }
    if !test_fallocate_tmpfs() { failures += 1; }
    if !test_fallocate_f2fs() { failures += 1; }
    if !test_dev_zero_big_read() { failures += 1; }
    if !test_mount_table_tmpfs() { failures += 1; }

    puts(b"--- vfstest done ---\0".as_ptr());
    failures
}

/// rmdir() must remove an empty tmpfs directory, refuse a non-empty one with
/// ENOTEMPTY, and succeed once the directory really is empty.
unsafe fn test_rmdir() -> bool {
    let name = b"rmdir\0";

    if mkdir(b"/tmp/vt_dir\0".as_ptr(), 0o755) != 0 { return report(name, false); }
    if rmdir(b"/tmp/vt_dir\0".as_ptr()) != 0 { return report(name, false); }
    // Gone: re-opening without O_CREAT must fail.
    if open(b"/tmp/vt_dir\0".as_ptr(), O_RDONLY, 0) != -1 { return report(name, false); }

    if mkdir(b"/tmp/vt_dir2\0".as_ptr(), 0o755) != 0 { return report(name, false); }
    let fd = open(b"/tmp/vt_dir2/f.txt\0".as_ptr(), O_CREAT | O_WRONLY, 0o644);
    if fd < 0 { return report(name, false); }
    close(fd);

    if rmdir(b"/tmp/vt_dir2\0".as_ptr()) != -1 || get_errno() != ENOTEMPTY {
        return report(name, false);
    }
    if unlink(b"/tmp/vt_dir2/f.txt\0".as_ptr()) != 0 { return report(name, false); }
    report(name, rmdir(b"/tmp/vt_dir2\0".as_ptr()) == 0)
}

/// rename() must move a tmpfs file: the old path disappears, the new path
/// serves the same content.
unsafe fn test_rename() -> bool {
    let name = b"rename\0";

    let fd = open(b"/tmp/vt_a\0".as_ptr(), O_CREAT | O_WRONLY, 0o644);
    if fd < 0 { return report(name, false); }
    write(fd, b"hello".as_ptr(), 5);
    close(fd);

    if rename(b"/tmp/vt_a\0".as_ptr(), b"/tmp/vt_b\0".as_ptr()) != 0 {
        return report(name, false);
    }
    if open(b"/tmp/vt_a\0".as_ptr(), O_RDONLY, 0) != -1 { return report(name, false); }

    let fd2 = open(b"/tmp/vt_b\0".as_ptr(), O_RDONLY, 0);
    if fd2 < 0 { return report(name, false); }
    let mut buf = [0u8; 5];
    let n = read(fd2, buf.as_mut_ptr(), 5);
    close(fd2);
    report(name, n == 5 && &buf == b"hello")
}

/// POSIX rename must atomically REPLACE an existing destination, and the
/// renameat form must resolve relative names against real dirfds. Together
/// these are the atomic-write idiom every config/state writer uses (tempfile
/// in an opened directory, then renameat over the live name — cosmic-config,
/// atomicwrites, dconf, ...). `dir` parameterizes the filesystem: /tmp for
/// tmpfs, /data for f2fs. RENAME_NOREPLACE must still refuse with EEXIST.
unsafe fn test_rename_replace(dir: &[u8], name: &[u8]) -> bool {
    let mut src = [0u8; 64]; let mut dst = [0u8; 64];
    let dlen = dir.len();
    src[..dlen].copy_from_slice(dir); src[dlen..dlen + 7].copy_from_slice(b"/renr_s");
    dst[..dlen].copy_from_slice(dir); dst[dlen..dlen + 7].copy_from_slice(b"/renr_d");

    let fd = open(src.as_ptr(), O_CREAT | O_WRONLY, 0o644);
    if fd < 0 { return report(name, false); }
    write(fd, b"SRC".as_ptr(), 3);
    close(fd);
    let fd = open(dst.as_ptr(), O_CREAT | O_WRONLY, 0o644);
    if fd < 0 { return report(name, false); }
    write(fd, b"OLDDATA".as_ptr(), 7);
    close(fd);

    // Replace an existing destination: must succeed, dest serves src's bytes,
    // src's name is gone.
    if rename(src.as_ptr(), dst.as_ptr()) != 0 { return report(name, false); }
    if open(src.as_ptr(), O_RDONLY, 0) != -1 { return report(name, false); }
    let fd = open(dst.as_ptr(), O_RDONLY, 0);
    if fd < 0 { return report(name, false); }
    let mut buf = [0u8; 8];
    let n = read(fd, buf.as_mut_ptr(), 8);
    close(fd);
    if n != 3 || &buf[..3] != b"SRC" { return report(name, false); }

    // Dirfd-relative renameat2: names resolve against the opened directory,
    // not the cwd (the shape atomicwrites uses after tempfile_in).
    let mut dbuf = [0u8; 64];
    dbuf[..dlen].copy_from_slice(dir); // NUL-terminated by the zeroed buffer
    let dfd = open(dbuf.as_ptr(), O_RDONLY, 0);
    if dfd < 0 { return report(name, false); }
    let r = syscall5(SYS_RENAMEAT2, dfd as usize, b"renr_d\0".as_ptr() as usize,
                     dfd as usize, b"renr_e\0".as_ptr() as usize, 0);
    if r < 0 { close(dfd); return report(name, false); }
    let mut moved = [0u8; 64];
    moved[..dlen].copy_from_slice(dir); moved[dlen..dlen + 7].copy_from_slice(b"/renr_e");
    let fd = open(moved.as_ptr(), O_RDONLY, 0);
    if fd < 0 { close(dfd); return report(name, false); }
    close(fd);

    // RENAME_NOREPLACE onto an existing name must still refuse with EEXIST.
    let fd = open(dst.as_ptr(), O_CREAT | O_WRONLY, 0o644);
    close(fd);
    let r = syscall5(SYS_RENAMEAT2, dfd as usize, b"renr_e\0".as_ptr() as usize,
                     dfd as usize, b"renr_d\0".as_ptr() as usize, RENAME_NOREPLACE);
    close(dfd);
    let noreplace_ok = r == -17; // EEXIST
    unlink(dst.as_ptr());
    unlink(moved.as_ptr());
    report(name, noreplace_ok)
}

/// An exclusive flock() held by one process must cause a non-blocking
/// LOCK_EX request from another process to fail with EAGAIN; releasing it
/// must allow the original holder to reacquire.
unsafe fn test_flock_conflict() -> bool {
    let name = b"flock_conflict\0";

    let fd = open(b"/tmp/vt_lock\0".as_ptr(), O_CREAT | O_RDWR, 0o644);
    if fd < 0 { return report(name, false); }
    if flock(fd, LOCK_EX) != 0 { close(fd); return report(name, false); }

    let pid = fork();
    if pid == 0 {
        let fd2 = open(b"/tmp/vt_lock\0".as_ptr(), O_RDWR, 0);
        let r = flock(fd2, LOCK_EX | LOCK_NB);
        let ok = r == -1 && get_errno() == EAGAIN;
        exit(if ok { 0 } else { 1 });
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    if status != 0 { close(fd); return report(name, false); }

    if flock(fd, LOCK_UN) != 0 { close(fd); return report(name, false); }
    let reacquired = flock(fd, LOCK_EX) == 0;
    close(fd);
    report(name, reacquired)
}

/// Byte-range fcntl() locks from different processes must conflict only when
/// their ranges overlap; F_GETLK must report the holder's pid.
unsafe fn test_fcntl_byte_range_conflict() -> bool {
    let name = b"fcntl_byte_range_conflict\0";
    let my_pid = getpid();

    let fd = open(b"/tmp/vt_fcntl_lock\0".as_ptr(), O_CREAT | O_RDWR, 0o644);
    if fd < 0 { return report(name, false); }

    let mut lk = flock_t::default();
    lk.l_type = F_WRLCK;
    lk.l_whence = SEEK_SET as i16;
    lk.l_start = 0;
    lk.l_len = 10;
    if fcntl_lock(fd, F_SETLK, &mut lk as *mut flock_t) != 0 {
        close(fd);
        return report(name, false);
    }

    let pid = fork();
    if pid == 0 {
        let fd2 = open(b"/tmp/vt_fcntl_lock\0".as_ptr(), O_RDWR, 0);

        let mut lk2 = flock_t::default();
        lk2.l_type = F_WRLCK;
        lk2.l_whence = SEEK_SET as i16;
        lk2.l_start = 5;
        lk2.l_len = 10;
        let denied = fcntl_lock(fd2, F_SETLK, &mut lk2 as *mut flock_t) == -1
            && get_errno() == EAGAIN;

        let mut lk3 = flock_t::default();
        lk3.l_type = F_WRLCK;
        lk3.l_whence = SEEK_SET as i16;
        lk3.l_start = 5;
        lk3.l_len = 10;
        let getlk_ok = fcntl_lock(fd2, F_GETLK, &mut lk3 as *mut flock_t) == 0
            && lk3.l_type == F_WRLCK
            && lk3.l_pid == my_pid;

        exit(if denied && getlk_ok { 0 } else { 1 });
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());

    let mut unlk = flock_t::default();
    unlk.l_type = F_UNLCK;
    unlk.l_whence = SEEK_SET as i16;
    unlk.l_start = 0;
    unlk.l_len = 10;
    fcntl_lock(fd, F_SETLK, &mut unlk as *mut flock_t);
    close(fd);

    report(name, status == 0)
}

/// A mode-0600 file must be readable by its root creator but denied to a
/// process that has dropped privilege via setuid(); root must remain able to
/// regain access, and an unprivileged process must not be able to setuid(0)
/// back to root.
unsafe fn test_permission_enforced() -> bool {
    let name = b"permission_enforced\0";

    let fd = open(b"/tmp/vt_secret\0".as_ptr(), O_CREAT | O_WRONLY | O_TRUNC, 0o600);
    if fd < 0 { return report(name, false); }
    write(fd, b"root-only".as_ptr(), 9);
    close(fd);

    let pid = fork();
    if pid == 0 {
        let dropped = setuid(1000) == 0 && getuid() == 1000 && geteuid() == 1000;
        let denied = open(b"/tmp/vt_secret\0".as_ptr(), O_RDONLY, 0) == -1
            && get_errno() == EACCES;
        let cant_regain_root = setuid(0) == -1;
        exit(if dropped && denied && cant_regain_root { 0 } else { 1 });
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    if status != 0 { return report(name, false); }

    // Root (still euid 0 in this process) must still be able to read its own file.
    let fd2 = open(b"/tmp/vt_secret\0".as_ptr(), O_RDONLY, 0);
    let ok = fd2 >= 0;
    if ok { close(fd2); }
    report(name, ok)
}

/// Ownership enforcement on the f2fs mount at `/data` (the tmpfs test above
/// exercises the same rule for tmpfs). A file created by root and chowned to
/// uid 1000 must reject a chmod from a *different* unprivileged uid with
/// EPERM, while its actual owner is allowed. This is the check that was
/// meaningless until f2fs began persisting i_uid — every file used to read
/// back as root-owned, so `euid == owner` was true for everyone.
unsafe fn test_f2fs_ownership_enforced() -> bool {
    let name = b"f2fs_ownership_enforced\0";

    let path = b"/data/vt_owned\0";
    let fd = open(path.as_ptr(), O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    close(fd);
    // Hand the file to uid 1000 while we are still root.
    if chown(path.as_ptr(), 1000, 1000) != 0 { return report(name, false); }

    // A stranger (uid 1001) must be refused with EPERM.
    let stranger = fork();
    if stranger == 0 {
        if setuid(1001) != 0 { exit(1); }
        let denied = chmod(path.as_ptr(), 0o600) == -1 && get_errno() == EPERM;
        exit(if denied { 0 } else { 1 });
    }
    let mut st: i32 = -1;
    wait4(stranger, &mut st as *mut i32, 0, core::ptr::null_mut());
    if st != 0 { return report(name, false); }

    // The real owner (uid 1000) must be allowed.
    let owner = fork();
    if owner == 0 {
        if setuid(1000) != 0 { exit(1); }
        let allowed = chmod(path.as_ptr(), 0o600) == 0;
        exit(if allowed { 0 } else { 1 });
    }
    let mut st2: i32 = -1;
    wait4(owner, &mut st2 as *mut i32, 0, core::ptr::null_mut());
    report(name, st2 == 0)
}

/// access(2)/faccessat(2) without `AT_EACCESS` must check the REAL uid/gid,
/// not the effective ones — the entire point of `access()` is letting a
/// privileged (often setuid) caller ask "could the *invoker* do this", not
/// "can I do this right now". Built without a setuid binary: `setresuid`
/// splits real and effective ids apart directly (root may set either to
/// anything), which is exactly the id pair a setuid-root program has right
/// after it does the equivalent split itself.
unsafe fn test_access_real_vs_effective(root: &[u8], name: &[u8]) -> bool {
    let mut b = [0u8; 96];
    let path = mkpath(&mut b, root, b"_access_ruid");
    unlink(path);

    // Owned by root, mode 0600: uid 1000 has zero bits in "other".
    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o600);
    if fd < 0 { return report(name, false); }
    close(fd);

    let pid = fork();
    if pid == 0 {
        // Real uid 1000, effective uid stays 0 (root).
        if setresuid(1000, 0, 0) != 0 { exit(1); }
        let real_denied = raw_faccessat_flags(path, R_OK, 0) == -1 && get_errno() == EACCES;
        let eff_allowed = raw_faccessat_flags(path, R_OK, AT_EACCESS) == 0;
        // Control: open() always checks the EFFECTIVE ids (this fix must not
        // touch that) — root's euid still opens its own 0600 file.
        let ofd = open(path, O_RDONLY, 0);
        let open_ok = ofd >= 0;
        if open_ok { close(ofd); }
        exit(if real_denied && eff_allowed && open_ok { 0 } else { 1 });
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    unlink(path);
    report(name, status == 0)
}

/// atime must move on a read, but only within Linux's `relatime` budget. A
/// freshly created file has atime == mtime, which `relatime_needs_update`
/// treats as stale, so the FIRST read after creation must move it forward.
/// A SECOND read immediately after — mtime/ctime unchanged, nowhere near a
/// day old — must NOT move it again; that's the whole difference between
/// relatime and a naive "touch atime on every read", and the property that
/// makes the update cheap enough to do on every backend.
unsafe fn test_atime_relatime(root: &[u8], name: &[u8]) -> bool {
    let ns = |t: (i64, i64)| t.0 * 1_000_000_000 + t.1;
    let mut b = [0u8; 96];
    let path = mkpath(&mut b, root, b"_relatime");
    unlink(path);

    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    if write(fd, b"hello".as_ptr(), 5) != 5 { close(fd); unlink(path); return report(name, false); }
    close(fd);

    let a0 = match raw_atime(path) { Some(a) => a, None => { unlink(path); return report(name, false); } };

    usleep(30_000); // well past the 10 ms tick, same margin test_timestamps uses
    let rfd = open(path, O_RDONLY, 0);
    if rfd < 0 { unlink(path); return report(name, false); }
    let mut rbuf = [0u8; 8];
    read(rfd, rbuf.as_mut_ptr(), 5);
    close(rfd);
    let a1 = match raw_atime(path) { Some(a) => a, None => { unlink(path); return report(name, false); } };
    let first_moved = ns(a1) > ns(a0);

    usleep(30_000);
    let rfd2 = open(path, O_RDONLY, 0);
    if rfd2 < 0 { unlink(path); return report(name, false); }
    read(rfd2, rbuf.as_mut_ptr(), 5);
    close(rfd2);
    let a2 = match raw_atime(path) { Some(a) => a, None => { unlink(path); return report(name, false); } };
    let second_held = a2 == a1;

    unlink(path);
    report(name, first_moved && second_held)
}

/// chroot() must actually confine tmpfs symlink resolution to the new root:
/// an absolute symlink target is re-anchored *inside* the jail, not resolved
/// against the host's real "/". `chroot(2)` is irreversible for the calling
/// process, so the whole check runs in a forked child — a jail escape here
/// would otherwise confine the rest of the test suite too.
///
/// The jail is `/tmp/jail`, containing a symlink `link -> /etc/passwd`. Under
/// correct confinement, resolving `/link` after chrooting re-anchors
/// "/etc/passwd" inside the jail, i.e. host path `/tmp/jail/etc/passwd`,
/// which does not exist, so `open("/link")` must fail with ENOENT. If it
/// instead succeeds, the resolver escaped the jail and opened the real
/// `/etc/passwd`.
unsafe fn test_chroot_confines_symlink_resolution() -> bool {
    let name = b"chroot_confines_symlink_resolution\0";

    // Idempotent for a re-run in the same boot: a prior run leaves
    // `/tmp/jail` (containing `link`) behind on purpose (see the comment at
    // the bottom of this function), so a bare `mkdir` here would fail with
    // EEXIST on the second run and turn a pass into a spurious FAIL. Clear
    // any leftovers first — best-effort, ignoring errors, since on a first
    // run neither exists yet.
    unlink(b"/tmp/jail/link\0".as_ptr());
    rmdir(b"/tmp/jail\0".as_ptr());

    let pid = fork();
    if pid == 0 {
        if mkdir(b"/tmp/jail\0".as_ptr(), 0o755) != 0 { exit(1); }
        if raw_symlink(b"/etc/passwd\0".as_ptr(), b"/tmp/jail/link\0".as_ptr()) != 0 { exit(1); }
        if raw_chroot(b"/tmp/jail\0".as_ptr()) != 0 { exit(1); }

        let fd = open(b"/link\0".as_ptr(), O_RDONLY, 0);
        if fd >= 0 {
            // Escaped the jail: this opened the real /etc/passwd.
            close(fd);
            exit(1);
        }
        exit(if get_errno() == ENOENT { 0 } else { 1 });
    }
    let mut status: i32 = -1;
    wait4(pid, &mut status as *mut i32, 0, core::ptr::null_mut());
    // Leaving /tmp/jail behind is fine: /tmp is volatile tmpfs.
    report(name, status == 0)
}

/// O_APPEND must survive the close: three separate opens of the same file,
/// the first truncating and the next two appending, have to leave all three
/// bodies end to end. This is the shape a shell produces for
/// `printf a > f; printf b >> f; printf c >> f`, and on a mounted filesystem
/// it used to leave only the *last* body: the mount protocol's VFS_WRITE
/// carried no position and the server's open-file slot started every open at
/// 0, so each append overwrote from offset 0. tmpfs was unaffected (the VFS
/// owns that position itself), which is exactly why the bug hid.
unsafe fn test_append_across_opens(dir: &[u8], name: &[u8]) -> bool {
    let mut pb = [0u8; 96];
    let path = mkpath(&mut pb, dir, b"/vt_append");

    // First writer truncates, so a leftover file from an earlier run cannot
    // make a broken append look correct.
    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    if write(fd, b"one\n".as_ptr(), 4) != 4 { close(fd); return report(name, false); }
    close(fd);

    for body in [&b"two\n"[..], &b"three\n"[..]] {
        let fd = open(path, O_WRONLY | O_APPEND, 0);
        if fd < 0 { return report(name, false); }
        let n = write(fd, body.as_ptr(), body.len());
        close(fd);
        if n != body.len() as isize { return report(name, false); }
    }

    let fd = open(path, O_RDONLY, 0);
    if fd < 0 { return report(name, false); }
    let mut buf = [0u8; 32];
    let n = read(fd, buf.as_mut_ptr(), 32);
    close(fd);
    unlink(path);
    report(name, n == 14 && &buf[..14] == b"one\ntwo\nthree\n")
}

/// With O_APPEND the file offset is ignored for writes: seeking back to 0 and
/// writing must still land at end of file (Linux: "the file offset is ignored
/// for writes"). Guards the difference between a correct implementation and
/// the tempting wrong one — seeking to EOF once, at open — which this test
/// would catch and `append_across_opens` would not.
unsafe fn test_append_after_lseek(dir: &[u8], name: &[u8]) -> bool {
    let mut pb = [0u8; 96];
    let path = mkpath(&mut pb, dir, b"/vt_append_seek");

    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    if write(fd, b"HEAD".as_ptr(), 4) != 4 { close(fd); return report(name, false); }
    close(fd);

    let fd = open(path, O_WRONLY | O_APPEND, 0);
    if fd < 0 { return report(name, false); }
    if lseek(fd, 0, SEEK_SET) != 0 { close(fd); return report(name, false); }
    let n = write(fd, b"TAIL".as_ptr(), 4);
    close(fd);
    if n != 4 { return report(name, false); }

    let fd = open(path, O_RDONLY, 0);
    if fd < 0 { return report(name, false); }
    let mut buf = [0u8; 16];
    let got = read(fd, buf.as_mut_ptr(), 16);
    close(fd);
    unlink(path);
    // "HEADTAIL", not "TAIL" (overwritten in place) and not "TAILHEAD".
    report(name, got == 8 && &buf[..8] == b"HEADTAIL")
}

#[cfg(target_arch = "aarch64")] const SYS_SOCKETPAIR_: usize = 199;
#[cfg(target_arch = "x86_64")]  const SYS_SOCKETPAIR_: usize = 53;
#[cfg(target_arch = "aarch64")] const SYS_PPOLL_: usize = 73;
#[cfg(target_arch = "x86_64")]  const SYS_PPOLL_: usize = 271;
#[cfg(target_arch = "aarch64")] const SYS_PSELECT6_: usize = 72;
#[cfg(target_arch = "x86_64")]  const SYS_PSELECT6_: usize = 270;

/// The fd-number layout (lane term20): VFS fds are [0, 512), socket fds start
/// at 0x200 and stay below 1024 so select() covers them. A compositor with 20
/// GL clients needed more than the old 256 VFS fds.
///   * dup2 of a pipe end onto 255, 256 and 511 works and carries data;
///     512 (the first socket number) is refused with EBADF.
///   * a socketpair lands in [0x200, 0x400), passes data, and both ppoll and
///     pselect6 see it readable.
///   * a process can hold at least 500 VFS fds before EMFILE.
unsafe fn test_fd_layout() -> bool {
    let name = b"fd_layout_512\0";
    let mut p = [0i32; 2];
    if pipe(p.as_mut_ptr()) != 0 { return report(name, false); }
    let mut ok = true;
    for &t in &[255i32, 256, 511] {
        if dup3(p[1], t, 0) != t { ok = false; continue; }
        let b = [t as u8];
        if write(t, b.as_ptr(), 1) != 1 { ok = false; }
        let mut r = [0u8; 1];
        if read(p[0], r.as_mut_ptr(), 1) != 1 || r[0] != t as u8 { ok = false; }
        close(t);
    }
    if dup3(p[1], 512, 0) != -1 || get_errno() != EBADF { ok = false; }
    close(p[0]); close(p[1]);

    let mut sv = [0i32; 2];
    let r = syscall4(SYS_SOCKETPAIR_, 1 /* AF_UNIX */, 1 /* SOCK_STREAM */, 0, sv.as_mut_ptr() as usize);
    if r != 0 { return report(name, false); }
    for &f in &sv { if !(0x200..0x400).contains(&f) { ok = false; } }
    if write(sv[0], b"x".as_ptr(), 1) != 1 { ok = false; }
    // struct pollfd { int fd; short events; short revents; }
    let mut pfd = [0u8; 8];
    pfd[..4].copy_from_slice(&sv[1].to_le_bytes());
    pfd[4] = 1; // POLLIN
    let ts = [0i64; 2];
    let n = syscall5(SYS_PPOLL_, pfd.as_mut_ptr() as usize, 1, ts.as_ptr() as usize, 0, 8);
    if n != 1 || pfd[6] & 1 == 0 { ok = false; }
    let mut set = [0u64; 16]; // fd_set, FD_SETSIZE 1024
    let fd = sv[1] as usize;
    set[fd / 64] |= 1u64 << (fd % 64);
    let n = leandros_libc::syscall::syscall6(SYS_PSELECT6_, fd + 1, set.as_mut_ptr() as usize, 0, 0, ts.as_ptr() as usize, 0);
    if n != 1 || set[fd / 64] & (1u64 << (fd % 64)) == 0 { ok = false; }
    let mut c = [0u8; 1];
    if read(sv[1], c.as_mut_ptr(), 1) != 1 || c[0] != b'x' { ok = false; }
    close(sv[0]); close(sv[1]);

    // Capacity: dup until EMFILE, then give them all back.
    let mut held = [0i32; 600];
    let mut n = 0;
    while n < held.len() {
        let f = dup(0);
        if f < 0 { break; }
        held[n] = f;
        n += 1;
    }
    if n < 500 || n == held.len() { ok = false; }
    for &f in &held[..n] { close(f); }
    report(name, ok)
}

// `struct flock` from leandros_libc::io, aliased for readability.
#[allow(non_camel_case_types)]
type flock_t = leandros_libc::io::flock;

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(STDOUT_FILENO, name.as_ptr(), name.len() - 1); // drop the NUL terminator
    if passed {
        write(STDOUT_FILENO, b": PASS\n".as_ptr(), 7);
    } else {
        write(STDOUT_FILENO, b": FAIL\n".as_ptr(), 7);
    }
    passed
}

// ── tmpfs file size (lane tmpcap) ───────────────────────────────────────────

#[cfg(target_arch = "aarch64")] const SYS_FTRUNCATE: usize = 46;
#[cfg(target_arch = "x86_64")]  const SYS_FTRUNCATE: usize = 77;
#[cfg(target_arch = "aarch64")] const SYS_FSTAT: usize = 80;
#[cfg(target_arch = "x86_64")]  const SYS_FSTAT: usize = 5;
#[cfg(target_arch = "aarch64")] const SYS_STATFS: usize = 43;
#[cfg(target_arch = "x86_64")]  const SYS_STATFS: usize = 137;
#[cfg(target_arch = "aarch64")] const SYS_PWRITE64: usize = 68;
#[cfg(target_arch = "x86_64")]  const SYS_PWRITE64: usize = 18;
// st_size / st_blocks: same offsets in both ABIs' `struct stat`.
const STAT_SIZE_OFF: usize = 48;
const STAT_BLOCKS_OFF: usize = 64;
const SEEK_DATA: i32 = 3;
const SEEK_HOLE: i32 = 4;
const EFBIG: i32 = 27;
/// The kernel's per-file limit (`MAX_TMP_FILE_SIZE` in servers/vfs).
const TMP_FILE_MAX: usize = 16 << 30;

static mut BUF_A: [u8; 65536] = [0u8; 65536];
static mut BUF_B: [u8; 65536] = [0u8; 65536];

/// Deterministic, offset-dependent fill so a misplaced page shows up.
fn pat(off: usize) -> u8 { ((off.wrapping_mul(2654435761) >> 13) ^ (off >> 12)) as u8 }

unsafe fn raw_ftruncate(fd: i32, len: usize) -> isize { xret(syscall2(SYS_FTRUNCATE, fd as usize, len)) }
unsafe fn raw_pwrite(fd: i32, buf: *const u8, n: usize, off: usize) -> isize {
    xret(syscall4(SYS_PWRITE64, fd as usize, buf as usize, n, off))
}
/// (st_size, st_blocks) of an open fd.
unsafe fn raw_fsize(fd: i32) -> Option<(u64, u64)> {
    let mut buf = [0u8; STAT_SIZE];
    if syscall2(SYS_FSTAT, fd as usize, buf.as_mut_ptr() as usize) < 0 { return None; }
    Some((core::ptr::read_unaligned(buf.as_ptr().add(STAT_SIZE_OFF) as *const u64),
          core::ptr::read_unaligned(buf.as_ptr().add(STAT_BLOCKS_OFF) as *const u64)))
}
/// Free 4 KiB blocks on /tmp (statfs f_bfree).
unsafe fn tmp_bfree() -> Option<u64> {
    let mut buf = [0u8; 128];
    if syscall2(SYS_STATFS, b"/tmp\0".as_ptr() as usize, buf.as_mut_ptr() as usize) < 0 { return None; }
    Some(core::ptr::read_unaligned(buf.as_ptr().add(24) as *const u64))
}
/// Write `total` pattern bytes from the current position in `chunk`-sized
/// writes. `base` is the file offset of the first byte (for the pattern).
unsafe fn fill_pat(fd: i32, base: usize, total: usize, chunk: usize) -> bool {
    let mut done = 0;
    while done < total {
        let n = chunk.min(total - done).min(65536);
        for i in 0..n { BUF_A[i] = pat(base + done + i); }
        if write(fd, BUF_A.as_ptr(), n) != n as isize { return false; }
        done += n;
    }
    true
}
/// Read `total` bytes from the current position and compare to the pattern.
unsafe fn check_pat(fd: i32, base: usize, total: usize) -> bool {
    let mut done = 0;
    while done < total {
        let want = (total - done).min(65536);
        let r = read(fd, BUF_B.as_mut_ptr(), want);
        if r <= 0 { return false; }
        for i in 0..r as usize { if BUF_B[i] != pat(base + done + i) { return false; } }
        done += r as usize;
    }
    // ...and EOF right after.
    read(fd, BUF_B.as_mut_ptr(), 1) == 0
}

/// 3 MiB — about a hundred times the old 32 KiB cap — written in one 1 MiB
/// write plus odd-sized chunks, then read back byte-exact; a single read()
/// of 1 MiB must come back whole (regular files do not short-read).
unsafe fn test_tmpfs_large_rw() -> bool {
    let name = b"tmpfs_large_rw\0";
    let path = b"/tmp/vt_large\0".as_ptr();
    let fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    let total = 3 << 20;
    // One big write straight from a mapping-sized region.
    let big = mmap(core::ptr::null_mut(), 1 << 20, 3, 0x22, -1, 0);
    let mut ok = big as isize != -1;
    if ok {
        for i in 0..(1 << 20) { *big.add(i) = pat(i); }
        ok = write(fd, big, 1 << 20) == 1 << 20;
    }
    ok = ok && fill_pat(fd, 1 << 20, total - (1 << 20), 12345);
    ok = ok && raw_fsize(fd).map(|s| s.0) == Some(total as u64);
    ok = ok && lseek(fd, 0, 0) == 0;
    if ok {
        for i in 0..(1 << 20) { *big.add(i) = 0; }
        ok = read(fd, big, 1 << 20) == 1 << 20;
        for i in 0..(1 << 20) { if ok && *big.add(i) != pat(i) { ok = false; } }
    }
    ok = ok && check_pat(fd, 1 << 20, total - (1 << 20));
    if big as isize != -1 { munmap(big, 1 << 20); }
    close(fd);
    unlink(path);
    report(name, ok)
}

/// lseek past EOF + write leaves a hole that reads as zeros, costs no pages
/// (st_blocks), and is reported by SEEK_DATA/SEEK_HOLE; writes at and past the
/// 16 GiB file limit are EFBIG.
unsafe fn test_tmpfs_sparse_hole() -> bool {
    let name = b"tmpfs_sparse_hole\0";
    let path = b"/tmp/vt_sparse\0".as_ptr();
    let fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    let off = (8 << 20) + 123;
    let mut ok = lseek(fd, off as i64, 0) == off as i64;
    ok = ok && write(fd, b"X".as_ptr(), 1) == 1;
    ok = ok && raw_fsize(fd) == Some(((off + 1) as u64, 8)); // one page
    // The hole reads back as zeros, in one read across many pages.
    ok = ok && lseek(fd, (off - 40000) as i64, 0) >= 0;
    ok = ok && read(fd, BUF_B.as_mut_ptr(), 40001) == 40001;
    for i in 0..40000 { if ok && BUF_B[i] != 0 { ok = false; } }
    ok = ok && BUF_B[40000] == b'X';
    ok = ok && lseek(fd, 0, SEEK_DATA) == (8 << 20);
    ok = ok && lseek(fd, 0, SEEK_HOLE) == 0;
    ok = ok && lseek(fd, (8 << 20) as i64, SEEK_HOLE) == (off + 1) as i64;
    // EFBIG at the limit, and a sparse write just below it works.
    ok = ok && raw_pwrite(fd, b"Y".as_ptr(), 1, TMP_FILE_MAX) == -1 && get_errno() == EFBIG;
    ok = ok && raw_ftruncate(fd, TMP_FILE_MAX + 1) == -1 && get_errno() == EFBIG;
    ok = ok && raw_ftruncate(fd, 1 << 30) == 0;
    ok = ok && raw_fsize(fd) == Some((1 << 30, 8));
    close(fd);
    unlink(path);
    report(name, ok)
}

/// ftruncate shrink drops the tail (and its pages), grow exposes zeros —
/// including over bytes that were written and then truncated away.
unsafe fn test_tmpfs_truncate() -> bool {
    let name = b"tmpfs_truncate\0";
    let path = b"/tmp/vt_trunc\0".as_ptr();
    let fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    let mut ok = fill_pat(fd, 0, 100_000, 7000);
    ok = ok && raw_fsize(fd) == Some((100_000, 200));
    ok = ok && raw_ftruncate(fd, 5000) == 0;
    ok = ok && raw_fsize(fd) == Some((5000, 16)); // pages 0 and 1
    ok = ok && raw_ftruncate(fd, 70_000) == 0;
    ok = ok && raw_fsize(fd).map(|s| s.0) == Some(70_000);
    ok = ok && lseek(fd, 0, 0) == 0 && read(fd, BUF_B.as_mut_ptr(), 65536) == 65536;
    for i in 0..65536 {
        let want = if i < 5000 { pat(i) } else { 0 };
        if ok && BUF_B[i] != want { ok = false; }
    }
    // O_TRUNC on reopen empties it and frees the pages.
    close(fd);
    let fd = open(path, O_RDWR | O_TRUNC, 0);
    ok = ok && fd >= 0 && raw_fsize(fd) == Some((0, 0));
    if fd >= 0 { close(fd); }
    unlink(path);
    report(name, ok)
}

/// O_APPEND well past 4 KiB and 32 KiB, from two descriptors in turn (what
/// `cmd >>log 2>>log` does), lands every byte in order.
unsafe fn test_tmpfs_append_big() -> bool {
    let name = b"tmpfs_append_big\0";
    let path = b"/tmp/vt_append\0".as_ptr();
    let fd0 = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd0 >= 0 { close(fd0); }
    let a = open(path, O_WRONLY | O_APPEND, 0);
    let b = open(path, O_WRONLY | O_APPEND, 0);
    let mut ok = a >= 0 && b >= 0;
    let chunk = 6001;
    let mut off = 0;
    for k in 0..20 {
        if !ok { break; }
        let fd = if k % 2 == 0 { a } else { b };
        ok = fill_pat(fd, off, chunk, chunk);
        off += chunk;
    }
    if a >= 0 { close(a); }
    if b >= 0 { close(b); }
    let r = open(path, O_RDONLY, 0);
    ok = ok && r >= 0 && raw_fsize(r).map(|s| s.0) == Some(off as u64) && check_pat(r, 0, off);
    if r >= 0 { close(r); }
    unlink(path);
    report(name, ok)
}

/// What `cp` does: a 5 MiB source copied through a 64 KiB buffer into a new
/// /tmp file, then compared. Also copies a real binary (a few MB) when one
/// is installed, checking the sizes agree.
unsafe fn test_tmpfs_cp_multi_mb() -> bool {
    let name = b"tmpfs_cp_multi_mb\0";
    let src = b"/tmp/vt_cp_src\0".as_ptr();
    let dst = b"/tmp/vt_cp_dst\0".as_ptr();
    let total = 5 << 20;
    let s = open(src, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    let mut ok = s >= 0 && fill_pat(s, 0, total, 65536);
    if s >= 0 { close(s); }
    ok = ok && copy_file(src, dst) == Some(total);
    let d = open(dst, O_RDONLY, 0);
    ok = ok && d >= 0 && check_pat(d, 0, total);
    if d >= 0 { close(d); }
    unlink(src);
    unlink(dst);
    // A real executable, if this image ships one.
    for bin in [b"/bin/brush\0".as_ptr(), b"/usr/bin/brush\0".as_ptr()] {
        let f = open(bin, O_RDONLY, 0);
        if f < 0 { continue; }
        let sz = raw_fsize(f).map(|s| s.0 as usize).unwrap_or(0);
        close(f);
        let copied = copy_file(bin, dst);
        ok = ok && sz > (1 << 20) && copied == Some(sz);
        let d = open(dst, O_RDONLY, 0);
        ok = ok && d >= 0 && raw_fsize(d).map(|s| s.0 as usize) == Some(sz);
        if d >= 0 { close(d); }
        unlink(dst);
        break;
    }
    report(name, ok)
}

unsafe fn copy_file(src: *const u8, dst: *const u8) -> Option<usize> {
    let s = open(src, O_RDONLY, 0);
    if s < 0 { return None; }
    let d = open(dst, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if d < 0 { close(s); return None; }
    let mut total = 0usize;
    let ok = loop {
        let r = read(s, BUF_A.as_mut_ptr(), 65536);
        if r < 0 { break false; }
        if r == 0 { break true; }
        if write(d, BUF_A.as_ptr(), r as usize) != r { break false; }
        total += r as usize;
    };
    close(s);
    close(d);
    if ok { Some(total) } else { None }
}

/// Pages come back to the tmpfs budget on unlink, and for an
/// open-then-unlinked file only on its last close.
unsafe fn test_tmpfs_unlink_frees() -> bool {
    let name = b"tmpfs_unlink_frees\0";
    let path = b"/tmp/vt_frees\0".as_ptr();
    let size = 2 << 20; // 512 pages
    let before = match tmp_bfree() { Some(v) => v, None => return report(name, false) };
    let fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    let mut ok = fd >= 0 && fill_pat(fd, 0, size, 65536);
    if fd >= 0 { close(fd); }
    let full = tmp_bfree().unwrap_or(0);
    ok = ok && before.saturating_sub(full) >= 512;
    ok = ok && unlink(path) == 0;
    // Other processes may be using /tmp concurrently; allow a little slack.
    let after = tmp_bfree().unwrap_or(0);
    ok = ok && after + 16 >= before;
    // open → unlink → (still readable) → close frees.
    let fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    ok = ok && fd >= 0 && fill_pat(fd, 0, size, 65536) && unlink(path) == 0;
    let held = tmp_bfree().unwrap_or(0);
    ok = ok && before.saturating_sub(held) >= 512;
    ok = ok && lseek(fd, 0, 0) == 0 && check_pat(fd, 0, size);
    if fd >= 0 { close(fd); }
    let released = tmp_bfree().unwrap_or(0);
    ok = ok && released + 16 >= before;
    report(name, ok)
}

/// MAP_SHARED of a sparse, ftruncate-grown file: the mapping reads zeros,
/// stores through it are seen by read(), and write() is seen by the mapping.
unsafe fn test_tmpfs_mmap_sparse() -> bool {
    let name = b"tmpfs_mmap_sparse\0";
    let path = b"/tmp/vt_mmap\0".as_ptr();
    let fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    let len = 256 * 1024;
    let mut ok = raw_ftruncate(fd, len) == 0;
    ok = ok && raw_fsize(fd) == Some((len as u64, 0));
    let m = mmap(core::ptr::null_mut(), len, 3, 1 /* MAP_SHARED */, fd, 0);
    ok = ok && m as isize != -1;
    if ok {
        for i in (0..len).step_by(997) { if *m.add(i) != 0 { ok = false; } }
        *m.add(200_000) = 0x5a;
        ok = ok && lseek(fd, 200_000, 0) == 200_000 && read(fd, BUF_B.as_mut_ptr(), 1) == 1 && BUF_B[0] == 0x5a;
        ok = ok && raw_pwrite(fd, b"Q".as_ptr(), 1, 70_000) == 1 && *m.add(70_000) == b'Q';
        munmap(m, len);
    }
    close(fd);
    unlink(path);
    report(name, ok)
}

/// Fill /tmp to its page budget (half of RAM): the write that hits it is
/// short or fails with ENOSPC, statfs reports it full, a /proc snapshot still
/// opens (kernel-generated files are never budget-refused), and unlink gives
/// every page back.
unsafe fn test_tmpfs_enospc() -> bool {
    let name = b"tmpfs_enospc\0";
    let path = b"/tmp/vt_enospc\0".as_ptr();
    let before = match tmp_bfree() { Some(v) => v, None => return report(name, false) };
    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    for i in 0..65536 { BUF_A[i] = i as u8; }
    let mut written = 0usize;
    let mut errno = 0;
    loop {
        let r = write(fd, BUF_A.as_ptr(), 65536);
        if r < 0 { errno = get_errno(); break; }
        written += r as usize;
        if r < 65536 { continue; } // short at the limit; the next one fails
        if written > (64usize << 30) { break; } // runaway guard
    }
    let full = tmp_bfree().unwrap_or(u64::MAX);
    let mut ok = errno == ENOSPC && written as u64 / 4096 + 64 >= before && full < 64;
    let p = open(b"/proc/meminfo\0".as_ptr(), O_RDONLY, 0);
    ok = ok && p >= 0 && read(p, BUF_B.as_mut_ptr(), 512) > 0;
    if p >= 0 { close(p); }
    close(fd);
    ok = ok && unlink(path) == 0;
    let after = tmp_bfree().unwrap_or(0);
    ok = ok && after + 16 >= before;
    if !ok {
        puts(b"tmpfs_enospc detail follows (errno, MiB written, bfree before/full/after)\0".as_ptr());
        print_num(errno as u64); print_num((written >> 20) as u64);
        print_num(before); print_num(full); print_num(after);
    }
    report(name, ok)
}

unsafe fn print_num(mut v: u64) {
    let mut d = [0u8; 21];
    let mut n = 20;
    d[20] = b'\n';
    loop { n -= 1; d[n] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    write(STDOUT_FILENO, d.as_ptr().add(n), 21 - n);
}

/// dup(2) and fork(2) copies of a tmpfs fd share one offset (one open file
/// description): `(child; parent) > /tmp/f` must append, not overwrite. This
/// was the "50 KB of session logs, wc -c says 4089" bug.
unsafe fn test_tmpfs_shared_offset() -> bool {
    let name = b"tmpfs_shared_offset\0";
    let path = b"/tmp/vt_ofd\0".as_ptr();
    let fd = open(path, O_CREAT | O_WRONLY | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    let mut ok = fill_pat(fd, 0, 10_000, 10_000);
    let pid = fork();
    if pid == 0 {
        // Child: continue where the parent left off, through the inherited fd.
        let r = fill_pat(fd, 10_000, 20_000, 3000);
        exit(if r { 0 } else { 1 });
    }
    let mut status = 0i32;
    ok = ok && pid > 0 && wait4(pid, &mut status, 0, core::ptr::null_mut()) == pid && status == 0;
    // The parent's offset moved with the child's writes.
    ok = ok && lseek(fd, 0, 1) == 30_000;
    let d = dup(fd);
    ok = ok && d >= 0 && fill_pat(d, 30_000, 5000, 5000) && lseek(fd, 0, 1) == 35_000;
    if d >= 0 { close(d); }
    ok = ok && fill_pat(fd, 35_000, 1000, 1000);
    close(fd);
    let r = open(path, O_RDONLY, 0);
    ok = ok && r >= 0 && check_pat(r, 0, 36_000);
    if r >= 0 { close(r); }
    unlink(path);
    report(name, ok)
}

// ── fallocate, /dev/zero, mount table (lane stdioredir) ─────────────────────

#[cfg(target_arch = "aarch64")] const SYS_FALLOCATE: usize = 47;
#[cfg(target_arch = "x86_64")]  const SYS_FALLOCATE: usize = 285;
const FALLOC_FL_KEEP_SIZE: usize = 0x01;
const FALLOC_FL_PUNCH_HOLE: usize = 0x02;
const FALLOC_FL_ZERO_RANGE: usize = 0x10;

unsafe fn raw_fallocate(fd: i32, mode: usize, off: usize, len: usize) -> isize {
    xret(syscall4(SYS_FALLOCATE, fd as usize, mode, off, len))
}

/// fallocate on tmpfs: mode 0 extends EOF and really allocates (st_blocks,
/// statfs), KEEP_SIZE allocates without moving EOF, PUNCH_HOLE|KEEP_SIZE
/// zeroes the range and frees its whole pages; unsupported modes are
/// EOPNOTSUPP and bad ranges EINVAL. It used to be a silent no-op.
unsafe fn test_fallocate_tmpfs() -> bool {
    let name = b"fallocate_tmpfs\0";
    let path = b"/tmp/vt_falloc\0".as_ptr();
    let fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    let free0 = tmp_bfree().unwrap_or(0);
    let mut ok = raw_fallocate(fd, 0, 0, 100_000) == 0;
    ok = ok && raw_fsize(fd) == Some((100_000, 25 * 8));
    let free1 = tmp_bfree().unwrap_or(0);
    ok = ok && free0 >= free1 + 25;
    // KEEP_SIZE: two pages past EOF, size unchanged.
    ok = ok && raw_fallocate(fd, FALLOC_FL_KEEP_SIZE, 102_400, 8192) == 0;
    ok = ok && raw_fsize(fd) == Some((100_000, 27 * 8));
    // Allocated range reads back as zeros.
    ok = ok && lseek(fd, 0, 0) == 0;
    ok = ok && read(fd, BUF_B.as_mut_ptr(), 65536) == 65536;
    for i in 0..65536 { if ok && BUF_B[i] != 0 { ok = false; } }
    // Punch: fill 3 pages with 0xFF, punch the middle one plus 100 bytes on
    // either side.
    for i in 0..12288 { BUF_A[i] = 0xFF; }
    ok = ok && raw_pwrite(fd, BUF_A.as_ptr(), 12288, 0) == 12288;
    ok = ok && raw_fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 4096 - 100, 4096 + 200) == 0;
    ok = ok && raw_fsize(fd) == Some((100_000, 26 * 8));
    ok = ok && lseek(fd, 0, 0) == 0;
    ok = ok && read(fd, BUF_B.as_mut_ptr(), 12288) == 12288;
    for i in 0..12288 {
        let want = if (3996..8292).contains(&i) { 0 } else { 0xFF };
        if ok && BUF_B[i] != want { ok = false; }
    }
    // Rejected modes / ranges.
    ok = ok && raw_fallocate(fd, FALLOC_FL_PUNCH_HOLE, 0, 4096) == -1 && get_errno() == EOPNOTSUPP;
    ok = ok && raw_fallocate(fd, FALLOC_FL_ZERO_RANGE, 0, 4096) == -1 && get_errno() == EOPNOTSUPP;
    ok = ok && raw_fallocate(fd, 0, 0, 0) == -1 && get_errno() == EINVAL;
    close(fd);
    unlink(path);
    ok = ok && tmp_bfree() == Some(free0);
    report(name, ok)
}

/// fallocate on f2fs: mode 0 extends EOF (posix_fallocate's contract);
/// KEEP_SIZE leaves it; PUNCH_HOLE is EOPNOTSUPP.
unsafe fn test_fallocate_f2fs() -> bool {
    let name = b"fallocate_f2fs\0";
    let path = b"/data/vt_falloc\0".as_ptr();
    let fd = open(path, O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }
    let mut ok = write(fd, b"abc".as_ptr(), 3) == 3;
    ok = ok && raw_fallocate(fd, 0, 0, 50_000) == 0;
    ok = ok && raw_fsize(fd).map(|s| s.0) == Some(50_000);
    ok = ok && raw_fallocate(fd, 0, 0, 10) == 0;
    ok = ok && raw_fsize(fd).map(|s| s.0) == Some(50_000);
    ok = ok && raw_fallocate(fd, FALLOC_FL_KEEP_SIZE, 0, 90_000) == 0;
    ok = ok && raw_fsize(fd).map(|s| s.0) == Some(50_000);
    ok = ok && raw_fallocate(fd, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 0, 4096) == -1
            && get_errno() == EOPNOTSUPP;
    ok = ok && lseek(fd, 0, 0) == 0;
    ok = ok && read(fd, BUF_B.as_mut_ptr(), 8) == 8 && &BUF_B[..8] == b"abc\0\0\0\0\0";
    close(fd);
    unlink(path);
    report(name, ok)
}

/// One read(2) of /dev/zero fills the whole buffer (it stopped at 4 KiB),
/// and /dev/null and /dev/zero accept lseek like Linux's.
unsafe fn test_dev_zero_big_read() -> bool {
    let name = b"dev_zero_big_read\0";
    let fd = open(b"/dev/zero\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 { return report(name, false); }
    let len = 4 << 20;
    let big = mmap(core::ptr::null_mut(), len, 3, 0x22, -1, 0);
    let mut ok = big as isize != -1;
    if ok {
        for i in (0..len).step_by(997) { *big.add(i) = 0xAA; }
        ok = read(fd, big, len) == len as isize;
        for i in (0..len).step_by(997) { if ok && *big.add(i) != 0 { ok = false; } }
        munmap(big, len);
    }
    ok = ok && lseek(fd, 12345, 0) == 0;
    close(fd);
    let nfd = open(b"/dev/null\0".as_ptr(), O_WRONLY, 0);
    ok = ok && nfd >= 0 && lseek(nfd, 0, 2) == 0;
    if nfd >= 0 { close(nfd); }
    report(name, ok)
}

/// True if the NUL-free `needle` occurs in the first `n` bytes of BUF_B.
unsafe fn buf_b_contains(n: usize, needle: &[u8]) -> bool {
    n >= needle.len() && (0..=n - needle.len()).any(|i| &BUF_B[i..i + needle.len()] == needle)
}
unsafe fn slurp_b(path: &[u8]) -> usize {
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    if fd < 0 { return 0; }
    let mut n = 0usize;
    loop {
        let r = read(fd, BUF_B.as_mut_ptr().add(n), 65536 - n);
        if r <= 0 { break; }
        n += r as usize;
        if n == 65536 { break; }
    }
    close(fd);
    n
}

/// The tmpfs roots are in the mount table, so `df /tmp` finds tmpfs instead
/// of matching "/" (it showed the root f2fs volume).
unsafe fn test_mount_table_tmpfs() -> bool {
    let name = b"mount_table_tmpfs\0";
    let n = slurp_b(b"/proc/self/mountinfo\0");
    let mut ok = buf_b_contains(n, b" / /tmp rw,relatime - tmpfs tmpfs rw\n");
    ok = ok && buf_b_contains(n, b" / /dev/shm rw,relatime - tmpfs tmpfs rw\n");
    let n = slurp_b(b"/proc/mounts\0");
    ok = ok && buf_b_contains(n, b"tmpfs /tmp tmpfs rw 0 0\n");
    ok = ok && buf_b_contains(n, b"tmpfs /run/user tmpfs rw 0 0\n");
    let n = slurp_b(b"/etc/mtab\0");
    ok = ok && buf_b_contains(n, b"tmpfs /tmp tmpfs rw 0 0\n");
    report(name, ok)
}
