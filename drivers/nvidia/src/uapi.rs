//! The nouveau DRM UAPI on top of NVIDIA's resource manager: what lets an
//! unmodified Mesa NVK (`libvulkan_nouveau.so`) drive the GPU OpenRM brought
//! up. Registered with the DRM layer through `drivers::gpu_uapi` once
//! `rm_init_adapter` succeeded (unless `nvrm.uapi=0`); from then on
//! /dev/dri/card0 and renderD128 say `nouveau` 1.4.0 and every nouveau ioctl
//! lands here.
//!
//! THE MODEL (artifacts/notes/lane-nvuapi-2026-10-08.md has the long form):
//!
//! * One RM client per DRM open (`Open`): NV01_ROOT -> NV01_DEVICE_0 ->
//!   NV20_SUBDEVICE_0, created on the open's first ioctl, freed (with every
//!   object under it) when the last fd closes.
//! * VM_INIT creates the open's FERMI_VASPACE_A. The VA space is managed by
//!   userspace (NVK's own heap); the kernel-managed window VM_INIT names
//!   ([2^39, 2^40) for NVK) is where this layer puts its own per-channel
//!   buffers, at fixed addresses. Binds are fixed-address RM objects:
//!   `uapi_vm` decides what a MAP/UNMAP/SPARSE request means, `RmVm` turns it
//!   into NV50_MEMORY_VIRTUAL + NV04_MAP_MEMORY_DMA.
//! * GEM_NEW -> NV01_MEMORY_SYSTEM (anything CPU-mappable: physically
//!   contiguous, cached, GPU-snooped, mmap'd through its physical address) or
//!   NV01_MEMORY_LOCAL_USER (VRAM, not CPU-mappable).
//! * CHANNEL_ALLOC -> an AMPERE_CHANNEL_GPFIFO_A on the graphics runlist with
//!   sysmem GPFIFO/USERD, a context-DMA error notifier and a fence semaphore;
//!   NVIF NEW then instantiates the engine classes on it (3D, compute, copy,
//!   2D, inline-to-memory).
//! * EXEC writes one GPFIFO entry per push, then a kernel segment that
//!   releases the channel's 64-bit fence semaphore (wait-for-idle, SYS
//!   membar, non-stall interrupt), and rings the doorbell. The fence id is
//!   (channel context, sequence); `fence_done` reads the semaphore, lock-free.
//! * Waits on fences are the DRM layer's (`wait_fences`, syncobj WAIT):
//!   woken by the non-stall interrupt, polled every scheduler tick as a
//!   backstop.
//!
//! Every ioctl logs at `nvrm.uapi_log` >= 1 (errors always); a watchdog
//! thread kills a channel that stops making progress (`nvrm.uapi_timeout`,
//! default 10 s) or whose RM error notifier fires, so a hung submission ends
//! as VK_ERROR_DEVICE_LOST instead of a hung machine.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{fence, AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use drivers::gpu_uapi::{UapiBackend, UapiPrime, EXT_FENCE};
use drivers::pci::PciFunction;
use drivers::DriverError;
use sched::ksync::KMutex;

use crate::client::*;
use core::result::Result;
use crate::{nvlog, nvscreen};
use crate::params::*;
use crate::uapi_abi::*;
use crate::uapi_vm::{Backing, VmOps, VmState};

// ------------------------------------------------------------- errnos --

const ENOENT: i32 = 2;
const EINTR: i32 = 4;
const EIO: i32 = 5;
const ENOMEM: i32 = 12;
const EBUSY: i32 = 16;
const EEXIST: i32 = 17;
const ENODEV: i32 = 19;
const EINVAL: i32 = 22;
const ENOSPC: i32 = 28;
const ENOSYS: i32 = 38;
const ETIME: i32 = 62;

fn err(e: i32) -> DriverError {
    DriverError::Errno(e)
}

fn rm_errno(e: RmError) -> i32 {
    match e.0 {
        crate::NV_ERR_NO_MEMORY => ENOMEM,
        crate::NV_ERR_INVALID_ARGUMENT | crate::NV_ERR_INVALID_ADDRESS => EINVAL,
        crate::NV_ERR_NOT_SUPPORTED => ENOSYS,
        crate::NV_ERR_OBJECT_NOT_FOUND => ENOENT,
        _ => EIO,
    }
}

// ------------------------------------------------------------ options --

/// `nvrm.uapi_log`: 0 errors + summaries, 1 every ioctl (default), 2 every
/// ioctl with its arguments, push and bind detail.
static LOG: AtomicU32 = AtomicU32::new(1);
/// Lines logged so far; past `LOG_BUDGET` level-1 lines are sampled.
static LOG_LINES: AtomicU64 = AtomicU64::new(0);
const LOG_BUDGET: u64 = 4000;
/// `nvrm.uapi_timeout`: seconds a channel may make no progress.
static TIMEOUT_S: AtomicU64 = AtomicU64::new(10);

fn log_on(level: u32) -> bool {
    let l = LOG.load(Ordering::Relaxed);
    if l < level {
        return false;
    }
    let n = LOG_LINES.fetch_add(1, Ordering::Relaxed);
    l >= 2 || n < LOG_BUDGET || n % 100 == 0
}

macro_rules! ulog {
    ($lvl:expr, $($t:tt)*) => {
        if log_on($lvl) { nvlog!("[nvuapi] +{}ms {}\n", crate::ms(), format_args!($($t)*)); }
    };
}
macro_rules! uerr {
    ($($t:tt)*) => {
        { ERRORS.fetch_add(1, Ordering::Relaxed); nvlog!("[nvuapi] +{}ms ERROR {}\n", crate::ms(), format_args!($($t)*)); }
    };
}
/// `uerr!` for the few errors the person at the PC's monitor must see (a
/// channel killed): the same line, through the console sink (UART + klog +
/// framebuffer) instead of the UART-only one. Process context only.
macro_rules! uerr_screen {
    ($($t:tt)*) => {
        { ERRORS.fetch_add(1, Ordering::Relaxed); nvscreen!("[nvuapi] +{}ms ERROR {}\n", crate::ms(), format_args!($($t)*)); }
    };
}

static ERRORS: AtomicU64 = AtomicU64::new(0);
static IOCTLS: AtomicU64 = AtomicU64::new(0);
static EXECS: AtomicU64 = AtomicU64::new(0);
static BINDS: AtomicU64 = AtomicU64::new(0);

// ------------------------------------------------------- device facts --

/// What GETPARAM, NVIF and sysfs answer, gathered once at init.
struct Facts {
    vendor: u16,
    device: u16,
    subvendor: u16,
    subdevice: u16,
    revision: u8,
    class: u32,
    bus: u8,
    dev: u8,
    func: u8,
    chipset: u32,
    family: u8,
    name: String,
    chip: String,
    vram: u64,
    bar1: u64,
    gpcs: u32,
    tpcs: u32,
    devinst: u32,
    /// Engine classes a channel offers (SCLASS answer): copy, 2D, 3D, M2MF, compute.
    engines: Vec<u32>,
    channel_class: u32,
}

static FACTS: spin::Mutex<Option<Facts>> = spin::Mutex::new(None);

/// The global "keeper" RM client: device-wide queries (VRAM used, PTIMER),
/// the doorbell (VOLTA_USERMODE_A), and the duplicates that keep an exported
/// buffer alive after its creator's client is gone.
struct Keeper {
    c: Client,
    dev: Handle,
    sub: Handle,
}

static KEEPER: KMutex<Option<Keeper>> = KMutex::new(None);
/// Kernel VA of the doorbell page (NVC361_NOTIFY_CHANNEL_PENDING).
static DOORBELL: AtomicUsize = AtomicUsize::new(0);

// -------------------------------------------------- channel fence slots --

const SLOTS: usize = 64;
const SEQ_BITS: u32 = 40;
const SEQ_MASK: u64 = (1 << SEQ_BITS) - 1;

/// Lock-free view of one channel, for `fence_done` (any context) and the
/// watchdog.
struct Slot {
    /// Fence context id (never reused; 0 = free).
    ctx: AtomicU64,
    /// Kernel VA of the u64 fence semaphore.
    sem: AtomicUsize,
    /// Kernel VA of the error notifier array (NvNotification[0] is RM's
    /// error slot; the work-submit token goes to `TOKEN_NOTIF_INDEX`).
    notifier: AtomicUsize,
    /// The channel's work-submit token, to recognise a token notification
    /// that landed in the error slot anyway.
    token: AtomicU32,
    dead: AtomicBool,
    /// Highest sequence submitted.
    submitted: AtomicU64,
    /// Watchdog: last sequence seen and when it changed.
    seen: AtomicU64,
    progress_ns: AtomicU64,
    /// (open_id << 32) | channel id, for the watchdog's kill.
    owner: AtomicU64,
    kill: AtomicBool,
}

impl Slot {
    const fn new() -> Slot {
        Slot {
            ctx: AtomicU64::new(0),
            sem: AtomicUsize::new(0),
            notifier: AtomicUsize::new(0),
            token: AtomicU32::new(0),
            dead: AtomicBool::new(false),
            submitted: AtomicU64::new(0),
            seen: AtomicU64::new(0),
            progress_ns: AtomicU64::new(0),
            owner: AtomicU64::new(0),
            kill: AtomicBool::new(false),
        }
    }

    fn sem_value(&self) -> u64 {
        let p = self.sem.load(Ordering::Acquire);
        if p == 0 { u64::MAX } else { unsafe { ptr::read_volatile(p as *const u64) } }
    }
}

static SLOT_TABLE: [Slot; SLOTS] = [const { Slot::new() }; SLOTS];
static NEXT_CTX: AtomicU64 = AtomicU64::new(1);

fn fence_id(ctx: u64, seq: u64) -> u64 {
    EXT_FENCE | (ctx << SEQ_BITS) | (seq & SEQ_MASK)
}

/// `UapiBackend::fence_done`: lock-free; true once the channel's semaphore
/// reached the sequence, or the channel is dead or gone.
fn fence_done(f: u64) -> bool {
    let ctx = (f & !EXT_FENCE) >> SEQ_BITS;
    let seq = f & SEQ_MASK;
    for s in SLOT_TABLE.iter() {
        if s.ctx.load(Ordering::Acquire) != ctx {
            continue;
        }
        if s.dead.load(Ordering::Acquire) {
            return true;
        }
        let v = s.sem_value();
        // Re-check: the slot may have been recycled while we read.
        if s.ctx.load(Ordering::Acquire) != ctx {
            return true;
        }
        return v >= seq;
    }
    true
}

fn slot_alloc(ctx: u64) -> Option<usize> {
    for (i, s) in SLOT_TABLE.iter().enumerate() {
        if s.ctx.compare_exchange(0, ctx, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
            s.dead.store(false, Ordering::Release);
            s.kill.store(false, Ordering::Release);
            s.submitted.store(0, Ordering::Release);
            s.seen.store(0, Ordering::Release);
            s.progress_ns.store(sched::monotonic_ns(), Ordering::Release);
            return Some(i);
        }
    }
    None
}

fn slot_free(i: usize) {
    let s = &SLOT_TABLE[i];
    s.sem.store(0, Ordering::Release);
    s.notifier.store(0, Ordering::Release);
    s.token.store(0, Ordering::Release);
    s.owner.store(0, Ordering::Release);
    s.ctx.store(0, Ordering::Release);
    drivers::drm_device_interface::gpu_fence_irq_hint();
}

// --------------------------------------------------------- the state --

/// A kernel-owned sysmem buffer of a channel, mapped for the CPU and (if
/// `va != 0`) into the open's VA space.
#[derive(Default)]
struct KBuf {
    mem: Handle,
    virt: Handle,
    va: u64,
    cpu: usize,
    size: u64,
}

const GP_ENTRIES: u32 = 4096;
/// EXEC_PUSH_MAX: pushes per EXEC (the fence segment takes one more entry).
const PUSH_MAX: u32 = 1023;
const KSEG_BYTES: usize = 64;
const KPUSH_BYTES: u64 = 64 * 1024;
const KSEGS: u64 = KPUSH_BYTES / KSEG_BYTES as u64;

struct Chan {
    id: u32,
    slot: usize,
    ctx: u64,
    h: Handle,
    /// Explicit channel group, when the channel had to be created inside one.
    tsg: Handle,
    ctxdma: Handle,
    gpfifo: KBuf,
    userd: KBuf,
    kpush: KBuf,
    sem: KBuf,
    notif: KBuf,
    token: u32,
    gp_put: u32,
    seq: u64,
    /// NVIF engine objects on this channel: (NVK token, RM handle, class).
    objs: Vec<(u64, Handle, u32)>,
}

impl Chan {
    fn userd_u32(&self, off: usize) -> u32 {
        unsafe { ptr::read_volatile((self.userd.cpu + off) as *const u32) }
    }
}

/// One mapping in RM terms.
struct RmMap {
    /// The fixed-address NV50_MEMORY_VIRTUAL of a mapping outside a sparse
    /// region (0 inside one: the region's range is used).
    virt: Handle,
    mem: Handle,
    va: u64,
}

