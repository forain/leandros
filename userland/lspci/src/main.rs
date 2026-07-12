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

unsafe fn out_hex8(v: u8) {
    out(&[hex_digit(v >> 4), hex_digit(v & 0xF)]);
}

unsafe fn out_hex16(v: u16) {
    out_hex8((v >> 8) as u8);
    out_hex8(v as u8);
}

/// Class-code name, covering what's actually attachable in this OS (VirtIO
/// devices + a PCI-to-ISA/host bridge + USB/xHCI). Not a full pci.ids
/// database — just enough for `lspci` output to read like the real thing.
fn class_name(class: u8) -> &'static [u8] {
    match class {
        0x00 => b"Unclassified device",
        0x01 => b"Mass storage controller",
        0x02 => b"Network controller",
        0x03 => b"Display controller",
        0x04 => b"Multimedia controller",
        0x06 => b"Bridge",
        0x0C => b"Serial bus controller",
        _ => b"Unknown",
    }
}

#[no_mangle]
pub unsafe extern "C" fn main(_argc: i32, _argv: *const *const u8, _envp: *const *const u8) -> i32 {
    let count = devinfo::pcidev_count();
    if count < 0 {
        return 0;
    }

    for i in 0..count as usize {
        let dev = match devinfo::pcidev_info(i) {
            Some(d) => d,
            None => continue,
        };

        out_hex8(dev.bus);
        out(b":");
        out_hex8(dev.dev);
        out(b".");
        out_hex8(dev.func & 0x7);
        out(b" ");
        out(class_name(dev.class));
        out(b": ");
        out_hex16(dev.vendor_id);
        out(b":");
        out_hex16(dev.device_id);
        out(b"\n");
    }
    0
}
