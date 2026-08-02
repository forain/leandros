use spin::Mutex;
use alloc::vec::Vec;
use crate::pci::{PciDevice, find_device, pci_read_config_8, pci_read_config_16, pci_read_config_32, pci_write_config_16};
use mm;

const VIRTIO_PCI_VENDOR: u16 = 0x1af4;
const VIRTIO_PCI_DEVICE_GPU: u16 = 0x1050;

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

#[repr(C, packed)]
struct VirtqDesc {
    addr: u64,
    len: u32,
    flags: u16,
    next: u16,
}

const VIRTQ_DESC_F_NEXT: u16 = 1;
const VIRTQ_DESC_F_WRITE: u16 = 2;

// The avail/used rings are variable length (one entry per queue slot), sized
// at runtime from the negotiated queue_size.  The `ring` field is a flexible
// array: it carries no entries in the struct itself, and the ring is always
// accessed through an explicit byte offset past the 4-byte header so the entry
// count is bounded by the queue size rather than a hardcoded array length.
#[repr(C, packed)]
struct VirtqAvail {
    flags: u16,
    idx: u16,
    ring: [u16; 0],
}

#[repr(C, packed)]
struct VirtqUsed {
    flags: u16,
    idx: u16,
    ring: [VirtqUsedElem; 0],
}

#[repr(C, packed)]
struct VirtqUsedElem {
    id: u32,
    len: u32,
}

pub struct VirtioGpuDevice {
    _pci_dev: PciDevice,
    common_cfg: *mut VirtioPciCommonCfg,
    notify_cfg: *mut u32,
    notify_off_multiplier: u32,
    _device_cfg: *mut u8,
    _features: u32,
    current_resource_id: u32,
    scanout_w: u32,
    scanout_h: u32,

    queues: [Option<VirtioQueue>; 2],

    /// Backing store for the 64x64 cursor image (resource `CURSOR_RESOURCE_ID`).
    /// Physically contiguous — `attach_backing` emits a single mem entry.
    cursor_phys: u64,
    cursor_virt: usize,
    /// Resource created + backed + a first image uploaded.
    cursor_ready: bool,
    /// Last position pushed to the host, to suppress redundant MOVE_CURSORs.
    cursor_pos: (u32, u32),
    /// `false` once the cursor has been hidden with `resource_id = 0`.
    cursor_visible: bool,
}

unsafe impl Send for VirtioGpuDevice {}
unsafe impl Sync for VirtioGpuDevice {}

struct VirtioQueue {
    _id: u16,
    size: u16,
    notify_off: u16,
    last_used_idx: u16,
    free_head: u16,
    num_free: u16,
    
    desc: *mut VirtqDesc,
    avail: *mut VirtqAvail,
    used: *mut VirtqUsed,
}

unsafe impl Send for VirtioQueue {}
unsafe impl Sync for VirtioQueue {}

impl VirtioQueue {
    unsafe fn add_desc(&mut self, addr: u64, len: u32, flags: u16) -> u16 {
        if self.num_free == 0 {
            panic!("[GPU] VirtIO Queue descriptor overflow!");
        }
        let id = self.free_head;
        if id == 0xFFFF {
            panic!("[GPU] VirtIO Queue free list corruption!");
        }
        let d = self.desc.add(id as usize);
        self.free_head = (*d).next;
        self.num_free -= 1;
        
        (*d).addr = addr;
        (*d).len = len;
        (*d).flags = flags;
        (*d).next = 0;
        id
    }

    unsafe fn free_chain(&mut self, mut head: u16) {
        while head != 0xFFFF {
            let d = self.desc.add(head as usize);
            let flags = (*d).flags;
            let next = (*d).next;
            
            // Push back to free list
            (*d).next = self.free_head;
            self.free_head = head;
            self.num_free += 1;
            
            if (flags & VIRTQ_DESC_F_NEXT) != 0 {
                head = next;
            } else {
                break;
            }
        }
    }

