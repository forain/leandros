//! VirtIO block device driver (PCI transport, polling mode).
//!
//! Enumerates all VirtIO block PCI devices and stores them in VIRTIO_BLK_DEVICES.
//! Public API: read_block / write_block (4096-byte granularity) and has_f2fs.

use spin::Mutex;
use crate::pci::{PciDevice, find_all_devices, pci_read_config_8, pci_read_config_16, pci_read_config_32, pci_write_config_16};
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
        core::ptr::addr_of_mut!((*cfg).queue_select).write_volatile(id);
        let reported = core::ptr::addr_of!((*cfg).queue_size).read_volatile();
        if reported == 0 { return None; }

        // The descriptor table, avail ring and used ring each live in a single
        // 4096-byte page.  A VirtqDesc is 16 bytes, so one page holds at most 256
        // descriptors.  If the device reports a larger max (or a bogus 0xFFFF,
        // which is what an unmapped BAR reads back), cap the queue and tell the
        // device via queue_size — otherwise the init loop below overruns the
        // page by up to ~1 MiB and corrupts adjacent allocations (page tables,
        // other queues), which manifests as spurious MMIO faults and QEMU
        // "Desc next is N" errors.
        const MAX_QUEUE: u16 = 256;
        let size = if reported > MAX_QUEUE { MAX_QUEUE } else { reported };
        if size != reported {
            core::ptr::addr_of_mut!((*cfg).queue_size).write_volatile(size);
        }
        let raw_notify_off = core::ptr::addr_of!((*cfg).queue_notify_off).read_volatile();
        // 0xFFFF is QEMU's signal that legacy I/O-port notification should be
        // used (not available on AArch64).  Treat it as 0: queue N's doorbell
        // is at the start of the NOTIFY_CFG region, which QEMU processes as a
        // valid queue notification for all transitional virtio-blk devices.
        let notify_off = if raw_notify_off == 0xFFFF { 0 } else { raw_notify_off };

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

        core::ptr::addr_of_mut!((*cfg).queue_desc).write_volatile(desc_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_driver).write_volatile(avail_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_device).write_volatile(used_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_enable).write_volatile(1u16);

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
        let idx_ptr = core::ptr::addr_of_mut!((*a).idx);
        let cur_idx = idx_ptr.read_volatile();
        let ring_slot = cur_idx as usize % self.size as usize;
        // Use manual offset to avoid out-of-bounds on the 32-element ring array
        let ring_ptr = (a as usize + 4) as *mut u16;
        ring_ptr.add(ring_slot).write_volatile(head);

        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        idx_ptr.write_volatile(cur_idx.wrapping_add(1));
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
        while self.last_used_idx == core::ptr::addr_of!((*self.used).idx).read_volatile() {
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
    /// Device capacity in 512-byte sectors, from VIRTIO_PCI_CAP_DEVICE_CFG.
    /// 0 if the device-config capability wasn't found (capacity unknown).
    capacity_sectors: u64,
}

unsafe impl Send for VirtioBlkDevice {}
unsafe impl Sync for VirtioBlkDevice {}

// ── Cap walk helpers ──────────────────────────────────────────────────────────

/// Enable PCI Memory Space (bit 1) and Bus Master (bit 2) for `pci`.
///
/// Without these the device's BARs do not decode (MMIO reads return all-ones)
/// and it performs no virtqueue DMA.  UEFI firmware normally sets them, but it
/// does not reliably cover every device (e.g. the third virtio-blk device on
/// the aarch64 `virt` machine), so the driver must enable them itself.
unsafe fn enable_pci_mmio(pci: &PciDevice) {
    let cmd = pci_read_config_16(pci.bus, pci.dev, pci.func, 0x04);
    pci_write_config_16(pci.bus, pci.dev, pci.func, 0x04, cmd | 0x0006);
}

/// Resolve a BAR index to its 64-bit MMIO base address, or 0 if I/O space.
fn bar64(pci: &PciDevice, bar_idx: usize) -> u64 {
    let raw = pci.bars[bar_idx];
    if raw & 1 != 0 { return 0; } // I/O space
    if (raw >> 1) & 3 == 2 && bar_idx + 1 < 6 {
        (raw & !0xF) as u64 | ((pci.bars[bar_idx + 1] as u64) << 32)
    } else {
        (raw & !0xF) as u64
    }
}

