#![no_std]
#![no_main]

//! lsusb — list the USB devices the kernel enumerated.
//!
//! `lsusb` prints one line per device. `lsusb --summary` prints the same
//! lines under a heading, and nothing at all when there are no USB devices;
//! init runs it right before the login prompt so the console shows what was
//! found (and what is not supported) on a machine whose boot log has
//! scrolled away.

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

unsafe fn out_dec(v: u8) {
    if v >= 100 { out(&[b'0' + v / 100]); }
    if v >= 10 { out(&[b'0' + (v / 10) % 10]); }
    out(&[b'0' + v % 10]);
}

fn speed_name(s: u8) -> &'static [u8] {
    match s {
        1 => b"full-speed",
        2 => b"low-speed",
        3 => b"high-speed",
        4 => b"super-speed",
        5 => b"super-speed+",
        _ => b"",
    }
}

/// What the device is to the kernel: its role bits, else its class.
fn role_name(role: u8, class: u8) -> &'static [u8] {
    match role & 0x0F {
        0x03 => b"keyboard + mouse",
        0x01 => b"keyboard",
        0x02 => b"mouse",
        0x04 => b"mass storage",
        0x08 => b"hub (devices behind it are not supported yet)",
        _ if role & 0x80 != 0 => b"HID device (not a boot keyboard/mouse: unsupported)",
        _ => match class {
            0x01 => b"audio (no driver)",
            0x02 | 0x0A => b"communications (no driver)",
            0x03 => b"HID (no driver)",
            0x06 => b"imaging (no driver)",
            0x07 => b"printer (no driver)",
            0x08 => b"mass storage (not bulk-only: unsupported)",
            0x09 => b"hub (devices behind it are not supported yet)",
            0x0B => b"smart card (no driver)",
            0x0E => b"video (no driver)",
            0xE0 => b"wireless (no driver)",
            0xEF => b"composite (no driver)",
            0xFF => b"vendor-specific (no driver)",
            _ => b"no driver",
        },
    }
}

unsafe fn arg_is(argv: *const *const u8, i: usize, s: &[u8]) -> bool {
    let p = *argv.add(i);
    if p.is_null() { return false; }
    for (k, &c) in s.iter().enumerate() {
        if *p.add(k) != c { return false; }
    }
    *p.add(s.len()) == 0
}

#[no_mangle]
pub unsafe extern "C" fn main(argc: i32, argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let summary = argc > 1 && (arg_is(argv, 1, b"--summary") || arg_is(argv, 1, b"-s"));
    let count = devinfo::usbdev_count();
    if count <= 0 {
        if !summary { out(b"No USB controllers or devices found.\n"); }
        return 0;
    }
    if summary { out(b"USB devices:\n"); }

    for i in 0..count as usize {
        let dev = match devinfo::usbdev_info(i) {
            Some(d) => d,
            None => continue,
        };
        if summary { out(b"  "); }
        out(b"Bus ");
        out_dec3(dev.bus + 1);
        out(b" Device ");
        out_dec3(dev.address);
        out(b": ID ");
        out_hex16(dev.vendor_id);
        out(b":");
        out_hex16(dev.product_id);
        if dev.port != 0 {
            out(b" port ");
            out_dec(dev.port);
        }
        let sp = speed_name(dev.speed);
        if !sp.is_empty() {
            out(b" ");
            out(sp);
        }
        out(b" - ");
        out(role_name(dev.role, dev.class));
        out(b"\n");
    }
    0
}