    unsafe fn submit(&mut self, head: u16) {
        let a = self.avail;
        let ring_idx = (*a).idx as usize % self.size as usize;
        // The avail ring is a flexible array starting 4 bytes in (past flags +
        // idx); index it explicitly, bounded by the negotiated queue size.
        let ring_ptr = (a as usize + 4) as *mut u16;
        ring_ptr.add(ring_idx).write_volatile(head);
        
        core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
        (*a).idx = (*a).idx.wrapping_add(1);
    }
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct VirtioGpuCtrlHdr {
    pub type_: u32,
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub padding: u32,
}

#[derive(Copy, Clone)]
pub enum VirtioGpuCmd {
    GetDisplayInfo = 0x0100,
    ResourceCreate2d = 0x0101,
    ResourceUnref = 0x0102,
    SetScanout = 0x0103,
    ResourceFlush = 0x0104,
    TransferToHost2d = 0x0105,
    ResourceAttachBacking = 0x0106,
    ResourceDetachBacking = 0x0107,
    ResourceCreate3d = 0x0108,
    Submit3d = 0x0109,
    GetCapset = 0x0110,
    TransferToHost3d = 0x0111,
    TransferFromHost3d = 0x0112,
    // Cursor-queue commands (queue 1).  These take no response descriptor.
    UpdateCursor = 0x0300,
    MoveCursor = 0x0301,
}

/// Position payload shared by UPDATE_CURSOR and MOVE_CURSOR (16 bytes).
#[repr(C, packed)]
#[derive(Copy, Clone)]
struct VirtioGpuCursorPos {
    scanout_id: u32,
    x: u32,
    y: u32,
    padding: u32,
}

/// `struct virtio_gpu_update_cursor` — 24 + 16 + 16 = 56 bytes.
#[repr(C, packed)]
#[derive(Copy, Clone)]
struct VirtioGpuUpdateCursor {
    hdr: VirtioGpuCtrlHdr,
    pos: VirtioGpuCursorPos,
    resource_id: u32,
    hot_x: u32,
    hot_y: u32,
    padding: u32,
}

/// The host requires cursor images to be exactly this size; QEMU silently drops
/// uploads of any other geometry (hw/display/virtio-gpu.c).
pub const CURSOR_W: u32 = 64;
pub const CURSOR_H: u32 = 64;
/// Resource 1 is the scanout framebuffer, so the cursor image lives in 2.
pub const CURSOR_RESOURCE_ID: u32 = 2;

/// Set to `true` to drive a kernel-owned cursor straight from pointer state, as
/// a standalone check of the cursor queue and the host overlay path.  Ships
/// `false`: the compositor owns the cursor via the atomic cursor plane.
pub const CURSOR_DEBUG: bool = false;

/// Cursor-path tracing.  Goes straight to the UART (not gated on RENDER_DEBUG,
/// which is compiled out) so the Stage-0 gate is observable in the serial log.
#[inline(always)]
fn cdebug(msg: &str) {
    if CURSOR_DEBUG {
        crate::pci::serial_debug(msg);
    }
}

const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
const VIRTIO_PCI_CAP_ISR_CFG:    u8 = 3;
const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

const VIRTIO_STATUS_ACKNOWLEDGE: u8 = 1;
const VIRTIO_STATUS_DRIVER:      u8 = 2;
const VIRTIO_STATUS_DRIVER_OK:   u8 = 4;
const VIRTIO_STATUS_FEATURES_OK: u8 = 8;

#[repr(C, packed)]
struct VirtioGpuResourceCreate2d {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    format: u32,
    width: u32,
    height: u32,
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
pub struct VirtioGpuRect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

#[repr(C, packed)]
struct VirtioGpuSetScanout {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C, packed)]
struct VirtioGpuTransferToHost2d {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    offset: u64,
    resource_id: u32,
    padding: u32,
}

#[repr(C, packed)]
struct VirtioGpuResourceFlush {
    hdr: VirtioGpuCtrlHdr,
    r: VirtioGpuRect,
    resource_id: u32,
    padding: u32,
}

#[repr(C, packed)]
struct VirtioGpuResourceCreate3d {
    hdr: VirtioGpuCtrlHdr,
    resource_id: u32,
    target: u32,
    format: u32,
    bind: u32,
    width: u32,
    height: u32,
    depth: u32,
    array_size: u32,
    last_level: u32,
    nr_samples: u32,
    flags: u32,
    padding: u32,
}

#[repr(C, packed)]
#[derive(Copy, Clone)]
struct VirtioGpuBox {
    x: u32,
    y: u32,
    z: u32,
    w: u32,
    h: u32,
    d: u32,
}

#[repr(C, packed)]
struct VirtioGpuTransferToHost3d {
    hdr: VirtioGpuCtrlHdr,
    box_: VirtioGpuBox,
    offset: u64,
    resource_id: u32,
    level: u32,
    stride: u32,
    layer_stride: u32,
}

impl VirtioGpuDevice {
    pub fn new() -> Option<Self> {
        let dev = find_device(VIRTIO_PCI_VENDOR, VIRTIO_PCI_DEVICE_GPU)?;
        crate::pci::rdebug("[GPU] Found VirtIO GPU device\n");

        // Enable PCI Memory Space (bit 1) and Bus Master (bit 2) in the command
        // register.  Without these the device decodes no MMIO BAR accesses and
        // performs no virtqueue DMA, so every command times out.  UEFI firmware
        // normally sets them, but on direct (-kernel) boot there is no firmware,
        // so the driver must enable them itself.
        unsafe {
            let cmd = pci_read_config_16(dev.bus, dev.dev, dev.func, 0x04);
            pci_write_config_16(dev.bus, dev.dev, dev.func, 0x04, cmd | 0x0006);
        }

        let mut common_cfg = core::ptr::null_mut();
        let mut notify_cfg = core::ptr::null_mut();
        let mut notify_off_multiplier = 0;
        let mut device_cfg = core::ptr::null_mut();
        let mut _isr_cfg = core::ptr::null_mut();

        unsafe {
            let mut cap_ptr = pci_read_config_8(dev.bus, dev.dev, dev.func, 0x34);
            while cap_ptr != 0 {
                let cap_id = pci_read_config_8(dev.bus, dev.dev, dev.func, cap_ptr);
                if cap_id == 0x09 { // VIRTIO_PCI_CAP_VENDOR_CFG
                    let cfg_type = pci_read_config_8(dev.bus, dev.dev, dev.func, cap_ptr + 3);
                    let bar_idx = pci_read_config_8(dev.bus, dev.dev, dev.func, cap_ptr + 4);
                    let offset = pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 8);
                    let length = pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 12);
                    
                    if (bar_idx as usize) < 6 {
                        let raw_bar = dev.bars[bar_idx as usize];
                        if raw_bar & 1 == 0 {
                            let bar64: u64 = if (raw_bar >> 1) & 3 == 2 && (bar_idx as usize) + 1 < 6 {
                                let lo = (raw_bar & !0xF) as u64;
                                let hi = dev.bars[bar_idx as usize + 1] as u64;
                                lo | (hi << 32)
                            } else {
                                (raw_bar & !0xF) as u64
                            };

                            if bar64 != 0 {
                                crate::pci::rdebug("[GPU] Mapping BAR ");
                                crate::pci::rdebug_hex(bar_idx as u32);
                                crate::pci::rdebug(" at ");
                                crate::pci::rdebug_hex((bar64 >> 32) as u32);
                                crate::pci::rdebug_hex(bar64 as u32);
                                crate::pci::rdebug("\n");

                                let virt = mm::paging::map_kernel_device(
                                    bar64 as usize + offset as usize,
                                    length as usize,
                                    mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO,
                                ).unwrap_or_else(|| mm::phys_to_virt(bar64 as usize + offset as usize));
                                    
                                match cfg_type {
                                    VIRTIO_PCI_CAP_COMMON_CFG => {
                                        crate::pci::rdebug("[GPU] Found COMMON_CFG\n");
                                        common_cfg = virt as *mut VirtioPciCommonCfg;
                                    },
                                    VIRTIO_PCI_CAP_NOTIFY_CFG => {
                                        crate::pci::rdebug("[GPU] Found NOTIFY_CFG\n");
                                        notify_off_multiplier = pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 16);
                                        notify_cfg = virt as *mut u32;
                                    },
                                    VIRTIO_PCI_CAP_ISR_CFG => _isr_cfg = virt as *mut u8,
                                    VIRTIO_PCI_CAP_DEVICE_CFG => {
                                        crate::pci::rdebug("[GPU] Found DEVICE_CFG\n");
                                        device_cfg = virt as *mut u8;
                                    },
                                    _ => {}
                                }
                            }
                        }
                    }
                }
                cap_ptr = pci_read_config_8(dev.bus, dev.dev, dev.func, cap_ptr + 1);
            }
        }

        if common_cfg.is_null() || notify_cfg.is_null() || device_cfg.is_null() {
            crate::pci::rdebug("[GPU] Missing required VirtIO capabilities\n");
            return None;
        }

        let mut gpu = Self {
            _pci_dev: dev,
            common_cfg,
            notify_cfg,
            notify_off_multiplier,
            _device_cfg: device_cfg,
            _features: 0,
            current_resource_id: 0,
            scanout_w: 1280,
            scanout_h: 800,
            queues: [None, None],
            cursor_phys: 0,
            cursor_virt: 0,
            cursor_ready: false,
            cursor_pos: (0, 0),
            cursor_visible: false,
        };

        gpu.init_device();
        
        Some(gpu)
    }

    fn init_device(&mut self) {
        unsafe {
            let cfg = self.common_cfg;
            let status = core::ptr::addr_of_mut!((*cfg).device_status);
            // 1. Reset device
            status.write_volatile(0);
            // 2. Set ACKNOWLEDGE status bit
            status.write_volatile(status.read_volatile() | VIRTIO_STATUS_ACKNOWLEDGE);
            // 3. Set DRIVER status bit
            status.write_volatile(status.read_volatile() | VIRTIO_STATUS_DRIVER);

            // 4. Negotiate features
            core::ptr::addr_of_mut!((*cfg).device_feature_select).write_volatile(0);
            let _f0 = core::ptr::addr_of!((*cfg).device_feature).read_volatile();
            core::ptr::addr_of_mut!((*cfg).driver_feature_select).write_volatile(0);
            core::ptr::addr_of_mut!((*cfg).driver_feature).write_volatile(0); // Minimal features for now

            // 5. Set FEATURES_OK status bit
            status.write_volatile(status.read_volatile() | VIRTIO_STATUS_FEATURES_OK);

            // 6. Setup queues: 0 = controlq, 1 = cursorq.  virtio-gpu always
            //    exposes both, but stay defensive — the cursor path checks for
            //    `None` and degrades to the software cursor.
            self.queues[0] = self.setup_queue(0);
            self.queues[1] = self.setup_queue(1);
            if self.queues[1].is_none() {
                cdebug("[GPU] no cursor queue; hardware cursor disabled\n");
            }

            // 7. Set DRIVER_OK status bit
            status.write_volatile(status.read_volatile() | VIRTIO_STATUS_DRIVER_OK);
        }
        crate::pci::rdebug("[GPU] VirtIO GPU initialized\n");
    }

    unsafe fn setup_queue(&mut self, id: u16) -> Option<VirtioQueue> {
        let cfg = self.common_cfg;
        core::ptr::addr_of_mut!((*cfg).queue_select).write_volatile(id);
        let max_size = core::ptr::addr_of!((*cfg).queue_size).read_volatile();
        if max_size == 0 { return None; } // 0 ⇒ queue unavailable

        // Each ring lives in a single 4 KiB page allocated below.  The binding
        // constraint is the descriptor table at 16 bytes/entry → 256 entries
        // per page (the avail ring fits ≤2045, the used ring ≤511).  A device
        // is free to advertise a larger queue, so cap to what fits and round
        // down to a power of two (queue_size must be a power of two), then
        // negotiate the reduced size back to the device.  This driver issues
        // commands synchronously — one descriptor chain outstanding at a time —
        // so even a small queue is ample.
        const MAX_FIT: u16 = (4096 / core::mem::size_of::<VirtqDesc>()) as u16; // 256
        let capped = max_size.min(MAX_FIT);
        // floor to a power of two; `capped` is in [1, 256] here.
        let size = 1u16 << (15 - capped.leading_zeros() as u16);
        if size < 2 { return None; } // need ≥2 descriptors per command chain
        if size != max_size {
            // The driver may reduce queue_size before enabling the queue.
            core::ptr::addr_of_mut!((*cfg).queue_size).write_volatile(size);
        }

        let notify_off = core::ptr::addr_of!((*cfg).queue_notify_off).read_volatile();

        // Allocate descriptors, avail ring, and used ring
        let desc_phys = mm::buddy::alloc(0)?;
        let avail_phys = mm::buddy::alloc(0)?;
        let used_phys = mm::buddy::alloc(0)?;

        let desc = mm::phys_to_virt(desc_phys) as *mut VirtqDesc;
        let avail = mm::phys_to_virt(avail_phys) as *mut VirtqAvail;
        let used = mm::phys_to_virt(used_phys) as *mut VirtqUsed;

        core::ptr::write_bytes(desc as *mut u8, 0, 4096);
        core::ptr::write_bytes(avail as *mut u8, 0, 4096);
        core::ptr::write_bytes(used as *mut u8, 0, 4096);

        // Link all descriptors into a free list chain
        for i in 0..(size - 1) {
            (*desc.add(i as usize)).next = i + 1;
        }
        (*desc.add((size - 1) as usize)).next = 0xFFFF; // Mark end of chain

        core::ptr::addr_of_mut!((*cfg).queue_desc).write_volatile(desc_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_driver).write_volatile(avail_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_device).write_volatile(used_phys as u64);
        core::ptr::addr_of_mut!((*cfg).queue_enable).write_volatile(1u16);

        Some(VirtioQueue {
            _id: id,
            size,
            notify_off,
            last_used_idx: 0,
            free_head: 0,
            num_free: size,
            desc,
            avail,
            used,
        })
    }

    fn send_command_raw(&mut self, cmd_data: &[u8]) -> Result<(), ()> {
        let q = self.queues[0].as_mut().ok_or(())?;
        if q.num_free < 2 { return Err(()); }

        let hdr_type = u32::from_le_bytes(cmd_data[0..4].try_into().unwrap_or([0; 4]));

        let req_phys = mm::buddy::alloc(0).ok_or(())?;
        let req_virt = mm::phys_to_virt(req_phys) as *mut u8;
        unsafe { core::ptr::copy_nonoverlapping(cmd_data.as_ptr(), req_virt, cmd_data.len()); }

        let resp_phys = mm::buddy::alloc(0).ok_or(())?;
        let resp_virt = mm::phys_to_virt(resp_phys) as *mut VirtioGpuCtrlHdr;
        unsafe { core::ptr::write_bytes(resp_virt as *mut u8, 0, 4096); }

        unsafe {
            let head = q.add_desc(req_phys as u64, cmd_data.len() as u32, VIRTQ_DESC_F_NEXT);
            let resp_idx = q.add_desc(resp_phys as u64, 4096, VIRTQ_DESC_F_WRITE);
            (*q.desc.add(head as usize)).next = resp_idx;

            q.submit(head);

            let notify_addr = (self.notify_cfg as usize + q.notify_off as usize * self.notify_off_multiplier as usize) as *mut u16;
            notify_addr.write_volatile(0);

            let mut timeout = 10_000_000;
            while q.last_used_idx == (*q.used).idx && timeout > 0 {
                core::hint::spin_loop();
                timeout -= 1;
            }

            if timeout == 0 {
                crate::pci::rdebug("[GPU] Command ");
                crate::pci::rdebug_hex(hdr_type);
                crate::pci::rdebug(" TIMEOUT!\n");
                return Err(());
            }

            q.last_used_idx = q.last_used_idx.wrapping_add(1);

            let resp_hdr = *resp_virt;

            // Cleanup
            q.free_chain(head);
            mm::buddy::free(req_phys, 0);
            mm::buddy::free(resp_phys, 0);

            if resp_hdr.type_ != 0x1100 && resp_hdr.type_ != 0x1101 {
                crate::pci::rdebug("[GPU] Command failed with resp ");
                crate::pci::rdebug_hex(resp_hdr.type_);
                crate::pci::rdebug("\n");
                return Err(());
            }
        }

        Ok(())
    }
    // ---------------------------------------------------------------------
    // Cursor queue (queue 1)
    //
    // The cursor queue carries only UPDATE_CURSOR and MOVE_CURSOR.  Unlike the
    // control queue these take a single read-only descriptor and produce no
    // response: the host consumes the command and completes the chain.  The
    // request page is therefore reclaimed lazily, from the used ring, on the
    // next call — never in the submit path, so a command the host has not yet
    // consumed can never have its buffer freed underneath it.
    // ---------------------------------------------------------------------

    /// Reclaim descriptors and request pages the host has finished with.
    fn cursor_reap(&mut self) {
        let q = match self.queues[1].as_mut() {
            Some(q) => q,
            None => return,
        };
        unsafe {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            while q.last_used_idx != (*q.used).idx {
                // The used ring is a flexible array 4 bytes past the header.
                let ring = (q.used as usize + 4) as *const VirtqUsedElem;
                let slot = ring.add(q.last_used_idx as usize % q.size as usize);
                let head = (slot as *const u32).read_volatile() as u16;
                if head as usize >= q.size as usize {
                    // Corrupt used entry — resync rather than free a bad page.
                    q.last_used_idx = (*q.used).idx;
                    break;
                }
                let phys = (*q.desc.add(head as usize)).addr;
                q.free_chain(head);
                q.last_used_idx = q.last_used_idx.wrapping_add(1);
                if phys != 0 {
                    mm::buddy::free(phys as usize, 0);
                }
            }
        }
    }

    /// Submit one cursor command.  Returns `false` if the queue is absent or
    /// has no free descriptor.  Does not wait for completion.
    fn send_cursor_command(&mut self, cmd: &VirtioGpuUpdateCursor) -> bool {
        self.cursor_reap();

        let notify_cfg = self.notify_cfg;
        let mult = self.notify_off_multiplier;
        let q = match self.queues[1].as_mut() {
            Some(q) => q,
            None => return false,
        };
        if q.num_free < 1 {
            return false;
        }

        let req_phys = match mm::buddy::alloc(0) {
            Some(p) => p,
            None => return false,
        };
        let req_virt = mm::phys_to_virt(req_phys) as *mut u8;
        let len = core::mem::size_of::<VirtioGpuUpdateCursor>();
        unsafe {
            core::ptr::copy_nonoverlapping(cmd as *const _ as *const u8, req_virt, len);

            // Single read-only descriptor: no NEXT, no WRITE.
            let head = q.add_desc(req_phys as u64, len as u32, 0);
            q.submit(head);

            let notify_addr =
                (notify_cfg as usize + q.notify_off as usize * mult as usize) as *mut u16;
            notify_addr.write_volatile(0);
        }
        true
    }

    /// Create and back the 64x64 cursor resource.  Idempotent.
    pub fn cursor_init(&mut self) -> bool {
        if self.cursor_ready {
            return true;
        }
        if self.queues[1].is_none() {
            return false;
        }
        if self.cursor_phys == 0 {
            // 64*64*4 = 16 KiB = order 2, physically contiguous as
            // `attach_backing` emits exactly one mem entry.
            let phys = match mm::buddy::alloc(2) {
                Some(p) => p,
                None => return false,
            };
            self.cursor_phys = phys as u64;
            self.cursor_virt = mm::phys_to_virt(phys);
            unsafe {
                core::ptr::write_bytes(
                    self.cursor_virt as *mut u8,
                    0,
                    (CURSOR_W * CURSOR_H * 4) as usize,
                );
            }
        }
        if !self.create_resource_2d(CURSOR_RESOURCE_ID, CURSOR_W, CURSOR_H) {
            cdebug("[GPU] cursor create_resource_2d failed\n");
            return false;
        }
        if !self.attach_backing(
            CURSOR_RESOURCE_ID,
            self.cursor_phys,
            CURSOR_W * CURSOR_H * 4,
        ) {
            cdebug("[GPU] cursor attach_backing failed\n");
            return false;
        }
        self.cursor_ready = true;
        cdebug("[GPU] cursor queue + resource ready\n");
        true
    }

    /// Copy a 64x64 BGRA image into the cursor resource and hand it to the host
    /// at `(x, y)` with hotspot `(hot_x, hot_y)`.  `pixels` shorter than
    /// 64*64*4 bytes is zero-padded; longer is truncated.
    pub fn cursor_update(
        &mut self,
        pixels: &[u8],
        hot_x: u32,
        hot_y: u32,
        x: u32,
        y: u32,
    ) -> bool {
        if !self.cursor_init() {
            return false;
        }
        let bytes = (CURSOR_W * CURSOR_H * 4) as usize;
        unsafe {
            let dst = self.cursor_virt as *mut u8;
            let n = pixels.len().min(bytes);
            core::ptr::copy_nonoverlapping(pixels.as_ptr(), dst, n);
            if n < bytes {
                core::ptr::write_bytes(dst.add(n), 0, bytes - n);
            }
        }
        self.cursor_present(hot_x, hot_y, x, y)
    }

    /// Publish whatever is already in the cursor backing: transfer it to the
    /// host and issue UPDATE_CURSOR.  Callers that wrote the backing directly
    /// use this instead of `cursor_update` to avoid a redundant copy.
    pub fn cursor_present(&mut self, hot_x: u32, hot_y: u32, x: u32, y: u32) -> bool {
        if !self.cursor_ready {
            return false;
        }
        // The image upload is a control-queue command; only UPDATE/MOVE_CURSOR
        // ride the cursor queue.
        let transfer = VirtioGpuTransferToHost2d {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::TransferToHost2d as u32,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            r: VirtioGpuRect { x: 0, y: 0, width: CURSOR_W, height: CURSOR_H },
            offset: 0,
            resource_id: CURSOR_RESOURCE_ID,
            padding: 0,
        };
        let data = unsafe {
            core::slice::from_raw_parts(
                &transfer as *const _ as *const u8,
                core::mem::size_of::<VirtioGpuTransferToHost2d>(),
            )
        };
        if self.send_command_raw(data).is_err() {
            return false;
        }

        let cmd = VirtioGpuUpdateCursor {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::UpdateCursor as u32,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            pos: VirtioGpuCursorPos { scanout_id: 0, x, y, padding: 0 },
            resource_id: CURSOR_RESOURCE_ID,
            hot_x,
            hot_y,
            padding: 0,
        };
        self.cursor_pos = (x, y);
        self.cursor_visible = true;
        self.send_cursor_command(&cmd)
    }

    /// Reposition the cursor.  No pixel traffic at all.
    pub fn cursor_move(&mut self, x: u32, y: u32) -> bool {
        if !self.cursor_ready || !self.cursor_visible {
            return false;
        }
        if self.cursor_pos == (x, y) {
            return true;
        }
        self.cursor_pos = (x, y);
        let cmd = VirtioGpuUpdateCursor {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::MoveCursor as u32,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            pos: VirtioGpuCursorPos { scanout_id: 0, x, y, padding: 0 },
            // Must stay nonzero: resource_id 0 means "hide" to the host.
            resource_id: CURSOR_RESOURCE_ID,
            hot_x: 0,
            hot_y: 0,
            padding: 0,
        };
        self.send_cursor_command(&cmd)
    }

    /// Hide the hardware cursor (`resource_id = 0`).
    pub fn cursor_hide(&mut self) -> bool {
        if !self.cursor_ready || !self.cursor_visible {
            return false;
        }
        self.cursor_visible = false;
        let cmd = VirtioGpuUpdateCursor {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::UpdateCursor as u32,
                flags: 0,
                fence_id: 0,
                ctx_id: 0,
                padding: 0,
            },
            pos: VirtioGpuCursorPos { scanout_id: 0, x: 0, y: 0, padding: 0 },
            resource_id: 0,
            hot_x: 0,
            hot_y: 0,
            padding: 0,
        };
        self.send_cursor_command(&cmd)
    }

    /// Has the host consumed everything we submitted on the cursor queue?
    /// Used only by the Stage 0 gate check.
    pub fn cursor_queue_drained(&mut self) -> bool {
        self.cursor_reap();
        match self.queues[1].as_ref() {
            Some(q) => q.num_free == q.size,
            None => false,
        }
    }

    pub fn create_resource_2d(&mut self, resource_id: u32, width: u32, height: u32) -> bool {
        let cmd = VirtioGpuResourceCreate2d {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::ResourceCreate2d as u32,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            resource_id,
            format: 1, // VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM
            width,
            height,
        };
        let data = unsafe { core::slice::from_raw_parts(&cmd as *const _ as *const u8, core::mem::size_of::<VirtioGpuResourceCreate2d>()) };
        self.send_command_raw(data).is_ok()
    }

    pub fn create_resource_3d(&mut self, resource_id: u32, width: u32, height: u32, format: u32) -> bool {
        let cmd = VirtioGpuResourceCreate3d {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::ResourceCreate3d as u32,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            resource_id,
            target: 2, // PIPE_TEXTURE_2D
            format,
            bind: 1,   // PIPE_BIND_RENDER_TARGET
            width, height, depth: 1, array_size: 1,
            last_level: 0, nr_samples: 0, flags: 0, padding: 0,
        };
        let data = unsafe { core::slice::from_raw_parts(&cmd as *const _ as *const u8, core::mem::size_of::<VirtioGpuResourceCreate3d>()) };
        self.send_command_raw(data).is_ok()
    }

    pub fn attach_backing(&mut self, resource_id: u32, phys_addr: u64, size: u32) -> bool {
        // ResourceAttachBacking expects:
        // hdr (24 bytes)
        // resource_id (4 bytes)
        // nr_entries (4 bytes)
        // entries[]: { addr (8 bytes), length (4 bytes), padding (4 bytes) }
        let mut buf = [0u8; 48];
        let hdr = VirtioGpuCtrlHdr {
            type_: VirtioGpuCmd::ResourceAttachBacking as u32,
            flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
        };
        unsafe {
            core::ptr::write_unaligned(buf.as_mut_ptr() as *mut VirtioGpuCtrlHdr, hdr);
            core::ptr::write_unaligned(buf.as_mut_ptr().add(24) as *mut u32, resource_id);
            core::ptr::write_unaligned(buf.as_mut_ptr().add(28) as *mut u32, 1); // nr_entries
            core::ptr::write_unaligned(buf.as_mut_ptr().add(32) as *mut u64, phys_addr);
            core::ptr::write_unaligned(buf.as_mut_ptr().add(40) as *mut u32, size);
            core::ptr::write_unaligned(buf.as_mut_ptr().add(44) as *mut u32, 0); // padding
        }
        self.send_command_raw(&buf).is_ok()
    }

    pub fn set_scanout(&mut self, resource_id: u32, width: u32, height: u32) -> bool {
        self.scanout_w = width;
        self.scanout_h = height;
        let cmd = VirtioGpuSetScanout {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::SetScanout as u32,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            r: VirtioGpuRect { x: 0, y: 0, width, height },
            scanout_id: 0,
            resource_id,
        };
        let data = unsafe { core::slice::from_raw_parts(&cmd as *const _ as *const u8, core::mem::size_of::<VirtioGpuSetScanout>()) };
        self.send_command_raw(data).is_ok()
    }

    pub fn flush(&mut self, resource_id: u32, x: u32, y: u32, width: u32, height: u32) -> bool {
        // Switch scanout if needed
        if self.current_resource_id != resource_id {
            if !self.set_scanout(resource_id, width, height) { return false; }
            self.current_resource_id = resource_id;
        }

        // Byte offset of (x, y) within the resource backing.  The device uses the
        // resource's own width as the stride, so a partial-rect transfer must
        // point `offset` at the rect origin rather than the start of the buffer.
        let offset = (y as u64 * self.scanout_w as u64 + x as u64) * 4;
        let transfer = VirtioGpuTransferToHost2d {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::TransferToHost2d as u32,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            r: VirtioGpuRect { x, y, width, height },
            offset,
            resource_id,
            padding: 0,
        };
        
        let transfer_data = unsafe {
            core::slice::from_raw_parts(&transfer as *const _ as *const u8, core::mem::size_of::<VirtioGpuTransferToHost2d>())
        };
        
        if self.send_command_raw(transfer_data).is_err() { return false; }
        
        let flush = VirtioGpuResourceFlush {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::ResourceFlush as u32,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            r: VirtioGpuRect { x, y, width, height },
            resource_id,
            padding: 0,
        };
        
        let flush_data = unsafe {
            core::slice::from_raw_parts(&flush as *const _ as *const u8, core::mem::size_of::<VirtioGpuResourceFlush>())
        };
        
        self.send_command_raw(flush_data).is_ok()
    }

    pub fn transfer_to_host_3d(&mut self, resource_id: u32, x: u32, y: u32, width: u32, height: u32) -> bool {
        let transfer = VirtioGpuTransferToHost3d {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::TransferToHost3d as u32,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            box_: VirtioGpuBox { x, y, z: 0, w: width, h: height, d: 1 },
            offset: 0,
            resource_id,
            level: 0,
            stride: width * 4,
            layer_stride: 0,
        };
        
        let transfer_data = unsafe {
            core::slice::from_raw_parts(&transfer as *const _ as *const u8, core::mem::size_of::<VirtioGpuTransferToHost3d>())
        };
        
        self.send_command_raw(transfer_data).is_ok()
    }

    pub fn scale_blit(&mut self, resource_id: u32, _scanout_id: u32, src: (u32, u32, u32, u32), _dst: (u32, u32, u32, u32)) -> bool {
        // Switch scanout if needed (use SOURCE dimensions for scaling)
        if self.current_resource_id != resource_id {
            if !self.set_scanout(resource_id, src.2, src.3) { return false; }
            self.current_resource_id = resource_id;
        }

        // 1. Transfer to host (using source region)
        let transfer = VirtioGpuTransferToHost2d {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::TransferToHost2d as u32,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            r: VirtioGpuRect { x: src.0, y: src.1, width: src.2, height: src.3 },
            offset: 0,
            resource_id,
            padding: 0,
        };
        let transfer_data = unsafe { core::slice::from_raw_parts(&transfer as *const _ as *const u8, core::mem::size_of::<VirtioGpuTransferToHost2d>()) };
        if self.send_command_raw(transfer_data).is_err() { return false; }
        
        // 2. Flush resource (using SOURCE region - host handles scaling to scanout)
        let flush = VirtioGpuResourceFlush {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::ResourceFlush as u32,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            r: VirtioGpuRect { x: src.0, y: src.1, width: src.2, height: src.3 },
            resource_id,
            padding: 0,
        };
        let flush_data = unsafe { core::slice::from_raw_parts(&flush as *const _ as *const u8, core::mem::size_of::<VirtioGpuResourceFlush>()) };
        
        self.send_command_raw(flush_data).is_ok()
    }

    /// True once a scanout resource has been bound on this device, e.g. by the
    /// early boot console in [`setup_console_framebuffer`].  The DRM/KMS handoff
    /// checks this to reuse the existing RAM-backed surface instead of resetting
    /// the device and re-creating resource 1 — a rebuild the host rejects
    /// (resource already exists) that leaves the control queue wedged.
    pub fn scanout_configured(&self) -> bool {
        self.current_resource_id != 0
    }

    pub fn send_command(&mut self, cmd: VirtioGpuCmd, data: &[u8]) -> Result<Vec<u8>, ()> {
        let q = self.queues[0].as_mut().ok_or(())?;
        if q.num_free < (if data.is_empty() { 2 } else { 3 }) { return Err(()); }
        
        let hdr = VirtioGpuCtrlHdr {
            type_: cmd as u32,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            padding: 0,
        };
        
        let req_phys = mm::buddy::alloc(0).ok_or(())?;
        let req_virt = mm::phys_to_virt(req_phys) as *mut VirtioGpuCtrlHdr;
        unsafe { core::ptr::write(req_virt, hdr); }
        
        let resp_phys = mm::buddy::alloc(0).ok_or(())?;
        let resp_virt = mm::phys_to_virt(resp_phys) as *mut VirtioGpuCtrlHdr;
        
        unsafe {
            let head = q.add_desc(req_phys as u64, core::mem::size_of::<VirtioGpuCtrlHdr>() as u32, VIRTQ_DESC_F_NEXT);
            
            let mut last_desc = head;
            let mut data_phys_opt = None;
            if !data.is_empty() {
                 let data_phys = mm::buddy::alloc(0).ok_or(())?;
                 data_phys_opt = Some(data_phys);
                 let data_virt = mm::phys_to_virt(data_phys) as *mut u8;
                 core::ptr::copy_nonoverlapping(data.as_ptr(), data_virt, data.len().min(4096));
                 let data_desc = q.add_desc(data_phys as u64, data.len().min(4096) as u32, VIRTQ_DESC_F_NEXT);
                 (*q.desc.add(last_desc as usize)).next = data_desc;
                 last_desc = data_desc;
            }

            let resp_desc = q.add_desc(resp_phys as u64, 4096, VIRTQ_DESC_F_WRITE);
            (*q.desc.add(last_desc as usize)).next = resp_desc;
            
            q.submit(head);
            
            let notify_addr = (self.notify_cfg as usize + q.notify_off as usize * self.notify_off_multiplier as usize) as *mut u16;
            notify_addr.write_volatile(0);

            let mut timeout = 100_000_000;
            while q.last_used_idx == (*q.used).idx && timeout > 0 {
                core::hint::spin_loop();
                timeout -= 1;
            }

            if timeout == 0 {
                crate::pci::rdebug("[GPU] Command timeout!\n");
                return Err(());
            }
            q.last_used_idx = q.last_used_idx.wrapping_add(1);
            
            let response = core::slice::from_raw_parts(resp_virt as *const u8, 4096).to_vec();
            
            // Cleanup
            q.free_chain(head);
            mm::buddy::free(req_phys, 0);
            mm::buddy::free(resp_phys, 0);
            if let Some(p) = data_phys_opt { mm::buddy::free(p, 0); }
            
            Ok(response)
        }
    }

    /// Query the host for the preferred display mode via GET_DISPLAY_INFO.
    ///
    /// Returns `(width, height)` of the first enabled scanout, or `None` if the
    /// command fails or no scanout is enabled.  The response is a
    /// `virtio_gpu_resp_display_info`: a 24-byte control header followed by
    /// `VIRTIO_GPU_MAX_SCANOUTS` (16) `virtio_gpu_display_one` entries, each 24
    /// bytes — `rect { x, y, width, height }` (16) + `enabled` (4) + `flags` (4).
    pub fn get_display_info(&mut self) -> Option<(u32, u32)> {
        const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
        const HDR_LEN: usize = 24;
        const ENTRY_LEN: usize = 24;
        const MAX_SCANOUTS: usize = 16;

        let resp = self.send_command(VirtioGpuCmd::GetDisplayInfo, &[]).ok()?;
        let resp_type = u32::from_le_bytes(resp.get(0..4)?.try_into().ok()?);
        if resp_type != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            return None;
        }

        for i in 0..MAX_SCANOUTS {
            let base = HDR_LEN + i * ENTRY_LEN;
            // rect: x@0 y@4 width@8 height@12 ; enabled@16
            let width   = u32::from_le_bytes(resp.get(base + 8..base + 12)?.try_into().ok()?);
            let height  = u32::from_le_bytes(resp.get(base + 12..base + 16)?.try_into().ok()?);
            let enabled = u32::from_le_bytes(resp.get(base + 16..base + 20)?.try_into().ok()?);
            if enabled != 0 && width > 0 && height > 0 {
                return Some((width, height));
            }
        }
        None
    }
}

