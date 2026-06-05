//! VirtIO block device driver (PCI transport, polling mode).
//!
//! Enumerates all VirtIO block PCI devices and stores them in VIRTIO_BLK_DEVICES.
//! Public API: read_block / write_block (4096-byte granularity) and has_f2fs.

use spin::Mutex;
use crate::pci::{PciDevice, find_all_devices, pci_read_config_8, pci_read_config_32};
use mm;

const VIRTIO_PCI_VENDOR: u16 = 0x1af4;
const VIRTIO_PCI_DEVICE_BLK_MODERN: u16 = 0x1042;
const VIRTIO_PCI_DEVICE_BLK_LEGACY: u16 = 0x1001;

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER: u8 = 2;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;
const VIRTIO_STATUS_DRIVER_OK: u8 = 4;

const VIRTIO_BLK_T_IN:  u32 = 0; // read from device
const VIRTIO_BLK_T_OUT: u32 = 1; // write to device

const VIRTQ_DESC_F_NEXT:  u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

const SECTORS_PER_BLOCK: u64 = 8; // 4096 / 512
const BLOCK_SIZE: usize = 4096;

const F2FS_MAGIC: u32 = 0xF2F5_2010;
const F2FS_SB_OFFSET: usize = 1024; // byte offset within first block

const MAX_BLK_DEVICES: usize = 8;

// ── On-device register layout ────────────────────────────────────────────────

#[repr(C, packed)]
struct VirtioPciCommonCfg {
    device_feature_select: u32,
    device_feature: u32,
    driver_feature_select: u32,
    driver_feature: u32,
    config_msix_vector: u16,
    num_queues: u16,
    device_status: u8,
    config_generation: u8,
    queue_select: u16,
    queue_size: u16,
    queue_msix_vector: u16,
    queue_enable: u16,
    queue_notify_off: u16,
    queue_desc: u64,
    queue_driver: u64,
    queue_device: u64,
}

// ── Virtqueue ────────────────────────────────────────────────────────────────

#[repr(C, packed)]
struct VirtqDesc { addr: u64, len: u32, flags: u16, next: u16 }

#[repr(C, packed)]
struct VirtqAvail { flags: u16, idx: u16, ring: [u16; 32] }

#[repr(C, packed)]
struct VirtqUsedElem { id: u32, len: u32 }

#[repr(C, packed)]
struct VirtqUsed { flags: u16, idx: u16, ring: [VirtqUsedElem; 32] }

struct VirtQueue {
    size: u16,
    notify_off: u16,
    free_head: u16,
    num_free: u16,
    last_used_idx: u16,
    desc: *mut VirtqDesc,
    avail: *mut VirtqAvail,
    used: *mut VirtqUsed,
}

unsafe impl Send for VirtQueue {}
unsafe impl Sync for VirtQueue {}

impl VirtQueue {
    unsafe fn alloc(id: u16, cfg: *mut VirtioPciCommonCfg) -> Option<Self> {
        (*cfg).queue_select = id;
        let size = (*cfg).queue_size;
        if size == 0 { return None; }
        let notify_off = (*cfg).queue_notify_off;

        let desc_phys = mm::buddy::alloc(0)?;
        let avail_phys = mm::buddy::alloc(0)?;
        let used_phys  = mm::buddy::alloc(0)?;

        let desc = mm::phys_to_virt(desc_phys) as *mut VirtqDesc;
        let avail = mm::phys_to_virt(avail_phys) as *mut VirtqAvail;
        let used  = mm::phys_to_virt(used_phys) as *mut VirtqUsed;

        core::ptr::write_bytes(desc  as *mut u8, 0, 4096);
        core::ptr::write_bytes(avail as *mut u8, 0, 4096);
        core::ptr::write_bytes(used  as *mut u8, 0, 4096);

        for i in 0..(size - 1) { (*desc.add(i as usize)).next = i + 1; }
        (*desc.add((size - 1) as usize)).next = 0xFFFF;

        (*cfg).queue_desc   = desc_phys as u64;
        (*cfg).queue_driver = avail_phys as u64;
        (*cfg).queue_device = used_phys as u64;
        (*cfg).queue_enable = 1;

        Some(Self { size, notify_off, free_head: 0, num_free: size, last_used_idx: 0, desc, avail, used })
    }

