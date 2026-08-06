//! f2fstest — regression coverage for the F2FS server
//!
//! Verifies basic F2FS read/write, direct, indirect, and double-indirect block pointer
//! writes/reads via sparse files, directory operations, and unmounting.

#![no_std]
#![no_main]

extern crate leandros_libc;
use leandros_libc::*;

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let mut failures = 0;

    if !test_f2fs_basic() { failures += 1; }
    if !test_f2fs_direct_node() { failures += 1; }
    if !test_f2fs_indirect_node() { failures += 1; }
    if !test_f2fs_double_indirect_node() { failures += 1; }
    if !test_f2fs_directories() { failures += 1; }
    if !test_f2fs_statfs_reclaim() { failures += 1; }

    puts(b"--- f2fstest done ---\0".as_ptr());
    failures
}

// ── statfs(2) / ftruncate(2) raw syscall helpers ───────────────────────────
//
// Neither is wrapped by leandros-libc yet, so this test calls them directly
// through the crate's syscall trampoline — the same `syscallN` functions
// io.rs itself is built on (see userland/libc/src/io.rs's `open`/`lseek`).
//
// Syscall numbers verified against the kernel's own dispatch tables in
// kernel/src/syscall.rs (the aarch64 `mod nr` starts at line 254, the
// x86_64 one — behind `#[cfg(not(target_arch = "aarch64"))]` — at line 447):
//   aarch64: STATFS = 43  (kernel/src/syscall.rs:382), FTRUNCATE = 46 (:380)
//   x86_64:  STATFS = 137 (kernel/src/syscall.rs:596), FTRUNCATE = 77 (:593)
#[cfg(target_arch = "aarch64")]
const SYS_STATFS: usize = 43;
#[cfg(target_arch = "aarch64")]
const SYS_FTRUNCATE: usize = 46;
#[cfg(not(target_arch = "aarch64"))]
const SYS_STATFS: usize = 137;
#[cfg(not(target_arch = "aarch64"))]
const SYS_FTRUNCATE: usize = 77;

// `struct statfs` as filled in by servers/vfs/src/lib.rs's `write_statfs()`:
// 120 bytes total (STATFS_SIZE, servers/vfs/src/lib.rs:4483), every field an
// 8-byte word in the asm-generic layout shared by both arches. `f_bfree` is
// the 4th word written (`put(3, v.bfree)`, servers/vfs/src/lib.rs:4520-4523),
// i.e. byte offset 24.
const STATFS_SIZE: usize = 120;
const F_BFREE_OFFSET: usize = 24;

/// Raw `statfs(path, buf)`; returns `f_bfree` (in `f_bsize`-sized blocks, 4096
/// on F2FS) or `None` on error.
unsafe fn statfs_bfree(path: *const u8) -> Option<u64> {
    let mut buf = [0u8; STATFS_SIZE];
    let r = leandros_libc::syscall::syscall2(SYS_STATFS, path as usize, buf.as_mut_ptr() as usize);
    if r != 0 { return None; }
    Some(core::ptr::read_unaligned(buf.as_ptr().add(F_BFREE_OFFSET) as *const u64))
}

/// Raw `ftruncate(fd, length)`.
unsafe fn ftruncate(fd: i32, length: off_t) -> bool {
    leandros_libc::syscall::syscall2(SYS_FTRUNCATE, fd as usize, length as usize) == 0
}

unsafe fn test_f2fs_basic() -> bool {
    let name = b"f2fs_basic\0";
    let filepath = b"/mnt/vt_basic.txt\0";

    let fd = open(filepath.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }

    let msg = b"hello F2FS";
    if write(fd, msg.as_ptr(), msg.len()) != msg.len() as isize {
        close(fd);
        return report(name, false);
    }

    if lseek(fd, 0, SEEK_SET) != 0 {
        close(fd);
        return report(name, false);
    }

    let mut buf = [0u8; 16];
    let n = read(fd, buf.as_mut_ptr(), msg.len());
    close(fd);

    let ok = n == msg.len() as isize && &buf[..msg.len()] == msg;
    report(name, ok)
}

unsafe fn test_f2fs_direct_node() -> bool {
    let name = b"f2fs_direct_node\0";
    let filepath = b"/mnt/vt_direct.txt\0";

    let fd = open(filepath.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }

    // Seek to block 923 + 5 (in the direct node range)
    let offset: off_t = (923 + 5) * 4096;
    if lseek(fd, offset, SEEK_SET) != offset {
        close(fd);
        return report(name, false);
    }

    let msg = b"direct node block test content";
    if write(fd, msg.as_ptr(), msg.len()) != msg.len() as isize {
        close(fd);
        return report(name, false);
    }

    if lseek(fd, offset, SEEK_SET) != offset {
        close(fd);
        return report(name, false);
    }

    let mut buf = [0u8; 64];
    let n = read(fd, buf.as_mut_ptr(), msg.len());
    close(fd);

    let ok = n == msg.len() as isize && &buf[..msg.len()] == msg;
    report(name, ok)
}

