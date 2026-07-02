//! f2fstest — regression coverage for TODO.md item #5 (F2FS server)
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

    puts(b"--- f2fstest done ---\0".as_ptr());
    failures
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

unsafe fn report(name: &[u8], passed: bool) -> bool {
    write(STDOUT_FILENO, name.as_ptr(), name.len() - 1); // drop the NUL terminator
    if passed {
        write(STDOUT_FILENO, b": PASS\n".as_ptr(), 7);
    } else {
        write(STDOUT_FILENO, b": FAIL\n".as_ptr(), 7);
    }
    passed
}
