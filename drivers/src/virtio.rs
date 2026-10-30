//! Common Virtio structures and logic for PCI transport and Virtqueues.

// Kernel APIs come through `kenv` so the helpers below also build into the virtio-net module.
use crate::kenv::mm;
#[allow(unused_imports)]
use crate::kenv::pci::{PciDevice, pci_read_config_8, pci_read_config_16, pci_read_config_32};

// ── Virtio PCI Capabilities ──────────────────────────────────────────────────

pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;
pub const VIRTIO_PCI_CAP_PCI_CFG: u8 = 5;

#[repr(C, packed)]
pub struct VirtioPciCap {
    pub cap_vndr: u8,
    pub cap_next: u8,
    pub cap_len: u8,
    pub cfg_type: u8,
    pub bar: u8,
    pub padding: [u8; 3],
    pub offset: u32,
    pub length: u32,
}

// ── Virtio Common Configuration ──────────────────────────────────────────────

#[repr(C, packed)]
pub struct VirtioPciCommonCfg {
    pub device_feature_select: u32,
    pub device_feature: u32,
    pub driver_feature_select: u32,
    pub driver_feature: u32,
    pub config_msix_vector: u16,
    pub num_queues: u16,
    pub device_status: u8,
    pub config_generation: u8,
    pub queue_select: u16,
    pub queue_size: u16,
    pub queue_msix_vector: u16,
    pub queue_enable: u16,
    pub queue_notify_off: u16,
    pub queue_desc_lo: u32,
    pub queue_desc_hi: u32,
    pub queue_avail_lo: u32,
    pub queue_avail_hi: u32,
    pub queue_used_lo: u32,
    pub queue_used_hi: u32,
}

// ── Virtqueue ────────────────────────────────────────────────────────────────

pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;

#[repr(C, packed)]
pub struct VirtqDesc {
    pub addr: u64,
    pub len: u32,
    pub flags: u16,
    pub next: u16,
}

#[repr(C, packed)]
pub struct VirtqAvail {
    pub flags: u16,
    pub idx: u16,
    pub ring: [u16; 0], // Variable size
}

#[repr(C, packed)]
pub struct VirtqUsedElem {
    pub id: u32,
    pub len: u32,
}

#[repr(C, packed)]
pub struct VirtqUsed {
    pub flags: u16,
    pub idx: u16,
    pub ring: [VirtqUsedElem; 0], // Variable size
}

pub struct Virtqueue {
    pub index: u16,
    pub size: u16,
    pub notify_off: u16,
    pub desc: *mut VirtqDesc,
    pub avail: *mut VirtqAvail,
    pub used: *mut VirtqUsed,
    pub last_used_idx: u16,
    pub free_head: u16,
    pub num_free: u16,
}

impl Virtqueue {
    pub fn new(index: u16, size: u16, notify_off: u16) -> Option<Self> {
        // Calculate memory requirements
        // Desc: 16 bytes * size
        // Avail: 6 + 2 * size
        // Used: 6 + 8 * size
        
        let desc_size = 16 * size as usize;
        let avail_size = 6 + 2 * size as usize;
        let used_size = 6 + 8 * size as usize;
        
        let total_size = desc_size + avail_size + used_size;
        let order = (total_size + 4095) / 4096;
        let order = 31 - (order.next_power_of_two().leading_zeros() as usize);
        
        let phys = mm::buddy::alloc(order)?;
        let virt = mm::phys_to_virt(phys);
        
        unsafe {
            core::ptr::write_bytes(virt as *mut u8, 0, 4096 << order);
        }
        
        let desc = virt as *mut VirtqDesc;
        let avail = (virt + desc_size) as *mut VirtqAvail;
        let used = (virt + desc_size + ((avail_size + 15) & !15)) as *mut VirtqUsed;

        Some(Self {
            index,
            size,
            notify_off,
            desc,
            avail,
            used,
            last_used_idx: 0,
            free_head: 0,
            num_free: size,
        })
    }

    pub fn add_desc(&mut self, addr: u64, len: u32, flags: u16) -> u16 {
        let head = self.free_head;
        unsafe {
            let d = &mut *self.desc.add(head as usize);
            d.addr = addr;
            d.len = len;
            d.flags = flags;
            self.free_head = d.next;
        }
        self.num_free -= 1;
        head
    }

    pub fn submit(&mut self, head: u16) {
        unsafe {
            let avail = &mut *self.avail;
            let idx = avail.idx as usize % self.size as usize;
            // avail.ring is zero-sized in struct, need to access it manually
            let ring_ptr = (self.avail as usize + 4) as *mut u16;
            *ring_ptr.add(idx) = head;
            avail.idx = avail.idx.wrapping_add(1);
        }
    }
}

// ── Capability helpers shared by virtio-net and virtio-input ─────────────────

/// BAR `bar_idx` as a physical base (0 for an I/O BAR).
pub(crate) fn bar64(pci: &PciDevice, bar_idx: usize) -> u64 {
    let raw = pci.bars[bar_idx];
    if raw & 1 != 0 { return 0; }
    if (raw >> 1) & 3 == 2 && bar_idx + 1 < 6 {
        (raw & !0xF) as u64 | ((pci.bars[bar_idx + 1] as u64) << 32)
    } else {
        (raw & !0xF) as u64
    }
}

/// First MSI-X capability: (cap offset, mapped vector table), x86_64 only.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
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

#[allow(dead_code)]
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