    unsafe fn alloc_desc(&mut self, addr: u64, len: u32, flags: u16) -> u16 {
        let id = self.free_head;
        let d = self.desc.add(id as usize);
        self.free_head = (*d).next;
        self.num_free -= 1;
        (*d).addr = addr; (*d).len = len; (*d).flags = flags; (*d).next = 0;
        id
    }

    unsafe fn chain(&mut self, from: u16, to: u16) {
        let d = self.desc.add(from as usize);
        (*d).flags |= VIRTQ_DESC_F_NEXT;
        (*d).next = to;
    }

    unsafe fn submit(&mut self, head: u16) {
        let a = self.avail;
        let idx = (*a).idx as usize % self.size as usize;
        // Use manual offset to avoid out-of-bounds on the 32-element ring array
        let ring_ptr = (a as usize + 4) as *mut u16;
        ring_ptr.add(idx).write_volatile(head);
        
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        (*a).idx = (*a).idx.wrapping_add(1);
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
    }

    unsafe fn free_chain(&mut self, mut head: u16) {
        loop {
            let d = self.desc.add(head as usize);
            let flags = (*d).flags;
            let next = (*d).next;
            (*d).next = self.free_head;
            self.free_head = head;
            self.num_free += 1;
            if (flags & VIRTQ_DESC_F_NEXT) != 0 { head = next; } else { break; }
        }
    }

    unsafe fn wait_for_completion(&mut self) {
        while self.last_used_idx == (*self.used).idx {
            core::hint::spin_loop();
        }
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        self.last_used_idx = self.last_used_idx.wrapping_add(1);
    }
}

// ── VirtIO block request header ──────────────────────────────────────────────

#[repr(C, packed)]
struct VirtioBlkReqHdr { type_: u32, reserved: u32, sector: u64 }

// ── Per-device state ─────────────────────────────────────────────────────────

struct VirtioBlkDevice {
    _pci: PciDevice,
    common_cfg: *mut VirtioPciCommonCfg,
    notify_cfg: *mut u32,
    notify_off_multiplier: u32,
    queue: VirtQueue,
    // Pre-allocated DMA pages reused per request (single-threaded polling).
    req_phys:  usize, // 4096-byte page: [VirtioBlkReqHdr (16B)] [status (1B)]
    data_phys: usize, // 4096-byte page: sector data
}

unsafe impl Send for VirtioBlkDevice {}
unsafe impl Sync for VirtioBlkDevice {}

