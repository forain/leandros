#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{write, open, read, close, O_RDONLY, STDOUT_FILENO, STDERR_FILENO};
use leandros_libc::fstab;

unsafe fn out(s: &[u8]) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

unsafe fn err(s: &[u8]) {
    write(STDERR_FILENO, s.as_ptr(), s.len());
}

/// Print `s` then pad with spaces to `width` columns (minimum one space),
/// mirroring `column -t`.
unsafe fn out_padded(s: &str, width: usize) {
    out(s.as_bytes());
    let pad = if s.len() < width { width - s.len() } else { 1 };
    for _ in 0..pad {
        out(b" ");
    }
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let fd = open(b"/etc/fstab\0".as_ptr(), O_RDONLY, 0);
    if fd < 0 {
        err(b"fstab: cannot open /etc/fstab\n");
        return 1;
    }
    let mut buf = [0u8; 4096];
    let n = read(fd, buf.as_mut_ptr(), buf.len());
    close(fd);
    if n <= 0 {
        return 0;
    }
    let content = core::str::from_utf8(&buf[..n as usize]).unwrap_or("");

    out_padded("DEVICE", 14);
    out_padded("MOUNTPOINT", 14);
    out_padded("FSTYPE", 10);
    out(b"OPTIONS\n");

    for line in content.lines() {
        if let Some(e) = fstab::parse_line(line) {
            out_padded(e.device(), 14);
            out_padded(e.mountpoint(), 14);
            out_padded(e.fstype(), 10);
            out(e.options().as_bytes());
            out(b"\n");
        }
    }
    0
}
