//! vfstest — regression coverage for TODO.md item #4 (VFS server): rmdir,
//! cross-mount-capable rename, advisory locking (flock + fcntl byte-range),
//! real file permissions/ownership (including setuid privilege drop), and
//! extended attributes / POSIX ACLs (setxattr/getxattr/listxattr/removexattr
//! and their l*/f* variants, plus ACL-driven access enforcement).
//!
//! Each check prints "<name>: PASS" or "<name>: FAIL" to stdout (serial
//! console); `main` returns the number of failures as the exit code.
//!
//! Note: this kernel's `wait4()` reports a child's raw `exit()` argument as
//! `wstatus` directly (not the shifted Linux `WEXITSTATUS` encoding), so
//! tests below compare `wstatus` to a plain 0/1, not `status >> 8`.

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

/// `faccessat(AT_FDCWD, path, mode, 0)` — used to probe ACL-enforced access.
unsafe fn raw_faccessat(path: *const u8, mode: i32) -> i32 {
    let r = syscall4(SYS_FACCESSAT, AT_FDCWD as usize, path as usize, mode as usize, 0);
    if r < 0 { set_errno(-r as i32); -1 } else { 0 }
}

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
    if !test_chroot_confines_symlink_resolution() { failures += 1; }

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