impl VirtioBlkDevice {
    unsafe fn new(pci: PciDevice) -> Option<Self> {
        let mut common_cfg: *mut VirtioPciCommonCfg = core::ptr::null_mut();
        let mut notify_cfg: *mut u32 = core::ptr::null_mut();
        let mut notify_off_multiplier = 0u32;

        let mut cap_ptr = pci_read_config_8(pci.bus, pci.dev, pci.func, 0x34);
        while cap_ptr != 0 {
            let cap_id = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr);
            if cap_id == 0x09 {
                let cfg_type = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 3);
                let bar_idx  = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 4);
                let offset   = pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 8);
                let length   = pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 12);

                if (bar_idx as usize) < 6 {
                    let raw_bar = pci.bars[bar_idx as usize];
                    // Skip I/O space BARs (bit 0 = 1)
                    if raw_bar & 1 == 0 {
                        // 64-bit MMIO BAR: bits 2:1 == 0b10 → address spans this and next slot
                        let bar64: u64 = if (raw_bar >> 1) & 3 == 2 && (bar_idx as usize) + 1 < 6 {
                            let lo = (raw_bar & !0xF) as u64;
                            let hi = pci.bars[bar_idx as usize + 1] as u64;
                            lo | (hi << 32)
                        } else {
                            (raw_bar & !0xF) as u64
                        };

                        if bar64 != 0 {
                            let virt = mm::paging::map_kernel_device(
                                bar64 as usize + offset as usize,
                                length as usize,
                                mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO,
                            ).unwrap_or_else(|| mm::phys_to_virt(bar64 as usize + offset as usize));
                            match cfg_type {
                                VIRTIO_PCI_CAP_COMMON_CFG => common_cfg = virt as *mut VirtioPciCommonCfg,
                                VIRTIO_PCI_CAP_NOTIFY_CFG => {
                                    notify_off_multiplier = pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 16);
                                    notify_cfg = virt as *mut u32;
                                },
                                VIRTIO_PCI_CAP_DEVICE_CFG => {},
                                _ => {}
                            }
                        }
                    }
                }
            }
            cap_ptr = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 1);
        }

        if common_cfg.is_null() || notify_cfg.is_null() { return None; }

        // Device init sequence
        (*common_cfg).device_status = 0;
        (*common_cfg).device_status |= VIRTIO_STATUS_ACKNOWLEDGE;
        (*common_cfg).device_status |= VIRTIO_STATUS_DRIVER;
        (*common_cfg).device_feature_select = 0;
        let _ = (*common_cfg).device_feature;
        (*common_cfg).driver_feature_select = 0;
        (*common_cfg).driver_feature = 0;
        (*common_cfg).device_status |= VIRTIO_STATUS_FEATURES_OK;

        let queue = VirtQueue::alloc(0, common_cfg)?;

        (*common_cfg).device_status |= VIRTIO_STATUS_DRIVER_OK;

        let req_phys  = mm::buddy::alloc(0)?;
        let data_phys = mm::buddy::alloc(0)?;
        core::ptr::write_bytes(mm::phys_to_virt(req_phys)  as *mut u8, 0, 4096);
        core::ptr::write_bytes(mm::phys_to_virt(data_phys) as *mut u8, 0, 4096);

        Some(Self { _pci: pci, common_cfg, notify_cfg, notify_off_multiplier, queue, req_phys, data_phys })
    }

    fn notify(&self) {
        unsafe {
            let notify_addr = (self.notify_cfg as usize
                + self.queue.notify_off as usize * self.notify_off_multiplier as usize) as *mut u16;
            core::ptr::write_volatile(notify_addr, 0);
        }
    }

    // Returns true on success (VIRTIO_BLK_S_OK == 0).
    fn do_io(&mut self, type_: u32, sector: u64, buf: *mut u8) -> bool {
        unsafe {
            // Fill request header
            let hdr = mm::phys_to_virt(self.req_phys) as *mut VirtioBlkReqHdr;
            (*hdr).type_ = type_;
            (*hdr).reserved = 0;
            (*hdr).sector = sector;
            // Status byte at offset 16 of the request page
            let status_phys = self.req_phys + 16;
            let status_ptr = mm::phys_to_virt(status_phys) as *mut u8;
            *status_ptr = 0xFF; // sentinel; device overwrites with 0=ok

            if type_ == VIRTIO_BLK_T_OUT {
                // Copy caller data into DMA page before write
                core::ptr::copy_nonoverlapping(buf, mm::phys_to_virt(self.data_phys) as *mut u8, BLOCK_SIZE);
            }

            let d0 = {
                let q = &mut self.queue;
                let d0 = q.alloc_desc(self.req_phys as u64, 16, 0);
                let d1 = q.alloc_desc(self.data_phys as u64, BLOCK_SIZE as u32,
                    if type_ == VIRTIO_BLK_T_IN { VIRTQ_DESC_F_WRITE } else { 0 });
                let d2 = q.alloc_desc(status_phys as u64, 1, VIRTQ_DESC_F_WRITE);
                q.chain(d0, d1);
                q.chain(d1, d2);
                q.submit(d0);
                d0
            };
            self.notify();
            self.queue.wait_for_completion();
            self.queue.free_chain(d0);

            if type_ == VIRTIO_BLK_T_IN {
                // Copy DMA page into caller buffer after read
                core::ptr::copy_nonoverlapping(mm::phys_to_virt(self.data_phys) as *const u8, buf, BLOCK_SIZE);
            }

            *status_ptr == 0
        }
    }
}