unsafe fn test_f2fs_indirect_node() -> bool {
    let name = b"f2fs_indirect_node\0";
    let filepath = b"/mnt/vt_indirect.txt\0";

    let fd = open(filepath.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }

    // Seek to block 923 + 1019 + 5 (in the single indirect range)
    let offset: off_t = (923 + 1019 + 5) * 4096;
    if lseek(fd, offset, SEEK_SET) != offset {
        close(fd);
        return report(name, false);
    }

    let msg = b"indirect node block test content";
    if write(fd, msg.as_ptr(), msg.len()) != msg.len() as isize {
        close(fd);
        return report(name, false);
    }

    if lseek(fd, offset, SEEK_SET) != offset {
        close(fd);
        return report(name, false);
    }

    let mut buf = [0u8; 64];
    let n = read(fd, buf.as_mut_ptr(), msg.len());
    close(fd);

    let ok = n == msg.len() as isize && &buf[..msg.len()] == msg;
    report(name, ok)
}

unsafe fn test_f2fs_double_indirect_node() -> bool {
    let name = b"f2fs_double_indirect_node\0";
    let filepath = b"/mnt/vt_dindirect.txt\0";

    let fd = open(filepath.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }

    // Seek to block 923 + 2038 + 2 * 1019 * 1019 + 5 = 2,079,688
    // 2,079,688 * 4096 = 8,518,402,048 bytes (which is within double indirect node range)
    let offset: off_t = 8_518_402_048;
    if lseek(fd, offset, SEEK_SET) != offset {
        close(fd);
        return report(name, false);
    }

    let msg = b"double indirect node block test content";
    if write(fd, msg.as_ptr(), msg.len()) != msg.len() as isize {
        close(fd);
        return report(name, false);
    }

    if lseek(fd, offset, SEEK_SET) != offset {
        close(fd);
        return report(name, false);
    }

    let mut buf = [0u8; 64];
    let n = read(fd, buf.as_mut_ptr(), msg.len());
    close(fd);

    let ok = n == msg.len() as isize && &buf[..msg.len()] == msg;
    report(name, ok)
}

unsafe fn test_f2fs_directories() -> bool {
    let name = b"f2fs_directories\0";

    if mkdir(b"/mnt/vt_dir\0".as_ptr(), 0o755) != 0 { return report(name, false); }

    let fd = open(b"/mnt/vt_dir/vt_file.txt\0".as_ptr(), O_CREAT | O_RDWR, 0o644);
    if fd < 0 {
        rmdir(b"/mnt/vt_dir\0".as_ptr());
        return report(name, false);
    }
    close(fd);

    // Cannot remove non-empty directory
    if rmdir(b"/mnt/vt_dir\0".as_ptr()) == 0 {
        unlink(b"/mnt/vt_dir/vt_file.txt\0".as_ptr());
        rmdir(b"/mnt/vt_dir\0".as_ptr());
        return report(name, false);
    }

    if unlink(b"/mnt/vt_dir/vt_file.txt\0".as_ptr()) != 0 {
        rmdir(b"/mnt/vt_dir\0".as_ptr());
        return report(name, false);
    }

    let ok = rmdir(b"/mnt/vt_dir\0".as_ptr()) == 0;
    report(name, ok)
}

