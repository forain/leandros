//! M8: a minimal KMS for the NVIDIA backend — card0 scans out by copying
//! framebuffers into the boot loader's (UEFI GOP) surface with the copy
//! engine. No modeset: the display engine keeps scanning out what the VBIOS
//! and GOP programmed, at the mode Limine reported; we only change the pixels
//! in that surface.
//!
//! THE MODEL (artifacts/notes/lane-nvkms-2026-10-08.md has the long form):
//!
//! * The DRM layer's synthetic connector/CRTC/primary plane, atomic
//!   properties, flip events, DRM master and console hand-off are reused as
//!   they are (`drivers::drm_device_interface`). This module registers the
//!   narrow `drivers::gpu_kms` seam: wrap a nouveau GEM handle in a
//!   framebuffer, present one, destroy one.
//! * One in-kernel RM client of its own: device, subdevice, a VA space, a
//!   channel group + channel on an asynchronous copy engine (class c7b5,
//!   the same setup the G3 self-test proved on the RTX 3070), its own
//!   doorbell page, and the GOP surface: RM's console memory object
//!   (NV01_MEMORY_FRAMEBUFFER_CONSOLE, VRAM offset 0 on the PC) mapped into
//!   that VA space. Kept apart from the nouveau-UAPI per-open clients: a
//!   crashing app cannot take the scanout with it.
//! * ADDFB2 of a GEM handle: the buffer's RM memory is duplicated into this
//!   client (NV04_DUP_OBJECT — the duplicate keeps it alive after the app
//!   closes it, until RMFB) and mapped into the VA space once.
//! * A present = wait (CPU) for the GPU work of every open that holds the
//!   buffer (implicit sync: smithay without IN_FENCE_FD already waits for its
//!   render, this is the safety net), then one 2D copy per damage rectangle
//!   (pitch-linear source, or block-linear de-tiled by the copy engine), a
//!   semaphore release, and a CPU wait for it. Synchronous: when the ioctl
//!   returns the frame is in the scanout surface; the DRM layer queues the
//!   flip event and its 100 Hz tick delivers it (the synthetic vblank).
//! * Verified at boot without touching the screen: the copy engine reads the
//!   first rows of the GOP surface back and they must equal what the CPU reads
//!   through BAR1. Only then is the seam registered; otherwise card0 keeps the
//!   CPU-copy KMS it always had (dumb buffers only).
//!
//! Options: `nvrm.kms=0` (do not register), `nvrm.kms_log=0|1|2`.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use core::ptr;
use core::sync::atomic::{fence, AtomicU32, AtomicU64, Ordering};

use drivers::gpu_kms::{KmsBackend, KmsFbReq};
use drivers::DriverError;
use sched::ksync::KMutex;

use crate::client::*;
use core::result::Result;
use crate::params::*;
use crate::{nvlog, nvscreen};

const EINVAL: i32 = 22;
const ENOENT: i32 = 2;
const EIO: i32 = 5;
const ENOMEM: i32 = 12;
const ENODEV: i32 = 19;

fn err(e: i32) -> DriverError {
    DriverError::Errno(e)
}

static LOG: AtomicU32 = AtomicU32::new(1);

macro_rules! klog {
    ($lvl:expr, $($t:tt)*) => {
        if LOG.load(Ordering::Relaxed) as i64 >= $lvl as i64 { nvlog!("[nvkms] +{}ms {}\n", crate::ms(), format_args!($($t)*)); }
    };
}

// ------------------------------------------------------ copy engine push --

/// GPFIFO entries (one 4 KiB page).
const GP_ENTRIES: u32 = 512;
/// Push segments: one per submission, reused round-robin (submissions are
/// synchronous, so a segment is always idle when it comes round again).
const SEG_BYTES: usize = 4096;
const SEGS: usize = 16;
const PB_BYTES: u64 = (SEG_BYTES * SEGS) as u64;
/// Damage rectangles copied individually; more than this become one full copy.
const MAX_RECTS: usize = 32;
/// A frame copy is ~1 ms at 1080p; anything near this bound is a hung channel.
const COPY_TIMEOUT_NS: u64 = 500_000_000;
/// Bound on the implicit-sync wait for the producer's GPU work.
const PRODUCER_TIMEOUT_NS: u64 = 1_000_000_000;

const SUBCH: u32 = NVA06F_SUBCHANNEL_COPY_ENGINE;

// clc7b5.h (AMPERE_DMA_COPY_B) method offsets and LAUNCH_DMA fields.
const C7B5_LAUNCH_DMA: u32 = 0x0300;
const C7B5_OFFSET_IN_UPPER: u32 = 0x0400; // IN_UPPER, IN_LOWER, OUT_UPPER, OUT_LOWER
const C7B5_PITCH_IN: u32 = 0x0410; // PITCH_IN, PITCH_OUT, LINE_LENGTH_IN, LINE_COUNT
const C7B5_SET_DST_PHYS_MODE: u32 = 0x0264;
const C7B5_SET_SRC_BLOCK_SIZE: u32 = 0x0728; // BLOCK_SIZE, WIDTH, HEIGHT, DEPTH, LAYER, ORIGIN
const LAUNCH_NON_PIPELINED: u32 = 2 << 0;
const LAUNCH_FLUSH_ENABLE: u32 = 1 << 2; // FLUSH_TYPE (bit 25) 0 = SYS
const LAUNCH_SRC_PITCH: u32 = 1 << 7; // 0 = block-linear
const LAUNCH_DST_PITCH: u32 = 1 << 8;
const LAUNCH_MULTI_LINE: u32 = 1 << 9;
const LAUNCH_DST_PHYSICAL: u32 = 1 << 13;
const LAUNCH_DISABLE_PLC: u32 = 1 << 26;
/// SET_SRC_BLOCK_SIZE: WIDTH ONE_GOB (3:0 = 0), HEIGHT (7:4) = log2 GOBs,
/// DEPTH ONE_GOB (11:8 = 0), GOB_HEIGHT FERMI_8 (15:12 = 1).
const BLOCK_GOB_HEIGHT_FERMI_8: u32 = 1 << 12;
/// SET_DST_PHYS_MODE TARGET LOCAL_FB.
const PHYS_TARGET_LOCAL_FB: u32 = 0;