struct RmSparse {
    virt: Handle,
}

struct Open {
    id: u32,
    c: Client,
    dev: Handle,
    sub: Handle,
    vas: Handle,
    vm: Option<VmState<RmMap, RmSparse>>,
    /// Bump allocator in the kernel-managed window.
    kva_next: u64,
    kva_end: u64,
    nvif_devices: Vec<u64>,
    chans: BTreeMap<u32, Chan>,
    next_chan: u32,
    /// GEM handle -> buffer object id.
    handles: BTreeMap<u32, u32>,
    /// Buffer object id -> RM memory handle in this client.
    mem: BTreeMap<u32, Handle>,
}

/// A buffer object (shared across opens by PRIME).
struct Bo {
    size: u64,
    sysmem: bool,
    /// Physical address of a sysmem buffer (its mmap token); 0 for VRAM.
    phys: u64,
    domain: u32,
    tile_mode: u32,
    tile_flags: u32,
    /// GEM handles naming it, over all opens.
    handles: u32,
    /// Exported fds (VFS dmabuf slots) holding it.
    exports: u32,
    /// Duplicate in the keeper client (made on first export), 0 if none.
    keeper_mem: Handle,
}

struct State {
    opens: BTreeMap<u32, Open>,
    bos: BTreeMap<u32, Bo>,
}

static STATE: KMutex<State> = KMutex::new(State { opens: BTreeMap::new(), bos: BTreeMap::new() });
static NEXT_HANDLE: AtomicU32 = AtomicU32::new(HANDLE_BASE + 1);
/// GEM handles of this layer live in [HANDLE_BASE, HANDLE_BASE + 2^30):
/// disjoint from the dumb/blob handles the DRM layer hands out for KMS.
const HANDLE_BASE: u32 = 0x4000_0000;

fn is_our_handle(h: u32) -> bool {
    h & 0xC000_0000 == HANDLE_BASE
}

/// mmap tokens (physical base -> size) of sysmem buffers, a spin-locked leaf:
/// the token is resolved from inside the DRM device lock, where nothing may
/// sleep.
static MMAP_TOKENS: spin::Mutex<BTreeMap<u64, u64>> = spin::Mutex::new(BTreeMap::new());

// ---------------------------------------------------------- user copy --

fn read_user<T: Copy + Default>(addr: u64) -> T {
    unsafe { ptr::read_unaligned(addr as *const T) }
}

fn write_user<T: Copy>(addr: u64, v: T) {
    unsafe { ptr::write_unaligned(addr as *mut T, v) }
}

fn read_array<T: Copy + Default>(addr: u64, n: u32, max: u32) -> Result<Vec<T>, DriverError> {
    if n == 0 {
        return Ok(Vec::new());
    }
    if n > max || addr == 0 {
        return Err(err(EINVAL));
    }
    let mut v = Vec::with_capacity(n as usize);
    for i in 0..n as u64 {
        v.push(read_user::<T>(addr + i * core::mem::size_of::<T>() as u64));
    }
    Ok(v)
}

fn cstr(b: &[u8]) -> &str {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    core::str::from_utf8(&b[..n]).unwrap_or("?")
}

// ------------------------------------------------------- open lifecycle --

impl Open {
    fn new(id: u32) -> Result<Open, i32> {
        let devinst = FACTS.lock().as_ref().map(|f| f.devinst).ok_or(ENODEV)?;
        let c = Client::new().map_err(|e| {
            uerr!("open {}: NV01_ROOT: {:?}", id, e);
            rm_errno(e)
        })?;
        let mut p = Nv0080AllocParameters { deviceId: devinst, hClientShare: c.handle(), ..Default::default() };
        let dev = c.alloc(c.handle(), NV01_DEVICE_0, &mut p).map_err(|e| {
            uerr!("open {}: NV01_DEVICE_0: {:?}", id, e);
            rm_errno(e)
        })?;
        let mut p = Nv2080AllocParameters { subDeviceId: 0 };
        let sub = c.alloc(dev, NV20_SUBDEVICE_0, &mut p).map_err(|e| {
            uerr!("open {}: NV20_SUBDEVICE_0: {:?}", id, e);
            rm_errno(e)
        })?;
        ulog!(1, "open {}: RM client {:#x}, device {:#x}, subdevice {:#x}", id, c.handle(), dev, sub);
        Ok(Open {
            id, c, dev, sub, vas: 0, vm: None, kva_next: 0, kva_end: 0, nvif_devices: Vec::new(),
            chans: BTreeMap::new(), next_chan: 1, handles: BTreeMap::new(), mem: BTreeMap::new(),
        })
    }

    /// Allocate, CPU-map and (if `gpu`) GPU-map a kernel sysmem buffer in the
    /// kernel-managed VA window.
    fn kbuf(&mut self, size: u64, gpu: bool) -> Result<KBuf, i32> {
        let attrs = MemAttrs { contiguous: true, ..Default::default() };
        let mem = self.c.alloc_memory(self.dev, self.sub, MemLocation::Sysmem, size, attrs).map_err(|e| {
            uerr!("open {}: kernel sysmem buffer ({} B): {:?}", self.id, size, e);
            rm_errno(e)
        })?;
        let mut b = KBuf { mem, size, ..Default::default() };
        let r = (|| -> Result<(), i32> {
            let cpu = self.c.map_cpu(self.dev, mem, 0, size, OS33_FLAGS_ACCESS_READ_WRITE).map_err(rm_errno)?;
            b.cpu = cpu as usize;
            unsafe { ptr::write_bytes(cpu, 0, size as usize) };
            if gpu {
                let va = self.kva_next;
                let len = (size + 0xffff) & !0xffff;
                if self.kva_end == 0 || va + len > self.kva_end {
                    uerr!("open {}: kernel VA window exhausted (or VM_INIT missing)", self.id);
                    return Err(ENOSPC);
                }
                let mut p = NvMemoryAllocationParams {
                    owner: HEAP_OWNER_RM_KERNEL_CLIENT,
                    r#type: NVOS32_TYPE_IMAGE,
                    flags: NVOS32_ALLOC_FLAGS_VIRTUAL | NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE | NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE,
                    attr: OS32_ATTR_PAGE_SIZE_4KB,
                    size: len,
                    alignment: 0x1000,
                    offset: va,
                    hVASpace: self.vas,
                    ..Default::default()
                };
                let virt = self.c.alloc(self.dev, NV50_MEMORY_VIRTUAL, &mut p).map_err(|e| {
                    uerr!("open {}: kernel VA {:#x}+{:#x}: {:?}", self.id, va, len, e);
                    rm_errno(e)
                })?;
                b.virt = virt;
                let got = self.c.map_dma(self.dev, virt, mem, 0, size, MapFlags::default()).map_err(|e| {
                    uerr!("open {}: map kernel buffer at {:#x}: {:?}", self.id, va, e);
                    rm_errno(e)
                })?;
                b.va = got;
                self.kva_next = va + len;
            }
            Ok(())
        })();
        if let Err(e) = r {
            self.free_kbuf(&mut b);
            return Err(e);
        }
        Ok(b)
    }

    fn free_kbuf(&self, b: &mut KBuf) {
        if b.va != 0 {
            let _ = self.c.unmap_dma(self.dev, b.virt, b.mem, b.va);
        }
        if b.virt != 0 {
            let _ = self.c.free(self.dev, b.virt);
        }
        if b.cpu != 0 {
            let _ = self.c.unmap_cpu(self.dev, b.mem, b.cpu as *mut u8);
        }
        if b.mem != 0 {
            let _ = self.c.free(self.dev, b.mem);
        }
        *b = KBuf::default();
    }

    fn chan_destroy(&mut self, mut ch: Chan) {
        SLOT_TABLE[ch.slot].dead.store(true, Ordering::Release);
        for &(_, h, _) in ch.objs.iter().rev() {
            if h != 0 {
                let _ = self.c.free(ch.h, h);
            }
        }
        if ch.h != 0 {
            let parent = if ch.tsg != 0 { ch.tsg } else { self.dev };
            if let Err(e) = self.c.free(parent, ch.h) {
                uerr!("open {}: free channel {}: {:?}", self.id, ch.id, e);
            }
        }
        if ch.tsg != 0 {
            let _ = self.c.free(self.dev, ch.tsg);
        }
        if ch.ctxdma != 0 {
            let _ = self.c.free(self.dev, ch.ctxdma);
        }
        for b in [&mut ch.gpfifo, &mut ch.userd, &mut ch.kpush, &mut ch.sem, &mut ch.notif] {
            let mut t = core::mem::take(b);
            self.free_kbuf(&mut t);
        }
        slot_free(ch.slot);
    }
}

fn open_mut<'a>(st: &'a mut State, open_id: u32) -> Result<&'a mut Open, DriverError> {
    if open_id == 0 {
        return Err(err(EINVAL));
    }
    if !st.opens.contains_key(&open_id) {
        let o = Open::new(open_id).map_err(err)?;
        st.opens.insert(open_id, o);
    }
    Ok(st.opens.get_mut(&open_id).expect("open"))
}

// --------------------------------------------------------- RM VM ops --

/// PTE kind -> NV50_MEMORY_VIRTUAL type/attr, after X547's nvkmd_nvrm (the
/// depth/stencil kinds RM chooses from these attributes on Turing+; every
/// other kind, colour included, is GENERIC_MEMORY).
fn kind_attrs(kind: u8) -> (u32, u32) {
    match kind {
        0x1 => (NVOS32_TYPE_DEPTH, OS32_ATTR_DEPTH_16 | OS32_ATTR_FORMAT_PITCH | OS32_ATTR_Z_TYPE_FIXED | OS32_ATTR_ZS_PACKING_Z16),
        0x2 => (NVOS32_TYPE_STENCIL, OS32_ATTR_DEPTH_8 | OS32_ATTR_FORMAT_PITCH | OS32_ATTR_Z_TYPE_FIXED | OS32_ATTR_ZS_PACKING_S8),
        0x3 => (NVOS32_TYPE_STENCIL, OS32_ATTR_DEPTH_32 | OS32_ATTR_FORMAT_BLOCK_LINEAR | OS32_ATTR_Z_TYPE_FIXED | OS32_ATTR_ZS_PACKING_S8Z24),
        0x4 => (NVOS32_TYPE_DEPTH, OS32_ATTR_DEPTH_64 | OS32_ATTR_FORMAT_BLOCK_LINEAR | OS32_ATTR_Z_TYPE_FLOAT | OS32_ATTR_ZS_PACKING_Z32_X24S8),
        0x5 => (NVOS32_TYPE_STENCIL, OS32_ATTR_DEPTH_32 | OS32_ATTR_FORMAT_BLOCK_LINEAR | OS32_ATTR_Z_TYPE_FIXED | OS32_ATTR_ZS_PACKING_Z24S8),
        _ => (NVOS32_TYPE_IMAGE, 0),
    }
}

static KIND_WARNED: AtomicU32 = AtomicU32::new(0);

struct RmVm<'a> {
    c: &'a Client,
    dev: Handle,
    vas: Handle,
    mem: &'a BTreeMap<u32, Handle>,
    sysmem: &'a BTreeMap<u32, bool>,
    open: u32,
}

