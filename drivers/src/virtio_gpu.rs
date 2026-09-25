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
    /// MSI-X: config-space offset of the capability and the mapped vector
    /// table, when the device has one and this arch can take the interrupt.
    /// `None` leaves completion to the tick poller alone.
    msix: Option<(u8, *mut u32)>,
    /// The ISR status byte (`VIRTIO_PCI_CAP_ISR_CFG`). Reading it returns and
    /// clears the queue/config interrupt bits and deasserts INTx; null when
    /// the device did not expose one.
    isr_cfg: *mut u8,
    /// GIC INTID the device's INTx pin is armed on (aarch64), `None` when
    /// completion is left to the tick poller.
    intx: Option<u32>,
    /// Monotonically increasing fence id.  Never reused, never zero: the host
    /// treats fence_id 0 as "no fence" on some paths.
    next_fence_id: u64,
    /// Every fence id `<= fence_floor` has retired. Fences that retired ahead
    /// of a lower unretired one (a Venus ring finishing before an older
    /// submission on another ring) wait in `fences_ahead` until the floor
    /// reaches them, so `fence_retired` is exact rather than a watermark that
    /// would declare an unfinished fence done because a later one finished.
    fence_floor: u64,
    /// Retired fence ids above `fence_floor`; 0 marks a free slot. Sized to the
    /// control queue, which bounds how many fences can be outstanding at all.
    fences_ahead: Vec<u64>,
    /// Control-queue commands the host has not answered yet, indexed by the
    /// head descriptor of their chain. See `Inflight`.
    inflight: Vec<Option<Inflight>>,
    /// Pages whose command has completed but which were reaped in tick context,
    /// where the buddy allocator must not be entered. Drained by the next
    /// `submit`/`submit_async` from task context. Capacity reserved at init so
    /// a push never allocates.
    deferred_free: Vec<(usize, usize)>,
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
// ── Control-queue census ─────────────────────────────────────────────────────
//
// Presents, transfers and SUBMIT_3D are submitted asynchronously (`submit_async`)
// and cost the vCPU only the enqueue; commands whose reply is needed (`submit`)
// still spin the vCPU for a host round trip, with the tick let in. Under Venus
// the host has real GPU work behind a fenced round trip, where softpipe had a
// memcpy — which is why the split matters.
//
// The cursor queue is deliberately NOT counted here: it is fire-and-forget (a
// single read-only descriptor, no response), so cursor motion cannot stall.
// That asymmetry is itself diagnostic — if a freeze correlates with input that
// moves only the cursor, this is not where it is.
//
// `ctrlq_n` counts every command kicked. `ctrlq_us` is the vCPU time spent
// waiting — synchronous round trips plus ring-full waits — against
// `ctrlq_sync` for the mean; `ctrlq_max` catches the outlier a mean hides.
// `ctrlq_to` counts the bounded-wait bail-out, which also prints
// `[GPU] control-queue TIMEOUT` on its own.
pub static CTRLQ_CMDS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CTRLQ_SPIN_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CTRLQ_SPIN_MAX_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CTRLQ_TIMEOUTS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Commands submitted without waiting (`submit_async`) / with a spin (`submit`).
/// `ctrlq_us` above now counts only the spinning half plus ring-full waits —
/// the vCPU time the queue actually costs — while `ctrlq_lat_us` is how long
/// the asynchronous commands took the host to answer, which nobody waited for.
pub static CTRLQ_ASYNC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CTRLQ_SYNC: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CTRLQ_ASYNC_LAT_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CTRLQ_ASYNC_LAT_MAX_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Times a submit found the ring full and had to wait for the host.
pub static CTRLQ_ROOM_WAITS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Asynchronous commands the host answered with an error (nobody else sees it).
pub static CTRLQ_ASYNC_REFUSED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Pages a tick-context reap could neither free nor defer. Should stay 0.
pub static CTRLQ_LEAKED_PAGES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

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
/// `enum virtio_gpu_formats`, the subset this KMS can hand SET_SCANOUT_BLOB.
/// Named after the fourcc each one is the host-side spelling of.
pub const VIRTIO_GPU_FORMAT_B8G8R8A8_UNORM: u32 = 1;   // DRM_FORMAT_ARGB8888
pub const VIRTIO_GPU_FORMAT_B8G8R8X8_UNORM: u32 = 2;   // DRM_FORMAT_XRGB8888
pub const VIRTIO_GPU_FORMAT_R8G8B8A8_UNORM: u32 = 67;  // DRM_FORMAT_ABGR8888
pub const VIRTIO_GPU_FORMAT_R8G8B8X8_UNORM: u32 = 134; // DRM_FORMAT_XBGR8888

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

/// One control-queue chain the host has not answered yet. Indexed by its
/// head descriptor in `VirtioGpuDevice::inflight`. `Copy` so a reap can lift it
/// out before mutating the device.
#[derive(Clone, Copy)]
struct Inflight {
    req_phys: usize, req_order: usize,
    /// 0 when the command carried no payload descriptor.
    pay_phys: usize, pay_order: usize,
    resp_phys: usize, resp_order: usize, resp_capacity: usize,
    /// 0 = unfenced.
    fence_id: u64,
    hdr_type: u32,
    /// `monotonic_us` at kick, for the completion-latency census.
    submitted_us: u64,
    /// A `submit` caller is spinning on `done`; the reaper must not free the
    /// buffers, the caller reads the reply out of them.
    sync: bool,
    done: bool,
    resp_type: u32,
}

/// Bound on any control-queue wait, in spin iterations (each ≤ one
/// `irq_window` in 256). The device answering nothing for this long is wedged.
const CTRLQ_WAIT_ITERS: u64 = 100_000_000;

/// Fence id of the most recent present (`send_present_async`), 0 before the
/// first. Written under `VIRTIO_GPU`; read by the DRM layer right after the
/// present it just issued, on the same task, so it names that present.
pub static LAST_PRESENT_FENCE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// All fence ids `<= GPU_FENCE_FLOOR` have retired. Lock-free mirror of
/// `VirtioGpuDevice::fence_floor` for the DRM layer's out-fence service, which
/// runs from the tick and must not take `VIRTIO_GPU`.
pub static GPU_FENCE_FLOOR: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// A reap retired at least one fence and the DRM layer has not been told yet.
/// Set under `VIRTIO_GPU` (any context), consumed by `ctrlq_tick`.
static FENCE_EVENT_PENDING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Called by `ctrlq_tick` after fences retired: the DRM layer's out-fence and
/// VIRTGPU_WAIT service. Returns false if it could not finish (a try_lock
/// lost) and wants to be called again next tick. Installed by the DRM layer.
static FENCE_EVENT_HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

