#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{write, open, read, close, ioctl, lseek, O_RDONLY, O_RDWR, STDOUT_FILENO};
use leandros_libc::devinfo;

unsafe fn out(s: &[u8]) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

unsafe fn out_padded(s: &[u8], width: usize) {
    out(s);
    let pad = if s.len() < width { width - s.len() } else { 1 };
    for _ in 0..pad {
        out(b" ");
    }
}

/// `/dev/<name>` for index `i` from the kernel's block registry (`vda`,
/// `vda1`, `sda2`), or `/dev/vd<letter>` on a kernel that cannot say.
unsafe fn device_name(i: usize) -> ([u8; 21], usize) {
    let mut buf = [0u8; 21];
    buf[..5].copy_from_slice(b"/dev/");
    let mut name = [0u8; 16];
    match devinfo::blkdev_name(i, &mut name) {
        Some(n) => {
            buf[5..5 + n].copy_from_slice(&name[..n]);
            (buf, 5 + n)
        }
        None => {
            buf[5..7].copy_from_slice(b"vd");
            buf[7] = b'a' + (i as u8);
            (buf, 8)
        }
    }
}

/// `/sys/class/block/<name>/ro` (`1` for a read-only disk such as an NVMe
/// SSD without `nvme.rw=1`); false when it cannot be read.
unsafe fn is_ro(dev: &[u8]) -> bool {
    let name = &dev[5..];
    let mut path = [0u8; 64];
    let pre = b"/sys/class/block/";
    let suf = b"/ro\0";
    if pre.len() + name.len() + suf.len() > path.len() { return false; }
    path[..pre.len()].copy_from_slice(pre);
    path[pre.len()..pre.len() + name.len()].copy_from_slice(name);
    path[pre.len() + name.len()..pre.len() + name.len() + suf.len()].copy_from_slice(suf);
    let fd = open(path.as_ptr(), O_RDONLY, 0);
    if fd < 0 { return false; }
    let mut b = [0u8; 4];
    let n = read(fd, b.as_mut_ptr(), b.len());
    close(fd);
    n > 0 && b[0] == b'1'
}

/// Find the mountpoint for `device_bytes` by scanning the live mount table,
/// or an empty slice if not mounted.
unsafe fn mountpoint_for(device_bytes: &[u8]) -> ([u8; 32], usize) {
    let count = devinfo::mounts_count().max(0) as usize;
    for i in 0..count {
        if let Some(m) = devinfo::mounts_info(i) {
            if m.device() == device_bytes {
                return (m.mountpoint, m.mountpoint_len);
            }
        }
    }
    ([0u8; 32], 0)
}

unsafe fn out_num(mut v: i64) {
    let mut b = [0u8; 24];
    let neg = v < 0;
    if neg { v = -v; }
    let mut i = b.len();
    loop { i -= 1; b[i] = b'0' + (v % 10) as u8; v /= 10; if v == 0 { break; } }
    if neg { i -= 1; b[i] = b'-'; }
    out(&b[i..]);
}

unsafe fn cstr_len(p: *const u8) -> usize {
    let mut n = 0;
    while *p.add(n) != 0 { n += 1; }
    n
}

/// `lsblk --setrw <dev>`: BLKROSET(0) on a whole disk (root only; the kernel
/// answers EACCES otherwise). `lsblk --wtest <dev>` then writes a 4 KiB
/// pattern at offset 0 and reads it back (DESTROYS the first 4 KiB).
unsafe fn setrw_cmd(argv: *const *const u8) -> i32 {
    let dev = *argv.add(2);
    let fd = open(dev, O_RDONLY, 0);
    if fd < 0 { out(b"setrw: open failed\n"); return 1; }
    let zero: i32 = 0;
    let r = ioctl(fd, 0x125D, &zero as *const i32 as usize);
    close(fd);
    out(b"BLKROSET(0) = ");
    out_num(r as i64);
    out(b"\n");
    if r < 0 { 1 } else { 0 }
}

unsafe fn wtest_cmd(argv: *const *const u8) -> i32 {
    let dev = *argv.add(2);
    let fd = open(dev, O_RDWR, 0);
    if fd < 0 { out(b"wtest: open failed\n"); return 1; }
    let wbuf = [0xA5u8; 4096];
    let mut rbuf = [0u8; 4096];
    lseek(fd, 0, 0);
    let w = write(fd, wbuf.as_ptr(), wbuf.len());
    out(b"write = "); out_num(w as i64); out(b"\n");
    if w != 4096 { close(fd); return 1; }
    lseek(fd, 0, 0);
    let r = read(fd, rbuf.as_mut_ptr(), rbuf.len());
    close(fd);
    out(b"read = "); out_num(r as i64); out(b"\n");
    if r == 4096 && rbuf == wbuf { out(b"WTEST OK\n"); 0 } else { out(b"WTEST MISMATCH\n"); 1 }
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    if argc >= 3 {
        let a = core::slice::from_raw_parts(*argv.add(1), cstr_len(*argv.add(1)));
        if a == b"--setrw" { return setrw_cmd(argv); }
        if a == b"--wtest" { return wtest_cmd(argv); }
    }
    out_padded(b"NAME", 16);
    out_padded(b"SIZE", 8);
    out_padded(b"RO", 3);
    out_padded(b"FSTYPE", 8);
    out(b"MOUNTPOINT\n");

    let count = devinfo::blkdev_count();
    if count < 0 {
        return 0;
    }

    for i in 0..count as usize {
        let info = match devinfo::blkdev_info(i) {
            Some(v) => v,
            None => continue,
        };

        let (dev_buf, dev_len) = device_name(i);
        let dev = &dev_buf[..dev_len];
        out_padded(dev, 16);

        if info.total_blocks == 0 {
            out_padded(b"?", 8);
        } else {
            let mib = (info.total_blocks * info.block_size as u64) / (1024 * 1024);
            let mut size_buf = [0u8; 24];
            let mut w = 0usize;
            let mut n = mib;
            if n == 0 {
                size_buf[0] = b'0';
                w = 1;
            } else {
                let start = w;
                while n > 0 {
                    size_buf[w] = b'0' + (n % 10) as u8;
                    n /= 10;
                    w += 1;
                }
                size_buf[start..w].reverse();
            }
            size_buf[w] = b'M';
            w += 1;
            out_padded(&size_buf[..w], 8);
        }
        out_padded(if is_ro(dev) { b"1" } else { b"0" }, 3);

        match info.fstype {
            Some(name) => {
                let n = name.iter().position(|&b| b == 0).unwrap_or(name.len());
                out_padded(&name[..n], 8);
            }
            None => out_padded(b"-", 8),
        }

        let (mp, mp_len) = mountpoint_for(dev);
        if mp_len > 0 {
            out(&mp[..mp_len]);
        } else {
            out(b"-");
        }
        out(b"\n");
    }

    0
}