impl VmOps for RmVm<'_> {
    type Map = RmMap;
    type Sparse = RmSparse;

    fn map(&mut self, start: u64, len: u64, b: Backing, sparse: Option<&RmSparse>) -> Result<RmMap, i32> {
        let mem = *self.mem.get(&b.bo).ok_or(ENOENT)?;
        let snoop = if *self.sysmem.get(&b.bo).unwrap_or(&false) { OS46_FLAGS_CACHE_SNOOP_ENABLE } else { OS46_FLAGS_CACHE_SNOOP_DISABLE };
        let flags = MapFlags(snoop | OS46_FLAGS_PAGE_SIZE_4KB | OS46_FLAGS_PAGE_KIND_VIRTUAL);
        if let Some(s) = sparse {
            let va = self.c.map_dma_at(self.dev, s.virt, mem, b.offset, len, MapFlags(flags.0 | OS46_FLAGS_DMA_OFFSET_FIXED_TRUE), start)
                .map_err(|e| {
                    uerr!("open {}: map {:#x}+{:#x} (bo {} +{:#x}) into sparse range: {:?}", self.open, start, len, b.bo, b.offset, e);
                    rm_errno(e)
                })?;
            return Ok(RmMap { virt: 0, mem, va });
        }
        let (ty, attr) = kind_attrs(b.kind);
        if b.kind > 6 && KIND_WARNED.fetch_add(1, Ordering::Relaxed) < 8 {
            nvlog!("[nvuapi] +{}ms WARNING pte kind {:#x} (compressible?) mapped as GENERIC_MEMORY\n", crate::ms(), b.kind);
        }
        let mut p = NvMemoryAllocationParams {
            owner: HEAP_OWNER_RM_KERNEL_CLIENT,
            r#type: ty,
            flags: NVOS32_ALLOC_FLAGS_VIRTUAL | NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE | NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE,
            attr: attr | OS32_ATTR_PAGE_SIZE_4KB,
            size: len,
            alignment: 0x1000,
            offset: start,
            hVASpace: self.vas,
            ..Default::default()
        };
        let virt = self.c.alloc(self.dev, NV50_MEMORY_VIRTUAL, &mut p).map_err(|e| {
            uerr!("open {}: VA range {:#x}+{:#x} kind {:#x}: {:?}", self.open, start, len, b.kind, e);
            rm_errno(e)
        })?;
        if b.kind >= 1 && b.kind <= 5 && p.format != b.kind as u32 && KIND_WARNED.fetch_add(1, Ordering::Relaxed) < 8 {
            nvlog!("[nvuapi] +{}ms WARNING pte kind {:#x} requested, RM chose {:#x}\n", crate::ms(), b.kind, p.format);
        }
        match self.c.map_dma(self.dev, virt, mem, b.offset, len, flags) {
            Ok(va) => {
                if va != start {
                    uerr!("open {}: RM mapped {:#x} at {:#x}", self.open, start, va);
                }
                Ok(RmMap { virt, mem, va })
            }
            Err(e) => {
                uerr!("open {}: map {:#x}+{:#x} (bo {} +{:#x} kind {:#x}): {:?}", self.open, start, len, b.bo, b.offset, b.kind, e);
                let _ = self.c.free(self.dev, virt);
                Err(rm_errno(e))
            }
        }
    }

    fn unmap(&mut self, m: RmMap, sparse: Option<&RmSparse>) {
        let range = if m.virt != 0 { m.virt } else { sparse.map_or(0, |s| s.virt) };
        if let Err(e) = self.c.unmap_dma(self.dev, range, m.mem, m.va) {
            ulog!(1, "open {}: unmap {:#x}: {:?} (buffer already freed?)", self.open, m.va, e);
        }
        if m.virt != 0 {
            let _ = self.c.free(self.dev, m.virt);
        }
    }

    fn sparse_new(&mut self, start: u64, len: u64) -> Result<RmSparse, i32> {
        let mut p = NvMemoryAllocationParams {
            owner: HEAP_OWNER_RM_KERNEL_CLIENT,
            r#type: NVOS32_TYPE_IMAGE,
            flags: NVOS32_ALLOC_FLAGS_VIRTUAL | NVOS32_ALLOC_FLAGS_FIXED_ADDRESS_ALLOCATE | NVOS32_ALLOC_FLAGS_ALIGNMENT_FORCE | NVOS32_ALLOC_FLAGS_SPARSE,
            attr: OS32_ATTR_PAGE_SIZE_4KB,
            size: len,
            alignment: 0x1000,
            offset: start,
            hVASpace: self.vas,
            ..Default::default()
        };
        let virt = self.c.alloc(self.dev, NV50_MEMORY_VIRTUAL, &mut p).map_err(|e| {
            uerr!("open {}: sparse range {:#x}+{:#x}: {:?}", self.open, start, len, e);
            rm_errno(e)
        })?;
        Ok(RmSparse { virt })
    }

    fn sparse_free(&mut self, s: RmSparse) {
        let _ = self.c.free(self.dev, s.virt);
    }
}

/// Run `f` on the open's VM bookkeeping with an RM-backed `VmOps`.
fn with_vm<R>(st: &mut State, open_id: u32, f: impl FnOnce(&mut VmState<RmMap, RmSparse>, &mut RmVm) -> R) -> Result<R, DriverError> {
    let sysmem: BTreeMap<u32, bool> = st.bos.iter().map(|(k, b)| (*k, b.sysmem)).collect();
    let o = st.opens.get_mut(&open_id).ok_or(err(EINVAL))?;
    let mut vm = o.vm.take().ok_or(err(EINVAL))?;
    let r = {
        let mut ops = RmVm { c: &o.c, dev: o.dev, vas: o.vas, mem: &o.mem, sysmem: &sysmem, open: open_id };
        f(&mut vm, &mut ops)
    };
    o.vm = Some(vm);
    Ok(r)
}

// ------------------------------------------------------------- ioctls --

/// `UapiBackend::ioctl`.
fn ioctl(cmd: u32, arg: usize, open_id: u32) -> Option<Result<usize, DriverError>> {
    let nr = cmd & 0xff;
    let size = (cmd >> 16) & 0x3fff;
    let arg = arg as u64;
    let need = |n: usize| -> Result<(), DriverError> {
        if arg == 0 || (size as usize) < n { Err(err(EINVAL)) } else { Ok(()) }
    };
    IOCTLS.fetch_add(1, Ordering::Relaxed);
    let r = match nr {
        NR_GETPARAM => need(16).and_then(|_| getparam(arg)),
        NR_SETPARAM => Err(err(EINVAL)),
        NR_CHANNEL_ALLOC => need(88).and_then(|_| channel_alloc(arg, open_id)),
        NR_CHANNEL_FREE => need(4).and_then(|_| channel_free(arg, open_id)),
        NR_NVIF => nvif(arg, size, open_id),
        NR_VM_INIT => need(16).and_then(|_| vm_init(arg, open_id)),
        NR_VM_BIND => need(40).and_then(|_| vm_bind(arg, open_id)),
        NR_EXEC => need(40).and_then(|_| exec(arg, open_id)),
        NR_GEM_NEW => need(48).and_then(|_| gem_new(arg, open_id)),
        NR_GEM_PUSHBUF => need(64).and_then(|_| gem_pushbuf(arg, open_id)),
        NR_GEM_CPU_PREP => need(8).and_then(|_| cpu_prep(arg, open_id)),
        NR_GEM_CPU_FINI => Ok(0),
        NR_GEM_INFO => need(40).and_then(|_| gem_info(arg, open_id)),
        0x40..=0x84 => {
            ulog!(1, "open {}: unsupported nouveau ioctl nr {:#x} size {}", open_id, nr, size);
            Err(err(ENOSYS))
        }
        _ => return None,
    };
    if let Err(DriverError::Errno(e)) = r {
        ulog!(1, "open {}: ioctl nr {:#x} -> -{}", open_id, nr, e);
    }
    Some(r)
}

fn getparam(arg: u64) -> Result<usize, DriverError> {
    let mut g: Getparam = read_user(arg);
    let v = {
        let f = FACTS.lock();
        let f = f.as_ref().ok_or(err(ENODEV))?;
        match g.param {
            GETPARAM_PCI_VENDOR => Some(f.vendor as u64),
            GETPARAM_PCI_DEVICE => Some(f.device as u64),
            GETPARAM_BUS_TYPE => Some(2), // PCIE
            GETPARAM_FB_SIZE => Some(f.vram),
            GETPARAM_AGP_SIZE => Some((mm::buddy::free_pages() as u64) << 12),
            GETPARAM_CHIPSET_ID => Some(f.chipset as u64),
            GETPARAM_VM_VRAM_BASE => Some(0),
            GETPARAM_GRAPH_UNITS => Some(f.gpcs as u64 | (f.tpcs as u64) << 8),
            GETPARAM_HAS_BO_USAGE | GETPARAM_HAS_PAGEFLIP => Some(1),
            GETPARAM_EXEC_PUSH_MAX => Some(PUSH_MAX as u64),
            GETPARAM_VRAM_BAR_SIZE => Some(f.bar1),
            // GEM_NEW keeps the tile_mode / PTE kind it is given and GEM_INFO
            // returns them (Linux 6.10's meaning of this flag). Two things
            // follow in Mesa 25.3: NVK advertises
            // VK_EXT_image_drm_format_modifier (dedicated allocations then
            // carry the image's kind), and the loader's
            // nouveau_zink_predicate() picks zink for GL on chipset >= 0x160
            // with no NOUVEAU_USE_ZINK / MESA_LOADER_DRIVER_OVERRIDE needed.
            // The GPU-side kind still comes from each VM_BIND (an image binds
            // its own VA with its own kind), so the stored value is metadata
            // for importers and for scanout.
            GETPARAM_HAS_VMA_TILEMODE => Some(1),
            _ => None,
        }
    };
    let v = match (v, g.param) {
        (Some(v), _) => v,
        (None, GETPARAM_PTIMER_TIME) => ptimer().unwrap_or_else(|| sched::monotonic_ns()),
        (None, GETPARAM_VRAM_USED) => vram_used().ok_or(err(EIO))?,
        _ => {
            ulog!(1, "GETPARAM {}: unknown", g.param);
            return Err(err(EINVAL));
        }
    };
    g.value = v;
    write_user(arg, g);
    ulog!(2, "GETPARAM {} = {:#x}", g.param, v);
    Ok(0)
}

fn ptimer() -> Option<u64> {
    let k = KEEPER.lock();
    let k = k.as_ref()?;
    let mut p = Nv2080CtrlTimerGetTimeParams::default();
    k.c.control(k.sub, NV2080_CTRL_CMD_TIMER_GET_TIME, &mut p).ok()?;
    Some(p.time_nsec)
}

fn vram_used() -> Option<u64> {
    let k = KEEPER.lock();
    let k = k.as_ref()?;
    let mut fb = zeroed::<Nv2080CtrlFbGetInfoV2Params>();
    fb.fbInfoListSize = 2;
    fb.fbInfoList[0].index = NV2080_CTRL_FB_INFO_INDEX_TOTAL_RAM_SIZE;
    fb.fbInfoList[1].index = NV2080_CTRL_FB_INFO_INDEX_HEAP_FREE;
    k.c.control(k.sub, NV2080_CTRL_CMD_FB_GET_INFO_V2, &mut *fb).ok()?;
    let total = fb.fbInfoList[0].data as u64 * 1024;
    let free = fb.fbInfoList[1].data as u64 * 1024;
    Some(total.saturating_sub(free).max(4096))
}

fn vm_init(arg: u64, open_id: u32) -> Result<usize, DriverError> {
    let v: VmInit = read_user(arg);
    let mut st = STATE.lock();
    let o = open_mut(&mut st, open_id)?;
    if o.vas != 0 {
        return Err(err(EEXIST));
    }
    let kend = v.kernel_managed_addr.checked_add(v.kernel_managed_size).ok_or(err(EINVAL))?;
    if v.kernel_managed_size < (64 << 20) || v.kernel_managed_addr % 0x10000 != 0 || kend > (1 << 48) {
        ulog!(1, "open {}: VM_INIT kernel window {:#x}+{:#x} refused", open_id, v.kernel_managed_addr, v.kernel_managed_size);
        return Err(err(EINVAL));
    }
    let mut p = NvVaspaceAllocationParameters { index: NV_VASPACE_ALLOCATION_INDEX_GPU_NEW, ..Default::default() };
    let vas = o.c.alloc(o.dev, FERMI_VASPACE_A, &mut p).map_err(|e| {
        uerr!("open {}: FERMI_VASPACE_A: {:?}", open_id, e);
        err(rm_errno(e))
    })?;
    o.vas = vas;
    let lo = p.vaBase.max(0x1000);
    let hi = (p.vaBase + p.vaSize).min(1 << 48);
    o.vm = Some(VmState::new(lo, hi, v.kernel_managed_addr, kend));
    o.kva_next = v.kernel_managed_addr;
    o.kva_end = kend;
    ulog!(1, "open {}: VM_INIT: FERMI_VASPACE_A {:#x} base {:#x} size {:#x}; user binds {:#x}..{:#x}, kernel window {:#x}..{:#x}",
          open_id, vas, p.vaBase, p.vaSize, lo, hi, v.kernel_managed_addr, kend);
    Ok(0)
}