pub static VIRTIO_GPU: Mutex<Option<VirtioGpuDevice>> = Mutex::new(None);

pub fn init() {
    let mut gpu = VIRTIO_GPU.lock();
    // Idempotent: only probe/reset the device the first time.  A second
    // `VirtioGpuDevice::new()` would write `device_status = 0` (a full device
    // reset) on a GPU that the early boot console has already configured,
    // destroying its resources and scanout and wedging the control queue.
    if gpu.is_none() {
        *gpu = VirtioGpuDevice::new();
    }
}

/// Upload a 64x64 BGRA cursor image and show it at `(x, y)`.
pub fn cursor_update(pixels: &[u8], hot_x: u32, hot_y: u32, x: u32, y: u32) -> bool {
    let mut guard = VIRTIO_GPU.lock();
    match guard.as_mut() {
        Some(gpu) => gpu.cursor_update(pixels, hot_x, hot_y, x, y),
        None => false,
    }
}

/// Reposition the hardware cursor.  Costs no pixel traffic.
pub fn cursor_move(x: u32, y: u32) -> bool {
    let mut guard = VIRTIO_GPU.lock();
    match guard.as_mut() {
        Some(gpu) => gpu.cursor_move(x, y),
        None => false,
    }
}

/// Hide the hardware cursor.
pub fn cursor_hide() -> bool {
    let mut guard = VIRTIO_GPU.lock();
    match guard.as_mut() {
        Some(gpu) => gpu.cursor_hide(),
        None => false,
    }
}

