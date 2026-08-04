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

/// A `VIRTIO_PCI_CAP_SHARED_MEMORY_CFG` window: a host-owned BAR region that
/// host-visible blob resources are mapped into by `RESOURCE_MAP_BLOB`.
///
/// This is a fundamentally different mechanism from `RESOURCE_ATTACH_BACKING`,
/// which the rest of this driver uses: attach-backing hands the host guest-RAM
/// pages the *guest* allocated, whereas a shared-memory region is *host* memory
/// the guest maps a window onto.  The region is deliberately **not** mapped into
/// kernel VA at probe time: QEMU's `hostmem=` is routinely gigabytes, and eagerly
/// mapping it would exhaust the kernel page tables.  Only the sub-ranges that
/// `RESOURCE_MAP_BLOB` actually hands out get mapped, on demand.
#[derive(Copy, Clone, Default)]
pub struct SharedMemRegion {
    /// Shared-memory region id (`shmid`) — see `VIRTIO_GPU_SHM_ID_*`.
    pub id: u8,
    /// Physical base of the window (BAR base + capability offset).
    pub phys: u64,
    /// Window length in bytes.
    pub len: u64,
}

pub struct VirtioGpuDevice {
    _pci_dev: PciDevice,
    common_cfg: *mut VirtioPciCommonCfg,
    notify_cfg: *mut u32,
    notify_off_multiplier: u32,
    _device_cfg: *mut u8,
    /// Feature bits actually negotiated with the host (bit N = feature N).
    /// Bit 32+ live in `features_hi`.
    features: u32,
    features_hi: u32,
    /// The host-visible blob window, if the device exposed one.
    shmem: Option<SharedMemRegion>,
    /// Monotonically increasing fence id.  Never reused, never zero: the host
    /// treats fence_id 0 as "no fence" on some paths.
    next_fence_id: u64,
    /// Highest fence id the host has retired (completed the used-ring entry for).
    last_completed_fence: u64,
    /// Next 3D context id to hand out.  Context 0 means "no context".
    next_ctx_id: u32,
    /// Next resource id for 3D/blob resources.  1 is the console scanout and 2
    /// is the cursor, so 3D allocation starts above them.
    next_3d_resource_id: u32,
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

/// virtio-gpu control commands.
///
/// These numbers are the authoritative ones from the Linux uAPI header
/// `include/uapi/linux/virtio_gpu.h` (byte-identical to QEMU's vendored copy in
/// `include/standard-headers/linux/virtio_gpu.h`).  The host demultiplexes the
/// control queue purely on `hdr.type_`, so a value that disagrees with the host
/// does not fail loudly — it silently executes a *different* command, or is
/// rejected as unknown.  2D commands live in `0x01xx`, 3D/context commands in
/// `0x02xx`, cursor commands in `0x03xx`.
///
/// Note `ResourceCreateBlob` is `0x010c`, i.e. in the 2D block, not the 3D one —
/// blob resources are not a 3D-only feature upstream.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum VirtioGpuCmd {
    // ── 2D commands ──
    GetDisplayInfo = 0x0100,
    ResourceCreate2d = 0x0101,
    ResourceUnref = 0x0102,
    SetScanout = 0x0103,
    ResourceFlush = 0x0104,
    TransferToHost2d = 0x0105,
    ResourceAttachBacking = 0x0106,
    ResourceDetachBacking = 0x0107,
    GetCapsetInfo = 0x0108,
    GetCapset = 0x0109,
    GetEdid = 0x010a,
    ResourceAssignUuid = 0x010b,
    ResourceCreateBlob = 0x010c,
    SetScanoutBlob = 0x010d,

    // ── 3D / context commands ──
    CtxCreate = 0x0200,
    CtxDestroy = 0x0201,
    CtxAttachResource = 0x0202,
    CtxDetachResource = 0x0203,
    ResourceCreate3d = 0x0204,
    TransferToHost3d = 0x0205,
    TransferFromHost3d = 0x0206,
    Submit3d = 0x0207,
    ResourceMapBlob = 0x0208,
    ResourceUnmapBlob = 0x0209,

    // Cursor-queue commands (queue 1).  These take no response descriptor.
    UpdateCursor = 0x0300,
    MoveCursor = 0x0301,
}

// ── Response codes (virtio_gpu_ctrl_type) ────────────────────────────────────
pub const VIRTIO_GPU_RESP_OK_NODATA: u32 = 0x1100;
pub const VIRTIO_GPU_RESP_OK_DISPLAY_INFO: u32 = 0x1101;
pub const VIRTIO_GPU_RESP_OK_CAPSET_INFO: u32 = 0x1102;
pub const VIRTIO_GPU_RESP_OK_CAPSET: u32 = 0x1103;
pub const VIRTIO_GPU_RESP_OK_MAP_INFO: u32 = 0x1106;

/// Set once the first SUBMIT_3D reply is seen not to echo the fence we asked
/// for, so the diagnosis is stated once instead of once per frame.
static SUBMIT3D_FENCE_ECHO_WARNED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Set in `hdr.flags` to ask the host to signal `hdr.fence_id` on completion.
pub const VIRTIO_GPU_FLAG_FENCE: u32 = 1 << 0;
/// Set in `hdr.flags` to declare that `hdr.ring_idx` (the first byte of what
/// this driver calls `padding`, matching `struct virtio_gpu_ctrl_hdr`) names the
/// per-context ring the fence belongs to. Without it the host creates a plain
/// context-wide fence, which is what every submission got before ring plumbing.
pub const VIRTIO_GPU_FLAG_INFO_RING_IDX: u32 = 1 << 1;