/// Exercises statfs-visible block reclaim on the F2FS volume at /mnt:
/// write reduces `f_bfree`, unlink recovers it, a half-truncate frees
/// roughly half the file's blocks, and extending the file back up reads
/// as zeros in the newly-extended range.
///
/// File sizes are kept modest (4 MiB) — the test volume has limited free
/// space margin.
unsafe fn test_f2fs_statfs_reclaim() -> bool {
    let name = b"f2fs_statfs_reclaim\0";
    let mnt = b"/mnt\0";
    let filepath = b"/mnt/vt_statfs.txt\0";

    const FILE_SIZE: usize = 4 * 1024 * 1024; // 4 MiB
    const CHUNK: usize = 128 * 1024;
    const BLOCK: u64 = 4096; // F2FS f_bsize
    const RECOVERY_TOLERANCE_BLOCKS: u64 = 8; // "a few blocks"
    const HALF_TOLERANCE_PCT: u64 = 25;

    // Baseline.
    let baseline = match statfs_bfree(mnt.as_ptr()) {
        Some(v) => v,
        None => return report(name, false),
    };

    let buf = malloc(CHUNK);
    if buf.is_null() { return report(name, false); }
    for i in 0..CHUNK { *buf.add(i) = 0xAA; }

    // --- 1. write FILE_SIZE bytes; f_bfree must drop --------------------------
    let fd = open(filepath.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }

    let mut written = 0usize;
    while written < FILE_SIZE {
        if write(fd, buf, CHUNK) != CHUNK as isize {
            close(fd);
            unlink(filepath.as_ptr());
            return report(name, false);
        }
        written += CHUNK;
    }

    let after_write = match statfs_bfree(mnt.as_ptr()) {
        Some(v) => v,
        None => { close(fd); unlink(filepath.as_ptr()); return report(name, false); }
    };
    if after_write >= baseline {
        close(fd);
        unlink(filepath.as_ptr());
        return report(name, false);
    }

    // --- 2. unlink; f_bfree must recover to ~baseline --------------------------
    close(fd);
    if unlink(filepath.as_ptr()) != 0 { return report(name, false); }

    let after_unlink = match statfs_bfree(mnt.as_ptr()) {
        Some(v) => v,
        None => return report(name, false),
    };
    let unlink_delta = after_unlink.abs_diff(baseline);
    if unlink_delta > RECOVERY_TOLERANCE_BLOCKS {
        return report(name, false);
    }

    // --- 3. write again, ftruncate to half; f_bfree rises by ~half the file ---
    let fd = open(filepath.as_ptr(), O_CREAT | O_RDWR | O_TRUNC, 0o644);
    if fd < 0 { return report(name, false); }

    written = 0;
    while written < FILE_SIZE {
        if write(fd, buf, CHUNK) != CHUNK as isize {
            close(fd);
            unlink(filepath.as_ptr());
            return report(name, false);
        }
        written += CHUNK;
    }

    let after_full_write = match statfs_bfree(mnt.as_ptr()) {
        Some(v) => v,
        None => { close(fd); unlink(filepath.as_ptr()); return report(name, false); }
    };

    let half = (FILE_SIZE / 2) as off_t;
    if !ftruncate(fd, half) {
        close(fd);
        unlink(filepath.as_ptr());
        return report(name, false);
    }

    let after_half = match statfs_bfree(mnt.as_ptr()) {
        Some(v) => v,
        None => { close(fd); unlink(filepath.as_ptr()); return report(name, false); }
    };

    if after_half <= after_full_write {
        close(fd);
        unlink(filepath.as_ptr());
        return report(name, false);
    }
    let rise = after_half - after_full_write;
    let expected = (FILE_SIZE as u64 / 2) / BLOCK; // ~512 blocks
    let low = expected * (100 - HALF_TOLERANCE_PCT) / 100;
    let high = expected * (100 + HALF_TOLERANCE_PCT) / 100;
    if rise < low || rise > high {
        close(fd);
        unlink(filepath.as_ptr());
        return report(name, false);
    }

    // --- 4. truncate back up; the newly-extended range must read as zero ------
    if !ftruncate(fd, FILE_SIZE as off_t) {
        close(fd);
        unlink(filepath.as_ptr());
        return report(name, false);
    }

    if lseek(fd, half, SEEK_SET) != half {
        close(fd);
        unlink(filepath.as_ptr());
        return report(name, false);
    }

    let mut zero_ok = true;
    let mut remaining = FILE_SIZE - half as usize;
    while remaining > 0 {
        let want = if remaining < CHUNK { remaining } else { CHUNK };
        let n = read(fd, buf, want);
        if n != want as isize {
            zero_ok = false;
            break;
        }
        for i in 0..want {
            if *buf.add(i) != 0 { zero_ok = false; break; }
        }
        if !zero_ok { break; }
        remaining -= want;
    }
    close(fd);
    if !zero_ok {
        unlink(filepath.as_ptr());
        return report(name, false);
    }

    // --- 5. clean up; f_bfree returns to ~baseline ------------------------------
    if unlink(filepath.as_ptr()) != 0 { return report(name, false); }

    let final_bfree = match statfs_bfree(mnt.as_ptr()) {
        Some(v) => v,
        None => return report(name, false),
    };
    let final_delta = final_bfree.abs_diff(baseline);

    report(name, final_delta <= RECOVERY_TOLERANCE_BLOCKS)
}

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(STDOUT_FILENO, name.as_ptr(), name.len() - 1); // drop the NUL terminator
    if passed {
        write(STDOUT_FILENO, b": PASS\n".as_ptr(), 7);
    } else {
        write(STDOUT_FILENO, b": FAIL\n".as_ptr(), 7);
    }
    passed
}