static CTRLQ_CORRUPT_WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// IDT vector the control queue's MSI-X message carries. Must match the entry
/// `arch_x86_64::idt::init` installs for `virtio_gpu_msix_isr`. 0x40 is the
/// reschedule IPI and 0xFD the TLB shootdown; 0x41 is free.
pub const MSIX_VECTOR_CTRLQ: u8 = 0x41;
/// MSI message address base (the LAPIC's), destination APIC id in bits 12..19.
const MSI_ADDR_BASE: u32 = 0xFEE0_0000;
/// The BSP. Physical destination mode, fixed delivery, edge — the data word
/// carries only the vector.
const MSIX_DEST_APIC_ID: u32 = 0;
/// Set once `enable_msix` succeeded; the interrupt census below is only
/// meaningful then.
pub static CTRLQ_IRQ_ARMED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// Control-queue interrupts taken.
pub static CTRLQ_IRQS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The control-queue MSI-X handler, entered from `arch_x86_64::idt` (vector
/// `MSIX_VECTOR_CTRLQ`) after the LAPIC EOI. IRQ context: it is exactly the
/// tick poller's body — try_lock the device, reap, pay the fence notification
/// with try-wakes — and nothing else. No allocation, no blocking lock.
#[no_mangle]
pub extern "C" fn virtio_gpu_msix_isr() {
    CTRLQ_IRQS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    ctrlq_tick();
    kick_parked_waiter();
}

/// QEMU virt wires the PCIe host bridge's four INTx lines to SPIs 3..6, i.e.
/// INTIDs 35..38, swizzled per slot: line = (slot + pin) % 4 with INTA = 0
/// (`hw/arm/virt.c` VIRT_PCIE, `pci_swizzle_map_irq_fn`).
#[cfg(target_arch = "aarch64")]
const VIRT_PCIE_INTX_BASE_INTID: u32 = 32 + 3;
/// Deliveries on the armed INTx line that found no GPU condition (`isr == 0`):
/// another function on the same line. Diagnostic; the guard below acts on the
/// consecutive count.
pub static INTX_SPURIOUS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[cfg(target_arch = "aarch64")]
static INTX_SPURIOUS_RUN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Consecutive deliveries with no GPU condition after which the line is judged
/// held by someone else and given up (a level line nobody clears re-fires at
/// every EOI and would livelock the CPU otherwise). Legitimate sharing yields
/// far fewer between two GPU completions.
#[cfg(target_arch = "aarch64")]
const INTX_STORM_LIMIT: u32 = 1024;
/// INTID the line is armed on, 0 when it is not, for the storm guard and the
/// `[DRMSTAT]` line.
static INTX_INTID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// The ISR status byte, published for the handler, which must reach it without
/// the device lock (the lock holder is often the very task it interrupted).
static INTX_ISR_PTR: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// The INTx handler, entered from `arch_aarch64::exception::handle_irq` via
/// the GIC dispatch table, IRQs masked, before EOI. Reading the ISR status
/// byte is what deasserts the level line, so it comes first and
/// unconditionally; then the same body as the MSI-X handler. Bit 0 is a
/// queue interrupt (control or cursor queue — the cursor queue's avail flags
/// ask for none), bit 1 a config change (display-info), which nobody
/// consumes yet.
#[cfg(target_arch = "aarch64")]
extern "C" fn virtio_gpu_intx_isr() {
    use core::sync::atomic::Ordering::Relaxed;
    let p = INTX_ISR_PTR.load(core::sync::atomic::Ordering::Acquire) as *mut u8;
    if p.is_null() { return; }
    let isr = unsafe { p.read_volatile() };
    if isr == 0 {
        INTX_SPURIOUS.fetch_add(1, Relaxed);
        let run = INTX_SPURIOUS_RUN.fetch_add(1, Relaxed) + 1;
        if run >= INTX_STORM_LIMIT {
            let id = INTX_INTID.swap(0, Relaxed);
            if id != 0 {
                extern "C" { fn arch_disable_irq(id: u32); }
                unsafe { arch_disable_irq(id); }
                CTRLQ_IRQ_ARMED.store(false, core::sync::atomic::Ordering::Release);
                crate::pci::serial_debug("[GPU] INTx storm: INTID ");
                crate::pci::serial_debug_hex(id);
                crate::pci::serial_debug(" held asserted by another function; masked, polling only\n");
            }
        }
        return;
    }
    INTX_SPURIOUS_RUN.store(0, Relaxed);
    if isr & 1 != 0 {
        if CTRLQ_IRQS.fetch_add(1, Relaxed) == 0 {
            crate::pci::serial_debug("[GPU] INTx: first control-queue completion interrupt\n");
        }
        ctrlq_tick();
        kick_parked_waiter();
    }
}

/// True when a completion interrupt is armed on this device (MSI-X on x86_64,
/// INTx on aarch64): a waiter may park the CPU instead of spinning, because
/// the host's answer will wake it.
#[inline]
pub fn irq_armed() -> bool {
    CTRLQ_IRQ_ARMED.load(core::sync::atomic::Ordering::Acquire)
}

/// Park the CPU until the next interrupt, IRQs masked at entry and exit, and
/// let that interrupt run. On aarch64 `wfi` wakes on a pending interrupt
/// whether or not PSTATE.I masks it, so the sequence is race-free: a
/// completion that lands between the caller's check and the `wfi` is pending
/// and returns immediately. On x86_64 `hlt` needs IF=1 to wake at all; the
/// `sti` shadow delays recognition to the `hlt` itself, which gives the same
/// atomicity. Either way the handler that ran could not take the device lock
/// the caller holds; the caller reaps after this returns.
#[inline]
fn park_until_irq() {
    unsafe {
        #[cfg(target_arch = "aarch64")]
        core::arch::asm!("wfi", options(nomem, nostack));
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("sti; hlt; cli", options(nomem, nostack));
    }
    sched::irq_window();
}

/// Register the fence-event service (`fn() -> bool`). One hook; the last
/// registration wins.
pub fn set_fence_event_hook(f: fn() -> bool) {
    FENCE_EVENT_HOOK.store(f as usize, core::sync::atomic::Ordering::Release);
}

/// Tick-hook entry: reap the control queue if the device is free, then pay any
/// pending fence notification. Never blocks — `try_lock` on the device and
/// no allocation or free inside — so it is safe from the 100 Hz tick on any CPU.
/// Called from `drm_device_interface::drm_tick`.
pub fn ctrlq_tick() {
    use core::sync::atomic::Ordering::{AcqRel, Acquire, Release};
    if let Some(mut g) = VIRTIO_GPU.try_lock() {
        if let Some(gpu) = g.as_mut() {
            if gpu.ctrlq_reap(false) { FENCE_EVENT_PENDING.store(true, Release); }
        }
    }
    if FENCE_EVENT_PENDING.swap(false, AcqRel) {
        let p = FENCE_EVENT_HOOK.load(Acquire);
        if p != 0 {
            // SAFETY: only ever written by `set_fence_event_hook` from a `fn() -> bool`.
            let f: fn() -> bool = unsafe { core::mem::transmute::<usize, fn() -> bool>(p) };
            if !f() { FENCE_EVENT_PENDING.store(true, Release); }
        }
    }
}


/// Release the preempt-disable taken for a spin window on EVERY exit from
/// the wait — timeout, panic unwind, or any future `?`. Leaking the count
/// would wedge preemption on this CPU for good.
///
/// Why a window at all: syscalls run with IRQs masked, so a spin here would
/// otherwise park the vCPU at IF=0 for the whole host round trip and stop
/// the tick — poll deadlines unmet, `nanosleep` overslept, the audio pump
/// starved, no page-flip deliveries. `irq_window()` lets the tick in. But
/// the tick's `preempt_check` would then `yield_now()` while the caller
/// still holds `VIRTIO_GPU` (and often the DRM device mutex above it); the
/// next task to touch the GPU spins at IF=0 forever on that mutex and the
/// holder never runs again — a deterministic hang on one vCPU. So the
/// window runs with preemption disabled: tick yes, switch no. The count is
/// released without a resched (`PREEMPT_NEEDED` stays set, so the switch
/// happens at the next `preempt_check`, where no GPU lock is held).
///
/// Gated on IRQs being masked at entry: the boot console reaches this code
/// before `timer::init`, with interrupts *enabled*, and `irq_window()` ends
/// by masking — running it there would hand back a CPU with them off.
struct SpinWindow { open: bool }
impl SpinWindow {
    fn new() -> Self {
        let open = sched::irqs_masked() && sched::current_pid() != 0;
        if open { sched::preempt_disable(); }
        SpinWindow { open }
    }
    /// One iteration in 256, not every one: `sti; pause; cli` forces a TCG
    /// exit and costs two to three orders of magnitude more than a bare
    /// `spin_loop`, and the bail-out below is an iteration count.
    #[inline]
    fn pulse(&self, iter: u64) {
        if self.open && iter & 0xFF == 0 { sched::irq_window(); }
    }
}
impl Drop for SpinWindow {
    fn drop(&mut self) {
        if self.open { sched::preempt_enable_no_resched(); }
    }
}

/// Synchronous waits `submit` and `ensure_ctrlq_room` still take under
/// `VIRTIO_GPU` (the reply-needing ~0.5 % of traffic and a full ring) no
/// longer spin the vCPU for long when a completion interrupt is armed: after
/// `CTRLQ_SPIN_BEFORE_PARK_US` of spinning, each step parks the CPU in
/// `wfi`/`hlt` until an interrupt — the device's, or the tick — and reaps on
/// return. The lock stays held, so this is not a sleep other tasks can use
/// the CPU through, but the host sees an idle vCPU for the
/// round trip and the wake is the interrupt's latency, not a poll's.
///
/// Without an interrupt the wake would be the tick, up to 10 ms away, so the
/// unarmed case keeps the spin. Bounded in ticks rather than iterations
/// because each parked step is at least one interrupt long.
pub static CTRLQ_PARKED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
const CTRLQ_PARK_TIMEOUT_TICKS: u64 = 500; // 5 s at 100 Hz: the device is wedged
/// The CPU a `CtrlqWait` is parked on, `NO_PARKED_CPU` when none. The
/// completion interrupt is routed to the BSP (MSI-X destination APIC 0, the
/// GIC IROUTER), so a waiter on any other CPU would sleep through it until
/// its own tick; the handler reads this and kicks that CPU with the
/// reschedule IPI (no flag set, so nothing reschedules — the SGI/vector
/// only ends the `wfi`/`hlt`). Set for the whole wait, not just around the
/// park, so a completion that lands between the waiter's check and its park
/// still sends the kick, which is then pending when the park begins and ends
/// it at once. One waiter at a time: the wait holds `VIRTIO_GPU`.
static CTRLQ_PARKED_CPU: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(NO_PARKED_CPU);
const NO_PARKED_CPU: usize = usize::MAX;
/// IPIs the completion handler sent to a parked waiter on another CPU.
pub static CTRLQ_PARK_KICKS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// From the completion handler: wake a waiter parked on another CPU.
#[inline]
fn kick_parked_waiter() {
    extern "C" { fn arch_send_resched_ipi(cpu: usize); }
    let c = CTRLQ_PARKED_CPU.load(core::sync::atomic::Ordering::Acquire);
    if c != NO_PARKED_CPU && c != unsafe { sched::cpu_id() } {
        CTRLQ_PARK_KICKS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        unsafe { arch_send_resched_ipi(c); }
    }
}

/// How long a wait spins before it parks, in microseconds. Most
/// reply-needing commands come back well inside this (x86_64/KVM Zink
/// session start: mean 55 us spinning), and a spun wait sees the reply the
/// moment it lands. A parked one depends on the completion interrupt
/// reaching the BSP and, when the waiter sits elsewhere, on the BSP's IPI
/// back — and the BSP takes neither while it runs with IRQs masked (a
/// syscall, or spinning at IF=0 for the very `VIRTIO_GPU` lock this wait
/// holds), so a parked wait can last until the waiter's own tick. Parking
/// from the first step measured mean 55 -> 330-370 us per reply-needing
/// command and max 2.5 -> 8.8-9.5 ms against spinning (x86_64/KVM, Zink
/// session start, ~130 commands); spinning 200 us first: 50-64 us, 2.8 ms.
/// Parking is kept for the slow tail, where an idle vCPU is worth a
/// wake-up's latency.
const CTRLQ_SPIN_BEFORE_PARK_US: u64 = 200;

/// Wait-time histogram of synchronous control-queue waits (`DRM_STATS`
/// only): < 50 us, < 200 us, < 1 ms, < 5 ms, >= 5 ms.
pub static CTRLQ_WAIT_HIST: [core::sync::atomic::AtomicU64; 5] = [
    core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0), core::sync::atomic::AtomicU64::new(0),
    core::sync::atomic::AtomicU64::new(0),
];
/// Waits that outlasted the spin and reached the parked phase (`DRM_STATS`
/// only); `CTRLQ_PARKED` counts every wait that was allowed to park.
pub static CTRLQ_PARK_PHASE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