fn gem_new(arg: u64, open_id: u32) -> Result<usize, DriverError> {
    let mut g: GemNew = read_user(arg);
    let size = g.info.size;
    let dom = g.info.domain;
    if size == 0 || size > (16u64 << 30) {
        return Err(err(EINVAL));
    }
    let size = (size + 0xfff) & !0xfff;
    // Placement: anything the CPU may map lives in system memory (cached,
    // snooped, mmap'd by physical address); only unmappable VRAM requests
    // get VRAM. See the lane notes for why BAR1 is not used.
    let sysmem = dom & GEM_DOMAIN_MAPPABLE != 0 || (dom & GEM_DOMAIN_GART != 0 && dom & GEM_DOMAIN_VRAM == 0);
    if dom & (GEM_DOMAIN_VRAM | GEM_DOMAIN_GART) == 0 {
        return Err(err(EINVAL));
    }
    let mut st = STATE.lock();
    let o = open_mut(&mut st, open_id)?;
    let (mem, phys) = if sysmem {
        let attrs = MemAttrs { contiguous: true, ..Default::default() };
        let mem = o.c.alloc_memory(o.dev, o.sub, MemLocation::Sysmem, size, attrs).map_err(|e| {
            uerr!("open {}: GEM_NEW sysmem {} KiB: {:?}", open_id, size >> 10, e);
            err(rm_errno(e))
        })?;
        let cpu = match o.c.map_cpu(o.dev, mem, 0, size, OS33_FLAGS_ACCESS_READ_WRITE) {
            Ok(p) => p as usize,
            Err(e) => {
                uerr!("open {}: GEM_NEW map_cpu: {:?}", open_id, e);
                let _ = o.c.free(o.dev, mem);
                return Err(err(rm_errno(e)));
            }
        };
        if mm::kva::in_window(cpu) {
            // Not the direct map: no single physical base to hand to mmap.
            uerr!("open {}: GEM_NEW sysmem {} KiB is not physically contiguous", open_id, size >> 10);
            let _ = o.c.unmap_cpu(o.dev, mem, cpu as *mut u8);
            let _ = o.c.free(o.dev, mem);
            return Err(err(ENOMEM));
        }
        let phys = mm::virt_to_phys(cpu) as u64;
        // The direct-map alias stays valid; the RM mapping is not needed.
        let _ = o.c.unmap_cpu(o.dev, mem, cpu as *mut u8);
        (mem, phys)
    } else {
        let mem = o.c.alloc_memory(o.dev, o.sub, MemLocation::Vidmem, size, MemAttrs::default()).map_err(|e| {
            uerr!("open {}: GEM_NEW vidmem {} KiB: {:?}", open_id, size >> 10, e);
            err(rm_errno(e))
        })?;
        (mem, 0)
    };
    let obj = drivers::drm_device_interface::alloc_bo_obj();
    let handle = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    o.handles.insert(handle, obj);
    o.mem.insert(obj, mem);
    let domain = if sysmem { GEM_DOMAIN_GART | (dom & GEM_DOMAIN_MAPPABLE) | GEM_DOMAIN_COHERENT } else { GEM_DOMAIN_VRAM };
    st.bos.insert(obj, Bo {
        size, sysmem, phys, domain, tile_mode: g.info.tile_mode, tile_flags: g.info.tile_flags,
        handles: 1, exports: 0, keeper_mem: 0,
    });
    drop(st);
    if sysmem {
        MMAP_TOKENS.lock().insert(phys, size);
    }
    g.info.handle = handle;
    g.info.size = size;
    g.info.offset = 0;
    g.info.map_handle = phys;
    g.info.domain = domain;
    write_user(arg, g);
    ulog!(1, "open {}: GEM_NEW {} KiB {} (asked {:#x}, align {:#x}, kind {:#x}) -> handle {:#x} obj {} map {:#x}",
          open_id, size >> 10, if sysmem { "sysmem" } else { "VRAM" }, dom, g.align, g.info.tile_flags >> 8, handle, obj, phys);
    Ok(0)
}

fn gem_info(arg: u64, open_id: u32) -> Result<usize, DriverError> {
    let mut g: GemInfo = read_user(arg);
    let st = STATE.lock();
    let o = st.opens.get(&open_id).ok_or(err(ENOENT))?;
    let obj = *o.handles.get(&g.handle).ok_or(err(ENOENT))?;
    let b = st.bos.get(&obj).ok_or(err(ENOENT))?;
    g.domain = b.domain;
    g.size = b.size;
    g.offset = 0;
    g.map_handle = b.phys;
    g.tile_mode = b.tile_mode;
    g.tile_flags = b.tile_flags;
    drop(st);
    write_user(arg, g);
    ulog!(2, "open {}: GEM_INFO {:#x}", open_id, g.handle);
    Ok(0)
}

/// Free this open's RM memory for `obj` and drop one handle reference.
fn drop_handle(st: &mut State, open_id: u32, handle: u32) -> Result<(), i32> {
    let obj = {
        let o = st.opens.get_mut(&open_id).ok_or(ENOENT)?;
        o.handles.remove(&handle).ok_or(ENOENT)?
    };
    // Its mappings in this open go first (RM would drop them with the memory).
    let _ = with_vm(st, open_id, |vm, ops| vm.forget_bo(ops, obj));
    let sysmem = st.bos.get(&obj).map_or(false, |b| b.sysmem);
    if let Some(o) = st.opens.get_mut(&open_id) {
        if let Some(mem) = o.mem.remove(&obj) {
            let parent = if sysmem { o.dev } else { o.sub };
            if let Err(e) = o.c.free(parent, mem) {
                ulog!(1, "open {}: free memory {:#x}: {:?}", open_id, mem, e);
            }
        }
    }
    bo_unref(st, obj, true);
    Ok(())
}

/// One reference (a handle if `handle`, else an export) went away.
fn bo_unref(st: &mut State, obj: u32, handle: bool) {
    let Some(b) = st.bos.get_mut(&obj) else { return };
    if handle {
        b.handles = b.handles.saturating_sub(1);
    } else {
        b.exports = b.exports.saturating_sub(1);
    }
    if b.handles == 0 && b.exports == 0 {
        let b = st.bos.remove(&obj).expect("bo");
        if b.keeper_mem != 0 {
            if let Some(k) = KEEPER.lock().as_ref() {
                let _ = k.c.free(if b.sysmem { k.dev } else { k.sub }, b.keeper_mem);
            }
        }
        if b.sysmem {
            MMAP_TOKENS.lock().remove(&b.phys);
        }
        ulog!(2, "bo {} freed", obj);
    }
}

fn gem_close(handle: u32, open_id: u32) -> Option<Result<usize, DriverError>> {
    if !is_our_handle(handle) {
        return None;
    }
    let mut st = STATE.lock();
    let r = drop_handle(&mut st, open_id, handle).map(|_| 0).map_err(err);
    drop(st);
    ulog!(1, "open {}: GEM_CLOSE {:#x} -> {:?}", open_id, handle, r.as_ref().map_err(|_| ()));
    Some(r)
}

fn cpu_prep(arg: u64, open_id: u32) -> Result<usize, DriverError> {
    let p: GemCpuPrep = read_user(arg);
    let fences: Vec<u64> = {
        let st = STATE.lock();
        let o = st.opens.get(&open_id).ok_or(err(ENOENT))?;
        if !o.handles.contains_key(&p.handle) {
            return Err(err(ENOENT));
        }
        o.chans.values().filter(|c| c.seq > 0).map(|c| fence_id(c.ctx, c.seq)).collect()
    };
    let busy = fences.iter().any(|&f| !fence_done(f));
    if !busy {
        return Ok(0);
    }
    if p.flags & GEM_CPU_PREP_NOWAIT != 0 {
        return Err(err(EBUSY));
    }
    match drivers::drm_device_interface::wait_fences(&fences, sched::monotonic_ns() + 30_000_000_000) {
        Ok(()) => Ok(0),
        Err(DriverError::Io) => Err(err(EINTR)),
        Err(_) => Err(err(EBUSY)),
    }
}

// ----------------------------------------------------------- channels --

fn channel_alloc(arg: u64, open_id: u32) -> Result<usize, DriverError> {
    let mut a: ChannelAlloc = read_user(arg);
    if a.fb_ctxdma_handle == !0 && a.tt_ctxdma_handle != 1 {
        // nouveau's engine-select hack; only the graphics runlist is offered.
        ulog!(1, "open {}: CHANNEL_ALLOC for engine {:#x} refused (graphics only)", open_id, a.tt_ctxdma_handle);
        return Err(err(ENOSYS));
    }
    let channel_class = FACTS.lock().as_ref().map(|f| f.channel_class).ok_or(err(ENODEV))?;
    let mut st = STATE.lock();
    let o = open_mut(&mut st, open_id)?;
    if o.vas == 0 {
        ulog!(1, "open {}: CHANNEL_ALLOC before VM_INIT (legacy nouveau ABI) refused", open_id);
        return Err(err(EINVAL));
    }
    let t0 = sched::monotonic_ns();
    let ctx = NEXT_CTX.fetch_add(1, Ordering::Relaxed);
    let slot = slot_alloc(ctx).ok_or_else(|| {
        uerr!("open {}: CHANNEL_ALLOC: all {} channel slots in use", open_id, SLOTS);
        err(ENOSPC)
    })?;
    let id = o.next_chan;
    o.next_chan += 1;
    let mut ch = Chan {
        id, slot, ctx, h: 0, tsg: 0, ctxdma: 0, gpfifo: KBuf::default(), userd: KBuf::default(), kpush: KBuf::default(),
        sem: KBuf::default(), notif: KBuf::default(), token: 0, gp_put: 0, seq: 0, objs: Vec::new(),
    };
    let mut tok_slot = None;
    let r = (|| -> Result<(), i32> {
        ch.gpfifo = o.kbuf(GP_ENTRIES as u64 * 8, true)?;
        ch.userd = o.kbuf(4096, false)?;
        ch.kpush = o.kbuf(KPUSH_BYTES, true)?;
        ch.sem = o.kbuf(4096, true)?;
        ch.notif = o.kbuf(4096, false)?;
        let mut cd = NvContextDmaAllocationParams {
            hSubDevice: 0,
            flags: OS03_FLAGS_MAPPING_KERNEL | OS03_FLAGS_HASH_TABLE_DISABLE,
            hMemory: ch.notif.mem,
            offset: 0,
            limit: ch.notif.size - 1,
        };
        ch.ctxdma = o.c.alloc(o.dev, NV01_CONTEXT_DMA, &mut cd).map_err(|e| {
            uerr!("open {}: channel {}: error notifier context DMA: {:?}", open_id, id, e);
            rm_errno(e)
        })?;
        let mut cp = zeroed::<NvChannelAllocParams>();
        cp.hObjectError = ch.ctxdma;
        cp.gpFifoOffset = ch.gpfifo.va;
        cp.gpFifoEntries = GP_ENTRIES;
        cp.hVASpace = o.vas;
        cp.hUserdMemory[0] = ch.userd.mem;
        cp.userdOffset[0] = 0;
        cp.engineType = NV2080_ENGINE_TYPE_GRAPHICS;
        // Under the device, as nvidia-haiku's NVK backend does (RM then
        // makes the channel group itself); if RM refuses that, inside an
        // explicit graphics channel group, as the copy-engine self-test does.
        ch.h = match o.c.alloc(o.dev, channel_class, &mut *cp) {
            Ok(h) => h,
            Err(e) => {
                ulog!(1, "open {}: channel {}: class {:#x} under the device: {:?}; retrying in a channel group", open_id, id, channel_class, e);
                let mut tp = NvChannelGroupAllocationParameters { hVASpace: o.vas, engineType: NV2080_ENGINE_TYPE_GRAPHICS, ..Default::default() };
                ch.tsg = o.c.alloc(o.dev, KEPLER_CHANNEL_GROUP_A, &mut tp).map_err(|e| {
                    uerr!("open {}: channel {}: KEPLER_CHANNEL_GROUP_A (graphics): {:?}", open_id, id, e);
                    rm_errno(e)
                })?;
                o.c.alloc(ch.tsg, channel_class, &mut *cp).map_err(|e| {
                    uerr!("open {}: channel {}: class {:#x} (GPFIFO {:#x}, USERD {:#x}): {:?}", open_id, id, channel_class, cp.gpFifoOffset, ch.userd.mem, e);
                    rm_errno(e)
                })?
            }
        };
        let mut bind = Nva06fCtrlBindParams { engineType: NV2080_ENGINE_TYPE_GRAPHICS };
        if let Err(e) = o.c.control(ch.h, NVA06F_CTRL_CMD_BIND, &mut bind) {
            ulog!(1, "open {}: channel {}: NVA06F BIND: {:?} (continuing)", open_id, id, e);
        }
        let mut sch = Nva06fCtrlGpfifoScheduleParams { bEnable: 1, bSkipSubmit: 0 };
        if let Err(e) = o.c.control(ch.h, NVA06F_CTRL_CMD_GPFIFO_SCHEDULE, &mut sch) {
            if ch.tsg == 0 {
                uerr!("open {}: channel {}: GPFIFO_SCHEDULE: {:?}", open_id, id, e);
                return Err(rm_errno(e));
            }
            // In an explicit group the group is what gets scheduled (as the
            // copy-engine self-test found).
            let mut sch = Nva06fCtrlGpfifoScheduleParams { bEnable: 1, bSkipSubmit: 0 };
            o.c.control(ch.tsg, NVA06C_CTRL_CMD_GPFIFO_SCHEDULE, &mut sch).map_err(|e2| {
                uerr!("open {}: channel {}: GPFIFO_SCHEDULE: channel {:?}, group {:?}", open_id, id, e, e2);
                rm_errno(e2)
            })?;
        }
        // Where the token notification comes from (boot4: it lands in slot 0
        // even after SET_..._NOTIF_INDEX(3), with status 0, not RM's 0xffff):
        // snapshot the slots before the two controls, log both views below.
        let notif_pre = notif_slots(ch.notif.cpu);
        // GET_WORK_SUBMIT_TOKEN also WRITES the token into the error
        // context as a notification (kchannelNotifyWorkSubmitToken: info32 =
        // token, status = IN_PROGRESS|0x7fff = 0xffff) at notifyIndex[
        // WORK_SUBMIT_TOKEN], 1 by default. Move it past RM's three slots,
        // as nvidia-push does (RequestChidToken), so slot 0 is only ever an
        // RC error.
        let mut ni = Nvc36fCtrlGpfifoSetWorkSubmitTokenNotifIndexParams { index: TOKEN_NOTIF_INDEX };
        if let Err(e) = o.c.control(ch.h, NVC36F_CTRL_CMD_GPFIFO_SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX, &mut ni) {
            ulog!(1, "open {}: channel {}: SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX {}: {:?} (token stays at RM's slot 1)", open_id, id, TOKEN_NOTIF_INDEX, e);
        }
        let mut tok = Nvc36fCtrlGetWorkSubmitTokenParams::default();
        o.c.control(ch.h, NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN, &mut tok).map_err(|e| {
            uerr!("open {}: channel {}: GET_WORK_SUBMIT_TOKEN: {:?}", open_id, id, e);
            rm_errno(e)
        })?;
        ch.token = tok.workSubmitToken;
        let notif_post = notif_slots(ch.notif.cpu);
        let show = |sl: &[NvNotification; NOTIF_SLOTS_LOGGED]| -> String {
            let mut out = String::new();
            for (i, n) in sl.iter().enumerate() {
                out.push_str(&alloc::format!(" [{}] info32 {:#x} info16 {:#x} status {:#x}{}", i, n.info32, n.info16, n.status,
                                             if n.timeStamp != [0, 0] { " ts" } else { "" }));
            }
            out
        };
        ulog!(1, "open {}: channel {}: notifier before SET_WORK_SUBMIT_TOKEN_NOTIF_INDEX/GET_WORK_SUBMIT_TOKEN:{}; after:{}",
              open_id, id, show(&notif_pre), show(&notif_post));
        tok_slot = check_token_notification(&ch, open_id);
        Ok(())
    })();
    if let Err(e) = r {
        o.chan_destroy(ch);
        return Err(err(e));
    }
    let s = &SLOT_TABLE[slot];
    s.sem.store(ch.sem.cpu, Ordering::Release);
    s.token.store(ch.token, Ordering::Release);
    s.notifier.store(ch.notif.cpu, Ordering::Release);
    s.owner.store((open_id as u64) << 32 | id as u64, Ordering::Release);
    let (gpva, token, h) = (ch.gpfifo.va, ch.token, ch.h);
    o.chans.insert(id, ch);
    drop(st);
    a.channel = id as i32;
    a.pushbuf_domains = GEM_DOMAIN_VRAM | GEM_DOMAIN_GART;
    a.notifier_handle = 0;
    a.nr_subchan = 0;
    write_user(arg, a);
    let slot_s = match tok_slot { Some(i) => alloc::format!("notifier slot {}", i), None => String::from("no notification") };
    ulog!(1, "open {}: CHANNEL_ALLOC -> channel {} (RM {:#x}, class {:#x}, graphics runlist, GPFIFO {:#x} x{}, token {:#x} ({}), fence ctx {}) in {} ms",
          open_id, id, h, channel_class, gpva, GP_ENTRIES, token, slot_s, ctx, (sched::monotonic_ns() - t0) / 1_000_000);
    Ok(0)
}

