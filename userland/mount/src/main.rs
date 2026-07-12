#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{
    write, mount, mkdir, open, read, close, O_RDONLY, STDOUT_FILENO, STDERR_FILENO,
};
use leandros_libc::devinfo;
use leandros_libc::fstab;

unsafe fn out(s: &[u8]) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

unsafe fn err(s: &[u8]) {
    write(STDERR_FILENO, s.as_ptr(), s.len());
}

unsafe fn ceq(a: *const u8, lit: &[u8]) -> bool {
    let mut i = 0usize;
    loop {
        let ac = *a.add(i);
        let bc = lit[i];
        if ac != bc { return false; }
        if ac == 0 { return true; }
        i += 1;
    }
}

unsafe fn arg(argv: *const *const u8, argc: i32, i: i32) -> *const u8 {
    if i >= argc { core::ptr::null() } else { *argv.offset(i as isize) }
}

/// `mount` with no arguments: list currently mounted filesystems, mirroring
/// Linux's behavior of reading /proc/mounts. There is no dynamic
/// /proc/mounts on this OS (see kernel/src/syscall.rs SYS_MOUNTS_INFO doc),
/// so this reads the live VFS mount table via syscall instead.
unsafe fn cmd_list() {
    let count = devinfo::mounts_count();
    if count < 0 { return; }
    for i in 0..count as usize {
        if let Some(m) = devinfo::mounts_info(i) {
            out(m.device());
            out(b" on ");
            out(m.mountpoint());
            out(b" type ");
            out(m.fstype());
            out(b" (rw)\n");
        }
    }
}

fn nul_terminate(s: &str, buf: &mut [u8; 65]) {
    let n = s.len().min(64);
    buf[..n].copy_from_slice(&s.as_bytes()[..n]);
}

/// `mount -a`: mount every /etc/fstab entry not already mounted.
unsafe fn cmd_mount_all() {
    let fd = open(b"/etc/fstab\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        err(b"mount: cannot open /etc/fstab\n");
        return;
    }
    let mut buf = [0u8; 4096];
    let n = read(fd, buf.as_mut_ptr(), buf.len());
    close(fd);
    if n <= 0 { return; }
    let content = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");

    let mount_count = devinfo::mounts_count().max(0) as usize;

    for line in content.lines() {
        let entry = match fstab::parse_line(line) {
            Some(e) => e,
            None => continue,
        };
        if entry.mountpoint() == "/" { continue; }

        let mut already = false;
        for i in 0..mount_count {
            if let Some(m) = devinfo::mounts_info(i) {
                if m.mountpoint() == entry.mountpoint().as_bytes() {
                    already = true;
                    break;
                }
            }
        }
        if already { continue; }

        let mut mp = [0u8; 65];
        nul_terminate(entry.mountpoint(), &mut mp);
        mkdir(mp.as_ptr(), 0o755);

        let mut dev = [0u8; 65];
        nul_terminate(entry.device(), &mut dev);
        let mut fst = [0u8; 65];
        nul_terminate(entry.fstype(), &mut fst);

        let r = mount(dev.as_ptr(), mp.as_ptr(), fst.as_ptr(), 0, core::ptr::null());
        if r < 0 {
            err(b"mount: failed to mount ");
            err(entry.device().as_bytes());
            err(b"\n");
        } else {
            out(b"mounted ");
            out(entry.device().as_bytes());
            out(b" at ");
            out(entry.mountpoint().as_bytes());
            out(b"\n");
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc < 2 {
        cmd_list();
        return 0;
    }

    if ceq(arg(argv, argc, 1), b"-a\0") {
        cmd_mount_all();
        return 0;
    }

    if argc < 3 {
        err(b"usage: mount [-a] | mount <device> <target> [-t fstype]\n");
        return 2;
    }

    let dev = arg(argv, argc, 1);
    let target = arg(argv, argc, 2);
    let fstype: *const u8 = if argc >= 5 && ceq(arg(argv, argc, 3), b"-t\0") {
        arg(argv, argc, 4)
    } else {
        b"f2fs\0".as_ptr()
    };

    let r = mount(dev, target, fstype, 0, core::ptr::null());
    if r < 0 {
        err(b"mount: failed\n");
        return 1;
    }
    0
}