struct CtrlqWait { park: bool, deadline: u64, t0: u64, parking: core::cell::Cell<bool> }
impl CtrlqWait {
    fn new(window: &SpinWindow) -> Self {
        let park = window.open && irq_armed();
        if park {
            CTRLQ_PARKED.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            CTRLQ_PARKED_CPU.store(unsafe { sched::cpu_id() }, core::sync::atomic::Ordering::Release);
        }
        CtrlqWait {
            park,
            deadline: sched::ticks().wrapping_add(CTRLQ_PARK_TIMEOUT_TICKS),
            t0: crate::snd::monotonic_us(),
            parking: core::cell::Cell::new(false),
        }
    }
    /// One wait step; false when the parked wait has run out of time.
    #[inline]
    fn step(&self, window: &SpinWindow, iter: u64) -> bool {
        if self.park && (self.parking.get()
            || crate::snd::monotonic_us().wrapping_sub(self.t0) >= CTRLQ_SPIN_BEFORE_PARK_US)
        {
            if !self.parking.replace(true) && crate::drm_device_interface::DRM_STATS {
                CTRLQ_PARK_PHASE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            }
            park_until_irq();
            (sched::ticks().wrapping_sub(self.deadline) as i64) < 0
        } else {
            window.pulse(iter);
            core::hint::spin_loop();
            true
        }
    }
}
impl Drop for CtrlqWait {
    fn drop(&mut self) {
        if self.park { CTRLQ_PARKED_CPU.store(NO_PARKED_CPU, core::sync::atomic::Ordering::Release); }
        if crate::drm_device_interface::DRM_STATS {
            let dt = crate::snd::monotonic_us().wrapping_sub(self.t0);
            let b = if dt < 50 { 0 } else if dt < 200 { 1 } else if dt < 1000 { 2 } else if dt < 5000 { 3 } else { 4 };
            CTRLQ_WAIT_HIST[b].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
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
        let mut isr_cfg: *mut u8 = core::ptr::null_mut();
        let mut shmem: Option<SharedMemRegion> = None;
        let mut msix: Option<(u8, *mut u32)> = None;

        unsafe {
            let mut cap_ptr = pci_read_config_8(dev.bus, dev.dev, dev.func, 0x34);
            while cap_ptr != 0 {
                let cap_id = pci_read_config_8(dev.bus, dev.dev, dev.func, cap_ptr);
                if cap_id == 0x11 && cfg!(target_arch = "x86_64") {
                    // MSI-X capability: Message Control @2 (table size - 1 in
                    // the low 11 bits), Table Offset/BIR @4 (BIR in bits 0..2).
                    // Map the vector table so `enable_msix` can program entry 0
                    // for the control queue. Delivery is a LAPIC message, so no
                    // IOAPIC routing or INTx pin swizzle is involved.
                    let ctrl = pci_read_config_16(dev.bus, dev.dev, dev.func, cap_ptr + 2);
                    let entries = (ctrl & 0x7FF) as usize + 1;
                    let tbl = pci_read_config_32(dev.bus, dev.dev, dev.func, cap_ptr + 4);
                    let bir = (tbl & 7) as usize;
                    let toff = (tbl & !7) as usize;
                    if bir < 6 && dev.bars[bir] & 1 == 0 {
                        let raw = dev.bars[bir];
                        let base: u64 = if (raw >> 1) & 3 == 2 && bir + 1 < 6 {
                            ((raw & !0xF) as u64) | ((dev.bars[bir + 1] as u64) << 32)
                        } else {
                            (raw & !0xF) as u64
                        };
                        if base != 0 {
                            let virt = mm::paging::map_kernel_device(
                                base as usize + toff,
                                entries * 16,
                                mm::paging::PageFlags::PRESENT | mm::paging::PageFlags::WRITABLE | mm::paging::PageFlags::MMIO,
                            ).unwrap_or_else(|| mm::phys_to_virt(base as usize + toff));
                            crate::pci::serial_debug("[GPU] MSI-X table: entries=");
                            crate::pci::serial_debug_hex(entries as u32);
                            crate::pci::serial_debug(" bar=");
                            crate::pci::serial_debug_hex(bir as u32);
                            crate::pci::serial_debug(" off=");
                            crate::pci::serial_debug_hex(toff as u32);
                            crate::pci::serial_debug("\n");
                            msix = Some((cap_ptr, virt as *mut u32));
                        }
                    }
                }
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
                                    VIRTIO_PCI_CAP_ISR_CFG => isr_cfg = virt as *mut u8,
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
            msix,
            isr_cfg,
            intx: None,
            next_fence_id: 1,
            fence_floor: 0,
            fences_ahead: Vec::new(),
            inflight: Vec::new(),
            deferred_free: Vec::new(),
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
            VIRGL_NEGOTIATED.store(dev_lo & (1 << VIRTIO_GPU_F_VIRGL) != 0,
                                   core::sync::atomic::Ordering::Relaxed);
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
            // In-flight bookkeeping for the control queue, sized once from the
            // negotiated ring: one slot per descriptor (a chain's head can be
            // any of them), one pending-fence slot per descriptor (every fence
            // rides exactly one chain), and room to defer every buffer of every
            // chain so a tick-context reap never allocates.
            let qsize = self.queues[0].as_ref().map(|q| q.size as usize).unwrap_or(0);
            self.inflight = alloc::vec![None; qsize];
            self.fences_ahead = alloc::vec![0u64; qsize.max(1)];
            self.deferred_free = Vec::with_capacity(qsize * 3 + 8);

            // 6b. Control-queue completion interrupt (MSI-X vector 0), before
            //     DRIVER_OK as the spec orders it. Failure leaves the poller.
            self.enable_msix();
            self.enable_intx();

            // 7. Set DRIVER_OK status bit
            status.write_volatile(status.read_volatile() | VIRTIO_STATUS_DRIVER_OK);
        }
        crate::pci::rdebug("[GPU] VirtIO GPU initialized\n");
    }

    /// Route control-queue completions to a LAPIC interrupt through MSI-X.
    ///
    /// Entry 0 of the vector table is programmed for `MSIX_VECTOR_CTRLQ` on
    /// APIC id 0 (the BSP under QEMU; the IDT is shared, so any CPU could take
    /// it), the function is enabled masked, the control queue is bound to
    /// vector 0 in `common_cfg` and the binding read back — the device answers
    /// `NO_VECTOR` if it could not take it — and only then is the mask lifted.
    /// The config-change vector stays unassigned. The handler is
    /// `virtio_gpu_msix_isr`, reached from `arch_x86_64::idt` by symbol.
    ///
    /// The 100 Hz poller (`ctrlq_tick`) keeps running regardless: an
    /// interrupt can be lost or unavailable (no MSI-X on the device, another
    /// arch, a host that never delivers), and the poller costs one try_lock
    /// per tick when nothing is in flight.
    unsafe fn enable_msix(&mut self) {
        let (cap, table) = match self.msix { Some(m) => m, None => return };
        const NO_VECTOR: u16 = 0xFFFF;
        let dev = &self._pci_dev;
        // Entry 0, masked while it is being written.
        table.add(3).write_volatile(1);
        table.add(0).write_volatile(MSI_ADDR_BASE | (MSIX_DEST_APIC_ID << 12));
        table.add(1).write_volatile(0);
        table.add(2).write_volatile(MSIX_VECTOR_CTRLQ as u32);
        // MSI-X Enable (bit 15) with Function Mask (bit 14) set.
        let ctrl = pci_read_config_16(dev.bus, dev.dev, dev.func, cap + 2);
        pci_write_config_16(dev.bus, dev.dev, dev.func, cap + 2, ctrl | 0xC000);
        let cfg = self.common_cfg;
        core::ptr::addr_of_mut!((*cfg).config_msix_vector).write_volatile(NO_VECTOR);
        core::ptr::addr_of_mut!((*cfg).queue_select).write_volatile(0);
        core::ptr::addr_of_mut!((*cfg).queue_msix_vector).write_volatile(0);
        let got = core::ptr::addr_of!((*cfg).queue_msix_vector).read_volatile();
        if got != 0 {
            crate::pci::serial_debug("[GPU] MSI-X: device refused vector 0 for the control queue; polling only\n");
            pci_write_config_16(dev.bus, dev.dev, dev.func, cap + 2, ctrl & !0x8000);
            self.msix = None;
            return;
        }
        // Lift the function mask and the entry mask.
        pci_write_config_16(dev.bus, dev.dev, dev.func, cap + 2, (ctrl | 0x8000) & !0x4000);
        table.add(3).write_volatile(0);
        CTRLQ_IRQ_ARMED.store(true, core::sync::atomic::Ordering::Release);
        crate::pci::serial_debug("[GPU] MSI-X armed: control queue -> vector ");
        crate::pci::serial_debug_hex(MSIX_VECTOR_CTRLQ as u32);
        crate::pci::serial_debug("\n");
    }

    /// Route control-queue completions to a GIC SPI through the device's INTx
    /// pin (aarch64, QEMU virt). The transport is virtio-gpu-pci behind the
    /// virt board's PCIe host bridge, whose INTA..D are SPIs 3..6; the line a
    /// function lands on is the standard swizzle of its slot and pin. INTx is
    /// level-triggered: the handler reads the ISR status byte, which is what
    /// deasserts it, before the common dispatcher EOIs.
    ///
    /// Every other virtio-pci function this kernel drives is polled and sets
    /// its own INTx-disable bit (PCI command bit 10), so a line shared with
    /// one of them is never held asserted by a device nobody services. The
    /// handler's storm guard covers a function that does not, by masking the
    /// SPI again and leaving the poller.
    ///
    /// The 100 Hz poller keeps running regardless, exactly as with MSI-X.
    #[cfg(not(target_arch = "aarch64"))]
    unsafe fn enable_intx(&mut self) {}

    #[cfg(target_arch = "aarch64")]
    unsafe fn enable_intx(&mut self) {
        if self.msix.is_some() || self.isr_cfg.is_null() { return; }
        extern "C" { fn arch_request_irq(id: u32, handler: usize) -> bool; }
        let dev = &self._pci_dev;
        let pin = pci_read_config_8(dev.bus, dev.dev, dev.func, 0x3D);
        if pin == 0 || pin > 4 {
            crate::pci::serial_debug("[GPU] INTx: device has no interrupt pin; polling only\n");
            return;
        }
        let intid = VIRT_PCIE_INTX_BASE_INTID + ((dev.dev as u32 + (pin as u32 - 1)) % 4);
        // INTx enabled (command bit 10 clear); MSI-X stays off, so the queue
        // and config vectors in common_cfg are irrelevant.
        let cmd = pci_read_config_16(dev.bus, dev.dev, dev.func, 0x04);
        pci_write_config_16(dev.bus, dev.dev, dev.func, 0x04, cmd & !0x0400);
        // Any stale condition from before the handler existed would be
        // delivered the instant the SPI is enabled; clear it first.
        let _ = self.isr_cfg.read_volatile();
        INTX_ISR_PTR.store(self.isr_cfg as usize, core::sync::atomic::Ordering::Release);
        INTX_INTID.store(intid, core::sync::atomic::Ordering::Release);
        if !arch_request_irq(intid, virtio_gpu_intx_isr as *const () as usize) {
            INTX_INTID.store(0, core::sync::atomic::Ordering::Release);
            crate::pci::serial_debug("[GPU] INTx: GIC refused INTID ");
            crate::pci::serial_debug_hex(intid);
            crate::pci::serial_debug("; polling only\n");
            return;
        }
        self.intx = Some(intid);
        CTRLQ_IRQ_ARMED.store(true, core::sync::atomic::Ordering::Release);
        crate::pci::serial_debug("[GPU] INTx armed: slot ");
        crate::pci::serial_debug_hex(dev.dev as u32);
        crate::pci::serial_debug(" pin ");
        crate::pci::serial_debug_hex(pin as u32);
        crate::pci::serial_debug(" -> INTID ");
        crate::pci::serial_debug_hex(intid);
        crate::pci::serial_debug("\n");
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
        // negotiate the reduced size back to the device.  The in-flight table
        // (`inflight`) is sized from the result, so up to a ring's worth of
        // chains can be outstanding at once.
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

        // The cursor queue is reaped lazily on the next cursor command and
        // nobody waits on it, so ask the device not to interrupt for it: on a
        // shared INTx line every cursor completion would otherwise enter the
        // control-queue handler for nothing. Advisory per the spec; QEMU
        // honours it.
        if id == 1 {
            core::ptr::addr_of_mut!((*avail).flags).write_volatile(1); // VIRTQ_AVAIL_F_NO_INTERRUPT
        }

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

    // ---------------------------------------------------------------------
    // Control queue (queue 0): asynchronous submission
    //
    // A command is a descriptor chain — request, optional payload, response —
    // and until the host writes the chain's head into the used ring, all three
    // buffers are the device's. `Inflight` remembers them, indexed by that
    // head descriptor, so the used ring can be drained in whatever order the
    // host answers: unfenced commands complete in issue order, but a fenced
    // one (every SUBMIT_3D) is only answered once the GPU work behind it has
    // finished, and QEMU parks it aside meanwhile. Nothing here assumes the
    // next used entry is the last thing submitted.
    //
    // Two kinds of caller share one enqueue path:
    //   * `submit_async` — presents, transfers, SUBMIT_3D — returns as soon as
    //     the chain is kicked. Completion is reaped later, from the next submit
    //     or from the tick (`ctrlq_tick`), which frees the buffers, retires the
    //     fence and flags the DRM layer to signal whoever waits on it.
    //   * `submit` — anything whose reply is needed now (GET_CAPSET,
    //     RESOURCE_MAP_BLOB, CTX_CREATE, RESOURCE_CREATE_*, the teardown
    //     commands that release guest pages) — still spins, with the 100 Hz
    //     tick let through, but the spin also reaps everything else that
    //     completes meanwhile.
    //
    // The callers hold `VIRTIO_GPU` throughout, which is why `submit` cannot
    // simply sleep: yielding under that spinlock deadlocks every other GPU user
    // (see `SpinWindow`). Making the reply-needing commands sleep would require
    // the wait to happen with the lock dropped, i.e. at the call sites, and the
    // census (`ctrlq_sync_n`) says how much that would buy before it is built.
    // ---------------------------------------------------------------------

    /// Give back every page a tick-context reap could not. Task context only.
    fn drain_deferred_frees(&mut self) {
        while let Some((phys, order)) = self.deferred_free.pop() {
            mm::buddy::free(phys, order);
        }
    }

    /// Free (or defer freeing) the three buffers of a completed chain.
    fn release_buffers(&mut self, e: &Inflight, task_ctx: bool) {
        let mut bufs = [(e.req_phys, e.req_order), (e.resp_phys, e.resp_order), (e.pay_phys, e.pay_order)];
        if e.pay_phys == 0 { bufs[2].0 = 0; }
        for &(phys, order) in bufs.iter() {
            if phys == 0 { continue; }
            if task_ctx {
                mm::buddy::free(phys, order);
            } else if self.deferred_free.len() < self.deferred_free.capacity() {
                self.deferred_free.push((phys, order));
            } else {
                // Cannot happen — capacity covers every buffer of every slot —
                // but a leaked page beats an allocation in tick context.
                CTRLQ_LEAKED_PAGES.fetch_add(1 << order, core::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    /// Record that the host retired `id`. Exact, not a watermark: the floor
    /// only advances over ids that have actually completed.
    fn fence_complete(&mut self, id: u64) {
        if id == 0 || id <= self.fence_floor { return; }
        if id == self.fence_floor + 1 {
            self.fence_floor = id;
            // Fold in everything that had finished ahead of the floor.
            loop {
                let next = self.fence_floor + 1;
                match self.fences_ahead.iter().position(|&f| f == next) {
                    Some(i) => { self.fences_ahead[i] = 0; self.fence_floor = next; }
                    None => break,
                }
            }
        } else if let Some(i) = self.fences_ahead.iter().position(|&f| f == 0) {
            self.fences_ahead[i] = id;
        } else {
            // No slot: more fences ahead of the floor than the ring can carry,
            // which the accounting above makes impossible. Fall back to the
            // watermark rather than lose the retirement.
            self.fence_floor = id;
        }
        GPU_FENCE_FLOOR.store(self.fence_floor, core::sync::atomic::Ordering::Release);
    }

    /// Drain the used ring. Every completed chain is either handed to its
    /// spinning `submit` caller (`sync`: marked done, buffers kept for it) or
    /// finished here (`async`: fence retired, buffers released). Returns true
    /// if any fence retired, so the caller can notify the DRM layer.
    ///
    /// `task_ctx` says whether the buddy allocator may be entered. The tick
    /// hook passes false; it must not take a lock a preempted task may hold.
    /// No allocation happens on either path.
    fn ctrlq_reap(&mut self, task_ctx: bool) -> bool {
        let mut retired = false;
        let (size, used) = match self.queues[0].as_ref() {
            Some(q) => (q.size as usize, q.used),
            None => return false,
        };
        let stat = crate::drm_device_interface::DRM_STATS;
        loop {
            core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
            let (last, idx) = {
                let q = match self.queues[0].as_ref() { Some(q) => q, None => break };
                (q.last_used_idx, unsafe { core::ptr::addr_of!((*used).idx).read_volatile() })
            };
            if last == idx { break; }
            let head = unsafe {
                let ring = (used as usize + 4) as *const VirtqUsedElem;
                let slot = ring.add(last as usize % size);
                (slot as *const u32).read_volatile() as usize
            };
            let entry = if head < size { self.inflight[head] } else { None };
            let e = match entry {
                Some(e) => e,
                None => {
                    // A used id naming no chain of ours: the ring is out of
                    // step with the device. Resync to its index rather than
                    // free pages by a corrupt id. Reported once.
                    if !CTRLQ_CORRUPT_WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
                        crate::pci::serial_debug("[GPU] ctrlq used ring named unknown chain head=");
                        crate::pci::serial_debug_hex(head as u32);
                        crate::pci::serial_debug("; resyncing\n");
                    }
                    if let Some(q) = self.queues[0].as_mut() { q.last_used_idx = idx; }
                    break;
                }
            };
            // The response header: type @0, flags @4, fence_id @8.
            let (resp_type, resp_flags, resp_fence) = unsafe {
                let r = mm::phys_to_virt(e.resp_phys) as *const u8;
                ((r as *const u32).read_volatile(),
                 (r.add(4) as *const u32).read_volatile(),
                 (r.add(8) as *const u64).read_unaligned())
            };
            if let Some(q) = self.queues[0].as_mut() {
                q.last_used_idx = q.last_used_idx.wrapping_add(1);
            }
            if e.fence_id != 0 {
                self.fence_complete(e.fence_id);
                retired = true;
                // The one independent liveness signal for SUBMIT_3D: a reply
                // that does not echo the fence came from somewhere other than
                // the fence path, and the stream was very likely not executed.
                // Reported once per boot.
                if e.hdr_type == VirtioGpuCmd::Submit3d as u32
                    && ((resp_flags & VIRTIO_GPU_FLAG_FENCE) == 0 || resp_fence != e.fence_id)
                    && !SUBMIT3D_FENCE_ECHO_WARNED.swap(true, core::sync::atomic::Ordering::Relaxed)
                {
                    crate::pci::serial_debug("[GPU] SUBMIT_3D reply did not echo our fence: sent=");
                    crate::pci::serial_debug_hex_64(e.fence_id);
                    crate::pci::serial_debug(" got=");
                    crate::pci::serial_debug_hex_64(resp_fence);
                    crate::pci::serial_debug(" flags=");
                    crate::pci::serial_debug_hex(resp_flags);
                    crate::pci::serial_debug(" (reported once per boot)\n");
                }
            }
            if e.sync {
                // The submitter is spinning on this slot; it copies the reply
                // and releases the chain itself.
                if let Some(slot) = self.inflight.get_mut(head) {
                    if let Some(s) = slot.as_mut() { s.done = true; s.resp_type = resp_type; }
                }
                continue;
            }
            // Asynchronous: nobody reads the reply, so a refusal is only ever
            // visible here. Same as upstream, which attaches no callback to
            // these commands — but say so, bounded, because a refused present
            // is a black screen with no other symptom.
            if resp_type >= 0x1200 {
                use core::sync::atomic::Ordering::Relaxed;
                let n = CTRLQ_ASYNC_REFUSED.fetch_add(1, Relaxed);
                if n < 8 {
                    crate::pci::serial_debug("[GPU] async cmd ");
                    crate::pci::serial_debug_hex(e.hdr_type);
                    crate::pci::serial_debug(" refused by host, resp=");
                    crate::pci::serial_debug_hex(resp_type);
                    crate::pci::serial_debug("\n");
                }
            }
            if stat {
                use core::sync::atomic::Ordering::Relaxed;
                let lat = crate::snd::monotonic_us().wrapping_sub(e.submitted_us);
                CTRLQ_ASYNC_LAT_US.fetch_add(lat, Relaxed);
                CTRLQ_ASYNC_LAT_MAX_US.fetch_max(lat, Relaxed);
            }
            if let Some(q) = self.queues[0].as_mut() { unsafe { q.free_chain(head as u16); } }
            self.inflight[head] = None;
            self.release_buffers(&e, task_ctx);
        }
        retired
    }

    /// Build, record and kick one control-queue chain. The caller has already
    /// checked there is room. Returns the head descriptor and the fence id
    /// (0 if unfenced).
    fn enqueue(
        &mut self,
        head: &[u8],
        payload: &[u8],
        resp_capacity: usize,
        fenced: bool,
        sync: bool,
    ) -> Result<(u16, u64), ()> {
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
        let stat = crate::drm_device_interface::DRM_STATS;
        let submitted_us = if stat { crate::snd::monotonic_us() } else { 0 };

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

            self.inflight[head_idx as usize] = Some(Inflight {
                req_phys, req_order, pay_phys, pay_order, resp_phys, resp_order, resp_capacity,
                fence_id, hdr_type, submitted_us, sync, done: false, resp_type: 0,
            });

            let q = self.queues[0].as_mut().ok_or(())?;
            q.submit(head_idx);
            let notify_addr =
                (notify_cfg as usize + q.notify_off as usize * mult as usize) as *mut u16;
            notify_addr.write_volatile(0);
            CTRLQ_CMDS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            Ok((head_idx, fence_id))
        }
    }

    /// Make room for a chain of `need` descriptors: reap what has completed,
    /// and if the ring is still full wait — spinning with the tick let in —
    /// for the host to finish something. False after the bounded wait, which
    /// means the device has stopped answering.
    fn ensure_ctrlq_room(&mut self, need: u16) -> bool {
        self.drain_deferred_frees();
        if self.ctrlq_reap(true) { FENCE_EVENT_PENDING.store(true, core::sync::atomic::Ordering::Release); }
        if self.queues[0].as_ref().map(|q| q.num_free >= need).unwrap_or(false) { return true; }

        let stat = crate::drm_device_interface::DRM_STATS;
        let t0 = if stat { crate::snd::monotonic_us() } else { 0 };
        CTRLQ_ROOM_WAITS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        let window = SpinWindow::new();
        let wait = CtrlqWait::new(&window);
        let mut iter = 0u64;
        let mut ok = false;
        while iter < CTRLQ_WAIT_ITERS {
            if self.ctrlq_reap(true) { FENCE_EVENT_PENDING.store(true, core::sync::atomic::Ordering::Release); }
            if self.queues[0].as_ref().map(|q| q.num_free >= need).unwrap_or(false) { ok = true; break; }
            if !wait.step(&window, iter) { break; }
            iter += 1;
        }
        drop(window);
        if stat { Self::account_spin(t0); }
        if !ok {
            CTRLQ_TIMEOUTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::pci::serial_debug("[GPU] control-queue TIMEOUT waiting for ring room\n");
        }
        ok
    }

    fn account_spin(t0: u64) {
        use core::sync::atomic::Ordering::Relaxed;
        let dt = crate::snd::monotonic_us().wrapping_sub(t0);
        CTRLQ_SPIN_US.fetch_add(dt, Relaxed);
        CTRLQ_SPIN_MAX_US.fetch_max(dt, Relaxed);
    }

    /// Submit one control-queue command and return without waiting for the
    /// host. Use for commands whose reply nobody reads: presents (SET_SCANOUT,
    /// SET_SCANOUT_BLOB, RESOURCE_FLUSH), TRANSFER_TO_HOST_*, SUBMIT_3D,
    /// CTX_{ATTACH,DETACH}_RESOURCE. NOT for anything that hands guest pages
    /// back afterwards (DETACH_BACKING, RESOURCE_UNREF), reads the reply, or
    /// must be ordered against the cursor queue.
    ///
    /// With `fenced`, returns the fence id; it retires when the host answers
    /// (`fence_retired`), and the DRM layer is told through `ctrlq_tick`.
    /// Ring full → bounded wait for the host, then Err.
    fn submit_async(&mut self, head: &[u8], payload: Option<&[u8]>, fenced: bool) -> Result<u64, ()> {
        const HDR_LEN: usize = core::mem::size_of::<VirtioGpuCtrlHdr>();
        if head.len() < HDR_LEN { return Err(()); }
        let payload = payload.unwrap_or(&[]);
        let need = if payload.is_empty() { 2 } else { 3 };
        if !self.ensure_ctrlq_room(need) { return Err(()); }
        let (_, fence) = self.enqueue(head, payload, HDR_LEN, fenced, false)?;
        CTRLQ_ASYNC.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        Ok(fence)
    }

    /// Submit one control-queue command and wait for the host's reply.
    ///
    /// `head` is the command struct (always beginning with a `VirtioGpuCtrlHdr`).
    /// `payload` is optional trailing data that upstream places in a descriptor
    /// of its own rather than inline — RESOURCE_CREATE_BLOB's
    /// `virtio_gpu_mem_entry` array works this way. `resp_capacity` sizes the
    /// device-writable response buffer. None of the three buffers is capped at
    /// one page: each is a physically contiguous buddy run sized to its content.
    ///
    /// The wait is a spin with the tick let through (`SpinWindow`), bounded by
    /// `CTRLQ_WAIT_ITERS`; other chains completing meanwhile are reaped along
    /// the way. On timeout the chain is left in flight — the host may still
    /// DMA into it — but is re-tagged asynchronous, so if the device ever does
    /// answer, the descriptors and pages come back.
    fn submit(
        &mut self,
        head: &[u8],
        payload: Option<&[u8]>,
        resp_capacity: usize,
        fenced: bool,
    ) -> Result<Vec<u8>, ()> {
        const HDR_LEN: usize = core::mem::size_of::<VirtioGpuCtrlHdr>();
        if head.len() < HDR_LEN { return Err(()); }
        let payload = payload.unwrap_or(&[]);
        let resp_capacity = resp_capacity.max(HDR_LEN);
        let need = if payload.is_empty() { 2 } else { 3 };
        if !self.ensure_ctrlq_room(need) { return Err(()); }
        let hdr_type = u32::from_le_bytes(head[0..4].try_into().unwrap_or([0; 4]));
        let (head_idx, _fence) = self.enqueue(head, payload, resp_capacity, fenced, true)?;
        CTRLQ_SYNC.fetch_add(1, core::sync::atomic::Ordering::Relaxed);

        let stat = crate::drm_device_interface::DRM_STATS;
        let t0 = if stat { crate::snd::monotonic_us() } else { 0 };
        let window = SpinWindow::new();
        let wait = CtrlqWait::new(&window);
        let mut iter = 0u64;
        let mut done = false;
        while iter < CTRLQ_WAIT_ITERS {
            if self.ctrlq_reap(true) { FENCE_EVENT_PENDING.store(true, core::sync::atomic::Ordering::Release); }
            if self.inflight[head_idx as usize].map(|e| e.done).unwrap_or(false) { done = true; break; }
            if !wait.step(&window, iter) { break; }
            iter += 1;
        }
        drop(window);
        if stat {
            use core::sync::atomic::Ordering::Relaxed;
            let dt = crate::snd::monotonic_us().wrapping_sub(t0);
            CTRLQ_SPIN_US.fetch_add(dt, Relaxed);
            CTRLQ_SPIN_MAX_US.fetch_max(dt, Relaxed);
            // Name every synchronous round trip over 20 ms, with the calling
            // task and the age of the last input event.
            if dt > 20_000 {
                let now = crate::snd::monotonic_us();
                mm::gap2::s("[SUBMIT] cmd="); mm::gap2::h(hdr_type as usize);
                mm::gap2::kv(" dt_us=", dt as usize);
                mm::gap2::kv(" pid=", sched::current_pid() as usize);
                mm::gap2::kv(" inp_age_us=", now.wrapping_sub(evdev_server::last_push_us()) as usize);
                mm::gap2::kv(" t_us=", now as usize);
                mm::gap2::nl();
            }
        }

        if !done {
            CTRLQ_TIMEOUTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            crate::pci::serial_debug("[GPU] control-queue TIMEOUT, cmd=");
            crate::pci::serial_debug_hex(hdr_type);
            crate::pci::serial_debug("\n");
            // Leave the chain to the reaper: the host may still DMA into it.
            if let Some(e) = self.inflight[head_idx as usize].as_mut() { e.sync = false; }
            return Err(());
        }

        let e = match self.inflight[head_idx as usize].take() {
            Some(e) => e,
            None => return Err(()),
        };
        let out = unsafe {
            let resp_virt = mm::phys_to_virt(e.resp_phys) as *const u8;
            core::slice::from_raw_parts(resp_virt, e.resp_capacity).to_vec()
        };
        if let Some(q) = self.queues[0].as_mut() { unsafe { q.free_chain(head_idx); } }
        self.release_buffers(&e, true);
        Ok(out)
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

    /// Fire-and-forget counterpart of `send_command_raw`: true once the chain
    /// is kicked. A host refusal surfaces in `ctrlq_reap`'s log, not here.
    fn send_command_async(&mut self, cmd_data: &[u8]) -> bool {
        self.submit_async(cmd_data, None, false).is_ok()
    }

    /// The last command of a present (RESOURCE_FLUSH, SET_SCANOUT_BLOB),
    /// fenced, with the fence id published in `LAST_PRESENT_FENCE` so the DRM
    /// layer can hang the flip-complete event on the host actually having
    /// shown the frame (`drm_device_interface::queue_flip_event`) rather
    /// than on the next tick.
    fn send_present_async(&mut self, cmd_data: &[u8]) -> bool {
        match self.submit_async(cmd_data, None, true) {
            Ok(f) => { LAST_PRESENT_FENCE.store(f, core::sync::atomic::Ordering::Release); true }
            Err(()) => false,
        }
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
        // Back-compat shim for callers with only 2D geometry to offer.
        self.create_resource_3d_full(resource_id, 2, format, 1, width, height, 1, 1, 0, 0, 0)
    }

    /// RESOURCE_CREATE_3D with the caller's **actual** pipe parameters.
    ///
    /// The old entry point hardcoded `target=PIPE_TEXTURE_2D`, `bind=RENDER_TARGET`,
    /// `depth=1`, `array_size=1` and dropped `last_level`/`nr_samples`/`flags`
    /// on the floor. virglrenderer builds the host-side resource from exactly
    /// these fields, so a Mesa allocation asking for anything else — a
    /// non-render-target bind, a mip chain, an array texture — got a host
    /// resource that did not match the guest's idea of it.
    #[allow(clippy::too_many_arguments)]
    pub fn create_resource_3d_full(
        &mut self, resource_id: u32, target: u32, format: u32, bind: u32,
        width: u32, height: u32, depth: u32, array_size: u32,
        last_level: u32, nr_samples: u32, flags: u32,
    ) -> bool {
        let cmd = VirtioGpuResourceCreate3d {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::ResourceCreate3d as u32,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            resource_id,
            target,
            format,
            bind,
            width, height, depth, array_size,
            last_level, nr_samples, flags, padding: 0,
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
        self.send_command_async(data)
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
        
        if !self.send_command_async(transfer_data) { return false; }
        
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
        
        self.send_present_async(flush_data)
    }

    /// SET_SCANOUT_BLOB: point scanout 0 at a **blob** resource, described the
    /// way the guest's ADDFB2 described it (format, stride, offset).
    ///
    /// This is how a buffer that lives in host memory is displayed at all.
    /// `set_scanout` + `flush` assume a 2D resource with guest backing the
    /// device can TRANSFER_TO_HOST_2D out of; a HOST3D blob — every image
    /// Venus allocates, hence every GBM buffer Zink renders — has no guest
    /// pages, so there is nothing to transfer and nothing to CPU-copy into the
    /// console's resource 1. Upstream's `virtio_gpu_primary_plane_update`
    /// makes exactly this split: `host3d_blob || guest_blob` → SET_SCANOUT_BLOB,
    /// otherwise SET_SCANOUT. On the host QEMU exports the blob as a dmabuf and
    /// hands it to the display backend (`dpy_gl_scanout_dmabuf`), which is the
    /// zero-copy path a GPU-rendered desktop is supposed to take.
    ///
    /// `struct virtio_gpu_set_scanout_blob { hdr; rect r; le32 scanout_id;
    /// le32 resource_id; le32 width; le32 height; le32 format; le32 padding;
    /// le32 strides[4]; le32 offsets[4]; }` — `width`/`height` are the
    /// framebuffer's, `r` is the visible rectangle. Single-plane only here,
    /// which is every format this KMS advertises.
    pub fn set_scanout_blob(
        &mut self,
        resource_id: u32,
        width: u32,
        height: u32,
        format: u32,
        stride: u32,
        offset: u32,
    ) -> bool {
        if !self.has_feature(VIRTIO_GPU_F_RESOURCE_BLOB) {
            crate::pci::serial_debug("[GPU] set_scanout_blob refused: no RESOURCE_BLOB\n");
            return false;
        }
        #[repr(C, packed)]
        struct SetScanoutBlob {
            hdr: VirtioGpuCtrlHdr,
            r: VirtioGpuRect,
            scanout_id: u32,
            resource_id: u32,
            width: u32,
            height: u32,
            format: u32,
            padding: u32,
            strides: [u32; 4],
            offsets: [u32; 4],
        }
        let cmd = SetScanoutBlob {
            hdr: self.hdr_for(VirtioGpuCmd::SetScanoutBlob, 0),
            r: VirtioGpuRect { x: 0, y: 0, width, height },
            scanout_id: 0,
            resource_id,
            width,
            height,
            format,
            padding: 0,
            strides: [stride, 0, 0, 0],
            offsets: [offset, 0, 0, 0],
        };
        let data = unsafe {
            core::slice::from_raw_parts(
                &cmd as *const _ as *const u8,
                core::mem::size_of::<SetScanoutBlob>(),
            )
        };
        let ok = self.send_present_async(data);
        if ok {
            // Same bookkeeping `flush` keeps for a 2D scanout, so the console's
            // next `flush(1, ..)` knows it has to re-point the scanout at
            // resource 1 rather than assuming it still owns it.
            self.scanout_w = width;
            self.scanout_h = height;
            self.current_resource_id = resource_id;
        }
        ok
    }

    /// RESOURCE_FLUSH alone — no TRANSFER_TO_HOST_2D, no scanout switch. For a
    /// blob resource the host already holds the pixels; this only tells it
    /// which rectangle of the scanout to repaint.
    pub fn resource_flush(&mut self, resource_id: u32, x: u32, y: u32, width: u32, height: u32) -> bool {
        let flush = VirtioGpuResourceFlush {
            hdr: VirtioGpuCtrlHdr {
                type_: VirtioGpuCmd::ResourceFlush as u32,
                flags: 0, fence_id: 0, ctx_id: 0, padding: 0,
            },
            r: VirtioGpuRect { x, y, width, height },
            resource_id,
            padding: 0,
        };
        let data = unsafe {
            core::slice::from_raw_parts(
                &flush as *const _ as *const u8,
                core::mem::size_of::<VirtioGpuResourceFlush>(),
            )
        };
        self.send_present_async(data)
    }

    /// The scanout resource currently bound on this device (0 = none yet).
    pub fn current_scanout(&self) -> u32 {
        self.current_resource_id
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
        
        self.send_command_async(transfer_data)
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
        if !self.send_command_async(transfer_data) { return false; }
        
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
        
        self.send_command_async(flush_data)
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
    /// Has the host retired fence `id`? Exact (see `fence_floor`); reads only
    /// device state, so it says nothing about a fence still in flight beyond
    /// "not yet" — reap first if the answer must be fresh.
    pub fn fence_retired(&self, id: u64) -> bool {
        id != 0 && (id <= self.fence_floor || self.fences_ahead.iter().any(|&f| f == id))
    }

    /// `fence_retired` after draining the used ring, for a waiter that needs
    /// the current truth rather than the last reap's. Task context.
    pub fn fence_retired_now(&mut self, id: u64) -> bool {
        if self.fence_retired(id) { return true; }
        self.drain_deferred_frees();
        if self.ctrlq_reap(true) { FENCE_EVENT_PENDING.store(true, core::sync::atomic::Ordering::Release); }
        self.fence_retired(id)
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
        // Gate on what a context of THIS capset actually needs, not on the
        // Venus superset. Classic virgl (capset 1/2) needs neither host-visible
        // blob resources nor, for the default context, CONTEXT_INIT — and
        // `virtio-vga-gl` offers exactly VIRGL + CONTEXT_INIT with
        // RESOURCE_BLOB=0. Gating all of 3D on `venus_available()` refused
        // every context on that device, so a host advertising working virgl
        // (SUPPORTED_CAPSET_IDs = 0b110) could never be used at all.
        // Blob remains gated where it belongs, in `resource_create_blob`.
        if !self.has_feature(VIRTIO_GPU_F_VIRGL) {
            crate::pci::serial_debug("[GPU] ctx_create refused: host lacks VIRTIO_GPU_F_VIRGL\n");
            return Err(());
        }
        // context_init carries the capset selector; without the feature the
        // host ignores the field and hands back its default (virgl) context,
        // so asking for a *specific* capset is the only case that needs it.
        if capset_id != 0 && !self.has_feature(VIRTIO_GPU_F_CONTEXT_INIT) {
            crate::pci::serial_debug("[GPU] ctx_create refused: capset requested but no CONTEXT_INIT\n");
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
        // Asynchronous: virglrenderer registers a blob at RESOURCE_CREATE_BLOB
        // time and this is bookkeeping ordered behind it on the same queue.
        self.submit_async(bytes, None, false).is_ok()
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
        // SUBMIT_3D carries an opaque, context-type-specific command stream —
        // Venus protocol for a capset-4 context, virgl commands for a virgl
        // one. The command is the same either way and needs only VIRGL; the
        // blob/context-init requirements belong to Venus's *resources*, not to
        // submission. Gating here on `venus_available()` let a virgl context be
        // created and then refused at its first draw, which surfaces as Mesa's
        // "got error from kernel - expect bad rendering" and a compositor that
        // dies with "Backend initialized without output".
        if !self.has_feature(VIRTIO_GPU_F_VIRGL) {
            crate::pci::serial_debug("[GPU] submit_3d refused: host lacks VIRTIO_GPU_F_VIRGL\n");
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
        // Asynchronous: the Venus reply travels through the shared-memory ring,
        // not this response, and the fence is what a waiter consults — so the
        // caller has nothing to wait for here. The fence-echo liveness check
        // that used to run on the reply now runs in `ctrlq_reap`.
        self.submit_async(bytes, Some(cmds), true)
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

pub static VIRTIO_GPU: sched::lockwatch::TrackedMutex<Option<VirtioGpuDevice>> =
    sched::lockwatch::TrackedMutex::new(sched::lockwatch::L_VIRTIO_GPU, None);

/// Whether the host negotiated `VIRTIO_GPU_F_VIRGL`, readable without taking
/// [`VIRTIO_GPU`].
///
/// Exists for `DRM_IOCTL_VERSION`, which must answer with a *different driver
/// name* depending on it: Mesa's DRI loader picks the gallium driver by that
/// string, so `virtio_gpu` routes it to virgl and anything else falls through
/// to the software backend. Reporting `virtio_gpu` unconditionally would send
/// GBM/EGL down the virgl path on a plain `virtio-vga` with no 3D at all,
/// breaking the software desktop that is otherwise fine — so the identity has
/// to track the device we actually got.
pub static VIRGL_NEGOTIATED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// True when this guest's virtio-gpu negotiated virgl, i.e. host 3D is real.
pub fn virgl_negotiated() -> bool {
    VIRGL_NEGOTIATED.load(core::sync::atomic::Ordering::Relaxed)
}

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