/// Walk the PCI cap list, map each VirtIO MMIO region, and return pointers to
/// COMMON_CFG and NOTIFY_CFG together with the notify_off_multiplier.
unsafe fn walk_caps(
    pci: &PciDevice,
) -> (Option<*mut VirtioPciCommonCfg>, Option<*mut u32>, u32, Option<*const u64>) {
    let mut common_cfg: Option<*mut VirtioPciCommonCfg> = None;
    let mut notify_cfg: Option<*mut u32> = None;
    let mut notify_off_multiplier = 0u32;
    let mut device_cfg: Option<*const u64> = None;

    let mut cap_ptr = pci_read_config_8(pci.bus, pci.dev, pci.func, 0x34);
    while cap_ptr != 0 {
        let cap_id = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr);
        if cap_id == 0x09 {
            let cfg_type = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 3);
            let bar_idx  = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 4) as usize;
            let offset   = pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 8);
            let length   = pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 12);

            if bar_idx < 6 {
                let base = bar64(pci, bar_idx);
                if base != 0 {
                    let phys = base as usize + offset as usize;
                    let virt = mm::paging::map_kernel_device(
                        phys, length as usize,
                        mm::paging::PageFlags::PRESENT
                            | mm::paging::PageFlags::WRITABLE
                            | mm::paging::PageFlags::MMIO,
                    ).unwrap_or_else(|| mm::phys_to_virt(phys));

                    match cfg_type {
                        VIRTIO_PCI_CAP_COMMON_CFG => {
                            common_cfg = Some(virt as *mut VirtioPciCommonCfg);
                        }
                        VIRTIO_PCI_CAP_NOTIFY_CFG => {
                            notify_off_multiplier =
                                pci_read_config_32(pci.bus, pci.dev, pci.func, cap_ptr + 16);
                            notify_cfg = Some(virt as *mut u32);
                        }
                        VIRTIO_PCI_CAP_DEVICE_CFG => {
                            // struct virtio_blk_config's first field is `capacity`
                            // (u64, in 512-byte sectors) at offset 0.
                            device_cfg = Some(virt as *const u64);
                        }
                        _ => {}
                    }
                }
            }
        }
        cap_ptr = pci_read_config_8(pci.bus, pci.dev, pci.func, cap_ptr + 1);
    }

    (common_cfg, notify_cfg, notify_off_multiplier, device_cfg)
}

/// Write device_status=0 to reset `pci` without touching its virtqueues.
///
/// Called in a first pass before any queue setup to avoid the QEMU
/// blk_drain() re-entrancy deadlock: after two devices have queue_enable=1,
/// writing device_status=0 to a third triggers blk_drain() from inside an
/// MMIO write handler, where aio_poll() cannot run — causing a deadlock.
/// Resetting all devices first means blk_drain() sees no pending I/O.
unsafe fn pre_reset_device(pci: &PciDevice) {
    enable_pci_mmio(pci);
    let (common_cfg, _, _, _) = walk_caps(pci);
    if let Some(cfg) = common_cfg {
        core::ptr::addr_of_mut!((*cfg).device_status).write_volatile(0u8);
    }
}

impl VirtioBlkDevice {
    /// Initialize a VirtIO block device.
    ///
    /// `reset_done`: if true, device_status=0 was already written in a prior
    /// pass and this function starts from ACKNOWLEDGE.
    unsafe fn new(pci: PciDevice, reset_done: bool) -> Option<Self> {
        enable_pci_mmio(&pci);
        let (common_cfg, notify_cfg, notify_off_multiplier, device_cfg) = walk_caps(&pci);
        let common_cfg = common_cfg?;
        let notify_cfg = notify_cfg?;
        let capacity_sectors = device_cfg.map_or(0, |p| p.read_volatile());

        let ds = core::ptr::addr_of_mut!((*common_cfg).device_status);

        if !reset_done {
            ds.write_volatile(0u8);
        }
        ds.write_volatile(VIRTIO_STATUS_ACKNOWLEDGE);
        ds.write_volatile(VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER);

        // Feature bits 0-31: no optional features needed.
        core::ptr::addr_of_mut!((*common_cfg).device_feature_select).write_volatile(0u32);
        let _ = core::ptr::addr_of!((*common_cfg).device_feature).read_volatile();
        core::ptr::addr_of_mut!((*common_cfg).driver_feature_select).write_volatile(0u32);
        core::ptr::addr_of_mut!((*common_cfg).driver_feature).write_volatile(0u32);

        // Feature bit 32 = VIRTIO_F_VERSION_1: mandatory for the modern PCI
        // transport.  Without it, transitional devices (0x1001) stay in legacy
        // mode and return queue_notify_off=0xFFFF to signal "use I/O-port
        // notification" — which doesn't exist on AArch64 — producing a wildly
        // wrong doorbell address.
        core::ptr::addr_of_mut!((*common_cfg).device_feature_select).write_volatile(1u32);
        let dev_feat_hi = core::ptr::addr_of!((*common_cfg).device_feature).read_volatile();
        const VIRTIO_F_VERSION_1: u32 = 1 << 0; // bit 32 of the full feature set
        if dev_feat_hi & VIRTIO_F_VERSION_1 != 0 {
            core::ptr::addr_of_mut!((*common_cfg).driver_feature_select).write_volatile(1u32);
            core::ptr::addr_of_mut!((*common_cfg).driver_feature).write_volatile(VIRTIO_F_VERSION_1);
        }

        ds.write_volatile(
            VIRTIO_STATUS_ACKNOWLEDGE | VIRTIO_STATUS_DRIVER | VIRTIO_STATUS_FEATURES_OK,
        );
        // Verify FEATURES_OK was accepted; if not, features negotiation failed.
        if ds.read_volatile() & VIRTIO_STATUS_FEATURES_OK == 0 {
            return None;
        }

        let queue = VirtQueue::alloc(0, common_cfg)?;

        ds.write_volatile(
            VIRTIO_STATUS_ACKNOWLEDGE
                | VIRTIO_STATUS_DRIVER
                | VIRTIO_STATUS_FEATURES_OK
                | VIRTIO_STATUS_DRIVER_OK,
        );

        let req_phys  = mm::buddy::alloc(0)?;
        let data_phys = mm::buddy::alloc(0)?;
        core::ptr::write_bytes(mm::phys_to_virt(req_phys)  as *mut u8, 0, 4096);
        core::ptr::write_bytes(mm::phys_to_virt(data_phys) as *mut u8, 0, 4096);

        Some(Self {
            _pci: pci,
            common_cfg,
            notify_cfg,
            notify_off_multiplier,
            queue,
            req_phys,
            data_phys,
            capacity_sectors,
        })
    }

