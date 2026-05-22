//! Common Virtio structures and logic for PCI transport and Virtqueues.

// No imports used currently in this file, keeping it for its structures.

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