/// Notifier slot for the work-submit token: the first one past RM's own
/// (ERROR 0, WORK_SUBMIT_TOKEN 1, KEY_ROTATION_STATUS 2).
const TOKEN_NOTIF_INDEX: u32 = NV_CHANNELGPFIFO_NOTIFICATION_TYPE__SIZE_1;
const NOTIF_SLOTS_LOGGED: usize = 4;

/// The first entries of a channel's notifier array.
fn notif_slots(cpu: usize) -> [NvNotification; NOTIF_SLOTS_LOGGED] {
    let mut out = [NvNotification::default(); NOTIF_SLOTS_LOGGED];
    for (i, n) in out.iter_mut().enumerate() {
        *n = unsafe { ptr::read_volatile((cpu as *const NvNotification).add(i)) };
    }
    out
}

/// True when slot-0 contents are the work-submit token notification
/// (info32 = token, info16 = 0) rather than an RC error (info32 = the
/// ROBUST_CHANNEL_* code, info16 = the NV2080 engine type, never 0 = NULL).
fn is_token_notification(n: &NvNotification, token: u32) -> bool {
    n.info32 == token && n.info16 == 0
}

/// After GET_WORK_SUBMIT_TOKEN: find where RM put the token notification
/// (the first PC run read it out of slot 0 as an "error" and killed every
/// channel) for the CHANNEL_ALLOC line, and if it is in the error slot
/// anyway, clear that slot — no work has been submitted yet, so it cannot
/// hold a real error.
fn check_token_notification(ch: &Chan, open_id: u32) -> Option<usize> {
    let slots = notif_slots(ch.notif.cpu);
    let at = slots.iter().position(|n| n.info32 == ch.token && (n.status != 0 || n.timeStamp != [0, 0]));
    match at {
        Some(0) => {
            nvlog!("[nvuapi] +{}ms WARNING open {}: channel {}: RM wrote the work-submit token {:#x} into the ERROR slot 0 (status {:#x}); cleared, slot 0 is watched by status only\n",
                   crate::ms(), open_id, ch.id, ch.token, slots[0].status);
            unsafe { ptr::write_volatile(ch.notif.cpu as *mut NvNotification, NvNotification::default()) };
        }
        Some(_) => {}
        None => ulog!(1, "open {}: channel {}: no token notification in slots 0..{} (slot 0: status {:#x} info32 {:#x})",
                      open_id, ch.id, NOTIF_SLOTS_LOGGED, slots[0].status, slots[0].info32),
    }
    at
}

fn channel_free(arg: u64, open_id: u32) -> Result<usize, DriverError> {
    let f: ChannelFree = read_user(arg);
    let mut st = STATE.lock();
    let o = st.opens.get_mut(&open_id).ok_or(err(ENOENT))?;
    let ch = o.chans.remove(&(f.channel as u32)).ok_or(err(ENOENT))?;
    // Let outstanding work drain (bounded) before RM tears the channel down.
    let last = fence_id(ch.ctx, ch.seq);
    let t0 = sched::monotonic_ns();
    while ch.seq > 0 && !fence_done(last) && sched::monotonic_ns() - t0 < 2_000_000_000 {
        sched::kthread::msleep(1);
    }
    o.chan_destroy(ch);
    ulog!(1, "open {}: CHANNEL_FREE {}", open_id, f.channel);
    Ok(0)
}

fn gem_pushbuf(arg: u64, open_id: u32) -> Result<usize, DriverError> {
    // Only the "is this channel dead" probe (nr_push == 0) of the legacy ABI.
    let p: GemPushbuf = read_user(arg);
    if p.nr_push != 0 || p.nr_buffers != 0 {
        return Err(err(ENOSYS));
    }
    let st = STATE.lock();
    let o = st.opens.get(&open_id).ok_or(err(ENOENT))?;
    let ch = o.chans.get(&p.channel).ok_or(err(ENOENT))?;
    if SLOT_TABLE[ch.slot].dead.load(Ordering::Acquire) { Err(err(ENODEV)) } else { Ok(0) }
}

// --------------------------------------------------------------- NVIF --

fn nvif(arg: u64, size: u32, open_id: u32) -> Result<usize, DriverError> {
    let hdr = core::mem::size_of::<NvifIoctlV0>() as u32;
    if arg == 0 || size < hdr {
        return Err(err(EINVAL));
    }
    let io: NvifIoctlV0 = read_user(arg);
    if io.version != 0 || (io.route != 0 && io.route != 0xff) {
        return Err(err(EINVAL));
    }
    let data = arg + hdr as u64;
    let argc = size - hdr;
    match io.type_ {
        NVIF_IOCTL_V0_NEW => nvif_new(&io, data, argc, open_id),
        NVIF_IOCTL_V0_DEL => nvif_del(&io, open_id),
        NVIF_IOCTL_V0_SCLASS => nvif_sclass(&io, data, argc, open_id),
        NVIF_IOCTL_V0_MTHD => nvif_mthd(&io, data, argc, open_id),
        t => {
            ulog!(1, "open {}: NVIF type {:#x} unsupported", open_id, t);
            Err(err(EINVAL))
        }
    }
}

fn nvif_new(io: &NvifIoctlV0, data: u64, argc: u32, open_id: u32) -> Result<usize, DriverError> {
    if argc < core::mem::size_of::<NvifNewV0>() as u32 {
        return Err(err(EINVAL));
    }
    let n: NvifNewV0 = read_user(data);
    if n.version != 0 {
        return Err(err(EINVAL));
    }
    let mut st = STATE.lock();
    let o = open_mut(&mut st, open_id)?;
    if io.route == 0 {
        // The device object.
        if io.object != 0 || n.oclass != NV_DEVICE {
            return Err(err(EINVAL));
        }
        o.nvif_devices.push(n.object);
        ulog!(1, "open {}: NVIF NEW device (token {:#x})", open_id, n.object);
        return Ok(0);
    }
    // An engine object on channel `io.token`.
    let chid = io.token as u32;
    let engines = FACTS.lock().as_ref().map(|f| f.engines.clone()).unwrap_or_default();
    let cls = n.oclass as u32;
    if !engines.contains(&cls) {
        ulog!(1, "open {}: NVIF NEW class {:#x} on channel {}: not offered", open_id, cls, chid);
        return Err(err(EINVAL));
    }
    let c = &o.c;
    let ch = o.chans.get_mut(&chid).ok_or(err(EINVAL))?;
    let t0 = sched::monotonic_ns();
    let h = match c.alloc_noparams(ch.h, cls) {
        Ok(h) => h,
        // NVK allocates the 2D and inline-to-memory classes on every channel
        // but drives neither on Ampere (copies and uploads go through the
        // copy and 3D classes); a refusal of those two must not fail the
        // whole device, so it is logged and answered as a success.
        Err(e) if cls == FERMI_TWOD_A || cls == KEPLER_INLINE_TO_MEMORY_B => {
            nvlog!("[nvuapi] +{}ms WARNING open {}: channel {}: class {:#x}: {:?}; answering success (NVK does not use it on Ampere)\n",
                   crate::ms(), open_id, chid, cls, e);
            0
        }
        Err(e) => {
            uerr!("open {}: channel {}: engine class {:#x}: {:?}", open_id, chid, cls, e);
            return Err(err(rm_errno(e)));
        }
    };
    ch.objs.push((n.object, h, cls));
    ulog!(1, "open {}: NVIF NEW class {:#x} on channel {} -> RM {:#x} ({} ms)", open_id, cls, chid, h,
          (sched::monotonic_ns() - t0) / 1_000_000);
    Ok(0)
}

fn nvif_del(io: &NvifIoctlV0, open_id: u32) -> Result<usize, DriverError> {
    let mut st = STATE.lock();
    let Some(o) = st.opens.get_mut(&open_id) else { return Ok(0) };
    if let Some(i) = o.nvif_devices.iter().position(|&t| t == io.object) {
        o.nvif_devices.remove(i);
        return Ok(0);
    }
    for ch in o.chans.values_mut() {
        if let Some(i) = ch.objs.iter().position(|e| e.0 == io.object) {
            let (_, h, cls) = ch.objs.remove(i);
            if h != 0 {
                let _ = o.c.free(ch.h, h);
            }
            ulog!(1, "open {}: NVIF DEL class {:#x} on channel {}", open_id, cls, ch.id);
            return Ok(0);
        }
    }
    Ok(0)
}

fn nvif_sclass(io: &NvifIoctlV0, data: u64, argc: u32, open_id: u32) -> Result<usize, DriverError> {
    let hs = core::mem::size_of::<NvifSclassV0>() as u32;
    if io.route == 0 || argc < hs {
        return Err(err(EINVAL));
    }
    let mut s: NvifSclassV0 = read_user(data);
    if argc != hs + s.count as u32 * 8 {
        return Err(err(EINVAL));
    }
    {
        let st = STATE.lock();
        let o = st.opens.get(&open_id).ok_or(err(EINVAL))?;
        o.chans.get(&(io.token as u32)).ok_or(err(EINVAL))?;
    }
    let engines = FACTS.lock().as_ref().map(|f| f.engines.clone()).unwrap_or_default();
    for (i, &cls) in engines.iter().enumerate().take(s.count as usize) {
        write_user(data + hs as u64 + i as u64 * 8, NvifSclassOclassV0 { oclass: cls as i32, minver: -1, maxver: -1 });
    }
    s.count = engines.len() as u8;
    write_user(data, s);
    ulog!(1, "open {}: NVIF SCLASS channel {}: {:x?}", open_id, io.token, engines);
    Ok(0)
}

