//! `CONFIG_VIRTIO_NET=n`: no virtio-net device is ever claimed. The virtio PCI
//! capability walkers are shared with virtio-input (copied verbatim from the
//! real driver), everything else reports "no device".
#![allow(dead_code)]
use crate::pci::{PciDevice, pci_read_config_8, pci_read_config_16, pci_read_config_32};
use mm;

pub fn init() {}
pub fn device_count() -> usize { 0 }
pub fn get_mac_address(_i: usize) -> Option<[u8; 6]> { None }
pub fn send_packet(_i: usize, _b: &[u8]) -> bool { false }
pub fn poll_receive(_i: usize, _b: &mut [u8]) -> Option<usize> { None }
pub fn rx_rearm(_i: usize) -> bool { false }
pub fn rx_irq_armed(_i: usize) -> bool { false }
pub fn link_up(_i: usize) -> bool { false }
pub fn link_generation(_i: usize) -> u32 { 0 }
pub fn rx_irq_count(_i: usize) -> u64 { 0 }

fn bar64(pci: &PciDevice, bar_idx: usize) -> u64 {
    let raw = pci.bars[bar_idx];
    if raw & 1 != 0 { return 0; }
    if (raw >> 1) & 3 == 2 && bar_idx + 1 < 6 {
        (raw & !0xF) as u64 | ((pci.bars[bar_idx + 1] as u64) << 32)
    } else {
        (raw & !0xF) as u64
    }
}

#[cfg(target_arch = "x86_64")]
pub(crate) unsafe fn find_msix(pci: &PciDevice) -> Option<(u8, *mut u32)> {
    let mut cap = pci_read_config_8(pci.bus, pci.dev, pci.func, 0x34);
    while cap != 0 {
        if pci_read_config_8(pci.bus, pci.dev, pci.func, cap) == 0x11 {
            let ctrl = pci_read_config_16(pci.bus, pci.dev, pci.func, cap + 2);
            let entries = (ctrl & 0x7FF) as usize + 1;
            let tbl = pci_read_config_32(pci.bus, pci.dev, pci.func, cap + 4);
            let bir = (tbl & 7) as usize;
            let toff = (tbl & !7) as usize;
            if bir < 6 && pci.bars[bir] & 1 == 0 {
                let base = bar64(pci, bir);
                if base != 0 {
                    let virt = mm::paging::map_kernel_device(
                        base as usize + toff, entries * 16,
                        mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO,
                    ).unwrap_or_else(|| mm::phys_to_virt(base as usize + toff));
                    return Some((cap, virt as *mut u32));
                }
            }
            return None;
        }
        cap = pci_read_config_8(pci.bus, pci.dev, pci.func, cap + 1);
    }
    None
}

pub(crate) unsafe fn find_isr(pci: &PciDevice) -> *mut u8 {
    let mut cap = pci_read_config_8(pci.bus, pci.dev, pci.func, 0x34);
    while cap != 0 {
        if pci_read_config_8(pci.bus, pci.dev, pci.func, cap) == 0x09
            && pci_read_config_8(pci.bus, pci.dev, pci.func, cap + 3) == 3
        {
            let bar_idx = pci_read_config_8(pci.bus, pci.dev, pci.func, cap + 4) as usize;
            let offset = pci_read_config_32(pci.bus, pci.dev, pci.func, cap + 8);
            let length = pci_read_config_32(pci.bus, pci.dev, pci.func, cap + 12);
            if bar_idx < 6 {
                let base = bar64(pci, bar_idx);
                if base != 0 {
                    let phys = base as usize + offset as usize;
                    return mm::paging::map_kernel_device(
                        phys, length as usize,
                        mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO,
                    ).unwrap_or_else(|| mm::phys_to_virt(phys)) as *mut u8;
                }
            }
        }
        cap = pci_read_config_8(pci.bus, pci.dev, pci.func, cap + 1);
    }
    core::ptr::null_mut()
}