// ── Global device table ───────────────────────────────────────────────────────

static DEVICES: Mutex<[Option<VirtioBlkDevice>; MAX_BLK_DEVICES]> =
    Mutex::new([const { None }; MAX_BLK_DEVICES]);

static DEVICE_COUNT: Mutex<usize> = Mutex::new(0);

// ── Public API ────────────────────────────────────────────────────────────────

pub fn init() {
    let mut found = 0usize;
    let mut devs = DEVICES.lock();
    let mut cnt  = DEVICE_COUNT.lock();

    for device_id in [VIRTIO_PCI_DEVICE_BLK_MODERN, VIRTIO_PCI_DEVICE_BLK_LEGACY] {
        for pci in find_all_devices(VIRTIO_PCI_VENDOR, device_id) {
            if found >= MAX_BLK_DEVICES { break; }
            unsafe {
                match VirtioBlkDevice::new(pci) {
                    Some(d) => {
                        crate::pci::serial_debug("[virtio-blk] Initialized device #");
                        crate::pci::serial_debug_hex(found as u32);
                        crate::pci::serial_debug("\n");
                        devs[found] = Some(d);
                        found += 1;
                    }
                    None => {
                        crate::pci::serial_debug("[virtio-blk] Device probe failed\n");
                    }
                }
            }
        }
        if found >= MAX_BLK_DEVICES { break; }
    }

    *cnt = found;
    if found > 0 {
        crate::pci::serial_debug("[virtio-blk] Found ");
        crate::pci::serial_debug_hex(found as u32);
        crate::pci::serial_debug(" VirtIO block device(s)\n");
    } else {
        crate::pci::serial_debug("[virtio-blk] No VirtIO block devices found\n");
    }
}

pub fn device_count() -> usize {
    *DEVICE_COUNT.lock()
}

/// Read one 4096-byte block from device `dev_idx` at logical block `blk`.
pub fn read_block(dev_idx: usize, blk: u64, buf: &mut [u8; BLOCK_SIZE]) -> bool {
    let mut devs = DEVICES.lock();
    if let Some(ref mut dev) = devs[dev_idx] {
        dev.do_io(VIRTIO_BLK_T_IN, blk * SECTORS_PER_BLOCK, buf.as_mut_ptr())
    } else {
        false
    }
}

/// Write one 4096-byte block to device `dev_idx` at logical block `blk`.
pub fn write_block(dev_idx: usize, blk: u64, buf: &[u8; BLOCK_SIZE]) -> bool {
    let mut devs = DEVICES.lock();
    if let Some(ref mut dev) = devs[dev_idx] {
        dev.do_io(VIRTIO_BLK_T_OUT, blk * SECTORS_PER_BLOCK, buf.as_ptr() as *mut u8)
    } else {
        false
    }
}

/// Returns true if device `dev_idx` contains an F2FS volume (magic at byte 1024 of block 0).
pub fn has_f2fs(dev_idx: usize) -> bool {
    let mut buf = [0u8; BLOCK_SIZE];
    if !read_block(dev_idx, 0, &mut buf) { return false; }
    if F2FS_SB_OFFSET + 4 > BLOCK_SIZE { return false; }
    let magic = u32::from_le_bytes(buf[F2FS_SB_OFFSET..F2FS_SB_OFFSET + 4].try_into().unwrap());
    magic == F2FS_MAGIC
}
