//! vfstest — regression coverage for TODO.md item #4 (VFS server): rmdir,
//! cross-mount-capable rename, advisory locking (flock + fcntl byte-range),
//! and real file permissions/ownership (including setuid privilege drop).
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

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut failures = 0;

    if !test_rmdir() { failures += 1; }
    if !test_rename() { failures += 1; }
    if !test_flock_conflict() { failures += 1; }
    if !test_fcntl_byte_range_conflict() { failures += 1; }
    if !test_permission_enforced() { failures += 1; }
    if !test_f2fs_ownership_enforced() { failures += 1; }

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
