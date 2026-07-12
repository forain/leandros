//! LeandrOS Init - userspace init program (PID 1)
//!
//! This is the first userspace program that runs and manages the system.
//! It mounts the F2FS root filesystem, copies userland files, pivots root,
//! and launches the shell.

#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{
    write, STDOUT_FILENO, getpid, execve, sched_yield, mount, pivot_root, mkdir,
    open, read, close, O_RDONLY, O_WRONLY, O_CREAT, O_TRUNC
};

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    sched_yield();
    write_str("LeandrOS Init (PID 1) starting...\n");

    write_str("Init PID: ");
    write_u32(getpid() as u32);
    write_str("\n");

    // 1. Mount F2FS disk (/dev/vdb is device index 1, i.e., f2fs-data0.img)
    write_str("Mounting F2FS disk /dev/vdb to /mnt...\n");
    let mount_res = mount(
        b"/dev/vdb\0".as_ptr(),
        b"/mnt\0".as_ptr(),
        b"f2fs\0".as_ptr(),
        0,
        core::ptr::null()
    );

    if mount_res < 0 {
        write_str("ERROR: Failed to mount /dev/vdb to /mnt!\n");
        loop { sched_yield(); }
    }
    write_str("F2FS mounted successfully at /mnt!\n");

    // 2. Create old_root directory on F2FS mount for pivoting
    mkdir(b"/mnt/old_root\0".as_ptr(), 0o755);

    // 4. Pivot root to F2FS mounted filesystem
    write_str("Pivoting root to /mnt (old root at /mnt/old_root)...\n");
    let pivot_res = pivot_root(b"/mnt\0".as_ptr(), b"/mnt/old_root\0".as_ptr());
    if pivot_res < 0 {
        write_str("ERROR: pivot_root failed!\n");
        loop { sched_yield(); }
    }
    write_str("pivot_root successful! Root is now F2FS.\n");

    // 4b. Mount anything else listed in /etc/fstab (the "/" entry was just
    // handled above by the hardcoded bootstrap mount — fstab can't drive
    // that one since fstab itself lives on the filesystem being mounted).
    mount_from_fstab();

    // 5. Exec shell from F2FS mounted root
    write_str("Launching shell via execve...\n");
    let path = b"/bin/shell\0";
    let argv: [*const u8; 2] = [path.as_ptr(), core::ptr::null()];
    let envp: [*const u8; 1] = [core::ptr::null()];

    execve(path.as_ptr(), argv.as_ptr(), envp.as_ptr());

    // If execve returns, it failed
    write_str("ERROR: execve /bin/shell failed!\n");

    loop {
        sched_yield();
    }
}

/// Read `/etc/fstab` and mount every entry except the root ("/") one, which
/// the hardcoded bootstrap mount above already handled.
unsafe fn mount_from_fstab() {
    let fd = open(b"/etc/fstab\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        return;
    }
    let mut buf = [0u8; 4096];
    let n = read(fd, buf.as_mut_ptr(), buf.len());
    close(fd);
    if n <= 0 {
        return;
    }
    let content = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");

    for line in content.lines() {
        let entry = match leandros_libc::fstab::parse_line(line) {
            Some(e) => e,
            None => continue,
        };
        if entry.mountpoint() == "/" {
            continue;
        }

        write_str("Mounting ");
        write_str(entry.device());
        write_str(" at ");
        write_str(entry.mountpoint());
        write_str(" (");
        write_str(entry.fstype());
        write_str(")...\n");

        let mut mp_buf = [0u8; 65];
        let mp = entry.mountpoint();
        mp_buf[..mp.len()].copy_from_slice(mp.as_bytes());
        mkdir(mp_buf.as_ptr(), 0o755);

        let mut dev_buf = [0u8; 65];
        let dev = entry.device();
        dev_buf[..dev.len()].copy_from_slice(dev.as_bytes());

        let mut fst_buf = [0u8; 65];
        let fst = entry.fstype();
        fst_buf[..fst.len()].copy_from_slice(fst.as_bytes());

        let r = mount(dev_buf.as_ptr(), mp_buf.as_ptr(), fst_buf.as_ptr(), 0, core::ptr::null());
        if r < 0 {
            write_str("  WARNING: mount failed\n");
        }
    }
}

unsafe fn copy_file(src: &[u8], dst: &[u8]) -> bool {
    let fd_in = open(src.as_ptr(), O_RDONLY, 0);
    if fd_in < 0 {
        return false;
    }

    let fd_out = open(dst.as_ptr(), O_WRONLY | O_CREAT | O_TRUNC, 0o755);
    if fd_out < 0 {
        close(fd_in);
        return false;
    }

    let mut buf = [0u8; 4096];
    loop {
        let n = read(fd_in, buf.as_mut_ptr(), buf.len());
        if n < 0 {
            close(fd_in);
            close(fd_out);
            return false;
        }
        if n == 0 {
            break;
        }
        let mut written = 0;
        while written < n {
            let w = write(fd_out, buf.as_ptr().add(written as usize), (n - written) as usize);
            if w <= 0 {
                close(fd_in);
                close(fd_out);
                return false;
            }
            written += w;
        }
    }

    close(fd_in);
    close(fd_out);
    true
}

unsafe fn write_str(s: &str) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

unsafe fn write_u32(mut n: u32) {
    let mut buf = [0u8; 10];
    if n == 0 {
        write(STDOUT_FILENO, b"0".as_ptr(), 1);
        return;
    }
    let mut i = 10usize;
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    write(STDOUT_FILENO, buf.as_ptr().add(i), 10 - i);
}