/// Stage-0 gate: prove the cursor queue exists, accepts an UPDATE_CURSOR with a
/// real image and a MOVE_CURSOR, and that the host consumes both.  Draws a
/// magenta-bordered arrow-ish block so it is unmistakable on screen.
///
/// Reports the outcome on the serial console and returns whether the queue
/// drained.  Only called when `CURSOR_DEBUG` is set.
pub fn cursor_selftest() -> bool {
    // Run once: the console-framebuffer path (AArch64 boot) and KMS/DRM init
    // (both arches, when a compositor opens the card) both call this.
    static RAN: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
    if RAN.swap(true, core::sync::atomic::Ordering::SeqCst) {
        return true;
    }

    // Build the image straight into the cursor backing — a 16 KiB array would
    // not fit comfortably on the kernel stack.
    let mut guard = VIRTIO_GPU.lock();
    let ok_init = match guard.as_mut() {
        Some(gpu) => {
            if gpu.cursor_init() {
                unsafe {
                    let p = gpu.cursor_virt as *mut u8;
                    for row in 0..CURSOR_H as usize {
                        for col in 0..CURSOR_W as usize {
                            // Filled right triangle: opaque magenta, else clear.
                            let inside = col <= row && col < 40 && row < 56;
                            let i = (row * CURSOR_W as usize + col) * 4;
                            let px = if inside { [0xFFu8, 0x00, 0xFF, 0xFF] } else { [0; 4] };
                            core::ptr::copy_nonoverlapping(px.as_ptr(), p.add(i), 4);
                        }
                    }
                }
                true
            } else {
                false
            }
        }
        None => false,
    };
    drop(guard);
    if !ok_init {
        cdebug("[GPU] cursor selftest init=FAIL\n");
        return false;
    }

    // The pattern is already in the backing, so present it in place rather
    // than copying it through a staging buffer.
    let ok_update = {
        let mut guard = VIRTIO_GPU.lock();
        match guard.as_mut() {
            Some(gpu) => gpu.cursor_present(0, 0, 200, 200),
            None => false,
        }
    };
    let ok_move = cursor_move(320, 240);

    let mut guard = VIRTIO_GPU.lock();
    let drained = match guard.as_mut() {
        Some(gpu) => {
            // Give the host a moment to consume the two commands.
            let mut spins = 2_000_000u32;
            while !gpu.cursor_queue_drained() && spins > 0 {
                core::hint::spin_loop();
                spins -= 1;
            }
            gpu.cursor_queue_drained()
        }
        None => false,
    };
    drop(guard);

    cdebug("[GPU] cursor selftest update=");
    cdebug(if ok_update { "ok" } else { "FAIL" });
    cdebug(" move=");
    cdebug(if ok_move { "ok" } else { "FAIL" });
    cdebug(" drained=");
    cdebug(if drained { "ok" } else { "FAIL" });
    cdebug("\n");
    ok_update && ok_move && drained
}

