//! PCI bus discovery and device enumeration.

use alloc::vec::Vec;

#[derive(Debug, Clone, Copy)]
pub struct PciDevice {
    pub bus: u8,
    pub dev: u8,
    pub func: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub bars: [u32; 6],
}

#[cfg(target_arch = "x86_64")]
unsafe fn pci_read_config(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let address = ((bus as u32) << 16) | ((dev as u32) << 11) |
                  ((func as u32) << 8) | ((offset as u32) & 0xFC) | 0x8000_0000;
    let mut val: u32;
    core::arch::asm!(
        "mov dx, 0xCF8",
        "out dx, eax",
        "mov dx, 0xCFC",
        "in eax, dx",
        inout("eax") address => val,
        out("dx") _,
        options(nomem, nostack)
    );
    val
}

#[cfg(not(target_arch = "x86_64"))]
unsafe fn pci_read_config(_bus: u8, _dev: u8, _func: u8, _offset: u8) -> u32 {
    0xFFFF_FFFF
}

#[cfg(target_arch = "x86_64")]
pub fn serial_debug(msg: &str) {
    for &b in msg.as_bytes() {
        unsafe {
            core::arch::asm!("out dx, al", in("dx") 0x3F8_u16, in("al") b);
        }
    }
}

#[cfg(not(target_arch = "x86_64"))]
pub fn serial_debug(_msg: &str) {}

fn hex_digit(v: u8) -> u8 {
    if v < 10 { b'0' + v } else { b'A' + v - 10 }
}

pub fn serial_debug_hex(v: u32) {
    let mut buf = [0u8; 10];
    buf[0] = b'0';
    buf[1] = b'x';
    for i in 0..8 {
        buf[9 - i] = hex_digit(((v >> (i * 4)) & 0xF) as u8);
    }
    serial_debug(unsafe { core::str::from_utf8_unchecked(&buf) });
}

pub fn serial_debug_hex_64(v: u64) {
    serial_debug("0x");
    for i in (0..16).rev() {
        serial_debug(unsafe { core::str::from_utf8_unchecked(&[hex_digit(((v >> (i * 4)) & 0xF) as u8)]) });
    }
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn pci_read_config_8(bus: u8, dev: u8, func: u8, offset: u8) -> u8 {
    let val = pci_read_config(bus, dev, func, offset);
    ((val >> ((offset & 3) * 8)) & 0xFF) as u8
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn pci_read_config_16(bus: u8, dev: u8, func: u8, offset: u8) -> u16 {
    let val = pci_read_config(bus, dev, func, offset);
    ((val >> ((offset & 3) * 8)) & 0xFFFF) as u16
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn pci_read_config_32(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    pci_read_config(bus, dev, func, offset)
}

#[cfg(target_arch = "x86_64")]
pub unsafe fn pci_read_config_32_any(bus: u8, dev: u8, func: u8, offset: u8) -> u32 {
    let b0 = pci_read_config_8(bus, dev, func, offset) as u32;
    let b1 = pci_read_config_8(bus, dev, func, offset + 1) as u32;
    let b2 = pci_read_config_8(bus, dev, func, offset + 2) as u32;
    let b3 = pci_read_config_8(bus, dev, func, offset + 3) as u32;
    b0 | (b1 << 8) | (b2 << 16) | (b3 << 24)
}

#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn pci_read_config_8(_bus: u8, _dev: u8, _func: u8, _offset: u8) -> u8 { 0 }
#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn pci_read_config_16(_bus: u8, _dev: u8, _func: u8, _offset: u8) -> u16 { 0 }
#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn pci_read_config_32(_bus: u8, _dev: u8, _func: u8, _offset: u8) -> u32 { 0 }
#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn pci_read_config_32_any(_bus: u8, _dev: u8, _func: u8, _offset: u8) -> u32 { 0 }

#[cfg(target_arch = "x86_64")]
pub unsafe fn pci_write_config_16(bus: u8, dev: u8, func: u8, offset: u8, val: u16) {
    let address = ((bus as u32) << 16) | ((dev as u32) << 11) |
                  ((func as u32) << 8) | ((offset as u32) & 0xFC) | 0x8000_0000;
    
    let mut current = pci_read_config(bus, dev, func, offset & 0xFC);
    let shift = (offset & 3) * 8;
    current &= !(0xFFFF << shift);
    current |= (val as u32) << shift;

    core::arch::asm!(
        "mov dx, 0xCF8",
        "out dx, eax",
        "mov eax, edi",
        "mov dx, 0xCFC",
        "out dx, eax",
        in("eax") address,
        in("edi") current,
        out("dx") _,
        options(nomem, nostack)
    );
}

#[cfg(not(target_arch = "x86_64"))]
pub unsafe fn pci_write_config_16(_bus: u8, _dev: u8, _func: u8, _offset: u8, _val: u16) {}

impl PciDevice {
    pub unsafe fn find_capability(&self, cap_id: u8) -> Option<u8> {
        let status = pci_read_config_16(self.bus, self.dev, self.func, 0x06);
        if (status & (1 << 4)) == 0 { return None; }
        let mut cap_ptr = pci_read_config_8(self.bus, self.dev, self.func, 0x34);
        while cap_ptr != 0 {
            let id = pci_read_config_8(self.bus, self.dev, self.func, cap_ptr);
            if id == cap_id { return Some(cap_ptr); }
            cap_ptr = pci_read_config_8(self.bus, self.dev, self.func, cap_ptr + 1);
        }
        None
    }
}

pub fn scan() -> Vec<PciDevice> {
    let mut devices = Vec::new();
    serial_debug("[PCI] Scanning bus...\n");

    for bus in 0..=255 {
        for dev in 0..32 {
            let val = unsafe { pci_read_config(bus as u8, dev as u8, 0, 0) };
            if val == 0xFFFF_FFFF { continue; }
            let vendor_id = (val & 0xFFFF) as u16;
            let device_id = (val >> 16) as u16;
            serial_debug("[PCI] Found dev ");
            serial_debug_hex(vendor_id as u32);
            serial_debug(":");
            serial_debug_hex(device_id as u32);
            serial_debug("\n");

            let class_rev = unsafe { pci_read_config(bus as u8, dev as u8, 0, 0x08) };
            let class = (class_rev >> 24) as u8;
            let subclass = (class_rev >> 16) as u8;

            let mut bars = [0u32; 6];
            for i in 0..6 {
                let bar = unsafe { pci_read_config(bus as u8, dev as u8, 0, 0x10 + (i as u8 * 4)) };
                bars[i] = bar;
                serial_debug("  BAR");
                serial_debug_hex(i as u32);
                serial_debug("=");
                serial_debug_hex(bar);
                serial_debug("\n");
            }
            devices.push(PciDevice {
                bus: bus as u8, dev: dev as u8, func: 0,
                vendor_id, device_id, class, subclass, bars,
            });
        }
    }
    devices
}

pub fn find_device(vendor_id: u16, device_id: u16) -> Option<PciDevice> {
    scan().into_iter().find(|d| d.vendor_id == vendor_id && d.device_id == device_id)
}