// ── Feature bits ─────────────────────────────────────────────────────────────
pub const VIRTIO_GPU_F_VIRGL: u32 = 0;
pub const VIRTIO_GPU_F_EDID: u32 = 1;
pub const VIRTIO_GPU_F_RESOURCE_UUID: u32 = 2;
pub const VIRTIO_GPU_F_RESOURCE_BLOB: u32 = 3;
pub const VIRTIO_GPU_F_CONTEXT_INIT: u32 = 4;
/// Transport feature: bit 32, i.e. bit 0 of feature-select word 1.
pub const VIRTIO_F_VERSION_1: u32 = 32;

// ── Capset ids (virtio_gpu.h) ────────────────────────────────────────────────
pub const VIRTIO_GPU_CAPSET_VIRGL: u32 = 1;
pub const VIRTIO_GPU_CAPSET_VIRGL2: u32 = 2;
pub const VIRTIO_GPU_CAPSET_VENUS: u32 = 4;

/// `context_init` low byte selects the context type; see
/// `VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK`.
pub const VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK: u32 = 0x0000_00ff;

// ── Shared-memory region ids (virtio_gpu.h) ──────────────────────────────────
pub const VIRTIO_GPU_SHM_ID_UNDEFINED: u8 = 0;
pub const VIRTIO_GPU_SHM_ID_HOST_VISIBLE: u8 = 1;

// ── Blob memory / flags ──────────────────────────────────────────────────────
pub const VIRTIO_GPU_BLOB_MEM_GUEST: u32 = 0x0001;
pub const VIRTIO_GPU_BLOB_MEM_HOST3D: u32 = 0x0002;
pub const VIRTIO_GPU_BLOB_MEM_HOST3D_GUEST: u32 = 0x0003;
pub const VIRTIO_GPU_BLOB_FLAG_USE_MAPPABLE: u32 = 0x0001;
pub const VIRTIO_GPU_BLOB_FLAG_USE_SHAREABLE: u32 = 0x0002;

// ── RESOURCE_MAP_BLOB `map_info` (cache type the host wants the guest to use) ─
// virtio_gpu.h: VIRTIO_GPU_MAP_CACHE_*.  The low nibble is the cache type.
pub const VIRTIO_GPU_MAP_CACHE_MASK: u32 = 0x0f;
pub const VIRTIO_GPU_MAP_CACHE_NONE: u32 = 0x00;
pub const VIRTIO_GPU_MAP_CACHE_CACHED: u32 = 0x01;
pub const VIRTIO_GPU_MAP_CACHE_UNCACHED: u32 = 0x02;
pub const VIRTIO_GPU_MAP_CACHE_WC: u32 = 0x03;

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
/// `struct virtio_pci_cap64` — carries a host-memory window (`shmid` at cap+5,
/// 64-bit offset/length split across cap+8/+12 and cap+16/+20).
const VIRTIO_PCI_CAP_SHARED_MEMORY_CFG: u8 = 8;

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