/// Bring up the VirtIO GPU and create a scanout-backed framebuffer in guest RAM.
///
/// Used when the bootloader does not hand the kernel a linear framebuffer.  On
/// AArch64 QEMU uses `virtio-gpu-pci`, which — unlike x86 `virtio-vga` — exposes
/// no VGA/GOP-compatible linear framebuffer, so Limine reports no framebuffer at
/// all and the early console has no surface to draw on.
///
/// We allocate a guest-RAM surface, attach it to resource 1, and set it as
/// scanout 0.  Resource 1 is the same id `fb_flush()` transfers/flushes on every
/// console character, so once this succeeds the kernel console renders on the
/// host display.
///
/// The mode is taken from the host's preferred scanout (GET_DISPLAY_INFO);
/// `default_width`/`default_height` are used only if that query fails.
///
/// Returns `(phys, virt, width, height, pitch_bytes)` of the new framebuffer, or
/// `None` if no VirtIO GPU is present or device setup fails.  The width/height
/// reflect the mode actually programmed, which may differ from the defaults.
pub fn setup_console_framebuffer(default_width: u32, default_height: u32) -> Option<(u64, usize, u32, u32, u32)> {
    init();

    let mut guard = VIRTIO_GPU.lock();
    let gpu = guard.as_mut()?;

    // Prefer the display's reported mode; fall back to the caller's default.
    let (width, height) = match gpu.get_display_info() {
        Some((w, h)) => {
            crate::pci::rdebug("[GPU] Preferred display mode ");
            crate::pci::rdebug_hex(w);
            crate::pci::rdebug("x");
            crate::pci::rdebug_hex(h);
            crate::pci::rdebug("\n");
            (w, h)
        }
        None => {
            crate::pci::rdebug("[GPU] GET_DISPLAY_INFO unavailable; using default mode\n");
            (default_width, default_height)
        }
    };

    let pitch = width * 4;
    let fb_bytes = pitch as usize * height as usize;

    // Smallest buddy order that covers the surface (ceil_log2 of the page count).
    let pages = (fb_bytes + 4095) >> 12;
    let order = (usize::BITS - pages.leading_zeros()) as usize;
    let order = order.min(mm::buddy::MAX_ORDER - 1);

    let phys = mm::buddy::alloc(order)?;
    let virt = mm::phys_to_virt(phys);

    // Start on a clean (black) surface.
    unsafe { core::ptr::write_bytes(virt as *mut u8, 0, fb_bytes); }

    if !gpu.create_resource_2d(1, width, height) {
        crate::pci::rdebug("[GPU] create_resource_2d failed\n");
        return None;
    }
    if !gpu.attach_backing(1, phys as u64, fb_bytes as u32) {
        crate::pci::rdebug("[GPU] attach_backing failed\n");
        return None;
    }
    if !gpu.set_scanout(1, width, height) {
        crate::pci::rdebug("[GPU] set_scanout failed\n");
        return None;
    }
    gpu.flush(1, 0, 0, width, height);
    drop(guard);

    // Stage-0 gate for the hardware cursor.  Takes the device lock itself, so
    // it must run after the guard above is released.
    if CURSOR_DEBUG {
        cursor_selftest();
    }

    Some((phys as u64, virt, width, height, pitch))
}