    fn notify(&self) {
        unsafe {
            let notify_addr = (self.notify_cfg as usize
                + self.queue.notify_off as usize * self.notify_off_multiplier as usize)
                as *mut u16;
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
                core::ptr::copy_nonoverlapping(
                    buf,
                    mm::phys_to_virt(self.data_phys) as *mut u8,
                    BLOCK_SIZE,
                );
            }

            let d0 = {
                let q = &mut self.queue;
                let d0 = q.alloc_desc(self.req_phys as u64, 16, 0);
                let d1 = q.alloc_desc(
                    self.data_phys as u64,
                    BLOCK_SIZE as u32,
                    if type_ == VIRTIO_BLK_T_IN { VIRTQ_DESC_F_WRITE } else { 0 },
                );
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
                core::ptr::copy_nonoverlapping(
                    mm::phys_to_virt(self.data_phys) as *const u8,
                    buf,
                    BLOCK_SIZE,
                );
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
    // Pass 1: reset all devices before any queue setup.  This ensures that
    // when QEMU's blk_drain() runs for each reset it finds no pending I/O and
    // returns immediately, avoiding the aio_poll() re-entrancy deadlock that
    // occurs when device_status=0 is written after other queues are enabled.
    for device_id in [VIRTIO_PCI_DEVICE_BLK_MODERN, VIRTIO_PCI_DEVICE_BLK_LEGACY] {
        for pci in find_all_devices(VIRTIO_PCI_VENDOR, device_id) {
            unsafe { pre_reset_device(&pci); }
        }
    }

    // Pass 2: full initialization (reset already done).
    let mut found = 0usize;
    let mut devs = DEVICES.lock();
    let mut cnt  = DEVICE_COUNT.lock();

    for device_id in [VIRTIO_PCI_DEVICE_BLK_MODERN, VIRTIO_PCI_DEVICE_BLK_LEGACY] {
        for pci in find_all_devices(VIRTIO_PCI_VENDOR, device_id) {
            if found >= MAX_BLK_DEVICES { break; }
            unsafe {
                if let Some(d) = VirtioBlkDevice::new(pci, true) {
                    devs[found] = Some(d);
                    found += 1;
                }
            }
        }
        if found >= MAX_BLK_DEVICES { break; }
    }

    *cnt = found;
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
    // Heap-allocate to avoid a 4 KiB stack frame that can overflow when this
    // function is inlined into a large kernel-init caller on AArch64.
    let mut buf = alloc::vec![0u8; BLOCK_SIZE];
    let arr: &mut [u8; BLOCK_SIZE] = buf.as_mut_slice().try_into().unwrap();
    if !read_block(dev_idx, 0, arr) { return false; }
    if F2FS_SB_OFFSET + 4 > BLOCK_SIZE { return false; }
    let magic = u32::from_le_bytes(buf[F2FS_SB_OFFSET..F2FS_SB_OFFSET + 4].try_into().unwrap());
    magic == F2FS_MAGIC
}

/// Metadata for `lsblk`. `total_blocks` comes from the VirtIO device-config
/// capacity capability read at probe time.
pub fn info(dev_idx: usize) -> Option<crate::BlkDevInfo> {
    let devs = DEVICES.lock();
    let dev = devs.get(dev_idx)?.as_ref()?;
    let total_blocks = dev.capacity_sectors / SECTORS_PER_BLOCK;
    drop(devs);
    Some(crate::BlkDevInfo {
        total_blocks,
        block_size: BLOCK_SIZE as u32,
        fstype: if has_f2fs(dev_idx) { Some("f2fs") } else { None },
    })
}
