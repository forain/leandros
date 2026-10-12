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

/// mount(2) MS_RDONLY.
const MS_RDONLY: usize = 1;

unsafe fn err_errno(prefix: &[u8]) {
    err(prefix);
    let e = leandros_libc::get_errno();
    let msg: &[u8] = match e {
        1 => b"operation not permitted",
        6 => b"no such device",
        19 => b"no such device (not yet registered)",
        22 => b"invalid argument (no f2fs volume there?)",
        30 => b"read-only device (use -r or -o ro)",
        76 => b"ambiguous /dev/root",
        _ => b"error",
    };
    err(b": ");
    err(msg);
    err(b"\n");
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

    // mount [-r] [-o ro|rw] [-t fstype] <device> <target> [-t fstype]
    let mut flags = 0usize;
    let mut fstype: *const u8 = b"f2fs\0".as_ptr();
    let mut pos: [*const u8; 2] = [core::ptr::null(); 2];
    let mut npos = 0usize;
    let mut i = 1;
    while i < argc {
        let a = arg(argv, argc, i);
        if ceq(a, b"-r\0") {
            flags |= MS_RDONLY;
        } else if ceq(a, b"-o\0") && i + 1 < argc {
            i += 1;
            let o = arg(argv, argc, i);
            if ceq(o, b"ro\0") { flags |= MS_RDONLY; }
            else if ceq(o, b"rw\0") { flags &= !MS_RDONLY; }
            else { err(b"mount: only -o ro / -o rw are supported\n"); return 2; }
        } else if ceq(a, b"-t\0") && i + 1 < argc {
            i += 1;
            fstype = arg(argv, argc, i);
        } else if npos < 2 {
            pos[npos] = a;
            npos += 1;
        } else {
            npos = 3;
            break;
        }
        i += 1;
    }
    if npos != 2 {
        err(b"usage: mount [-a] | mount [-r] [-o ro|rw] <device> <target> [-t fstype]\n");
        return 2;
    }

    let r = mount(pos[0], pos[1], fstype, flags, core::ptr::null());
    if r < 0 {
        err_errno(b"mount: failed");
        return 1;
    }
    0
}