fn nvif_mthd(io: &NvifIoctlV0, data: u64, argc: u32, open_id: u32) -> Result<usize, DriverError> {
    let hs = core::mem::size_of::<NvifMthdV0>() as u32;
    if io.route != 0 || argc != hs + core::mem::size_of::<NvDeviceInfoV0>() as u32 {
        return Err(err(EINVAL));
    }
    let m: NvifMthdV0 = read_user(data);
    if m.method != NV_DEVICE_V0_INFO {
        return Err(err(EINVAL));
    }
    {
        let st = STATE.lock();
        let o = st.opens.get(&open_id).ok_or(err(EINVAL))?;
        if !o.nvif_devices.contains(&io.object) {
            return Err(err(EINVAL));
        }
    }
    let infop = data + hs as u64;
    let mut info: NvDeviceInfoV0 = unsafe { ptr::read_unaligned(infop as *const NvDeviceInfoV0) };
    if info.version != 0 {
        return Err(err(EINVAL));
    }
    {
        let f = FACTS.lock();
        let f = f.as_ref().ok_or(err(ENODEV))?;
        info.platform = NV_DEVICE_INFO_V0_PCIE;
        info.chipset = f.chipset as u16;
        info.revision = f.revision;
        info.family = f.family;
        info.ram_size = f.vram;
        info.ram_user = f.vram;
        info.chip = [0; 16];
        info.name = [0; 64];
        let c = f.chip.as_bytes();
        info.chip[..c.len().min(15)].copy_from_slice(&c[..c.len().min(15)]);
        let n = f.name.as_bytes();
        info.name[..n.len().min(63)].copy_from_slice(&n[..n.len().min(63)]);
    }
    unsafe { ptr::write_unaligned(infop as *mut NvDeviceInfoV0, info) };
    ulog!(1, "open {}: NVIF MTHD DEVICE_INFO: chipset {:#x} family {:#x} VRAM {} MiB '{}' '{}'", open_id, info.chipset,
          info.family, info.ram_size >> 20, cstr(&info.chip), cstr(&info.name));
    Ok(0)
}

// ------------------------------------------------------------ VM_BIND --

fn resolve_in_fences(syncs: &[Sync], open_id: u32, skip_ctx: u64) -> Result<Vec<u64>, DriverError> {
    let mut all = Vec::new();
    for s in syncs {
        let point = match s.flags & SYNC_TYPE_MASK {
            SYNC_SYNCOBJ => 0,
            SYNC_TIMELINE_SYNCOBJ => s.timeline_value,
            _ => return Err(err(EINVAL)),
        };
        let ids = drivers::drm_device_interface::syncobj_in_fences(s.handle, open_id, point).map_err(|e| match e {
            DriverError::NotFound => err(ENOENT),
            _ => err(EINVAL),
        })?;
        for f in ids {
            // Same channel: executes in order anyway.
            if skip_ctx != 0 && f & EXT_FENCE != 0 && (f & !EXT_FENCE) >> SEQ_BITS == skip_ctx {
                continue;
            }
            all.push(f);
        }
    }
    Ok(all)
}

fn check_out_syncs(syncs: &[Sync], open_id: u32) -> Result<(), DriverError> {
    for s in syncs {
        if s.flags & SYNC_TYPE_MASK > SYNC_TIMELINE_SYNCOBJ {
            return Err(err(EINVAL));
        }
        if !drivers::drm_device_interface::syncobj_exists(s.handle, open_id) {
            return Err(err(ENOENT));
        }
    }
    Ok(())
}

fn signal_out_syncs(syncs: &[Sync], open_id: u32, fence: u64) {
    for s in syncs {
        let point = if s.flags & SYNC_TYPE_MASK == SYNC_TIMELINE_SYNCOBJ { s.timeline_value } else { 0 };
        if let Err(e) = drivers::drm_device_interface::syncobj_install(s.handle, open_id, point, fence) {
            ulog!(1, "open {}: out-sync {} point {}: {:?}", open_id, s.handle, point, e);
        }
    }
}

fn wait_in(fences: &[u64], what: &str, open_id: u32) -> Result<(), DriverError> {
    if fences.is_empty() {
        return Ok(());
    }
    // The waited channels' own watchdog ends a real hang first (their fences
    // then read as done); this bound only catches a fence nobody watches.
    let cap = (TIMEOUT_S.load(Ordering::Relaxed) + 5) * 1_000_000_000;
    match drivers::drm_device_interface::wait_fences(fences, sched::monotonic_ns() + cap) {
        Ok(()) => Ok(()),
        Err(DriverError::Io) => Err(err(EINTR)),
        Err(_) => {
            uerr!("open {}: {}: in-fences {:x?} not signalled after {} s", open_id, what, fences, cap / 1_000_000_000);
            Err(err(ETIME))
        }
    }
}

fn vm_bind(arg: u64, open_id: u32) -> Result<usize, DriverError> {
    let b: VmBind = read_user(arg);
    let ops: Vec<VmBindOp> = read_array(b.op_ptr, b.op_count, 4096)?;
    let waits: Vec<Sync> = read_array(b.wait_ptr, b.wait_count, 256)?;
    let sigs: Vec<Sync> = read_array(b.sig_ptr, b.sig_count, 256)?;
    if b.flags & !VM_BIND_RUN_ASYNC != 0 {
        return Err(err(EINVAL));
    }
    if b.flags & VM_BIND_RUN_ASYNC == 0 && (!waits.is_empty() || !sigs.is_empty()) {
        return Err(err(EINVAL));
    }
    BINDS.fetch_add(1, Ordering::Relaxed);
    check_out_syncs(&sigs, open_id)?;
    // Executed synchronously: wait for the in-fences, bind, signal.
    let fences = resolve_in_fences(&waits, open_id, 0)?;
    wait_in(&fences, "VM_BIND", open_id)?;
    let mut st = STATE.lock();
    open_mut(&mut st, open_id)?;
    if st.opens.get(&open_id).map_or(true, |o| o.vm.is_none()) {
        return Err(err(EINVAL));
    }
    let mut result = Ok(0);
    for (i, op) in ops.iter().enumerate() {
        let sparse = op.flags & VM_BIND_SPARSE != 0;
        let kind = (op.flags & 0xff) as u8;
        let r: Result<(), i32> = match (op.op, sparse) {
            (VM_BIND_OP_MAP, true) => with_vm(&mut st, open_id, |vm, o| vm.op_sparse_map(o, op.addr, op.range)).map_err(|_| EINVAL).and_then(|r| r),
            (VM_BIND_OP_UNMAP, true) => with_vm(&mut st, open_id, |vm, o| vm.op_sparse_unmap(o, op.addr, op.range)).map_err(|_| EINVAL).and_then(|r| r),
            // MAP without a buffer is an unbind (NVK's sparse unbind path).
            (VM_BIND_OP_MAP, false) if op.handle == 0 => with_vm(&mut st, open_id, |vm, o| vm.op_unmap(o, op.addr, op.range)).map_err(|_| EINVAL).and_then(|r| r),
            (VM_BIND_OP_MAP, false) => {
                let obj = st.opens.get(&open_id).and_then(|o| o.handles.get(&op.handle).copied());
                match obj {
                    None => Err(ENOENT),
                    Some(obj) => {
                        let size = st.bos.get(&obj).map_or(0, |b| b.size);
                        if op.bo_offset.checked_add(op.range).map_or(true, |e| e > size) {
                            Err(EINVAL)
                        } else {
                            let back = Backing { bo: obj, offset: op.bo_offset, kind };
                            with_vm(&mut st, open_id, |vm, o| vm.op_map(o, op.addr, op.range, back)).map_err(|_| EINVAL).and_then(|r| r)
                        }
                    }
                }
            }
            (VM_BIND_OP_UNMAP, false) => with_vm(&mut st, open_id, |vm, o| vm.op_unmap(o, op.addr, op.range)).map_err(|_| EINVAL).and_then(|r| r),
            _ => Err(EINVAL),
        };
        ulog!(if r.is_ok() { 2 } else { 1 }, "open {}: VM_BIND[{}] {} {:#x}+{:#x} handle {:#x} off {:#x} kind {:#x} -> {:?}", open_id, i,
              match (op.op, sparse) { (0, false) => "MAP", (1, false) => "UNMAP", (0, true) => "MAP|SPARSE", _ => "UNMAP|SPARSE" },
              op.addr, op.range, op.handle, op.bo_offset, kind, r);
        if let Err(e) = r {
            result = Err(err(e));
            break;
        }
    }
    drop(st);
    if result.is_ok() {
        signal_out_syncs(&sigs, open_id, 0);
    }
    ulog!(1, "open {}: VM_BIND {} op(s){} waits {} sigs {} -> {}", open_id, ops.len(),
          if b.flags & VM_BIND_RUN_ASYNC != 0 { " async" } else { "" }, waits.len(), sigs.len(),
          if result.is_ok() { "ok" } else { "FAILED" });
    result
}

// --------------------------------------------------------------- EXEC --

extern "C" {
    fn lnv_push_host_fence(p: *mut u32, sem: u64, payload: u64, intr: u8) -> u32;
    fn lnv_gp_entry_sync(va: u64, bytes: u32, wait: u8) -> u64;
}

fn exec(arg: u64, open_id: u32) -> Result<usize, DriverError> {
    let e: Exec = read_user(arg);
    let mut pushes: Vec<ExecPush> = read_array(e.push_ptr, e.push_count, PUSH_MAX)?;
    let waits: Vec<Sync> = read_array(e.wait_ptr, e.wait_count, 256)?;
    let sigs: Vec<Sync> = read_array(e.sig_ptr, e.sig_count, 256)?;
    for p in &pushes {
        if p.va % 4 != 0 || p.va_len % 4 != 0 || p.va_len >= (1 << 23) {
            return Err(err(EINVAL));
        }
    }
    // Linux skips empty pushes rather than failing the submission.
    pushes.retain(|p| p.va_len != 0);
    let ctx = {
        let st = STATE.lock();
        let o = st.opens.get(&open_id).ok_or(err(ENOENT))?;
        let ch = o.chans.get(&e.channel).ok_or(err(ENOENT))?;
        if SLOT_TABLE[ch.slot].dead.load(Ordering::Acquire) {
            return Err(err(ENODEV));
        }
        ch.ctx
    };
    check_out_syncs(&sigs, open_id)?;
    let fences = resolve_in_fences(&waits, open_id, ctx)?;
    wait_in(&fences, "EXEC", open_id)?;
    if pushes.is_empty() && sigs.is_empty() {
        return Ok(0);
    }
    EXECS.fetch_add(1, Ordering::Relaxed);
    let doorbell = DOORBELL.load(Ordering::Acquire);
    let mut st = STATE.lock();
    let o = st.opens.get_mut(&open_id).ok_or(err(ENOENT))?;
    let ch = o.chans.get_mut(&e.channel).ok_or(err(ENOENT))?;
    let slot = &SLOT_TABLE[ch.slot];
    if slot.dead.load(Ordering::Acquire) {
        return Err(err(ENODEV));
    }
    // GPFIFO space: pushes + the fence segment.
    let need = pushes.len() as u32 + 1;
    let t0 = sched::monotonic_ns();
    loop {
        let get = ch.userd_u32(USERD_GP_GET) % GP_ENTRIES;
        let used = (ch.gp_put + GP_ENTRIES - get) % GP_ENTRIES;
        if GP_ENTRIES - 1 - used >= need {
            break;
        }
        if sched::monotonic_ns() - t0 > 5_000_000_000 || slot.dead.load(Ordering::Acquire) {
            uerr!("open {}: channel {}: GPFIFO full for 5 s (GP_GET {} GP_PUT {})", open_id, ch.id, get, ch.gp_put);
            return Err(err(EBUSY));
        }
        sched::kthread::msleep(1);
    }
    let seq = ch.seq + 1;
    // The kernel segment `seq` reuses the one of `seq - KSEGS`: that fence
    // must have signalled (the GPU has fetched the segment).
    if seq > KSEGS {
        let old = fence_id(ch.ctx, seq - KSEGS);
        let t0 = sched::monotonic_ns();
        while !fence_done(old) {
            if sched::monotonic_ns() - t0 > 5_000_000_000 {
                uerr!("open {}: channel {}: fence {} still pending after 5 s, kernel segment busy", open_id, ch.id, seq - KSEGS);
                return Err(err(EBUSY));
            }
            sched::kthread::msleep(1);
        }
    }
    let gp = ch.gpfifo.cpu as *mut u64;
    for p in &pushes {
        let ent = unsafe { lnv_gp_entry_sync(p.va, p.va_len, (p.flags & EXEC_PUSH_NO_PREFETCH != 0) as u8) };
        unsafe { ptr::write_volatile(gp.add(ch.gp_put as usize), ent) };
        ch.gp_put = (ch.gp_put + 1) % GP_ENTRIES;
    }
    let seg = (seq % KSEGS) as usize * KSEG_BYTES;
    let words = (ch.kpush.cpu + seg) as *mut u32;
    let n = unsafe { lnv_push_host_fence(words, ch.sem.va, seq, 1) };
    let ent = unsafe { lnv_gp_entry_sync(ch.kpush.va + seg as u64, n * 4, 0) };
    unsafe { ptr::write_volatile(gp.add(ch.gp_put as usize), ent) };
    ch.gp_put = (ch.gp_put + 1) % GP_ENTRIES;
    ch.seq = seq;
    slot.submitted.store(seq, Ordering::Release);
    fence(Ordering::SeqCst);
    unsafe { ptr::write_volatile((ch.userd.cpu + USERD_GP_PUT) as *mut u32, ch.gp_put) };
    fence(Ordering::SeqCst);
    if doorbell != 0 {
        unsafe { ptr::write_volatile((doorbell + NVC361_NOTIFY_CHANNEL_PENDING) as *mut u32, ch.token) };
    }
    let (chid, gp_put, cctx) = (ch.id, ch.gp_put, ch.ctx);
    drop(st);
    let f = fence_id(cctx, seq);
    signal_out_syncs(&sigs, open_id, f);
    if log_on(1) {
        let mut line = String::new();
        if LOG.load(Ordering::Relaxed) >= 2 {
            for p in &pushes {
                line.push_str(&alloc::format!(" {:#x}+{:#x}{}", p.va, p.va_len, if p.flags != 0 { "!" } else { "" }));
            }
        }
        nvlog!("[nvuapi] +{}ms open {}: EXEC channel {} pushes {} waits {} (gpu {}) sigs {} -> seq {} GP_PUT {}{}\n", crate::ms(),
               open_id, chid, pushes.len(), waits.len(), fences.len(), sigs.len(), seq, gp_put, line);
    }
    Ok(0)
}

