#![no_std]
#![no_main]

extern crate leandros_libc;

use leandros_libc::{write, STDOUT_FILENO};
use leandros_libc::devinfo;

unsafe fn out(s: &[u8]) {
    write(STDOUT_FILENO, s.as_ptr(), s.len());
}

fn hex_digit(v: u8) -> u8 {
    if v < 10 { b'0' + v } else { b'a' + v - 10 }
}

unsafe fn out_hex16(v: u16) {
    let b = v.to_be_bytes();
    for byte in b {
        out(&[hex_digit(byte >> 4), hex_digit(byte & 0xF)]);
    }
}

unsafe fn out_dec3(v: u8) {
    out(&[
        b'0' + (v / 100) % 10,
        b'0' + (v / 10) % 10,
        b'0' + v % 10,
    ]);
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let count = devinfo::usbdev_count();
    if count <= 0 {
        out(b"No USB controllers or devices found.\n");
        return 0;
    }

    for i in 0..count as usize {
        let dev = match devinfo::usbdev_info(i) {
            Some(d) => d,
            None => continue,
        };
        out(b"Bus ");
        out_dec3(dev.bus + 1);
        out(b" Device ");
        out_dec3(dev.address);
        out(b": ID ");
        out_hex16(dev.vendor_id);
        out(b":");
        out_hex16(dev.product_id);
        out(b"\n");
    }
    0
}