/// Smallest buddy order whose allocation covers `bytes` (a contiguous run of
/// `1 << order` pages).  `bytes == 0` still yields one page.
pub fn order_for_bytes(bytes: usize) -> usize {
    let pages = ((bytes + 4095) >> 12).max(1);
    if pages == 1 {
        0
    } else {
        (usize::BITS - (pages - 1).leading_zeros()) as usize
    }
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
        let mut shmem: Option<SharedMemRegion> = None;

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

                            if bar64 != 0 && cfg_type == VIRTIO_PCI_CAP_SHARED_MEMORY_CFG {
                                // Host-visible blob window.  Record it only —
                                // mapping it here would try to build page tables
                                // for the whole `hostmem=` region (gigabytes).
                                let shmid = pci_read_config_8(dev.bus, dev.dev, dev.func, cap_ptr + 5);
                                let off_hi = pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 16);
                                let len_hi = pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 20);
                                let off64 = (offset as u64) | ((off_hi as u64) << 32);
                                let len64 = (length as u64) | ((len_hi as u64) << 32);
                                crate::pci::serial_debug("[GPU] SHARED_MEMORY_CFG shmid=");
                                crate::pci::serial_debug_hex(shmid as u32);
                                crate::pci::serial_debug(" phys=");
                                crate::pci::serial_debug_hex(((bar64 + off64) >> 32) as u32);
                                crate::pci::serial_debug_hex((bar64 + off64) as u32);
                                crate::pci::serial_debug(" len=");
                                crate::pci::serial_debug_hex((len64 >> 32) as u32);
                                crate::pci::serial_debug_hex(len64 as u32);
                                crate::pci::serial_debug("\n");
                                // virtio_gpu.h: SHM_ID_UNDEFINED = 0,
                                // SHM_ID_HOST_VISIBLE = 1.  Prefer the
                                // host-visible region; accept the first seen
                                // otherwise.
                                if shmem.is_none() || shmid == VIRTIO_GPU_SHM_ID_HOST_VISIBLE {
                                    shmem = Some(SharedMemRegion {
                                        id: shmid,
                                        phys: bar64 + off64,
                                        len: len64,
                                    });
                                }
                            } else if bar64 != 0 {
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
            features: 0,
            features_hi: 0,
            shmem,
            next_fence_id: 1,
            last_completed_fence: 0,
            next_ctx_id: 1,
            next_3d_resource_id: 16,
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

            // 4. Negotiate features.
            //
            // Feature bits 0..=31 live in feature-select word 0 and bits 32..=63
            // in word 1.  The transport bit VIRTIO_F_VERSION_1 is bit 32, so a
            // driver that only ever touches word 0 — as this one used to, writing
            // a flat `driver_feature = 0` — never acks VERSION_1 and never gets
            // VIRGL / RESOURCE_BLOB / CONTEXT_INIT either.  Nothing about that
            // failure is visible: the 2D console keeps working and every 3D
            // command is quietly dropped by the host.
            let fsel = core::ptr::addr_of_mut!((*cfg).device_feature_select);
            let fval = core::ptr::addr_of!((*cfg).device_feature);
            fsel.write_volatile(0);
            let dev_lo = fval.read_volatile();
            fsel.write_volatile(1);
            let dev_hi = fval.read_volatile();

            crate::pci::serial_debug("[GPU] device features hi=");
            crate::pci::serial_debug_hex(dev_hi);
            crate::pci::serial_debug(" lo=");
            crate::pci::serial_debug_hex(dev_lo);
            crate::pci::serial_debug("\n");

            // Everything this driver can drive.  Bits the host does not offer are
            // dropped from the ack — acking an unoffered bit makes the device
            // refuse FEATURES_OK outright — but each omission is reported.
            let want_lo: u32 = (1 << VIRTIO_GPU_F_VIRGL)
                | (1 << VIRTIO_GPU_F_EDID)
                | (1 << VIRTIO_GPU_F_RESOURCE_UUID)
                | (1 << VIRTIO_GPU_F_RESOURCE_BLOB)
                | (1 << VIRTIO_GPU_F_CONTEXT_INIT);
            let want_hi: u32 = 1 << (VIRTIO_F_VERSION_1 - 32);

            let ack_lo = dev_lo & want_lo;
            let ack_hi = dev_hi & want_hi;

            // Report every Venus prerequisite the host withheld.  The driver
            // stays up (the 2D console must keep working on plain `virtio-gpu-pci`,
            // which offers none of these), but `venus_available()` goes false and
            // every 3D entry point below refuses with a diagnostic instead of
            // issuing commands the host will silently drop.
            let checks: [(u32, &str); 3] = [
                (VIRTIO_GPU_F_VIRGL, "VIRGL"),
                (VIRTIO_GPU_F_RESOURCE_BLOB, "RESOURCE_BLOB"),
                (VIRTIO_GPU_F_CONTEXT_INIT, "CONTEXT_INIT"),
            ];
            for &(bit, name) in checks.iter() {
                if dev_lo & (1 << bit) == 0 {
                    crate::pci::serial_debug("[GPU] *** host does NOT offer VIRTIO_GPU_F_");
                    crate::pci::serial_debug(name);
                    crate::pci::serial_debug(" -- 3D/Venus unavailable ***\n");
                }
            }
            if ack_hi & (1 << (VIRTIO_F_VERSION_1 - 32)) == 0 {
                crate::pci::serial_debug("[GPU] *** host does NOT offer VIRTIO_F_VERSION_1 ***\n");
            }

            let dsel = core::ptr::addr_of_mut!((*cfg).driver_feature_select);
            let dval = core::ptr::addr_of_mut!((*cfg).driver_feature);
            dsel.write_volatile(0);
            dval.write_volatile(ack_lo);
            dsel.write_volatile(1);
            dval.write_volatile(ack_hi);

            self.features = ack_lo;
            self.features_hi = ack_hi;

            crate::pci::serial_debug("[GPU] acked features hi=");
            crate::pci::serial_debug_hex(ack_hi);
            crate::pci::serial_debug(" lo=");
            crate::pci::serial_debug_hex(ack_lo);
            crate::pci::serial_debug("\n");

            // 5. Set FEATURES_OK, then read it back — the device clears the bit
            //    if it cannot accept the subset we acked, and continuing past
            //    that point produces undefined behaviour per the spec.
            status.write_volatile(status.read_volatile() | VIRTIO_STATUS_FEATURES_OK);
            if status.read_volatile() & VIRTIO_STATUS_FEATURES_OK == 0 {
                crate::pci::serial_debug("[GPU] *** device REJECTED the acked feature set ***\n");
                self.features = 0;
                self.features_hi = 0;
            }

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

    /// Submit one control-queue command and block until the host completes it.
    ///
    /// `head` is the command struct (always beginning with a `VirtioGpuCtrlHdr`).
    /// `payload` is optional trailing data that upstream places in a descriptor
    /// of its own rather than inline — SUBMIT_3D's command stream and
    /// RESOURCE_CREATE_BLOB's `virtio_gpu_mem_entry` array both work this way.
    /// `resp_capacity` sizes the device-writable response buffer.
    ///
    /// None of the three buffers is capped at one page: each is a physically
    /// contiguous buddy run sized to its content, so a multi-kilobyte Venus
    /// command stream or a large capset response rides a single descriptor and
    /// needs no scatter-gather.  (The previous implementation hardcoded a single
    /// 4 KiB page for request and response alike and truncated anything longer.)
    ///
    /// With `fenced`, VIRTIO_GPU_FLAG_FENCE and a fresh monotonic fence id are
    /// patched into the *copied* header.  Because this path waits on the used
    /// ring, the fence has necessarily retired by the time it returns — that is
    /// what `last_completed_fence` records.
    fn submit(
        &mut self,
        head: &[u8],
        payload: Option<&[u8]>,
        resp_capacity: usize,
        fenced: bool,
    ) -> Result<Vec<u8>, ()> {
        const HDR_LEN: usize = core::mem::size_of::<VirtioGpuCtrlHdr>();
        if head.len() < HDR_LEN {
            return Err(());
        }
        let payload = payload.unwrap_or(&[]);
        let resp_capacity = resp_capacity.max(HDR_LEN);
        let need_desc = if payload.is_empty() { 2 } else { 3 };

        // Verify queue capacity before allocating, so no failure path below has
        // to unwind a partially-built allocation set.
        match self.queues[0].as_ref() {
            Some(q) if q.num_free >= need_desc => {}
            _ => return Err(()),
        }

        let hdr_type = u32::from_le_bytes(head[0..4].try_into().unwrap_or([0; 4]));

        let fence_id = if fenced {
            let f = self.next_fence_id;
            self.next_fence_id = self.next_fence_id.wrapping_add(1).max(1);
            f
        } else {
            0
        };

        let req_order = order_for_bytes(head.len());
        let resp_order = order_for_bytes(resp_capacity);
        let pay_order = order_for_bytes(payload.len().max(1));

        let req_phys = mm::buddy::alloc(req_order).ok_or(())?;
        let resp_phys = match mm::buddy::alloc(resp_order) {
            Some(p) => p,
            None => {
                mm::buddy::free(req_phys, req_order);
                return Err(());
            }
        };
        let pay_phys = if payload.is_empty() {
            0
        } else {
            match mm::buddy::alloc(pay_order) {
                Some(p) => p,
                None => {
                    mm::buddy::free(req_phys, req_order);
                    mm::buddy::free(resp_phys, resp_order);
                    return Err(());
                }
            }
        };

        let notify_cfg = self.notify_cfg;
        let mult = self.notify_off_multiplier;
        let mut out: Result<Vec<u8>, ()> = Err(());

        unsafe {
            let req_virt = mm::phys_to_virt(req_phys) as *mut u8;
            core::ptr::copy_nonoverlapping(head.as_ptr(), req_virt, head.len());
            if fenced {
                // VirtioGpuCtrlHdr: flags @4, fence_id @8.
                let flags = (req_virt.add(4) as *mut u32).read_unaligned();
                (req_virt.add(4) as *mut u32).write_unaligned(flags | VIRTIO_GPU_FLAG_FENCE);
                (req_virt.add(8) as *mut u64).write_unaligned(fence_id);
            }
            if !payload.is_empty() {
                core::ptr::copy_nonoverlapping(
                    payload.as_ptr(),
                    mm::phys_to_virt(pay_phys) as *mut u8,
                    payload.len(),
                );
            }
            core::ptr::write_bytes(mm::phys_to_virt(resp_phys) as *mut u8, 0, resp_capacity);

            let q = self.queues[0].as_mut().ok_or(())?;

            let head_idx = q.add_desc(req_phys as u64, head.len() as u32, VIRTQ_DESC_F_NEXT);
            let mut last = head_idx;
            if !payload.is_empty() {
                let d = q.add_desc(pay_phys as u64, payload.len() as u32, VIRTQ_DESC_F_NEXT);
                (*q.desc.add(last as usize)).next = d;
                last = d;
            }
            let resp_idx = q.add_desc(resp_phys as u64, resp_capacity as u32, VIRTQ_DESC_F_WRITE);
            (*q.desc.add(last as usize)).next = resp_idx;

            q.submit(head_idx);

            let notify_addr =
                (notify_cfg as usize + q.notify_off as usize * mult as usize) as *mut u16;
            notify_addr.write_volatile(0);

            let mut timeout = 100_000_000u64;
            while q.last_used_idx == (*q.used).idx && timeout > 0 {
                core::hint::spin_loop();
                timeout -= 1;
            }

            if timeout == 0 {
                crate::pci::serial_debug("[GPU] control-queue TIMEOUT, cmd=");
                crate::pci::serial_debug_hex(hdr_type);
                crate::pci::serial_debug("\n");
                // Deliberately leak the descriptors and the three pages: the
                // host may still DMA into them at any later point, and handing
                // them back to the buddy allocator would corrupt whoever gets
                // them next.  The queue is wedged regardless.
            } else {
                q.last_used_idx = q.last_used_idx.wrapping_add(1);
                let resp_virt = mm::phys_to_virt(resp_phys) as *const u8;
                out = Ok(core::slice::from_raw_parts(resp_virt, resp_capacity).to_vec());

                q.free_chain(head_idx);
                mm::buddy::free(req_phys, req_order);
                mm::buddy::free(resp_phys, resp_order);
                if !payload.is_empty() {
                    mm::buddy::free(pay_phys, pay_order);
                }
            }
        }

        if out.is_ok() && fenced {
            self.last_completed_fence = fence_id;
        }
        out
    }

    /// `submit` + check that the host answered with a success response type.
    /// Returns the full response bytes so callers can read result payloads.
    fn submit_checked(
        &mut self,
        head: &[u8],
        payload: Option<&[u8]>,
        resp_capacity: usize,
        fenced: bool,
        expect: u32,
    ) -> Result<Vec<u8>, ()> {
        let resp = self.submit(head, payload, resp_capacity, fenced)?;
        let ty = u32::from_le_bytes(resp.get(0..4).ok_or(())?.try_into().map_err(|_| ())?);
        if ty != expect && ty != VIRTIO_GPU_RESP_OK_NODATA {
            let cmd = u32::from_le_bytes(head[0..4].try_into().unwrap_or([0; 4]));
            crate::pci::serial_debug("[GPU] cmd ");
            crate::pci::serial_debug_hex(cmd);
            crate::pci::serial_debug(" failed, resp=");
            crate::pci::serial_debug_hex(ty);
            crate::pci::serial_debug("\n");
            return Err(());
        }
        Ok(resp)
    }

    fn send_command_raw(&mut self, cmd_data: &[u8]) -> Result<(), ()> {
        let resp = self.submit(cmd_data, None, 4096, false)?;
        let ty = u32::from_le_bytes(resp.get(0..4).ok_or(())?.try_into().map_err(|_| ())?);
        if ty != VIRTIO_GPU_RESP_OK_NODATA && ty != VIRTIO_GPU_RESP_OK_DISPLAY_INFO {
            crate::pci::rdebug("[GPU] Command failed with resp ");
            crate::pci::rdebug_hex(ty);
            crate::pci::rdebug("\n");
            return Err(());
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
        let hdr = VirtioGpuCtrlHdr {
            type_: cmd as u32,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            padding: 0,
        };
        let head = unsafe {
            core::slice::from_raw_parts(
                &hdr as *const _ as *const u8,
                core::mem::size_of::<VirtioGpuCtrlHdr>(),
            )
        };
        let payload = if data.is_empty() { None } else { Some(data) };
        self.submit(head, payload, 4096, false)
    }

    // ─────────────────────────────────────────────────────────────────────────
    // 3D / context / blob surface (Venus transport)
    // ─────────────────────────────────────────────────────────────────────────

    /// Was feature bit `bit` (0..=31) actually negotiated with the host?
    pub fn has_feature(&self, bit: u32) -> bool {
        bit < 32 && self.features & (1 << bit) != 0
    }

    /// Every prerequisite for creating a Venus context is present.  This is the
    /// single gate the 3D entry points check, so a host that did not offer the
    /// features produces an explicit refusal rather than commands it will drop.
    pub fn venus_available(&self) -> bool {
        self.has_feature(VIRTIO_GPU_F_VIRGL)
            && self.has_feature(VIRTIO_GPU_F_RESOURCE_BLOB)
            && self.has_feature(VIRTIO_GPU_F_CONTEXT_INIT)
    }

    pub fn shared_mem_region(&self) -> Option<SharedMemRegion> {
        self.shmem
    }

    /// `virtio_gpu_config.num_capsets` (device config offset 12).
    pub fn num_capsets(&self) -> u32 {
        if self._device_cfg.is_null() {
            return 0;
        }
        unsafe { (self._device_cfg.add(12) as *const u32).read_volatile() }
    }

    /// Allocate a fresh resource id for 3D/blob use.
    pub fn alloc_resource_id(&mut self) -> u32 {
        let id = self.next_3d_resource_id;
        self.next_3d_resource_id += 1;
        id
    }

    /// Has the host retired fence `id`?  Submission is synchronous, so any fence
    /// this driver ever handed out is retired by the time the submitting call
    /// returned; the counter exists so VIRTGPU_WAIT can answer truthfully rather
    /// than unconditionally reporting success.
    pub fn fence_retired(&self, id: u64) -> bool {
        id != 0 && id <= self.last_completed_fence
    }

    fn hdr_for(&self, cmd: VirtioGpuCmd, ctx_id: u32) -> VirtioGpuCtrlHdr {
        VirtioGpuCtrlHdr {
            type_: cmd as u32,
            flags: 0,
            fence_id: 0,
            ctx_id,
            padding: 0,
        }
    }

    /// GET_CAPSET_INFO for `capset_index`.  Returns
    /// `(capset_id, capset_max_version, capset_max_size)`.
    ///
    /// This is a *different command* from GET_CAPSET (0x0108 vs 0x0109); the two
    /// were previously conflated under a single wrong opcode.  The index is a
    /// slot number in `[0, num_capsets)`, not a capset id — the id is what comes
    /// back in the response.
    pub fn get_capset_info(&mut self, capset_index: u32) -> Result<(u32, u32, u32), ()> {
        #[repr(C, packed)]
        struct GetCapsetInfo {
            hdr: VirtioGpuCtrlHdr,
            capset_index: u32,
            padding: u32,
        }
        let cmd = GetCapsetInfo {
            hdr: self.hdr_for(VirtioGpuCmd::GetCapsetInfo, 0),
            capset_index,
            padding: 0,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &cmd as *const _ as *const u8,
                core::mem::size_of::<GetCapsetInfo>(),
            )
        };
        // virtio_gpu_resp_capset_info: hdr(24) + id + max_version + max_size + pad.
        let resp = self.submit_checked(bytes, None, 64, false, VIRTIO_GPU_RESP_OK_CAPSET_INFO)?;
        let rd = |o: usize| -> Result<u32, ()> {
            Ok(u32::from_le_bytes(
                resp.get(o..o + 4).ok_or(())?.try_into().map_err(|_| ())?,
            ))
        };
        Ok((rd(24)?, rd(28)?, rd(32)?))
    }

    /// GET_CAPSET: fetch the host's capability blob for `capset_id`.
    ///
    /// The response is `virtio_gpu_resp_capset` — a 24-byte header followed by
    /// `max_size` bytes of opaque capset data.  `max_size` comes from
    /// GET_CAPSET_INFO and is routinely far larger than one page, which is why
    /// the response buffer here is sized rather than fixed.
    pub fn get_capset(&mut self, capset_id: u32, capset_version: u32, max_size: usize) -> Result<Vec<u8>, ()> {
        #[repr(C, packed)]
        struct GetCapset {
            hdr: VirtioGpuCtrlHdr,
            capset_id: u32,
            capset_version: u32,
        }
        let cmd = GetCapset {
            hdr: self.hdr_for(VirtioGpuCmd::GetCapset, 0),
            capset_id,
            capset_version,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &cmd as *const _ as *const u8,
                core::mem::size_of::<GetCapset>(),
            )
        };
        let cap = 24 + max_size;
        let resp = self.submit_checked(bytes, None, cap, false, VIRTIO_GPU_RESP_OK_CAPSET)?;
        Ok(resp.get(24..24 + max_size).ok_or(())?.to_vec())
    }

    /// Walk the host's capset table looking for `capset_id`, returning
    /// `(max_version, max_size)`.
    ///
    /// The table is indexed by slot, not by id, so finding Venus means issuing
    /// GET_CAPSET_INFO for each of `num_capsets` slots and comparing the id that
    /// comes back.  A `None` here is itself the answer to "does the host's
    /// virglrenderer expose Venus at all".
    pub fn find_capset(&mut self, capset_id: u32) -> Option<(u32, u32)> {
        let n = self.num_capsets();
        for i in 0..n.min(16) {
            if let Ok((id, max_version, max_size)) = self.get_capset_info(i) {
                if id == capset_id {
                    return Some((max_version, max_size));
                }
            }
        }
        None
    }

    /// CTX_CREATE with an explicit `context_init` (the capset id in its low
    /// byte) — this is what selects Venus rather than the default virgl context.
    /// Returns the new context id.
    pub fn ctx_create(&mut self, capset_id: u32, debug_name: &str) -> Result<u32, ()> {
        if !self.venus_available() {
            crate::pci::serial_debug("[GPU] ctx_create refused: host lacks VIRGL/BLOB/CONTEXT_INIT\n");
            return Err(());
        }
        #[repr(C, packed)]
        struct CtxCreate {
            hdr: VirtioGpuCtrlHdr,
            nlen: u32,
            context_init: u32,
            debug_name: [u8; 64],
        }
        let ctx_id = self.next_ctx_id;
        let mut name = [0u8; 64];
        let n = debug_name.len().min(63);
        name[..n].copy_from_slice(&debug_name.as_bytes()[..n]);

        let cmd = CtxCreate {
            hdr: self.hdr_for(VirtioGpuCmd::CtxCreate, ctx_id),
            nlen: n as u32,
            context_init: capset_id & VIRTIO_GPU_CONTEXT_INIT_CAPSET_ID_MASK,
            debug_name: name,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &cmd as *const _ as *const u8,
                core::mem::size_of::<CtxCreate>(),
            )
        };
        self.submit_checked(bytes, None, 64, false, VIRTIO_GPU_RESP_OK_NODATA)?;
        self.next_ctx_id += 1;
        Ok(ctx_id)
    }

    pub fn ctx_destroy(&mut self, ctx_id: u32) -> bool {
        let hdr = self.hdr_for(VirtioGpuCmd::CtxDestroy, ctx_id);
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &hdr as *const _ as *const u8,
                core::mem::size_of::<VirtioGpuCtrlHdr>(),
            )
        };
        self.submit_checked(bytes, None, 64, false, VIRTIO_GPU_RESP_OK_NODATA)
            .is_ok()
    }

    fn ctx_resource(&mut self, cmd: VirtioGpuCmd, ctx_id: u32, resource_id: u32) -> bool {
        #[repr(C, packed)]
        struct CtxResource {
            hdr: VirtioGpuCtrlHdr,
            resource_id: u32,
            padding: u32,
        }
        let c = CtxResource {
            hdr: self.hdr_for(cmd, ctx_id),
            resource_id,
            padding: 0,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &c as *const _ as *const u8,
                core::mem::size_of::<CtxResource>(),
            )
        };
        self.submit_checked(bytes, None, 64, false, VIRTIO_GPU_RESP_OK_NODATA)
            .is_ok()
    }

    pub fn ctx_attach_resource(&mut self, ctx_id: u32, resource_id: u32) -> bool {
        self.ctx_resource(VirtioGpuCmd::CtxAttachResource, ctx_id, resource_id)
    }

    pub fn ctx_detach_resource(&mut self, ctx_id: u32, resource_id: u32) -> bool {
        self.ctx_resource(VirtioGpuCmd::CtxDetachResource, ctx_id, resource_id)
    }

    /// RESOURCE_CREATE_BLOB.
    ///
    /// For `VIRTIO_GPU_BLOB_MEM_GUEST` (and HOST3D_GUEST) the guest supplies the
    /// backing pages inline as a `virtio_gpu_mem_entry` array appended to the
    /// command; `guest_backing` is `(phys, len)`.  For `VIRTIO_GPU_BLOB_MEM_HOST3D`
    /// the storage is host-side and the array is empty — the guest reaches it
    /// through RESOURCE_MAP_BLOB into the shared-memory BAR window instead.
    pub fn resource_create_blob(
        &mut self,
        ctx_id: u32,
        resource_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        blob_id: u64,
        size: u64,
        guest_backing: Option<(u64, u32)>,
    ) -> Result<(), ()> {
        if !self.has_feature(VIRTIO_GPU_F_RESOURCE_BLOB) {
            crate::pci::serial_debug("[GPU] resource_create_blob refused: no RESOURCE_BLOB\n");
            return Err(());
        }
        #[repr(C, packed)]
        struct CreateBlob {
            hdr: VirtioGpuCtrlHdr,
            resource_id: u32,
            blob_mem: u32,
            blob_flags: u32,
            nr_entries: u32,
            blob_id: u64,
            size: u64,
        }
        #[repr(C, packed)]
        struct MemEntry {
            addr: u64,
            length: u32,
            padding: u32,
        }

        let (nr_entries, entries): (u32, Vec<u8>) = match guest_backing {
            Some((phys, len)) => {
                let e = MemEntry { addr: phys, length: len, padding: 0 };
                let b = unsafe {
                    core::slice::from_raw_parts(
                        &e as *const _ as *const u8,
                        core::mem::size_of::<MemEntry>(),
                    )
                }
                .to_vec();
                (1, b)
            }
            None => (0, Vec::new()),
        };

        let cmd = CreateBlob {
            hdr: self.hdr_for(VirtioGpuCmd::ResourceCreateBlob, ctx_id),
            resource_id,
            blob_mem,
            blob_flags,
            nr_entries,
            blob_id,
            size,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &cmd as *const _ as *const u8,
                core::mem::size_of::<CreateBlob>(),
            )
        };
        let payload = if entries.is_empty() { None } else { Some(&entries[..]) };
        self.submit_checked(bytes, payload, 64, false, VIRTIO_GPU_RESP_OK_NODATA)?;
        Ok(())
    }

    /// RESOURCE_MAP_BLOB: ask the host to expose `resource_id` at `offset` inside
    /// the shared-memory BAR window.  Returns the response's `map_info` (cache
    /// type, `VIRTIO_GPU_MAP_CACHE_*`).  Only meaningful for host-side blob
    /// memory.
    ///
    /// `struct virtio_gpu_resource_map_blob { hdr; le32 resource_id; le32 padding;
    /// le64 offset; }` and `struct virtio_gpu_resp_map_info { hdr; le32 map_info;
    /// le32 padding; }` — the header carries no context id (upstream's
    /// `virtio_gpu_cmd_map` leaves it zero), so the resource is named globally.
    ///
    /// The response type is checked STRICTLY against OK_MAP_INFO rather than
    /// through `submit_checked` alone: that helper also accepts OK_NODATA (many
    /// commands legitimately answer with it), and an OK_NODATA here would leave
    /// `map_info` reading the response buffer's zero fill — i.e. a host that
    /// answered the wrong shape would look like a successful map with cache type
    /// NONE.  This is the one command whose entire value is in its payload.
    pub fn resource_map_blob(&mut self, resource_id: u32, offset: u64) -> Result<u32, ()> {
        // A map has nowhere to land without the window the host advertises it in.
        let window = match self.shmem {
            Some(r) if r.len != 0 => r,
            _ => {
                crate::pci::serial_debug(
                    "[GPU] resource_map_blob refused: no host-visible shmem region\n",
                );
                return Err(());
            }
        };
        if offset >= window.len {
            crate::pci::serial_debug("[GPU] resource_map_blob refused: offset past window\n");
            return Err(());
        }
        #[repr(C, packed)]
        struct MapBlob {
            hdr: VirtioGpuCtrlHdr,
            resource_id: u32,
            padding: u32,
            offset: u64,
        }
        let cmd = MapBlob {
            hdr: self.hdr_for(VirtioGpuCmd::ResourceMapBlob, 0),
            resource_id,
            padding: 0,
            offset,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &cmd as *const _ as *const u8,
                core::mem::size_of::<MapBlob>(),
            )
        };
        let resp = self.submit_checked(bytes, None, 64, false, VIRTIO_GPU_RESP_OK_MAP_INFO)?;
        let ty = u32::from_le_bytes(resp.get(0..4).ok_or(())?.try_into().map_err(|_| ())?);
        if ty != VIRTIO_GPU_RESP_OK_MAP_INFO {
            crate::pci::serial_debug("[GPU] MAP_BLOB: wrong response type resp=");
            crate::pci::serial_debug_hex(ty);
            crate::pci::serial_debug("\n");
            return Err(());
        }
        Ok(u32::from_le_bytes(
            resp.get(24..28).ok_or(())?.try_into().map_err(|_| ())?,
        ))
    }

    /// RESOURCE_UNREF — drop a host-side resource of any kind.
    pub fn resource_unref(&mut self, resource_id: u32) -> bool {
        #[repr(C, packed)]
        struct Unref {
            hdr: VirtioGpuCtrlHdr,
            resource_id: u32,
            padding: u32,
        }
        let cmd = Unref {
            hdr: self.hdr_for(VirtioGpuCmd::ResourceUnref, 0),
            resource_id,
            padding: 0,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &cmd as *const _ as *const u8,
                core::mem::size_of::<Unref>(),
            )
        };
        self.submit_checked(bytes, None, 64, false, VIRTIO_GPU_RESP_OK_NODATA)
            .is_ok()
    }

    /// RESOURCE_UNMAP_BLOB — retract a host-visible blob from the shared-memory
    /// window.  `struct virtio_gpu_resource_unmap_blob { hdr; le32 resource_id;
    /// le32 padding; }`, answered with a plain OK_NODATA.  Must precede
    /// RESOURCE_UNREF for a mapped blob, and must precede any re-map of the same
    /// resource: the host tracks one window sub-region per resource and refuses a
    /// second map of an already-mapped one.
    pub fn resource_unmap_blob(&mut self, resource_id: u32) -> bool {
        #[repr(C, packed)]
        struct UnmapBlob {
            hdr: VirtioGpuCtrlHdr,
            resource_id: u32,
            padding: u32,
        }
        let cmd = UnmapBlob {
            hdr: self.hdr_for(VirtioGpuCmd::ResourceUnmapBlob, 0),
            resource_id,
            padding: 0,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &cmd as *const _ as *const u8,
                core::mem::size_of::<UnmapBlob>(),
            )
        };
        self.submit_checked(bytes, None, 64, false, VIRTIO_GPU_RESP_OK_NODATA)
            .is_ok()
    }

    /// SUBMIT_3D: hand `cmds` (an opaque, context-type-specific command stream —
    /// for a Venus context, Venus wire-protocol bytes) to the host.
    ///
    /// The stream travels in its own descriptor, so it is bounded by the buddy
    /// allocator rather than by a page.  Fenced, so the returned fence id can be
    /// waited on; returns that id.  `ring_idx` names the per-context ring the
    /// completion fence belongs to, or `None` for an unringed submission.
    ///
    /// ── WHAT "Ok" DOES AND DOES NOT MEAN ────────────────────────────────────
    ///
    /// It does NOT mean the host executed the command stream. QEMU's
    /// `virgl_cmd_submit_3d()` calls `virgl_renderer_submit_cmd()` and then
    /// **discards its return value**; whatever the renderer thought of the
    /// stream — malformed, rejected, or dropped because the render worker is
    /// dead — never reaches the wire. The guest's only answer is the generic
    /// success the device sends anyway. This is a limitation of the host we run
    /// against, not something this driver can fix, and it is the reason a dead
    /// renderer once looked like a working one for a whole session.
    ///
    /// What we CAN check, and now do, is narrower but real. A fenced command is
    /// answered by QEMU's *fence* path, not by its inline reply path: the reply
    /// is written only when the renderer retires the fence we asked for, and it
    /// echoes our `fence_id` back with VIRTIO_GPU_FLAG_FENCE set. So:
    ///   * a reply whose fence_id matches proves the renderer was alive enough
    ///     to reach and retire this fence — it says nothing about whether the
    ///     commands inside were accepted;
    ///   * a reply that does NOT echo the fence means the response came from
    ///     somewhere other than the fence path (a non-3D device answering
    ///     inline, or a host that never armed the fence) and the submission was
    ///     almost certainly not executed at all;
    ///   * a renderer that is truly wedged never retires the fence, so the
    ///     control queue times out and `submit` reports that loudly instead of
    ///     returning a fake success.
    /// The mismatch is reported, not turned into an error: it is a diagnosis of
    /// the host, and failing the ioctl on it would break clients on hosts that
    /// answer differently but work.
    pub fn submit_3d(&mut self, ctx_id: u32, cmds: &[u8], ring_idx: Option<u8>) -> Result<u64, ()> {
        if !self.venus_available() {
            crate::pci::serial_debug("[GPU] submit_3d refused: 3D features unavailable\n");
            return Err(());
        }
        if cmds.is_empty() {
            return Err(());
        }
        #[repr(C, packed)]
        struct CmdSubmit {
            hdr: VirtioGpuCtrlHdr,
            size: u32,
            padding: u32,
        }
        let mut hdr = self.hdr_for(VirtioGpuCmd::Submit3d, ctx_id);
        if let Some(r) = ring_idx {
            // `virtio_gpu_ctrl_hdr` ends in `u8 ring_idx; u8 padding[3]`, which
            // this driver models as one little-endian `u32 padding` — so the
            // ring index is its low byte on both target arches.
            hdr.flags |= VIRTIO_GPU_FLAG_INFO_RING_IDX;
            hdr.padding = r as u32;
        }
        let cmd = CmdSubmit {
            hdr,
            size: cmds.len() as u32,
            padding: 0,
        };
        let bytes = unsafe {
            core::slice::from_raw_parts(
                &cmd as *const _ as *const u8,
                core::mem::size_of::<CmdSubmit>(),
            )
        };
        let fence = self.next_fence_id;
        let resp = self.submit_checked(bytes, Some(cmds), 64, true, VIRTIO_GPU_RESP_OK_NODATA)?;

        // The one independent liveness signal available here — see the header
        // comment. Reported once per boot: EXECBUFFER runs per frame, and a
        // host that answers this way answers it every time.
        let rflags = u32::from_le_bytes(resp.get(4..8).ok_or(())?.try_into().map_err(|_| ())?);
        let rfence = u64::from_le_bytes(resp.get(8..16).ok_or(())?.try_into().map_err(|_| ())?);
        if (rflags & VIRTIO_GPU_FLAG_FENCE) == 0 || rfence != fence {
            if !SUBMIT3D_FENCE_ECHO_WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
                crate::pci::serial_debug("[GPU] SUBMIT_3D reply did not echo our fence: sent=");
                crate::pci::serial_debug_hex_64(fence);
                crate::pci::serial_debug(" got=");
                crate::pci::serial_debug_hex_64(rfence);
                crate::pci::serial_debug(" flags=");
                crate::pci::serial_debug_hex(rflags);
                crate::pci::serial_debug(
                    " — the host did NOT answer from its fence path, so this stream was\
                     \n      very likely never executed. Note the host discards the renderer's\
                     \n      own verdict either way, so a matching fence is not proof of\
                     \n      execution. (reported once per boot)\n",
                );
            }
        }
        Ok(fence)
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