/// Incrementing method header (NVC56F_DMA_SEC_OP_INC_METHOD).
fn mthd(method: u32, count: u32) -> u32 {
    (1 << 29) | (count << 16) | (SUBCH << 13) | (method >> 2)
}

extern "C" {
    fn lnv_push_set_object(p: *mut u32, subch: u32, cls: u32) -> u32;
    fn lnv_push_ce_sem_release(p: *mut u32, subch: u32, sem: u64, payload: u32, intr: u8) -> u32;
    fn lnv_gp_entry(pb: u64, bytes: u32) -> u64;
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Layout {
    Pitch,
    /// NVIDIA 16Bx2 block-linear, blocks of 2^log2_gobs GOBs (8 rows each) high.
    Block { log2_gobs: u32 },
}

/// One 2D copy: `lines` rows of `line_bytes` from the source at (x_bytes, y)
/// into the destination at `dst`.
struct Copy2d {
    src_base: u64,
    src_pitch: u32,
    src_rows: u32,
    layout: Layout,
    x_bytes: u32,
    y: u32,
    dst: u64,
    dst_pitch: u32,
    line_bytes: u32,
    lines: u32,
}

fn push_copy(w: &mut Vec<u32>, c: &Copy2d, dst_phys: bool) {
    let mut launch = LAUNCH_NON_PIPELINED | LAUNCH_FLUSH_ENABLE | LAUNCH_DST_PITCH | LAUNCH_MULTI_LINE | LAUNCH_DISABLE_PLC;
    if dst_phys {
        w.push(mthd(C7B5_SET_DST_PHYS_MODE, 1));
        w.push(PHYS_TARGET_LOCAL_FB);
        launch |= LAUNCH_DST_PHYSICAL;
    }
    let src = match c.layout {
        Layout::Pitch => {
            launch |= LAUNCH_SRC_PITCH;
            c.src_base + c.y as u64 * c.src_pitch as u64 + c.x_bytes as u64
        }
        Layout::Block { log2_gobs } => {
            // The engine walks the block-linear surface itself: describe the
            // whole surface (width in bytes, rows, one layer) and start at the
            // origin, in bytes and rows (REMAP off: the element is a byte).
            w.push(mthd(C7B5_SET_SRC_BLOCK_SIZE, 6));
            w.push((log2_gobs << 4) | BLOCK_GOB_HEIGHT_FERMI_8);
            w.push(c.src_pitch); // SET_SRC_WIDTH
            w.push(c.src_rows); // SET_SRC_HEIGHT
            w.push(1); // SET_SRC_DEPTH
            w.push(0); // SET_SRC_LAYER
            w.push((c.x_bytes & 0xffff) | (c.y << 16)); // SET_SRC_ORIGIN
            c.src_base
        }
    };
    w.push(mthd(C7B5_OFFSET_IN_UPPER, 4));
    w.push((src >> 32) as u32 & 0x1ffff);
    w.push(src as u32);
    w.push((c.dst >> 32) as u32 & 0x1ffff);
    w.push(c.dst as u32);
    w.push(mthd(C7B5_PITCH_IN, 4));
    w.push(c.src_pitch);
    w.push(c.dst_pitch);
    w.push(c.line_bytes);
    w.push(c.lines);
    w.push(mthd(C7B5_LAUNCH_DMA, 1));
    w.push(launch);
}

// ------------------------------------------------------------- state --

#[derive(Default)]
struct SysBuf {
    mem: Handle,
    virt: Handle,
    va: u64,
    cpu: usize,
}

struct KFb {
    /// Duplicate of the buffer's RM memory in our client.
    mem: Handle,
    sysmem: bool,
    virt: Handle,
    va: u64,
    size: u64,
    w: u32,
    h: u32,
    pitch: u32,
    offset: u32,
    layout: Layout,
    /// Buffer object id in the nouveau layer (for the implicit-sync wait).
    obj: u32,
    presents: u64,
}

struct Kms {
    c: Client,
    dev: Handle,
    sub: Handle,
    vas: Handle,
    tsg: Handle,
    chan: Handle,
    ce_class: u32,
    ce_engine: u32,
    doorbell: usize,
    token: u32,
    gpfifo: SysBuf,
    userd: SysBuf,
    pb: SysBuf,
    sem: SysBuf,
    gp_put: u32,
    nsub: u64,
    payload: u32,
    /// Destination: GPU VA of the GOP surface (or its VRAM offset if `dst_phys`).
    dst: u64,
    dst_phys: bool,
    w: u32,
    h: u32,
    pitch: u32,
    fbs: BTreeMap<u32, KFb>,
    next_token: u32,
    dead: bool,
}

static KMS: KMutex<Option<Kms>> = KMutex::new(None);
/// (w << 32) | h of the scanout; refresh is reported as 60.
static MODE: AtomicU64 = AtomicU64::new(0);

static PRESENTS: AtomicU64 = AtomicU64::new(0);
static PRESENT_NS: AtomicU64 = AtomicU64::new(0);
static PRESENT_MAX_NS: AtomicU64 = AtomicU64::new(0);
static PRODUCER_WAIT_NS: AtomicU64 = AtomicU64::new(0);
// Window counters of the periodic `[nvkms] frames` line (lane nvstall).
static W_START_NS: AtomicU64 = AtomicU64::new(0);
static W_PRESENTS: AtomicU64 = AtomicU64::new(0);
static W_PRESENT_NS: AtomicU64 = AtomicU64::new(0);
static W_PRESENT_MAX: AtomicU64 = AtomicU64::new(0);
static W_PROD_N: AtomicU64 = AtomicU64::new(0);
static W_PROD_NS: AtomicU64 = AtomicU64::new(0);
static W_PROD_MAX: AtomicU64 = AtomicU64::new(0);
static W_EXECS0: AtomicU64 = AtomicU64::new(0);
static W_ISR0: AtomicU64 = AtomicU64::new(0);
/// Seconds between two `[nvkms] frames` lines while presents happen.
const FRAMES_LINE_NS: u64 = 5_000_000_000;
static ERRORS: AtomicU64 = AtomicU64::new(0);

impl Kms {
    fn sem_value(&self) -> u32 {
        unsafe { ptr::read_volatile(self.sem.cpu as *const u32) }
    }

    fn userd_u32(&self, off: usize) -> u32 {
        unsafe { ptr::read_volatile((self.userd.cpu + off) as *const u32) }
    }

    /// Queue one push segment (SET_OBJECT, `body`, semaphore release) and ring
    /// the doorbell; returns the payload to wait for.
    fn submit(&mut self, body: &[u32]) -> Result<u32, i32> {
        let seg = (self.nsub % SEGS as u64) as usize;
        self.nsub += 1;
        let words = unsafe { core::slice::from_raw_parts_mut((self.pb.cpu + seg * SEG_BYTES) as *mut u32, SEG_BYTES / 4) };
        let mut n = unsafe { lnv_push_set_object(words.as_mut_ptr(), SUBCH, self.ce_class) } as usize;
        if n + body.len() + 16 > words.len() {
            return Err(EINVAL);
        }
        words[n..n + body.len()].copy_from_slice(body);
        n += body.len();
        self.payload = self.payload.wrapping_add(1);
        let payload = self.payload;
        n += unsafe { lnv_push_ce_sem_release(words[n..].as_mut_ptr(), SUBCH, self.sem.va, payload, 0) } as usize;
        let seg_va = self.pb.va + (seg * SEG_BYTES) as u64;
        let entry = unsafe { lnv_gp_entry(seg_va, (n * 4) as u32) };
        unsafe { ptr::write_volatile((self.gpfifo.cpu as *mut u64).add(self.gp_put as usize), entry) };
        self.gp_put = (self.gp_put + 1) % GP_ENTRIES;
        fence(Ordering::SeqCst);
        unsafe { ptr::write_volatile((self.userd.cpu + USERD_GP_PUT) as *mut u32, self.gp_put) };
        fence(Ordering::SeqCst);
        unsafe { ptr::write_volatile((self.doorbell + NVC361_NOTIFY_CHANNEL_PENDING) as *mut u32, self.token) };
        Ok(payload)
    }

    /// Wait for the semaphore to reach `payload`: a short spin (most copies
    /// finish in well under a millisecond), then yielding, bounded.
    fn wait(&mut self, payload: u32, what: &str) -> Result<u64, i32> {
        let t0 = sched::monotonic_ns();
        let spin_until = t0 + 300_000;
        loop {
            let v = self.sem_value();
            if (v.wrapping_sub(payload) as i32) >= 0 {
                return Ok(sched::monotonic_ns() - t0);
            }
            let now = sched::monotonic_ns();
            if now - t0 > COPY_TIMEOUT_NS {
                self.dead = true;
                ERRORS.fetch_add(1, Ordering::Relaxed);
                nvscreen!("[nvkms] +{}ms ERROR copy engine did not finish {} within {} ms (semaphore {:#x}, want {:#x}; USERD GP_GET {} GP_PUT {} ours {}; Xid lines so far {}) -- scanout disabled until reboot\n",
                          crate::ms(), what, COPY_TIMEOUT_NS / 1_000_000, v, payload, self.userd_u32(USERD_GP_GET),
                          self.userd_u32(USERD_GP_PUT), self.gp_put, crate::xid_lines());
                return Err(EIO);
            }
            if now < spin_until {
                core::hint::spin_loop();
            } else {
                sched::yield_now("nvkms copy");
            }
        }
    }

    fn alloc_sysbuf(&self, size: u64, gpu: bool) -> Result<SysBuf, alloc::string::String> {
        let attrs = MemAttrs { contiguous: true, ..Default::default() };
        let mem = self.c.alloc_memory(self.dev, self.sub, MemLocation::Sysmem, size, attrs)
            .map_err(|e| alloc::format!("sysmem {} B: {:?}", size, e))?;
        let mut b = SysBuf { mem, ..Default::default() };
        let cpu = self.c.map_cpu(self.dev, mem, 0, size, OS33_FLAGS_ACCESS_READ_WRITE)
            .map_err(|e| alloc::format!("map_cpu {} B: {:?}", size, e))?;
        unsafe { ptr::write_bytes(cpu, 0, size as usize) };
        b.cpu = cpu as usize;
        if gpu {
            let (virt, _) = self.c.alloc_va(self.dev, self.vas, size, 0x1000, PageSize::Small)
                .map_err(|e| alloc::format!("VA {} B: {:?}", size, e))?;
            b.virt = virt;
            b.va = self.c.map_dma(self.dev, virt, mem, 0, size, MapFlags::default())
                .map_err(|e| alloc::format!("map_dma {} B: {:?}", size, e))?;
        }
        Ok(b)
    }
}

// --------------------------------------------------------------- init --

/// After the nouveau UAPI registered (`[nvrm]` thread): bring up the copy
/// engine channel, map the GOP surface, verify it, register the KMS seam.
pub(crate) fn init() {
    if let Some(v) = crate::opt_str("nvrm.kms_log").and_then(crate::parse_num) {
        LOG.store(v as u32, Ordering::Relaxed);
    }
    if crate::opt_str("nvrm.kms") == Some("0") {
        nvlog!("[nvkms] nvrm.kms=0: card0 keeps the CPU-copy KMS (dumb buffers only)\n");
        return;
    }
    let (fb_phys, w, h, pitch) = crate::platform::boot_fb();
    if fb_phys == 0 || w == 0 || h == 0 || pitch < w * 4 {
        nvscreen!("[nvkms] +{}ms ==== NVIDIA KMS NOT available: no usable GOP framebuffer from the boot loader ({:#x} {}x{} pitch {}) ====\n",
                  crate::ms(), fb_phys, w, h, pitch);
        return;
    }
    let t0 = sched::monotonic_ns();
    match init_inner(fb_phys, w, h, pitch) {
        Ok((k, line)) => {
            MODE.store((w as u64) << 32 | h as u64, Ordering::Release);
            *KMS.lock() = Some(k);
            drivers::gpu_kms::register(&KMS_BACKEND);
            nvscreen!("[nvkms] +{}ms ==== NVIDIA KMS ready: card0 scans out {}x{} (the GOP mode, no modeset) by copy-engine blits into the GOP surface; {}; modifiers LINEAR; setup {} ms ====\n",
                      crate::ms(), w, h, line, (sched::monotonic_ns() - t0) / 1_000_000);
        }
        Err(why) => {
            nvscreen!("[nvkms] +{}ms ==== NVIDIA KMS NOT available: {} (card0 keeps the CPU-copy KMS: dumb buffers only) ====\n",
                      crate::ms(), why);
        }
    }
    crate::device::flush_log();
}

fn init_inner(fb_phys: u64, w: u32, h: u32, pitch: u32) -> Result<(Kms, alloc::string::String), alloc::string::String> {
    use alloc::format;
    let c = Client::new().map_err(|e| format!("NV01_ROOT: {:?}", e))?;
    let mut ids = zeroed::<Nv0000CtrlGpuGetAttachedIdsParams>();
    c.control(c.handle(), NV0000_CTRL_CMD_GPU_GET_ATTACHED_IDS, &mut *ids).map_err(|e| format!("GET_ATTACHED_IDS: {:?}", e))?;
    let id = ids.gpuIds.iter().copied().find(|&i| i != NV0000_CTRL_GPU_INVALID_ID && i != 0).ok_or("no GPU attached to RM")?;
    let mut info = Nv0000CtrlGpuGetIdInfoV2Params { gpuId: id, ..Default::default() };
    c.control(c.handle(), NV0000_CTRL_CMD_GPU_GET_ID_INFO_V2, &mut info).map_err(|e| format!("GET_ID_INFO_V2: {:?}", e))?;
    let mut p = Nv0080AllocParameters { deviceId: info.deviceInstance, hClientShare: c.handle(), ..Default::default() };
    let dev = c.alloc(c.handle(), NV01_DEVICE_0, &mut p).map_err(|e| format!("NV01_DEVICE_0: {:?}", e))?;
    let mut p = Nv2080AllocParameters { subDeviceId: 0 };
    let sub = c.alloc(dev, NV20_SUBDEVICE_0, &mut p).map_err(|e| format!("NV20_SUBDEVICE_0: {:?}", e))?;

    let mut cl = zeroed::<Nv0080CtrlGpuGetClasslistV2Params>();
    c.control(dev, NV0080_CTRL_CMD_GPU_GET_CLASSLIST_V2, &mut *cl).map_err(|e| format!("GET_CLASSLIST_V2: {:?}", e))?;
    let classes = &cl.classList[..(cl.numClasses as usize).min(cl.classList.len())];
    let ce_class = [AMPERE_DMA_COPY_B, AMPERE_DMA_COPY_A].iter().copied().find(|c| classes.contains(c))
        .ok_or("no Ampere copy class")?;
    if ce_class != AMPERE_DMA_COPY_B {
        return Err(format!("copy class {:#x}: only c7b5 is wired", ce_class));
    }

    // The first asynchronous (non-GRCE) copy engine, as the self-test picks.
    let mut eng = zeroed::<Nv2080CtrlGpuGetEnginesV2Params>();
    let mut ce_engine = None;
    let mut first_ce = None;
    if c.control(sub, NV2080_CTRL_CMD_GPU_GET_ENGINES_V2, &mut *eng).is_ok() {
        let n = (eng.engineCount as usize).min(eng.engineList.len());
        for &e in &eng.engineList[..n] {
            if copy_index(e).is_none() {
                continue;
            }
            first_ce.get_or_insert(e);
            let mut caps = Nv2080CtrlCeGetCapsV2Params { ceEngineType: e, ..Default::default() };
            if c.control(sub, NV2080_CTRL_CMD_CE_GET_CAPS_V2, &mut caps).is_ok() && caps.capsTbl[0] & CE_CAPS_GRCE == 0 {
                ce_engine = Some(e);
                break;
            }
        }
    }
    let ce_engine = ce_engine.or(first_ce).unwrap_or(engine_copy(2));

    let mut vp = NvVaspaceAllocationParameters { index: NV_VASPACE_ALLOCATION_INDEX_GPU_NEW, ..Default::default() };
    let vas = c.alloc(dev, FERMI_VASPACE_A, &mut vp).map_err(|e| format!("FERMI_VASPACE_A: {:?}", e))?;

    let mut k = Kms {
        c, dev, sub, vas, tsg: 0, chan: 0, ce_class, ce_engine, doorbell: 0, token: 0,
        gpfifo: SysBuf::default(), userd: SysBuf::default(), pb: SysBuf::default(), sem: SysBuf::default(),
        gp_put: 0, nsub: 0, payload: 0, dst: 0, dst_phys: false, w, h, pitch,
        fbs: BTreeMap::new(), next_token: 1, dead: false,
    };
    k.gpfifo = k.alloc_sysbuf(GP_ENTRIES as u64 * 8, true)?;
    k.userd = k.alloc_sysbuf(4096, false)?;
    k.pb = k.alloc_sysbuf(PB_BYTES, true)?;
    k.sem = k.alloc_sysbuf(4096, true)?;

    let mut tp = NvChannelGroupAllocationParameters { hVASpace: k.vas, engineType: ce_engine, ..Default::default() };
    k.tsg = k.c.alloc(k.dev, KEPLER_CHANNEL_GROUP_A, &mut tp).map_err(|e| format!("KEPLER_CHANNEL_GROUP_A (engine {:#x}): {:?}", ce_engine, e))?;
    let mut cp = zeroed::<NvChannelAllocParams>();
    cp.gpFifoOffset = k.gpfifo.va;
    cp.gpFifoEntries = GP_ENTRIES;
    cp.hUserdMemory[0] = k.userd.mem;
    cp.userdOffset[0] = 0;
    cp.engineType = ce_engine;
    k.chan = k.c.alloc(k.tsg, AMPERE_CHANNEL_GPFIFO_A, &mut *cp).map_err(|e| format!("AMPERE_CHANNEL_GPFIFO_A: {:?}", e))?;
    let mut ce = Nvb0b5AllocationParameters { version: NVB0B5_ALLOCATION_PARAMETERS_VERSION_1, engineType: ce_engine };
    k.c.alloc(k.chan, ce_class, &mut ce).map_err(|e| format!("copy engine object {:#x}: {:?}", ce_class, e))?;
    let usermode = k.c.alloc_noparams(k.sub, VOLTA_USERMODE_A).map_err(|e| format!("VOLTA_USERMODE_A: {:?}", e))?;
    k.doorbell = k.c.map_cpu(k.sub, usermode, 0, NVC361_NV_USERMODE__SIZE, OS33_FLAGS_ACCESS_WRITE_ONLY)
        .map_err(|e| format!("mapping the doorbell: {:?}", e))? as usize;
    let mut tok = Nvc36fCtrlGetWorkSubmitTokenParams::default();
    k.c.control(k.chan, NVC36F_CTRL_CMD_GPFIFO_GET_WORK_SUBMIT_TOKEN, &mut tok).map_err(|e| format!("GET_WORK_SUBMIT_TOKEN: {:?}", e))?;
    k.token = tok.workSubmitToken;
    let mut sch = Nva06fCtrlGpfifoScheduleParams { bEnable: 1, bSkipSubmit: 0 };
    if k.c.control(k.chan, NVA06F_CTRL_CMD_GPFIFO_SCHEDULE, &mut sch).is_err() {
        let mut sch = Nva06fCtrlGpfifoScheduleParams { bEnable: 1, bSkipSubmit: 0 };
        k.c.control(k.tsg, NVA06C_CTRL_CMD_GPFIFO_SCHEDULE, &mut sch).map_err(|e| format!("GPFIFO_SCHEDULE: {:?}", e))?;
    }

    // The GOP surface: RM's reserved console memory, mapped into our VA space.
    let surface = pitch as u64 * h as u64;
    let len = (surface + 0xfff) & !0xfff;
    let con = k.c.alloc_noparams(k.dev, NV01_MEMORY_FRAMEBUFFER_CONSOLE)
        .map_err(|e| format!("NV01_MEMORY_FRAMEBUFFER_CONSOLE: {:?} (RM reserved no console memory)", e))?;
    let mut pa = Nv0041CtrlGetSurfacePhysAttrParams::default();
    let pa_ok = k.c.control(con, NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR, &mut pa).is_ok();
    let vidmem = pa_ok && pa.memAperture == NV0041_CTRL_CMD_GET_SURFACE_PHYS_ATTR_APERTURE_VIDMEM;
    let mapped = k.c.alloc_va(k.dev, k.vas, len, 0x10000, PageSize::Small)
        .and_then(|(virt, _)| k.c.map_dma(k.dev, virt, con, 0, len, MapFlags::default()));
    match mapped {
        Ok(va) => k.dst = va,
        Err(e) if vidmem && pa.contigSegmentSize >= surface => {
            klog!(0, "mapping the console memory failed ({:?}); copying to VRAM offset {:#x} physically", e, pa.memOffset);
            k.dst = pa.memOffset;
            k.dst_phys = true;
        }
        Err(e) => return Err(format!("mapping the console memory: {:?}", e)),
    }

    // Verify without touching the screen: the copy engine reads the first
    // rows of the surface into sysmem; they must equal the CPU's BAR1 view.
    let rows = 8u32.min(h);
    let probe = (rows * pitch) as u64;
    let scratch = k.alloc_sysbuf((probe + 0xfff) & !0xfff, true)?;
    let mut body = Vec::new();
    let launch = LAUNCH_NON_PIPELINED | LAUNCH_FLUSH_ENABLE | LAUNCH_SRC_PITCH | LAUNCH_DST_PITCH | LAUNCH_DISABLE_PLC
        | if k.dst_phys { 1 << 12 } else { 0 }; // SRC_TYPE PHYSICAL
    if k.dst_phys {
        body.push(mthd(0x0260 /* SET_SRC_PHYS_MODE */, 1));
        body.push(PHYS_TARGET_LOCAL_FB);
    }
    body.push(mthd(C7B5_OFFSET_IN_UPPER, 4));
    body.push((k.dst >> 32) as u32);
    body.push(k.dst as u32);
    body.push((scratch.va >> 32) as u32);
    body.push(scratch.va as u32);
    body.push(mthd(0x0418 /* LINE_LENGTH_IN */, 1));
    body.push(probe as u32);
    body.push(mthd(C7B5_LAUNCH_DMA, 1));
    body.push(launch);
    let t = k.submit(&body).map_err(|e| format!("verification submit: {}", e))?;
    let rt = k.wait(t, "the GOP surface read-back").map_err(|_| alloc::string::String::from("the copy engine did not answer (channel never ran?)"))?;
    let cpu = mm::kva::ioremap(fb_phys, probe as usize, mm::kva::CacheMode::Uncached).ok_or("ioremap of the GOP framebuffer")?;
    let (mut same, mut distinct, mut first) = (0usize, 0usize, None);
    // For the failure report: what each side looked like (nvrm-boot14: 0 of
    // 8192 words equal, after an RM "GPPut < WATCHDOG_GPFIFO_ENTRIES"
    // assertion, i.e. a CPU read-back through BAR1 that returned garbage).
    let (mut cpu_ones, mut cpu_zero, mut ce_zero) = (0usize, 0usize, 0usize);
    let (mut cpu0, mut ce0) = (0u32, 0u32);
    let words = (probe / 4) as usize;
    for i in 0..words {
        let a = unsafe { ptr::read_volatile((cpu as *const u32).add(i)) };
        let b = unsafe { ptr::read_volatile((scratch.cpu as *const u32).add(i)) };
        if i == 0 {
            cpu0 = a;
            ce0 = b;
        }
        cpu_ones += (a == u32::MAX) as usize;
        cpu_zero += (a == 0) as usize;
        ce_zero += (b == 0) as usize;
        if a == b {
            same += 1;
        }
        match first {
            None => first = Some(a),
            Some(f) if f != a => distinct += 1,
            _ => {}
        }
    }
    mm::kva::iounmap(cpu);
    let _ = k.c.unmap_dma(k.dev, scratch.virt, scratch.mem, scratch.va);
    let _ = k.c.free(k.dev, scratch.virt);
    let _ = k.c.unmap_cpu(k.dev, scratch.mem, scratch.cpu as *mut u8);
    let _ = k.c.free(k.dev, scratch.mem);
    if same != words {
        // All-ones from BAR1: the CPU's BAR1 window no longer reaches VRAM,
        // so the GOP console cannot be drawn either (the screen keeps its
        // last frame). All zeros from the copy engine: its read never landed.
        let hint = if cpu_ones == words {
            "; BAR1 reads all-ones: the CPU's BAR1 window is dead after GSP init, the console cannot update either -- power-cycle the PC"
        } else if ce_zero == words {
            "; the copy engine wrote nothing back"
        } else {
            ""
        };
        return Err(format!("the copy engine's view of the GOP surface differs from BAR1 ({} of {} words equal): not the scanout surface \
                            [BAR1 word0 {:#010x}, {} all-ones, {} zero; CE word0 {:#010x}, {} zero; surface at {} {:#x}, phys attr {} aperture {} offset {:#x}{}]",
                           same, words, cpu0, cpu_ones, cpu_zero, ce0, ce_zero,
                           if k.dst_phys { "VRAM offset" } else { "GPU VA" }, k.dst,
                           if pa_ok { "ok" } else { "failed" }, pa.memAperture, pa.memOffset, hint));
    }
    let line = format!("GOP surface at {} {:#x} (RM console memory, VRAM offset {:#x}), pitch {}, CE{} (engine {:#x}) class {:#x}, read-back of {} rows matches BAR1 ({}; {} us)",
                       if k.dst_phys { "VRAM offset" } else { "GPU VA" }, k.dst, if pa_ok { pa.memOffset } else { 0 }, pitch,
                       copy_index(ce_engine).unwrap_or(0), ce_engine, ce_class, rows,
                       if distinct == 0 { "uniform content: inconclusive but consistent" } else { "content verified" }, rt / 1000);
    Ok((k, line))
}

// ------------------------------------------------------------ backend --

static KMS_MODIFIERS: [u64; 1] = [0 /* DRM_FORMAT_MOD_LINEAR */];

static KMS_BACKEND: KmsBackend = KmsBackend {
    name: "nvidia-ce",
    mode,
    modifiers: &KMS_MODIFIERS,
    fb_create,
    fb_destroy,
    present,
    console_restore: None,
};

fn mode() -> (u32, u32, u32) {
    let m = MODE.load(Ordering::Acquire);
    ((m >> 32) as u32, m as u32, 60)
}

const FOURCC_XR24: u32 = 0x3432_5258;
const FOURCC_AR24: u32 = 0x3432_5241;
/// fourcc_mod_code(NVIDIA, ...): vendor 0x03 in bits 63:56.
const MOD_VENDOR_NVIDIA: u64 = 0x03;

fn fourcc_str(f: u32) -> [u8; 4] {
    f.to_le_bytes().map(|b| if b.is_ascii_graphic() { b } else { b'?' })
}

fn fb_create(req: &KmsFbReq, open_id: u32) -> Option<Result<u32, DriverError>> {
    if !crate::uapi::kms_is_handle(req.handle) {
        return None;
    }
    Some(fb_create_inner(req, open_id).map_err(|e| {
        ERRORS.fetch_add(1, Ordering::Relaxed);
        err(e)
    }))
}

fn fb_create_inner(req: &KmsFbReq, open_id: u32) -> Result<u32, i32> {
    let f = fourcc_str(req.fourcc);
    let fs = core::str::from_utf8(&f).unwrap_or("?");
    if req.fourcc != FOURCC_XR24 && req.fourcc != FOURCC_AR24 {
        klog!(0, "open {}: ADDFB2 handle {:#x}: format {} refused (XR24/AR24 only)", open_id, req.handle, fs);
        return Err(EINVAL);
    }
    if req.width == 0 || req.height == 0 || req.width > 16384 || req.height > 16384 {
        return Err(EINVAL);
    }
    let bo = crate::uapi::kms_bo(open_id, req.handle).ok_or_else(|| {
        klog!(0, "open {}: ADDFB2 handle {:#x}: no such buffer in this open", open_id, req.handle);
        ENOENT
    })?;
    let kind = (bo.tile_flags >> 8) & 0xff;
    let layout = match req.modifier {
        Some(0) => Layout::Pitch,
        Some(m) if m >> 56 == MOD_VENDOR_NVIDIA && m & 0x10 != 0 => {
            // DRM_FORMAT_MOD_NVIDIA_BLOCK_LINEAR_2D(c, s, g, k, h): h = bits
            // 3:0, compression c = bits 25:23 (not supported: no
            // decompression on this path).
            if (m >> 23) & 0x7 != 0 {
                klog!(0, "open {}: ADDFB2 handle {:#x}: compressed block-linear modifier {:#x} refused", open_id, req.handle, m);
                return Err(EINVAL);
            }
            Layout::Block { log2_gobs: (m & 0xf) as u32 }
        }
        Some(m) => {
            klog!(0, "open {}: ADDFB2 handle {:#x}: modifier {:#x} refused (IN_FORMATS offers LINEAR)", open_id, req.handle, m);
            return Err(EINVAL);
        }
        // Implicit layout: the buffer's own (GEM_NEW tile_flags/tile_mode).
        None if kind == 0 => Layout::Pitch,
        None => Layout::Block { log2_gobs: (bo.tile_mode >> 4) & 0xf },
    };
    if let Layout::Block { log2_gobs } = layout {
        if log2_gobs > 5 {
            return Err(EINVAL);
        }
    }
    let rows = match layout {
        Layout::Pitch => req.height as u64,
        Layout::Block { log2_gobs } => {
            let bh = 8u64 << log2_gobs;
            (req.height as u64 + bh - 1) / bh * bh
        }
    };
    if req.pitch < req.width * 4 || (matches!(layout, Layout::Block { .. }) && req.pitch % 64 != 0) {
        klog!(0, "open {}: ADDFB2 handle {:#x}: pitch {} invalid for width {} ({:?})", open_id, req.handle, req.pitch, req.width, layout);
        return Err(EINVAL);
    }
    let need = req.offset as u64 + req.pitch as u64 * rows;
    if need > bo.size {
        klog!(0, "open {}: ADDFB2 handle {:#x}: {}x{} pitch {} offset {} needs {} B, buffer has {}", open_id, req.handle,
              req.width, req.height, req.pitch, req.offset, need, bo.size);
        return Err(EINVAL);
    }

    let mut g = KMS.lock();
    let k = g.as_mut().ok_or(ENODEV)?;
    let parent = if bo.sysmem { k.dev } else { k.sub };
    let mem = k.c.dup(parent, bo.client, bo.mem).map_err(|e| {
        klog!(0, "open {}: ADDFB2 handle {:#x}: duplicating the buffer: {:?}", open_id, req.handle, e);
        EIO
    })?;
    let (virt, va) = match k.c.alloc_va(k.dev, k.vas, bo.size, 0x10000, PageSize::Small)
        .and_then(|(virt, _)| k.c.map_dma(k.dev, virt, mem, 0, bo.size, MapFlags::default()).map(|va| (virt, va)))
    {
        Ok(x) => x,
        Err(e) => {
            klog!(0, "open {}: ADDFB2 handle {:#x}: mapping {} KiB: {:?}", open_id, req.handle, bo.size >> 10, e);
            let _ = k.c.free(parent, mem);
            return Err(ENOMEM);
        }
    };
    let token = k.next_token;
    k.next_token = k.next_token.wrapping_add(1).max(1);
    k.fbs.insert(token, KFb {
        mem, sysmem: bo.sysmem, virt, va, size: bo.size, w: req.width, h: req.height, pitch: req.pitch,
        offset: req.offset, layout, obj: bo.obj, presents: 0,
    });
    let nfb = k.fbs.len();
    drop(g);
    klog!(1, "open {}: ADDFB2 handle {:#x} (obj {}, {} {} KiB) {}x{} {} pitch {} offset {} {:?}{} -> fb token {} at GPU VA {:#x} ({} framebuffer(s))",
          open_id, req.handle, bo.obj, if bo.sysmem { "sysmem" } else { "VRAM" }, bo.size >> 10, req.width, req.height, fs,
          req.pitch, req.offset, layout, if req.modifier.is_none() { " (implicit)" } else { "" }, token, va, nfb);
    Ok(token)
}

fn fb_destroy(token: u32) {
    let mut g = KMS.lock();
    let Some(k) = g.as_mut() else { return };
    let Some(fb) = k.fbs.remove(&token) else { return };
    let ce_engine = k.ce_engine;
    let parent = if fb.sysmem { k.dev } else { k.sub };
    let _ = k.c.unmap_dma(k.dev, fb.virt, fb.mem, fb.va);
    let _ = k.c.free(k.dev, fb.virt);
    if let Err(e) = k.c.free(parent, fb.mem) {
        klog!(0, "fb token {}: freeing the duplicate: {:?}", token, e);
    }
    let n = k.fbs.len();
    drop(g);
    klog!(1, "fb token {} ({} KiB, CE engine {:#x}) destroyed after {} present(s) ({} framebuffer(s) left)", token, fb.size >> 10,
          ce_engine, fb.presents, n);
}

fn present(token: u32, src_x: u32, src_y: u32, rects: Option<&[(i32, i32, i32, i32)]>) -> Result<(), DriverError> {
    let t0 = sched::monotonic_ns();
    // Implicit sync: the GPU work of every open holding the buffer, outside
    // our lock (it may take a frame).
    let obj = {
        let g = KMS.lock();
        let k = g.as_ref().ok_or(err(ENODEV))?;
        if k.dead {
            return Err(err(EIO));
        }
        k.fbs.get(&token).ok_or(err(ENOENT))?.obj
    };
    let fences = crate::uapi::kms_bo_fences(obj);
    if !fences.is_empty() {
        let w0 = sched::monotonic_ns();
        if drivers::drm_device_interface::wait_fences(&fences, w0 + PRODUCER_TIMEOUT_NS).is_err() {
            klog!(0, "present fb {}: the producer's GPU work did not finish within {} ms; copying anyway", token,
                  PRODUCER_TIMEOUT_NS / 1_000_000);
        }
        let wns = sched::monotonic_ns() - w0;
        PRODUCER_WAIT_NS.fetch_add(wns, Ordering::Relaxed);
        W_PROD_N.fetch_add(1, Ordering::Relaxed);
        W_PROD_NS.fetch_add(wns, Ordering::Relaxed);
        W_PROD_MAX.fetch_max(wns, Ordering::Relaxed);
    }

    let mut g = KMS.lock();
    let k = g.as_mut().ok_or(err(ENODEV))?;
    if k.dead {
        return Err(err(EIO));
    }
    let (sw, sh, dpitch, dst, dphys) = (k.w, k.h, k.pitch, k.dst, k.dst_phys);
    let fb = k.fbs.get_mut(&token).ok_or(err(ENOENT))?;
    fb.presents += 1;
    if src_x >= fb.w || src_y >= fb.h {
        return Err(err(EINVAL));
    }
    // Visible window of the framebuffer: (src_x, src_y) .. + screen size.
    let vw = (fb.w - src_x).min(sw);
    let vh = (fb.h - src_y).min(sh);
    let rows = match fb.layout {
        Layout::Pitch => fb.h,
        Layout::Block { log2_gobs } => (fb.h + (8 << log2_gobs) - 1) / (8 << log2_gobs) * (8 << log2_gobs),
    };
    let mut boxes: Vec<(u32, u32, u32, u32)> = Vec::new(); // in window coordinates
    match rects {
        Some(r) if r.len() <= MAX_RECTS => {
            for &(x1, y1, x2, y2) in r {
                let ax = (x1.max(0) as u32).max(src_x) - src_x;
                let ay = (y1.max(0) as u32).max(src_y) - src_y;
                let bx = ((x2.max(0) as u32).saturating_sub(src_x)).min(vw);
                let by = ((y2.max(0) as u32).saturating_sub(src_y)).min(vh);
                if ax < bx && ay < by {
                    boxes.push((ax, ay, bx, by));
                }
            }
        }
        _ => boxes.push((0, 0, vw, vh)),
    }
    if boxes.is_empty() {
        return Ok(());
    }
    let mut body = Vec::with_capacity(boxes.len() * 24);
    for &(ax, ay, bx, by) in &boxes {
        let c = Copy2d {
            src_base: fb.va + fb.offset as u64,
            src_pitch: fb.pitch,
            src_rows: rows,
            layout: fb.layout,
            x_bytes: (src_x + ax) * 4,
            y: src_y + ay,
            dst: dst + ay as u64 * dpitch as u64 + ax as u64 * 4,
            dst_pitch: dpitch,
            line_bytes: (bx - ax) * 4,
            lines: by - ay,
        };
        push_copy(&mut body, &c, dphys);
    }
    let payload = k.submit(&body).map_err(err)?;
    let r = k.wait(payload, "a frame copy");
    drop(g);
    let ns = sched::monotonic_ns() - t0;
    match r {
        Ok(_) => {
            let n = PRESENTS.fetch_add(1, Ordering::Relaxed) + 1;
            PRESENT_NS.fetch_add(ns, Ordering::Relaxed);
            PRESENT_MAX_NS.fetch_max(ns, Ordering::Relaxed);
            if n == 1 || LOG.load(Ordering::Relaxed) >= 2 {
                klog!(1, "present fb {}: {} rect(s), window {}x{} from ({},{}), {} us{}", token, boxes.len(), vw, vh,
                      src_x, src_y, ns / 1000, if n == 1 { " (first present)" } else { "" });
            }
            if n % 1000 == 0 {
                klog!(1, "{} presents: avg {} us, max {} us, producer waits {} ms total, errors {}", n,
                      PRESENT_NS.load(Ordering::Relaxed) / n / 1000, PRESENT_MAX_NS.load(Ordering::Relaxed) / 1000,
                      PRODUCER_WAIT_NS.load(Ordering::Relaxed) / 1_000_000, ERRORS.load(Ordering::Relaxed));
            }
            W_PRESENTS.fetch_add(1, Ordering::Relaxed);
            W_PRESENT_NS.fetch_add(ns, Ordering::Relaxed);
            W_PRESENT_MAX.fetch_max(ns, Ordering::Relaxed);
            frames_line(t0 + ns);
            Ok(())
        }
        Err(e) => Err(err(e)),
    }
}

/// Every `FRAMES_LINE_NS` while frames are presented: where a frame's time
/// goes, so a PC boot log says why the desktop is (or is not) at 60 fps.
///
/// ```text
/// [nvkms] +T frames 5.0 s: 287 presents = 57/s (active 4.9 s at 58 fps; longest gap 40 ms);
///   present avg 140 us max 900 us; producer wait n 12 avg 30 us max 200 us;
///   vblank events 287, queue->deliver avg 9.0 ms max 16.6 ms;
///   event->next commit n 280 avg 7.1 ms max 31 ms (7 idle);
///   fence waits 512 avg 450 us max 2100 us (300 slept); GPU irq hints +900, ISR +900, EXEC +600; sync files pending 2
/// ```
/// Reading it: `active ... fps` well under 60 with `event->next commit`
/// near the frame time = the compositor's own frame (render, CPU fence
/// waits: compare `fence waits`) is the limit; `queue->deliver` near 16.6 ms
/// = paced at the 60 Hz synthetic vblank (fine); a `longest gap` of seconds
/// while active = something stalled the compositor (look for `[LOCKWAIT]`
/// and `[WDOG]` lines at that time); `GPU irq hints +0` with many fence
/// waits = the non-stall interrupt is not arriving (waits then end on the
/// 1 ms re-probe, `slept` ~ `fence waits`).
fn frames_line(now: u64) {
    let start = W_START_NS.load(Ordering::Relaxed);
    if start == 0 {
        W_START_NS.store(now, Ordering::Relaxed);
        W_EXECS0.store(crate::uapi::EXECS.load(Ordering::Relaxed), Ordering::Relaxed);
        W_ISR0.store(crate::device::ISR_COUNT.load(Ordering::Relaxed), Ordering::Relaxed);
        return;
    }
    let span = now.saturating_sub(start);
    if span < FRAMES_LINE_NS || LOG.load(Ordering::Relaxed) == 0 {
        return;
    }
    // One printer per window (presents are serialised by the DRM master,
    // but be safe against a racing second open).
    if W_START_NS.compare_exchange(start, now, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return;
    }
    let t = |a: &AtomicU64| a.swap(0, Ordering::Relaxed);
    let (np, pns, pmax) = (t(&W_PRESENTS), t(&W_PRESENT_NS), t(&W_PRESENT_MAX));
    let (wn, wns, wmax) = (t(&W_PROD_N), t(&W_PROD_NS), t(&W_PROD_MAX));
    let f = drivers::drm_device_interface::ext_frame_stats_take();
    let (fwn, fwns, fwmax, fwslept, hints) = drivers::drm_device_interface::fence_wait_stats_take();
    let execs = crate::uapi::EXECS.load(Ordering::Relaxed);
    let isr = crate::device::ISR_COUNT.load(Ordering::Relaxed);
    let dexec = execs.wrapping_sub(W_EXECS0.swap(execs, Ordering::Relaxed));
    let disr = isr.wrapping_sub(W_ISR0.swap(isr, Ordering::Relaxed));
    let div = |a: u64, b: u64| if b == 0 { 0 } else { a / b };
    let span_ms = span / 1_000_000;
    let active_ms = f.active_ns / 1_000_000;
    nvlog!("[nvkms] +{}ms frames {}.{} s: {} presents = {}/s (active {}.{} s at {} fps; longest gap {} ms); present avg {} us max {} us; producer wait n {} avg {} us max {} us; vblank events {}, queue->deliver avg {} us max {} us; event->next commit n {} avg {} us max {} us ({} idle); fence waits {} avg {} us max {} us ({} slept); GPU irq hints +{}, ISR +{}, EXEC +{}; sync files pending {}\n",
           crate::ms(), span_ms / 1000, span_ms % 1000 / 100, np, div(np * 1000, span_ms),
           active_ms / 1000, active_ms % 1000 / 100, div(f.active_n * 1_000_000_000, f.active_ns), f.gap_max_ns / 1_000_000,
           div(pns, np) / 1000, pmax / 1000, wn, div(wns, wn) / 1000, wmax / 1000,
           f.events, div(f.q2d_ns, f.events) / 1000, f.q2d_max_ns / 1000,
           f.d2c_n, div(f.d2c_ns, f.d2c_n) / 1000, f.d2c_max_ns / 1000, f.d2c_idle,
           fwn, div(fwns, fwn) / 1000, fwmax / 1000, fwslept,
           hints, disr, dexec, drivers::sync_file::pending_count());
}

/// Counters for the stick log's summary lines.
pub(crate) fn summary() {
    if !drivers::gpu_kms::active() {
        return;
    }
    let n = PRESENTS.load(Ordering::Relaxed);
    let (dp, de, vt) = drivers::drm_device_interface::ext_present_stats();
    nvlog!("[nvkms] presents {} (avg {} us, max {} us), DRM-side presents {} errors {} VT repaints {}, errors {}\n", n,
           if n == 0 { 0 } else { PRESENT_NS.load(Ordering::Relaxed) / n / 1000 }, PRESENT_MAX_NS.load(Ordering::Relaxed) / 1000,
           dp, de, vt, ERRORS.load(Ordering::Relaxed));
}