// ------------------------------------------------- mmap, PRIME, close --

fn mmap(token: u64, _open_id: u32) -> Option<(u64, bool)> {
    let t = MMAP_TOKENS.lock();
    if t.contains_key(&token) { Some((token, false)) } else { None }
}

fn prime_export(handle: u32, open_id: u32) -> Option<UapiPrime> {
    if !is_our_handle(handle) {
        return None;
    }
    let mut st = STATE.lock();
    let (obj, mem) = {
        let o = st.opens.get(&open_id)?;
        let obj = *o.handles.get(&handle)?;
        (obj, *o.mem.get(&obj)?)
    };
    let owner = st.opens.get(&open_id)?.c.handle();
    let b = st.bos.get_mut(&obj)?;
    if b.keeper_mem == 0 {
        let k = KEEPER.lock();
        let k = k.as_ref()?;
        match k.c.dup(if b.sysmem { k.dev } else { k.sub }, owner, mem) {
            Ok(h) => b.keeper_mem = h,
            Err(e) => {
                uerr!("open {}: PRIME export of {:#x}: dup into the keeper client: {:?}", open_id, handle, e);
                return None;
            }
        }
    }
    b.exports += 1;
    // Sysmem buffers are physically contiguous (GEM_NEW allocates them so),
    // so the dmabuf's frames are simply `size / 4096` consecutive pages from
    // `phys`, whatever the count. VRAM has no CPU pages here (BAR1 is not
    // used for user mappings): a token-only export, which still round-trips
    // through PRIME_FD_TO_HANDLE — the path zink/GBM/EGL take — but cannot
    // be mmap'd through the fd.
    let pages = (b.size / 4096) as usize;
    let phys = if b.sysmem { b.phys as usize } else { 0 };
    ulog!(1, "open {}: PRIME export {:#x} (obj {}, {} KiB{})", open_id, handle, obj, b.size >> 10,
          if phys != 0 { ", with frames" } else { ", token only" });
    Some(UapiPrime { phys, order: 0, pages: if phys != 0 { pages } else { 0 }, len: b.size as usize, obj })
}

fn prime_import(obj: u32, open_id: u32) -> Option<u32> {
    let mut st = STATE.lock();
    let (keeper_mem, sysmem) = {
        let b = st.bos.get(&obj)?;
        (b.keeper_mem, b.sysmem)
    };
    if open_id == 0 || keeper_mem == 0 {
        return None;
    }
    let o = match open_mut(&mut st, open_id) {
        Ok(o) => o,
        Err(_) => return None,
    };
    if let Some((&h, _)) = o.handles.iter().find(|(_, &v)| v == obj) {
        return Some(h);
    }
    let kh = KEEPER.lock().as_ref()?.c.handle();
    let mem = match o.c.dup(if sysmem { o.dev } else { o.sub }, kh, keeper_mem) {
        Ok(m) => m,
        Err(e) => {
            uerr!("open {}: PRIME import of obj {}: dup: {:?}", open_id, obj, e);
            return None;
        }
    };
    let h = NEXT_HANDLE.fetch_add(1, Ordering::Relaxed);
    o.handles.insert(h, obj);
    o.mem.insert(obj, mem);
    st.bos.get_mut(&obj)?.handles += 1;
    ulog!(1, "open {}: PRIME import obj {} -> handle {:#x}", open_id, obj, h);
    Some(h)
}

fn bo_release(obj: u32) -> bool {
    let mut st = STATE.lock();
    if !st.bos.contains_key(&obj) {
        return false;
    }
    bo_unref(&mut st, obj, false);
    true
}

fn release_open(open_id: u32) {
    let mut st = STATE.lock();
    let Some(mut o) = st.opens.remove(&open_id) else { return };
    let t0 = sched::monotonic_ns();
    let nchan = o.chans.len();
    // Channels first (their fences retire as "gone"); then the bindings and
    // buffers; then the client, which frees whatever RM still holds.
    let chans: Vec<Chan> = core::mem::take(&mut o.chans).into_values().collect();
    for ch in chans {
        o.chan_destroy(ch);
    }
    let objs: Vec<u32> = o.handles.values().copied().collect();
    let nbo = objs.len();
    let mapped = o.vm.as_ref().map_or(0, |v| v.mapped_bytes());
    // NV01_FREE of the client: RM drops the mappings, the VA space and the
    // memory with it, so the bookkeeping is simply discarded.
    drop(o);
    for obj in objs {
        bo_unref(&mut st, obj, true);
    }
    drop(st);
    ulog!(1, "open {}: closed: {} channel(s), {} buffer handle(s), {} KiB still bound, released in {} ms", open_id, nchan, nbo,
          mapped >> 10, (sched::monotonic_ns() - t0) / 1_000_000);
}

// --------------------------------------------------------------- sysfs --

fn put(out: &mut [u8], s: &[u8]) -> usize {
    let n = s.len().min(out.len());
    out[..n].copy_from_slice(&s[..n]);
    n
}

fn sysfs_attr(attr: &[u8], out: &mut [u8]) -> Option<usize> {
    let f = FACTS.lock();
    let f = f.as_ref()?;
    let s = match attr {
        b"vendor" => alloc::format!("0x{:04x}\n", f.vendor),
        b"device" => alloc::format!("0x{:04x}\n", f.device),
        b"subsystem_vendor" => alloc::format!("0x{:04x}\n", f.subvendor),
        b"subsystem_device" => alloc::format!("0x{:04x}\n", f.subdevice),
        b"revision" => alloc::format!("0x{:02x}\n", f.revision),
        b"class" => alloc::format!("0x{:06x}\n", f.class),
        b"uevent" => alloc::format!(
            "DRIVER=nouveau\nPCI_CLASS={:X}\nPCI_ID={:04X}:{:04X}\nPCI_SUBSYS_ID={:04X}:{:04X}\nPCI_SLOT_NAME=0000:{:02x}:{:02x}.{}\nMODALIAS=pci:v{:08X}d{:08X}sv{:08X}sd{:08X}bc{:02X}sc{:02X}i{:02X}\n",
            f.class, f.vendor, f.device, f.subvendor, f.subdevice, f.bus, f.dev, f.func,
            f.vendor, f.device, f.subvendor, f.subdevice, f.class >> 16, (f.class >> 8) & 0xff, f.class & 0xff),
        b"config" => {
            let mut c = [0u8; 64];
            c[0..2].copy_from_slice(&f.vendor.to_le_bytes());
            c[2..4].copy_from_slice(&f.device.to_le_bytes());
            c[8] = f.revision;
            c[9] = f.class as u8;
            c[10] = (f.class >> 8) as u8;
            c[11] = (f.class >> 16) as u8;
            c[0x2c..0x2e].copy_from_slice(&f.subvendor.to_le_bytes());
            c[0x2e..0x30].copy_from_slice(&f.subdevice.to_le_bytes());
            return Some(put(out, &c));
        }
        _ => return None,
    };
    Some(put(out, s.as_bytes()))
}

// ----------------------------------------------------------- watchdog --

static WD_RUNNING: AtomicBool = AtomicBool::new(false);

fn watchdog() {
    let mut xid0 = crate::xid_lines();
    let mut reported: [u64; SLOTS] = [0; SLOTS];
    loop {
        let mut busy = false;
        let now = sched::monotonic_ns();
        let timeout = TIMEOUT_S.load(Ordering::Relaxed) * 1_000_000_000;
        for (i, s) in SLOT_TABLE.iter().enumerate() {
            let ctx = s.ctx.load(Ordering::Acquire);
            if ctx == 0 || s.dead.load(Ordering::Acquire) {
                continue;
            }
            // The RM error notifier, slot 0. An RC error is written by
            // krcErrorSetNotifier (CPU-RM) or by GSP-RM through the channel's
            // errorNotifierMem: info32 = ROBUST_CHANNEL_* code, info16 = the
            // NV2080 engine type, status = 0xffff, status written last. The
            // status is the only test (UVM's uvm_channel_get_status: nonzero;
            // nvidia-push: == 0xffff): the work-submit token notification
            // also carries a timestamp and info32, and reading those as an
            // error killed every channel on the first PC run.
            let np = s.notifier.load(Ordering::Acquire);
            if np != 0 && reported[i] != ctx {
                let n: NvNotification = unsafe { ptr::read_volatile(np as *const NvNotification) };
                if n.status != 0 && is_token_notification(&n, s.token.load(Ordering::Acquire)) {
                    // Belt and braces: the token was moved to its own slot at
                    // CHANNEL_ALLOC; if RM still put it here, it is not an
                    // error. Clear the slot so a later real error shows.
                    unsafe { ptr::write_volatile(np as *mut NvNotification, NvNotification::default()) };
                    let owner = s.owner.load(Ordering::Acquire);
                    nvlog!("[nvuapi] +{}ms WARNING channel {} of open {}: work-submit token notification (info32 {:#x}, status {:#x}) in the error slot; cleared, not an error\n",
                           crate::ms(), owner & 0xffff_ffff, owner >> 32, n.info32, n.status);
                } else if n.status != 0 {
                    reported[i] = ctx;
                    let owner = s.owner.load(Ordering::Acquire);
                    uerr_screen!("channel {} of open {}: RM error notifier fired: info32 {} = {}, engine {:#x}, status {:#x} -- channel killed (Xid lines so far {})",
                          owner & 0xffff_ffff, owner >> 32, n.info32, rc_name(n.info32), n.info16, n.status, crate::xid_lines());
                    s.dead.store(true, Ordering::Release);
                    s.kill.store(true, Ordering::Release);
                    drivers::drm_device_interface::gpu_fence_irq_hint();
                    continue;
                }
            }
            let sub = s.submitted.load(Ordering::Acquire);
            let v = s.sem_value();
            if v >= sub {
                s.progress_ns.store(now, Ordering::Relaxed);
                s.seen.store(v, Ordering::Relaxed);
                continue;
            }
            busy = true;
            if v != s.seen.load(Ordering::Relaxed) {
                s.seen.store(v, Ordering::Relaxed);
                s.progress_ns.store(now, Ordering::Relaxed);
                continue;
            }
            if now.saturating_sub(s.progress_ns.load(Ordering::Relaxed)) > timeout {
                let owner = s.owner.load(Ordering::Acquire);
                uerr_screen!("channel {} of open {}: HUNG: fence {} of {} after {} s without progress -- channel killed (Xid lines so far {})",
                      owner & 0xffff_ffff, owner >> 32, v, sub, timeout / 1_000_000_000, crate::xid_lines());
                s.dead.store(true, Ordering::Release);
                s.kill.store(true, Ordering::Release);
                drivers::drm_device_interface::gpu_fence_irq_hint();
            }
        }
        let xid = crate::xid_lines();
        if xid != xid0 {
            uerr_screen!("RM reported {} new Xid line(s) (see the NVRM lines above)", xid - xid0);
            xid0 = xid;
        }
        // Stop scheduling killed channels (RM calls: task context, under the
        // state lock so the channel cannot be freed underneath).
        if SLOT_TABLE.iter().any(|s| s.kill.load(Ordering::Acquire)) {
            let st = STATE.lock();
            for s in SLOT_TABLE.iter() {
                if !s.kill.swap(false, Ordering::AcqRel) {
                    continue;
                }
                let owner = s.owner.load(Ordering::Acquire);
                let (oid, cid) = ((owner >> 32) as u32, owner as u32);
                if let Some(o) = st.opens.get(&oid) {
                    if let Some(ch) = o.chans.get(&cid) {
                        let mut sch = Nva06fCtrlGpfifoScheduleParams { bEnable: 0, bSkipSubmit: 0 };
                        let r = o.c.control(ch.h, NVA06F_CTRL_CMD_GPFIFO_SCHEDULE, &mut sch);
                        nvlog!("[nvuapi] +{}ms channel {} of open {}: descheduled ({:?}); the next EXEC answers ENODEV\n",
                               crate::ms(), cid, oid, r.err());
                    }
                }
            }
        }
        sched::kthread::msleep(if busy { 50 } else { 250 });
    }
}

