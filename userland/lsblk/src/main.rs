#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{write, STDOUT_FILENO};
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

/// Device name for index `i` as `/dev/vd<letter>` — mirrors the naming
/// `sys_mount`/`servers/f2fs` already use elsewhere.
fn device_name(i: usize) -> [u8; 8] {
    let mut buf = *b"/dev/vd?";
    buf[7] = b'a' + (i as u8);
    buf
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

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    out_padded(b"NAME", 10);
    out_padded(b"SIZE", 8);
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

        let name = device_name(i);
        out_padded(&name, 10);

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

        match info.fstype {
            Some(name) => {
                let n = name.iter().position(|&b| b == 0).unwrap_or(name.len());
                out_padded(&name[..n], 8);
            }
            None => out_padded(b"-", 8),
        }

        let (mp, mp_len) = mountpoint_for(&name);
        if mp_len > 0 {
            out(&mp[..mp_len]);
        } else {
            out(b"-");
        }
        out(b"\n");
    }

    0
}