/// ROBUST_CHANNEL_* error names for the notifier's info32 (nverror.h).
fn rc_name(code: u32) -> &'static str {
    match code {
        13 => "GR_EXCEPTION",
        31 => "FIFO_ERROR_MMU_ERR_FLT (MMU fault)",
        32 => "PBDMA_ERROR",
        39..=41 | 70..=72 | 75..=77 | 85 => "CEn_ERROR (copy engine)",
        43 => "RESETCHANNEL_VERIF_ERROR",
        44 => "GR_FAULT_DURING_CTXSW",
        45 => "PREEMPTIVE_REMOVAL",
        69 => "GR_CLASS_ERROR",
        119 => "GSP_RPC_TIMEOUT",
        120 => "GSP_ERROR",
        _ => "see nverror.h",
    }
}

// ------------------------------------------------------------- backend --

static BACKEND: UapiBackend = UapiBackend {
    name: "nouveau",
    version: (1, 4, 0),
    date: "20120801",
    desc: "nVidia Riva/TNT/GeForce/Quadro/Tesla/Tegra K1+ (OpenRM translation)",
    ioctl,
    gem_close,
    mmap,
    release_open,
    prime_export,
    prime_import,
    bo_release,
    fence_done,
    sysfs_attr,
};

/// The non-stall interrupt path (device.rs ISR): wake fence waiters.
pub(crate) fn irq_hint() {
    if READY.load(Ordering::Relaxed) {
        drivers::drm_device_interface::gpu_fence_irq_hint();
    }
}

static READY: AtomicBool = AtomicBool::new(false);

/// After `rm_init_adapter` succeeded: gather the device facts, set up the
/// keeper client and the doorbell, and register the backend. `[nvrm]` thread.
pub(crate) fn init(f: PciFunction) {
    match crate::opt_str("nvrm.uapi") {
        Some("0") => {
            nvlog!("[nvuapi] nvrm.uapi=0: nouveau UAPI not registered (card0/renderD128 keep their identity)\n");
            return;
        }
        _ => {}
    }
    if let Some(v) = crate::opt_str("nvrm.uapi_log").and_then(crate::parse_num) {
        LOG.store(v as u32, Ordering::Relaxed);
    }
    if let Some(v) = crate::opt_str("nvrm.uapi_timeout").and_then(crate::parse_num) {
        TIMEOUT_S.store(v.clamp(1, 3600), Ordering::Relaxed);
    }
    let t0 = sched::monotonic_ns();
    match init_inner(f) {
        Ok(()) => {
            drivers::gpu_uapi::register(&BACKEND);
            READY.store(true, Ordering::Release);
            // M8: the KMS half (copy-engine scanout into the GOP surface).
            crate::uapi_kms::init();
            if !WD_RUNNING.swap(true, Ordering::AcqRel) && sched::kthread::spawn("nvuapi-wd", watchdog).is_none() {
                nvlog!("[nvuapi] WARNING: no watchdog thread\n");
            }
            let f = FACTS.lock();
            let f = f.as_ref().expect("facts");
            nvscreen!("[nvuapi] +{}ms ==== nouveau UAPI ready: /dev/dri/card0 + renderD128 answer 'nouveau' 1.4.0 for {} ({:04x}:{:04x}, {} chipset {:#x}), VRAM {} MiB, BAR1 {} MiB, {} GPCs / {} TPCs; engines {:x?}; log level {}, hang timeout {} s ({} ms) ====\n",
                   crate::ms(), f.name, f.vendor, f.device, f.chip, f.chipset, f.vram >> 20, f.bar1 >> 20, f.gpcs, f.tpcs,
                   f.engines, LOG.load(Ordering::Relaxed), TIMEOUT_S.load(Ordering::Relaxed), (sched::monotonic_ns() - t0) / 1_000_000);
        }
        Err(why) => {
            nvscreen!("[nvuapi] +{}ms ==== nouveau UAPI NOT available: {} ====\n", crate::ms(), why);
        }
    }
    crate::device::flush_log();
}

fn init_inner(f: PciFunction) -> Result<(), String> {
    let c = Client::new().map_err(|e| alloc::format!("NV01_ROOT: {:?}", e))?;
    let mut ids = zeroed::<Nv0000CtrlGpuGetAttachedIdsParams>();
    c.control(c.handle(), NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS, &mut *ids).map_err(|e| alloc::format!("GET_ATTACHED_IDS: {:?}", e))?;
    let id = ids.gpuIds.iter().copied().find(|&i| i != NV0000_CTRL_GPU_INVALID_ID && i != 0).ok_or("no GPU attached to RM")?;
    let mut info = Nv0000CtrlGpuGetIdInfoV2Params { gpuId: id, ..Default::default() };
    c.control(c.handle(), NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2, &mut info).map_err(|e| alloc::format!("GET_ID_INFO_V2: {:?}", e))?;
    let mut p = Nv0080AllocParameters { deviceId: info.deviceInstance, hClientShare: c.handle(), ..Default::default() };
    let dev = c.alloc(c.handle(), NV01_DEVICE_0, &mut p).map_err(|e| alloc::format!("NV01_DEVICE_0: {:?}", e))?;
    let mut p = Nv2080AllocParameters { subDeviceId: 0 };
    let sub = c.alloc(dev, NV20_SUBDEVICE_0, &mut p).map_err(|e| alloc::format!("NV20_SUBDEVICE_0: {:?}", e))?;

    let mut cl = zeroed::<Nv0080CtrlGpuGetClasslistV2Params>();
    c.control(dev, NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2, &mut *cl).map_err(|e| alloc::format!("GET_CLASSLIST_V2: {:?}", e))?;
    let classes: Vec<u32> = cl.classList[..(cl.numClasses as usize).min(cl.classList.len())].to_vec();
    let pick = |cands: &[u32]| cands.iter().copied().find(|c| classes.contains(c));
    let eng3d = pick(&[AMPERE_B, AMPERE_A]).ok_or("no Ampere 3D class (only GA10x is wired)")?;
    let compute = pick(&[AMPERE_COMPUTE_B, AMPERE_COMPUTE_A]).ok_or("no Ampere compute class")?;
    let copy = pick(&[AMPERE_DMA_COPY_B, AMPERE_DMA_COPY_A]).ok_or("no Ampere copy class")?;
    let channel_class = pick(&[AMPERE_CHANNEL_GPFIFO_A]).ok_or("no AMPERE_CHANNEL_GPFIFO_A")?;
    let mut engines = vec![copy, eng3d, compute];
    for c2 in [FERMI_TWOD_A, KEPLER_INLINE_TO_MEMORY_B] {
        if classes.contains(&c2) {
            engines.push(c2);
        }
    }
    if !classes.contains(&VOLTA_USERMODE_A) {
        return Err("no VOLTA_USERMODE_A (doorbell)".into());
    }

    let mut name = zeroed::<Nv2080CtrlGpuGetNameStringParams>();
    let _ = c.control(sub, NV2080_CTRL_CMD_GPU_GET_NAME_STRING, &mut *name);
    let mut short = zeroed::<Nv2080CtrlGpuGetShortNameStringParams>();
    let _ = c.control(sub, NV2080_CTRL_CMD_GPU_GET_SHORT_NAME_STRING, &mut *short);
    let mut arch = Nv2080CtrlMcGetArchInfoParams::default();
    c.control(sub, NV2080_CTRL_CMD_MC_GET_ARCH_INFO, &mut arch).map_err(|e| alloc::format!("MC_GET_ARCH_INFO: {:?}", e))?;
    let chipset = arch.architecture | arch.implementation;
    let family = match chipset {
        0x160..=0x16f => NV_DEVICE_INFO_V0_TURING,
        0x170..=0x17f => NV_DEVICE_INFO_V0_AMPERE,
        0x190..=0x19f => NV_DEVICE_INFO_V0_ADA,
        _ => NV_DEVICE_INFO_V0_AMPERE,
    };
    let mut fb = zeroed::<Nv2080CtrlFbGetInfoV2Params>();
    fb.fbInfoListSize = 2;
    fb.fbInfoList[0].index = NV2080_CTRL_FB_INFO_INDEX_RAM_SIZE;
    fb.fbInfoList[1].index = NV2080_CTRL_FB_INFO_INDEX_BAR1_SIZE;
    c.control(sub, NV2080_CTRL_CMD_FB_GET_INFO_V2, &mut *fb).map_err(|e| alloc::format!("FB_GET_INFO_V2: {:?}", e))?;
    let vram = fb.fbInfoList[0].data as u64 * 1024;
    let bar1 = fb.fbInfoList[1].data as u64 * 1024;
    let mut gm = Nv2080CtrlGrGetGpcMaskParams::default();
    let mut tpcs = 0;
    if c.control(sub, NV2080_CTRL_CMD_GR_GET_GPC_MASK, &mut gm).is_ok() {
        for g in 0..32 {
            if gm.gpcMask & (1 << g) != 0 {
                let mut tm = Nv2080CtrlGrGetTpcMaskParams { gpcId: g, ..Default::default() };
                if c.control(sub, NV2080_CTRL_CMD_GR_GET_TPC_MASK, &mut tm).is_ok() {
                    tpcs += tm.tpcMask.count_ones();
                }
            }
        }
    }
    // The doorbell page, shared by every channel (the work-submit token
    // names the channel).
    let usermode = c.alloc_noparams(sub, VOLTA_USERMODE_A).map_err(|e| alloc::format!("VOLTA_USERMODE_A: {:?}", e))?;
    let bell = c.map_cpu(sub, usermode, 0, NVC361_NV_USERMODE__SIZE, OS33_FLAGS_ACCESS_WRITE_ONLY)
        .map_err(|e| alloc::format!("mapping the doorbell: {:?}", e))?;
    DOORBELL.store(bell as usize, Ordering::Release);

    let facts = Facts {
        vendor: f.vendor_id(),
        device: f.device_id(),
        subvendor: f.read16(0x2C),
        subdevice: f.read16(0x2E),
        revision: f.read8(0x08),
        class: f.read32(0x08) >> 8,
        bus: f.bus,
        dev: f.dev,
        func: f.func,
        chipset,
        family,
        name: String::from(cstr_bytes(&name.gpuNameString[..NV2080_GPU_MAX_NAME_STRING_LENGTH])),
        chip: String::from(cstr_bytes(&short.gpuShortNameString)),
        vram,
        bar1,
        gpcs: gm.gpcMask.count_ones(),
        tpcs,
        devinst: info.deviceInstance,
        engines,
        channel_class,
    };
    *FACTS.lock() = Some(facts);
    *KEEPER.lock() = Some(Keeper { c, dev, sub });
    Ok(())
}

/// Counters for the stick log's summary lines.
pub(crate) fn summary() {
    if READY.load(Ordering::Relaxed) {
        nvlog!("[nvuapi] ioctls {}, EXECs {}, VM_BINDs {}, errors {}\n", IOCTLS.load(Ordering::Relaxed),
               EXECS.load(Ordering::Relaxed), BINDS.load(Ordering::Relaxed), ERRORS.load(Ordering::Relaxed));
    }
    crate::uapi_kms::summary();
}

// ------------------------------------------------- for the KMS half (M8) --

/// Is `h` a GEM handle of this layer (whatever open)?
pub(crate) fn kms_is_handle(h: u32) -> bool {
    is_our_handle(h)
}

/// The RM objects behind GEM `handle` of `open_id`, for `uapi_kms` to
/// duplicate into its own client.
pub(crate) struct KmsBo {
    /// The RM client that holds `mem` (the open's).
    pub client: Handle,
    pub mem: Handle,
    pub sysmem: bool,
    pub size: u64,
    pub tile_mode: u32,
    pub tile_flags: u32,
    pub obj: u32,
}

pub(crate) fn kms_bo(open_id: u32, handle: u32) -> Option<KmsBo> {
    let st = STATE.lock();
    let o = st.opens.get(&open_id)?;
    let obj = *o.handles.get(&handle)?;
    let mem = *o.mem.get(&obj)?;
    let b = st.bos.get(&obj)?;
    Some(KmsBo { client: o.c.handle(), mem, sysmem: b.sysmem, size: b.size, tile_mode: b.tile_mode, tile_flags: b.tile_flags, obj })
}

/// Fences of the newest submission of every channel of every open that holds
/// a handle to `obj`: what a scanout copy of it waits for (implicit sync).
pub(crate) fn kms_bo_fences(obj: u32) -> Vec<u64> {
    let st = STATE.lock();
    let mut v = Vec::new();
    for o in st.opens.values() {
        if !o.handles.values().any(|&b| b == obj) {
            continue;
        }
        for c in o.chans.values() {
            if c.seq > 0 && !fence_done(fence_id(c.ctx, c.seq)) {
                v.push(fence_id(c.ctx, c.seq));
            }
        }
    }
    v
}
