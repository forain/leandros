//! G4a: KMS on the pipe the firmware (GOP) left lit — no modeset.
//!
//! The boot loader's GOP leaves eDP on pipe A, plane 1A, XRGB8888 linear at
//! GGTT 0 (stolen memory), PIPESRC = the GOP mode, the pipe scaler stretching
//! it to the panel. We keep all of that (pipe, transcoder, DDI, PLL, scaler,
//! watermarks, DDB) and only re-point the plane: a framebuffer's GEM object is
//! bound into a private GGTT window and a flip writes PLANE_STRIDE/OFFSET and
//! then PLANE_SURF, which the hardware latches at the next vblank
//! (PLANE_SURFLIVE says when). The connector/CRTC/plane objects, atomic
//! properties, page-flip events (the 100 Hz synthetic vblank) and DRM master
//! are the DRM core's (`drm_device_interface` through `gpu_kms`), exactly as
//! for the NVIDIA copy-engine backend.
//!
//! Coherency: the display engine does not snoop the LLC, and every object is
//! mapped LLC-cached for the CPU and the GPU (G3), so a flip first waits for
//! the object's last request (implicit sync) and clflushes the scanned-out
//! rows (Linux does the same for a non-WT display object).
//!
//! Console hand-off: when the scanout goes back to the console (card0 master
//! closed, VT switch, panic) `console_restore` re-points the plane at the GOP
//! surface — lock-free, MMIO writes only, so the panic path may call it.
//!
//! Native resolution (`i915.native`, default 1): the GOP mode is usually
//! not the panel's (KBL laptop: plane 1A 1024x768, PS_CTRL1 upscaling to the
//! transcoder's 1920x1080). The mode reported is then the ACTIVE transcoder
//! timings (pixel clock from the DPLL link rate and LINK_M/N), and the first
//! flip "fastsets" pipe A to it inside one vblank window: PIPESRC = native,
//! pipe scaler off (PS_CTRL/WIN_POS/WIN_SZ = 0, WIN_SZ arms), PLANE_SIZE/
//! POS/STRIDE/OFFSET/WM, PLANE_SURF last — no pipe disable, no link retrain.
//! `console_restore` puts the GOP state (PIPESRC, saved PS_*, plane size and
//! watermarks, GOP surface) back the same way. The DDB (PLANE_BUF_CFG) is
//! kept; the plane watermarks are the GOP's scaled by blocks-per-line
//! (`i915.native_wm=0`: keep the GOP's), levels that do not fit the DDB off.
//! `i915.native=0`: the old behaviour (GOP mode, scaler kept).
//!
//! Only with `i915.kms=1` (and execution ready on hardware: GPU rendering
//! only). On the fake device (`i915.fake=2 i915.kms=1`, QEMU) the registers
//! are RAM: the whole ioctl path runs, nothing reaches a screen.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

use crate::gpu_kms::{KmsBackend, KmsFbReq};
use crate::DriverError;

use super::hw::{self, reg};
use super::uapi_abi::{EINVAL, ENOSPC, ETIME};
use super::{ilog, log, opt, uapi, BootEnv, Device, Mmio};

/// Plane 1A's registers (pipe A, plane 0).
const PIPE: u32 = 0;
const PL: u32 = 0;
pub(super) const PLANE_SURFLIVE_1_A: u32 = 0x701AC;
/// eDP PSR (transcoder EDP, HSW..TGL offsets) and PSR2 control.
const EDP_PSR_CTL: u32 = 0x64800;
const EDP_PSR_STATUS: u32 = 0x64840;
const EDP_PSR_DEBUG: u32 = 0x64860;
const EDP_PSR2_CTL: u32 = 0x6F900;
const EDP_PSR_ENABLE: u32 = 1 << 31;
/// Frame-buffer compression control (gen5..gen9), bit 31 = enable.
const ILK_DPFC_CONTROL: u32 = 0x43208;
/// gen8+ display-engine pipe A interrupt registers; bit 31 = FIFO underrun.
const DE_PIPE_A_IMR: u32 = 0x44404;
const DE_PIPE_A_IIR: u32 = 0x44408;
const DE_PIPE_A_IER: u32 = 0x4440C;
const PIPE_FIFO_UNDERRUN: u32 = 1 << 31;
/// PLANE_WM_1_A level 0 (levels 1..7 follow at +4), PLANE_WM_TRANS_1_A,
/// PLANE_BUF_CFG_1_A.
pub(super) const PLANE_WM_1_A: u32 = 0x70240;
const PLANE_WM_TRANS_1_A: u32 = 0x70268;
pub(super) const PLANE_BUF_CFG_1_A: u32 = 0x7027C;

/// The scanout GGTT window: 16 slots of 32 MiB from 1 GiB (clear of the GOP
/// surface at 0, the CPU aperture, exec's LRC slots and the G2 window at
/// the top of the GGTT). 32 MiB slots are 256 KiB aligned (gen9 linear).
const WIN_BASE: u64 = 1 << 30;
const SLOT: u64 = 32 << 20;
const NSLOT: usize = 16;
/// The hardware cursor's two image buffers (`cursor.rs`): right above the
/// scanout window.
pub(super) const CURSOR_GGTT: u64 = WIN_BASE + NSLOT as u64 * SLOT;

const WM_LEVELS: usize = 8;

const FOURCC_XR24: u32 = 0x3432_5258;
const FOURCC_AR24: u32 = 0x3432_5241;

static MMIO: AtomicUsize = AtomicUsize::new(0);
static GSM_VA: AtomicUsize = AtomicUsize::new(0);
static GSM_ENTRIES: AtomicUsize = AtomicUsize::new(0);
pub(super) static FAKE: AtomicBool = AtomicBool::new(false);
static MODE_W: AtomicU32 = AtomicU32::new(0);
static MODE_H: AtomicU32 = AtomicU32::new(0);
/// The GOP's plane state, put back by `console_restore`.
static ORIG_CTL: AtomicU32 = AtomicU32::new(0);
static ORIG_STRIDE: AtomicU32 = AtomicU32::new(0);
static ORIG_OFFSET: AtomicU32 = AtomicU32::new(0);
static ORIG_SURF: AtomicU32 = AtomicU32::new(0);
/// Token on screen (0: the console's GOP surface).
static ON_SCREEN: AtomicU32 = AtomicU32::new(0);
/// GOP pipe/scaler/plane-geometry state (restored by `console_restore`).
static ORIG_PIPESRC: AtomicU32 = AtomicU32::new(0);
static ORIG_PS_CTRL: AtomicU32 = AtomicU32::new(0);
static ORIG_PS_POS: AtomicU32 = AtomicU32::new(0);
static ORIG_PS_SZ: AtomicU32 = AtomicU32::new(0);
static ORIG_PSIZE: AtomicU32 = AtomicU32::new(0);
static ORIG_PPOS: AtomicU32 = AtomicU32::new(0);
static ORIG_WM: [AtomicU32; 9] = [const { AtomicU32::new(0) }; 9];
/// Native-mode watermarks (levels 0..7, then WM_TRANS).
static NAT_WM: [AtomicU32; 9] = [const { AtomicU32::new(0) }; 9];
static NAT_WM_SET: AtomicBool = AtomicBool::new(false);
/// Native mode (0: off — scan out at the GOP mode, scaler kept).
static NATIVE_W: AtomicU32 = AtomicU32::new(0);
static NATIVE_H: AtomicU32 = AtomicU32::new(0);
static REFRESH: AtomicU32 = AtomicU32::new(60);
/// Vblank start line and the evasion margin (lines) of the active timings.
pub(super) static VBL_START: AtomicU32 = AtomicU32::new(0);
/// Transcoder vertical total and line time (ns) of the active timings:
/// the vblank timestamp is corrected by the lines since vblank start.
pub(super) static VTOTAL: AtomicU32 = AtomicU32::new(0);
pub(super) static LINE_NS: AtomicU32 = AtomicU32::new(0);
static VBL_MARGIN: AtomicU32 = AtomicU32::new(16);
/// Pipe A currently fastset to the native mode.
static NATIVE_ON: AtomicBool = AtomicBool::new(false);
static FASTSETS: AtomicU32 = AtomicU32::new(0);
static FLIPS: AtomicU32 = AtomicU32::new(0);
static LATE: AtomicU32 = AtomicU32::new(0);
/// `i915.kmsdebug=1`: rate-limited per-flip / per-fb lines and pipe A
/// FIFO-underrun counting (IMR bit 31 unmasked, IER untouched: the IIR
/// latches, no interrupt is raised).
static DEBUG: AtomicBool = AtomicBool::new(false);
pub(super) static UNDERRUNS: AtomicU32 = AtomicU32::new(0);
/// Flips that clflushed the scanned-out rows (`flush`).
pub(super) static FLUSHED_FLIPS: AtomicU32 = AtomicU32::new(0);
static RESTORES: AtomicU32 = AtomicU32::new(0);
static DBG_LINES: AtomicU32 = AtomicU32::new(0);
/// `i915.kmsdump=N` / `i915.kmsdump2=M`: the scanout fb of flips N, N+1
/// (and M, M+1) goes into the log as a base64 PPM, box-downscaled by
/// `i915.kmsdump_div` (default 4). Read through the CPU (LLC-coherent view,
/// no flush: the dump does not change what the display reads).
static DUMP_AT: [AtomicU32; 2] = [const { AtomicU32::new(0) }; 2];
static DUMP_DIV: AtomicU32 = AtomicU32::new(4);

pub(super) fn debug() -> bool { DEBUG.load(Ordering::Relaxed) }

/// Rate limit for debug lines: the first 64, then one in 256.
fn dbg_ok() -> bool {
    let n = DBG_LINES.fetch_add(1, Ordering::Relaxed);
    n < 64 || n % 256 == 0
}

/// Pipe A FIFO underruns latched since the last call (kmsdebug only).
/// Pipe A FIFO underruns so far. The underrun bit is kept unmasked in IMR
/// (IER stays off unless `i915.kmsdebug=1`), so IIR latches it without
/// interrupting; the display IRQ handler (vblank.rs) and this poll count and
/// clear it. One count per latch: underruns between two polls/vblank IRQs
/// coalesce, so this counts frames that underran, not underrun events.
pub(super) fn underruns() -> u32 {
    if FAKE.load(Ordering::Relaxed) || MMIO.load(Ordering::Acquire) == 0 { return UNDERRUNS.load(Ordering::Relaxed); }
    let m = mmio();
    let imr = m.r(DE_PIPE_A_IMR);
    if imr & PIPE_FIFO_UNDERRUN != 0 { m.w(DE_PIPE_A_IMR, imr & !PIPE_FIFO_UNDERRUN); }
    if m.r(DE_PIPE_A_IIR) & PIPE_FIFO_UNDERRUN == 0 { return UNDERRUNS.load(Ordering::Relaxed); }
    m.w(DE_PIPE_A_IIR, PIPE_FIFO_UNDERRUN);
    UNDERRUNS.fetch_add(1, Ordering::Relaxed) + 1
}

#[derive(Clone, Copy)]
struct Fb { obj: u32, phys: u64, pages: u64, pitch: u32, offset: u32, width: u32, height: u32 }

static FBS: spin::Mutex<[Option<Fb>; NSLOT]> = spin::Mutex::new([None; NSLOT]);

pub(super) fn mmio() -> Mmio { Mmio { va: MMIO.load(Ordering::Acquire) } }
pub(super) fn preg(a: u32) -> u32 { reg::plane(a, PIPE, PL) }

pub(super) fn gsm_write(idx: u64, pte: u64) {
    let va = GSM_VA.load(Ordering::Acquire);
    if va == 0 || idx >= GSM_ENTRIES.load(Ordering::Relaxed) as u64 { return; }
    unsafe { core::ptr::write_volatile((va + idx as usize * 8) as *mut u64, pte) }
}

fn ggtt_of(slot: usize) -> u64 { WIN_BASE + slot as u64 * SLOT }

/// Write back the CPU/GPU LLC lines of `[phys, phys + len)` so the display
/// engine (which does not snoop the LLC) reads what was rendered.
/// CLFLUSHOPT where the CPU has it (Skylake+: the flushes overlap instead of
/// serialising; ~4x faster on a 3 MiB frame), fenced by the MFENCE.
pub(super) fn flush_for_display(phys: u64, len: u64) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        static OPT: AtomicU32 = AtomicU32::new(2);
        let mut has = OPT.load(Ordering::Relaxed);
        if has == 2 {
            has = (core::arch::x86_64::__cpuid_count(7, 0).ebx >> 23) & 1; // CPUID.7.0:EBX.CLFLUSHOPT
            OPT.store(has, Ordering::Relaxed);
        }
        let va = mm::phys_to_virt(phys as usize);
        let mut p = va & !63;
        let end = va + len as usize;
        if has == 1 {
            while p < end {
                // clflushopt = 66 0F AE /7
                core::arch::asm!(".byte 0x66", "clflush [{0}]", in(reg) p, options(nostack, preserves_flags));
                p += 64;
            }
        } else {
            while p < end {
                core::arch::asm!("clflush [{0}]", in(reg) p, options(nostack, preserves_flags));
                p += 64;
            }
        }
        core::arch::asm!("mfence", options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "x86_64"))]
    { let _ = (phys, len); }
}

/// Program plane 1A and arm the flip (PLANE_SURF last: it latches the rest at
/// the next vblank). With `wait`, return once PLANE_SURFLIVE shows it (or
/// after 50 ms).
///
/// `native`: the pipe state to be in (native mode / GOP mode). When it
/// differs from the current one the transition (PIPESRC, pipe scaler, plane
/// size/position/watermarks) is written in the same vblank-evaded window,
/// before PLANE_SURF.
fn program(ctl: u32, stride: u32, offset: u32, surf: u32, wait: bool, native: bool) -> bool {
    let m = mmio();
    if m.va == 0 { return true; }
    let switch = NATIVE_ON.load(Ordering::Acquire) != native && NATIVE_W.load(Ordering::Relaxed) != 0;
    let flags = if switch { evade_and_cli() } else { 0 };
    if switch {
        NATIVE_ON.store(native, Ordering::Release);
        if native {
            let (w, h) = (NATIVE_W.load(Ordering::Relaxed), NATIVE_H.load(Ordering::Relaxed));
            m.w(reg::pipe(reg::PIPESRC_A, PIPE), (w - 1) << 16 | (h - 1));
            m.w(reg::PS_CTRL_1A, 0);
            m.w(reg::PS_WIN_POS_1A, 0);
            m.w(reg::PS_WIN_SZ_1A, 0); // arms the scaler update
            m.w(preg(reg::PLANE_POS_1_A), 0);
            m.w(preg(reg::PLANE_SIZE_1_A), (h - 1) << 16 | (w - 1));
            if NAT_WM_SET.load(Ordering::Relaxed) { write_wm(&NAT_WM); }
        } else {
            m.w(reg::pipe(reg::PIPESRC_A, PIPE), ORIG_PIPESRC.load(Ordering::Relaxed));
            m.w(reg::PS_CTRL_1A, ORIG_PS_CTRL.load(Ordering::Relaxed));
            m.w(reg::PS_WIN_POS_1A, ORIG_PS_POS.load(Ordering::Relaxed));
            m.w(reg::PS_WIN_SZ_1A, ORIG_PS_SZ.load(Ordering::Relaxed));
            m.w(preg(reg::PLANE_POS_1_A), ORIG_PPOS.load(Ordering::Relaxed));
            m.w(preg(reg::PLANE_SIZE_1_A), ORIG_PSIZE.load(Ordering::Relaxed));
            if NAT_WM_SET.load(Ordering::Relaxed) { write_wm(&ORIG_WM); }
        }
    }
    m.w(preg(reg::PLANE_CTL_1_A), ctl);
    m.w(preg(reg::PLANE_STRIDE_1_A), stride);
    m.w(preg(reg::PLANE_OFFSET_1_A), offset);
    m.w(preg(reg::PLANE_SURF_1_A), surf);
    if switch {
        irq_restore(flags);
        FASTSETS.fetch_add(1, Ordering::Relaxed);
    }
    if !wait || FAKE.load(Ordering::Relaxed) { return true; }
    // Bounded by time, not by iterations: msleep(1) may sleep a whole tick.
    let t0 = super::now_ns();
    while super::now_ns().wrapping_sub(t0) < 50_000_000 {
        if m.r(preg(PLANE_SURFLIVE_1_A)) & !0xFFF == surf & !0xFFF { return true; }
        sched::kthread::msleep(1);
    }
    if LATE.fetch_add(1, Ordering::Relaxed) < 4 {
        ilog!("kms: flip to GGTT 0x{:08x} not latched after 50 ms (SURFLIVE=0x{:08x})", surf, m.r(preg(PLANE_SURFLIVE_1_A)));
    }
    false
}

fn write_wm(v: &[AtomicU32; 9]) {
    let m = mmio();
    for i in 0..WM_LEVELS { m.w(preg(PLANE_WM_1_A) + 4 * i as u32, v[i].load(Ordering::Relaxed)); }
    m.w(preg(PLANE_WM_TRANS_1_A), v[8].load(Ordering::Relaxed));
}

#[cfg(target_arch = "x86_64")]
fn irq_save() -> u64 {
    let f: u64;
    unsafe { core::arch::asm!("pushfq; pop {0}; cli", out(reg) f, options(nomem, preserves_flags)) }
    f
}
#[cfg(target_arch = "x86_64")]
fn irq_restore(f: u64) {
    if f & 1 << 9 != 0 { unsafe { core::arch::asm!("sti", options(nomem, nostack)) } }
}
#[cfg(not(target_arch = "x86_64"))]
fn irq_save() -> u64 { 0 }
#[cfg(not(target_arch = "x86_64"))]
fn irq_restore(_f: u64) {}

/// Vblank evasion (as Linux's intel_pipe_update_start): wait until pipe A's
/// scan line is clear of the last `VBL_MARGIN` lines before vblank start,
/// then return with interrupts off so the double-buffered writes all land
/// in one frame. Bounded (2 frames' worth of 50 ms); the fake device and
/// the panic path (interrupts already off) skip the wait.
fn evade_and_cli() -> u64 {
    let m = mmio();
    let vs = VBL_START.load(Ordering::Relaxed);
    if FAKE.load(Ordering::Relaxed) || vs == 0 { return irq_save(); }
    let lo = vs.saturating_sub(VBL_MARGIN.load(Ordering::Relaxed));
    let t0 = super::now_ns();
    loop {
        let f = irq_save();
        let dsl = m.r(reg::PIPEDSL_A) & 0x1FFF;
        if dsl < lo || super::now_ns().wrapping_sub(t0) > 50_000_000 { return f; }
        irq_restore(f);
        core::hint::spin_loop();
    }
}

/// Scanout back to the GOP surface (the console's). Lock-free: MMIO writes
/// only, safe from the panic path.
pub fn console_restore() {
    // A queued real-vblank flip must not land on the console afterwards,
    // and the hardware cursor goes with the compositor (both lock-free).
    super::vblank::console_gen_bump();
    super::cursor::console_hide();
    if ON_SCREEN.swap(0, Ordering::AcqRel) == 0 { return; }
    // A counter only (lock-free, panic-safe); kmsdebug's flip lines report it.
    RESTORES.fetch_add(1, Ordering::Relaxed);
    let _ = program(ORIG_CTL.load(Ordering::Relaxed), ORIG_STRIDE.load(Ordering::Relaxed),
            ORIG_OFFSET.load(Ordering::Relaxed), ORIG_SURF.load(Ordering::Relaxed), false, false);
}

pub(super) fn mode() -> (u32, u32, u32) { (MODE_W.load(Ordering::Acquire), MODE_H.load(Ordering::Acquire), REFRESH.load(Ordering::Relaxed)) }

fn err(e: i32) -> DriverError { DriverError::Errno(e) }

fn fb_create(req: &KmsFbReq, open_id: u32) -> Option<Result<u32, DriverError>> {
    let bo = uapi::kms_bo_get(open_id, req.handle)?;
    let r = fb_create_inner(req, &bo);
    if let Err(e) = r {
        uapi::kms_bo_put(bo.obj);
        ilog!("kms: open {} ADDFB handle {} {}x{} pitch {} fourcc 0x{:08x} mod {:?} refused (errno {})",
              open_id, req.handle, req.width, req.height, req.pitch, req.fourcc, req.modifier, e);
    }
    Some(r.map_err(err))
}

fn fb_create_inner(req: &KmsFbReq, bo: &uapi::KmsBo) -> Result<u32, i32> {
    if req.fourcc != FOURCC_XR24 && req.fourcc != FOURCC_AR24 { return Err(EINVAL); }
    match req.modifier {
        Some(0) => {}
        None if bo.tiling == 0 => {}
        _ => return Err(EINVAL), // IN_FORMATS offers LINEAR only
    }
    // Smaller than the mode is fine (a cursor image); a flip checks that the
    // framebuffer covers the mode.
    if req.width == 0 || req.height == 0 || req.width > 8192 || req.height > 8192 { return Err(EINVAL); }
    // gen9 linear: stride in 64-byte units, at most 32 KiB.
    if req.pitch < req.width * 4 || req.pitch % 64 != 0 || req.pitch > 32768 || req.offset % 4096 != 0 { return Err(EINVAL); }
    let need = req.offset as u64 + req.pitch as u64 * req.height as u64;
    if need > bo.size || need > SLOT { return Err(EINVAL); }
    let pages = (need + 4095) / 4096;
    let mut fbs = FBS.lock();
    let slot = fbs.iter().position(|f| f.is_none()).ok_or(ENOSPC)?;
    let base = ggtt_of(slot) / 4096;
    for i in 0..pages { gsm_write(base + i, hw::ggtt_pte(bo.phys + i * 4096)); }
    mmio().w(reg::GFX_FLSH_CNTL_GEN6, 1);
    if debug() && dbg_ok() {
        let va = GSM_VA.load(Ordering::Acquire);
        let pte0 = if va == 0 { 0 } else { unsafe { core::ptr::read_volatile((va + base as usize * 8) as *const u64) } };
        ilog!("kms: ADDFB fb {} obj {} phys 0x{:x} {}x{} pitch {} -> GGTT 0x{:08x} ({} pages, PTE0 0x{:x})",
              slot + 1, bo.obj, bo.phys, req.width, req.height, req.pitch, ggtt_of(slot), pages, pte0);
    }
    fbs[slot] = Some(Fb { obj: bo.obj, phys: bo.phys, pages, pitch: req.pitch, offset: req.offset, width: req.width, height: req.height });
    Ok(slot as u32 + 1)
}

fn fb_destroy(token: u32) {
    let slot = token.wrapping_sub(1) as usize;
    if slot >= NSLOT { return; }
    // A real-vblank flip of (or away from) it still in flight: dropped if
    // not armed yet, waited out if armed (the plane may still read it).
    super::vblank::quiesce(token);
    if ON_SCREEN.load(Ordering::Acquire) == token {
        // Never unbind what the plane is reading: back to the GOP surface,
        // and wait for that flip to latch.
        ON_SCREEN.store(0, Ordering::Release);
        if debug() { ilog!("kms: RMFB of the on-screen fb {}: back to the GOP surface", token); }
        let _ = program(ORIG_CTL.load(Ordering::Relaxed), ORIG_STRIDE.load(Ordering::Relaxed),
                ORIG_OFFSET.load(Ordering::Relaxed), ORIG_SURF.load(Ordering::Relaxed), true, false);
    }
    let fb = FBS.lock()[slot].take();
    let Some(fb) = fb else { return };
    if debug() && dbg_ok() { ilog!("kms: RMFB fb {} obj {}", token, fb.obj); }
    let base = ggtt_of(slot) / 4096;
    for i in 0..fb.pages { gsm_write(base + i, 0); }
    mmio().w(reg::GFX_FLSH_CNTL_GEN6, 1);
    uapi::kms_bo_put(fb.obj);
}

/// A flip, validated and computed: everything `arm` writes, and what the
/// fence wait / clflush before it need.
#[derive(Clone, Copy)]
pub(super) struct FlipRegs {
    pub token: u32,
    pub obj: u32,
    pub fb: FbInfo,
    pub src_x: u32,
    pub src_y: u32,
    pub ctl: u32,
    pub stride: u32,
    pub offset: u32,
    pub surf: u32,
}

#[derive(Clone, Copy)]
pub(super) struct FbInfo { pub phys: u64, pub pitch: u32, pub offset: u32, pub width: u32, pub height: u32 }

/// (phys of byte 0 of the framebuffer, pitch, width, height) of a backend
/// framebuffer (the cursor's image source).
pub(super) fn fb_info(token: u32) -> Option<FbInfo> {
    let slot = token.wrapping_sub(1) as usize;
    let fb = FBS.lock().get(slot).copied().flatten()?;
    Some(FbInfo { phys: fb.phys, pitch: fb.pitch, offset: fb.offset, width: fb.width, height: fb.height })
}

/// Validate a flip of `token` from (src_x, src_y) and compute its registers.
pub(super) fn prepare(token: u32, src_x: u32, src_y: u32) -> Result<FlipRegs, DriverError> {
    let slot = token.wrapping_sub(1) as usize;
    let fb = FBS.lock().get(slot).copied().flatten().ok_or(err(EINVAL))?;
    let (mw, mh, _) = mode();
    if src_x + mw > fb.width || src_y + mh > fb.height { return Err(err(EINVAL)); }
    let ctl = (ORIG_CTL.load(Ordering::Relaxed) & !(0xF << 24 | 0x7 << 10)) | 1 << 31 | 0x4 << 24; // RGB 8888, linear
    let surf = (ggtt_of(slot) + fb.offset as u64) as u32;
    Ok(FlipRegs {
        token, obj: fb.obj,
        fb: FbInfo { phys: fb.phys, pitch: fb.pitch, offset: fb.offset, width: fb.width, height: fb.height },
        src_x, src_y, ctl, stride: fb.pitch / 64, offset: src_y << 16 | src_x, surf,
    })
}

/// Has the frame's rendering finished (implicit sync), without waiting?
pub(super) fn fence_ready(r: &FlipRegs) -> bool { uapi::kms_bo_wait(r.obj, 0) }

/// Wait (bounded) for the frame's rendering. False on timeout.
pub(super) fn fence_wait(r: &FlipRegs, timeout_ns: u64) -> bool { uapi::kms_bo_wait(r.obj, timeout_ns) }

pub(super) fn needs_flush(r: &FlipRegs) -> bool { uapi::kms_bo_needs_clflush(r.obj) }

/// clflush the rows the plane will scan out.
pub(super) fn flush(r: &FlipRegs) {
    uapi::kms_bo_take_dirty(r.obj);
    FLUSHED_FLIPS.fetch_add(1, Ordering::Relaxed);
    let (_, mh, _) = mode();
    let first = r.fb.offset as u64 + r.src_y as u64 * r.fb.pitch as u64;
    flush_for_display(r.fb.phys + first, mh as u64 * r.fb.pitch as u64);
}

/// What `arm` saw, for `after_arm`'s log lines.
#[derive(Clone, Copy)]
pub(super) struct Armed { pub prev: u32, pub was_native: bool, pub latched: bool }

/// Write the flip (PLANE_SURF last; the first one fastsets to the native
/// mode). With `wait`, return once PLANE_SURFLIVE shows it. No logging: the
/// real-vblank path calls this under its IRQ-safe lock.
pub(super) fn arm(r: &FlipRegs, wait: bool) -> Armed {
    let prev = ON_SCREEN.swap(r.token, Ordering::AcqRel);
    let native = NATIVE_W.load(Ordering::Relaxed) != 0;
    let was_native = NATIVE_ON.load(Ordering::Acquire);
    let latched = program(r.ctl, r.stride, r.offset, r.surf, wait, native);
    Armed { prev, was_native, latched }
}

/// The log lines (and kmsdump) of a flip, outside any lock. `t` = (fence
/// waited from, fence done, flushed) for the kmsdebug timing line.
pub(super) fn after_arm(r: &FlipRegs, a: &Armed, t: (u64, u64, u64)) {
    let (mw, mh, _) = mode();
    let native = NATIVE_W.load(Ordering::Relaxed) != 0;
    if native && !a.was_native {
        let m = mmio();
        ilog!("kms: fastset to native {}x{} (fastset #{}): PIPESRC 0x{:08x} PS_CTRL1 0x{:08x} PS_WIN_SZ1 0x{:08x} PLANE_SIZE 0x{:08x} latched={} underruns {}",
              mw, mh, FASTSETS.load(Ordering::Relaxed), m.r(reg::pipe(reg::PIPESRC_A, PIPE)), m.r(reg::PS_CTRL_1A),
              m.r(reg::PS_WIN_SZ_1A), m.r(preg(reg::PLANE_SIZE_1_A)), a.latched, underruns());
    }
    super::rps::flip_stat(t.0, t.1, t.2, super::now_ns(), a.latched);
    if debug() {
        let t3 = super::now_ns();
        let ur = underruns();
        let slow = t.1 - t.0 > 20_000_000 || !a.latched || a.prev == 0;
        if slow || ur != 0 || dbg_ok() {
            let m = mmio();
            let (pres, perr, vtr) = crate::drm_device_interface::ext_present_stats();
            ilog!("kms: flip #{} fb {} (prev {}) fence {} us flush {} us latch {} us{} FRMCOUNT {} underruns {} console-restores {} presents/errors/VT-repaints {}/{}/{} PSR_STATUS 0x{:08x}",
                  FLIPS.load(Ordering::Relaxed) + 1, r.token, a.prev, (t.1 - t.0) / 1000, (t.2 - t.1) / 1000, t3.saturating_sub(t.2) / 1000,
                  if a.latched { "" } else { " NOT LATCHED" }, m.r(reg::pipe(reg::PIPE_FRMCOUNT_A, PIPE)),
                  UNDERRUNS.load(Ordering::Relaxed), RESTORES.load(Ordering::Relaxed), pres, perr, vtr, m.r(EDP_PSR_STATUS));
        }
    }
    let n = FLIPS.fetch_add(1, Ordering::Relaxed) + 1;
    if DUMP_AT.iter().any(|d| { let d = d.load(Ordering::Relaxed); d != 0 && (n == d || n == d + 1) }) {
        let slot = r.token.wrapping_sub(1) as usize;
        if let Some(fb) = FBS.lock().get(slot).copied().flatten() { dump(n, r.token, &fb, r.src_x, r.src_y, mw, mh); }
    }
    if n == 1 {
        ilog!("kms: first flip: fb {} ({}x{} pitch {}) at GGTT 0x{:08x}", r.token, r.fb.width, r.fb.height, r.fb.pitch, r.surf);
    }
}

/// The synchronous present (SETCRTC, and every flip with `i915.vblank=0`):
/// fence wait, clflush, flip, wait for the latch — all inside the ioctl.
fn present(token: u32, src_x: u32, src_y: u32, _rects: Option<&[(i32, i32, i32, i32)]>) -> Result<(), DriverError> {
    let r = prepare(token, src_x, src_y)?;
    // Never overtake a real-vblank flip still in flight.
    super::vblank::wait_idle_quiet(250_000_000);
    // Implicit sync: the frame's rendering must be done (bounded: a hung
    // context is reset by hangcheck long before this gives up).
    let t0 = super::now_ns();
    if !fence_wait(&r, 1_000_000_000) { return Err(err(ETIME)); }
    let t1 = super::now_ns();
    if needs_flush(&r) { flush(&r); }
    let t2 = super::now_ns();
    let a = arm(&r, true);
    after_arm(&r, &a, (t0, t1, t2));
    Ok(())
}

/// `i915.kmsdump`: flip `n`'s scanout as a binary PPM (P6), base64, between
/// `kmsdump begin`/`kmsdump end` lines (`scripts/kmsdump-extract.py`).
fn dump(n: u32, token: u32, fb: &Fb, src_x: u32, src_y: u32, mw: u32, mh: u32) {
    let div = DUMP_DIV.load(Ordering::Relaxed).clamp(1, 16);
    let (ow, oh) = (mw / div, mh / div);
    let hdr = alloc::format!("P6\n{} {}\n255\n", ow, oh);
    let mut img = alloc::vec::Vec::with_capacity(hdr.len() + (ow * oh * 3) as usize);
    img.extend_from_slice(hdr.as_bytes());
    let base = mm::phys_to_virt(fb.phys as usize) + fb.offset as usize + src_y as usize * fb.pitch as usize + src_x as usize * 4;
    let mut row = alloc::vec![[0u32; 3]; ow as usize];
    for oy in 0..oh {
        for a in row.iter_mut() { *a = [0; 3]; }
        for dy in 0..div {
            let line = base + ((oy * div + dy) * fb.pitch) as usize;
            for x in 0..ow * div {
                let px = unsafe { core::ptr::read_volatile((line + x as usize * 4) as *const u32) };
                let a = &mut row[(x / div) as usize];
                a[0] += px >> 16 & 0xFF; a[1] += px >> 8 & 0xFF; a[2] += px & 0xFF;
            }
        }
        for a in row.iter() { for c in a { img.push((c / (div * div)) as u8); } }
    }
    ilog!("kmsdump begin flip {} fb {} obj {} phys 0x{:x} {}x{} /{} -> {}x{} PPM, {} bytes base64 (CPU view)",
          n, token, fb.obj, fb.phys, mw, mh, div, ow, oh, img.len());
    // The log partition on hardware; also the serial console on the fake
    // device (QEMU), where there is no log partition to read it from.
    super::probe::b64_to(&img, FAKE.load(Ordering::Relaxed));
    ilog!("kmsdump end flip {}", n);
}

static MODIFIERS: [u64; 1] = [0 /* DRM_FORMAT_MOD_LINEAR */];

static BACKEND: KmsBackend = KmsBackend {
    name: "i915-plane",
    mode,
    modifiers: &MODIFIERS,
    fb_create,
    fb_destroy,
    present,
    console_restore: Some(console_restore),
};

/// Derive the native mode from the transcoder feeding pipe A; store the
/// timings, refresh and native watermarks. `None`: stay at the GOP mode.
fn native_setup(m: &Mmio, gw: u32, gh: u32) -> Option<(u32, u32)> {
    // The transcoder feeding pipe A: eDP when enabled with input pipe A,
    // else transcoder A.
    let edp = m.r(reg::TRANS_DDI_FUNC_CTL_EDP);
    let edp_on = m.r(reg::TRANSCONF_EDP) >> 31 != 0 && edp >> 31 != 0 && matches!((edp >> 12) & 7, 0 | 4);
    let (tbase, ddi, name) = if edp_on {
        (reg::TRANS_HTOTAL_EDP, edp, "eDP")
    } else if m.r(reg::TRANSCONF_A) >> 31 != 0 {
        (reg::TRANS_HTOTAL_A, m.r(reg::TRANS_DDI_FUNC_CTL_A), "A")
    } else {
        log("kms: native: no enabled transcoder feeds pipe A; staying at the GOP mode");
        return None;
    };
    let (hact, htot) = hw::timing_pair(m.r(tbase));
    let (vact, vtot) = hw::timing_pair(m.r(tbase + 0xC));
    let (vbs, _) = hw::timing_pair(m.r(tbase + 0x10));
    // Pixel clock: DP port clock (DPLL link rate of the DDI's PLL) * LINK_M / LINK_N.
    let port = (ddi >> 28) & 7;
    let c2 = m.r(reg::DPLL_CTRL2);
    let pll = if c2 & 1 << (port * 3) != 0 { (c2 >> (port * 3 + 1)) & 3 } else { 0 };
    let rate = (m.r(reg::DPLL_CTRL1) >> (pll * 6 + 1)) & 7;
    let port_khz: u64 = match rate { 0 => 540_000, 1 => 270_000, 2 => 162_000, 3 => 324_000, 4 => 216_000, 5 => 432_000, _ => 0 };
    let lm = (m.r(tbase + 0x40) & 0xFF_FFFF) as u64;
    let ln = (m.r(tbase + 0x44) & 0xFF_FFFF) as u64;
    let clk = if ln != 0 && (ddi >> 24) & 7 == 2 { port_khz * lm / ln } else { 0 }; // DP SST only
    let tot = htot as u64 * vtot as u64;
    let refresh = if clk != 0 { ((clk * 1000 + tot / 2) / tot) as u32 } else { 60 };
    let refresh = if (20..=240).contains(&refresh) { refresh } else { 60 };
    REFRESH.store(refresh, Ordering::Relaxed);
    VBL_START.store(vbs.min(vtot), Ordering::Relaxed);
    VTOTAL.store(vtot, Ordering::Relaxed);
    let line_ns = if clk != 0 { htot as u64 * 1_000_000 / clk } else { 1_000_000_000 / (refresh as u64 * vtot.max(1) as u64) };
    LINE_NS.store(line_ns.min(1_000_000) as u32, Ordering::Relaxed);
    // 100 us of lines (Linux's evasion window), at least 8.
    let margin = if clk != 0 { (100 * clk / (htot as u64 * 1000)) as u32 + 1 } else { 16 };
    VBL_MARGIN.store(margin.max(8), Ordering::Relaxed);
    let ps = m.r(reg::PS_CTRL_1A);
    let buf = m.r(preg(PLANE_BUF_CFG_1_A));
    let ddb = if buf == 0 { 0 } else { ((buf >> 16) & 0x7FF).saturating_sub(buf & 0x7FF) + 1 };
    ilog!("kms: native: transcoder {} active {}x{} total {}x{} vblank start {} (evasion {} lines); DDI port {} DPLL{} rate {} port clock {} kHz LINK M/N {}/{} -> pixel clock {} kHz, {} Hz; GOP {}x{} PS_CTRL1 0x{:08x} WIN_POS 0x{:08x} WIN_SZ 0x{:08x}; plane 1A DDB {} blocks",
          name, hact, vact, htot, vtot, vbs, VBL_MARGIN.load(Ordering::Relaxed), (b'A' + port as u8) as char, pll, rate, port_khz, lm, ln, clk, refresh,
          gw, gh, ps, m.r(reg::PS_WIN_POS_1A), m.r(reg::PS_WIN_SZ_1A), ddb);
    if opt("i915.native") == Some(0) { log("kms: native: i915.native=0: staying at the GOP mode (scaled)"); return None; }
    if (hact, vact) == (gw, gh) { return None; }
    if hact < 640 || vact < 480 || hact > 4096 || vact > 4096 || htot <= hact || vtot <= vact {
        ilog!("kms: native: implausible timings {}x{} (total {}x{}); staying at the GOP mode", hact, vact, htot, vtot);
        return None;
    }
    if ps >> 31 == 0 {
        ilog!("kms: native: GOP mode {}x{} != transcoder {}x{} but pipe scaler 1 is off; staying at the GOP mode", gw, gh, hact, vact);
        return None;
    }
    // Watermarks: Linux's skl_compute_wm from the pcode memory latencies
    // when they can be read (`i915.native_wm=2` forces the old scaling).
    let lat = if FAKE_PROBE.load(Ordering::Relaxed) || opt("i915.native_wm") == Some(2) { None } else { skl_wm_latency(m) };
    if let Some(lat) = lat {
        let nat = skl_plane_wm(&lat, clk, htot, hact, ddb);
        ilog!("kms: native: pcode WM latencies {:?} us; skl_compute_wm for {}x{} linear XRGB8888, {} kHz, htotal {}, DDB {} blocks -> levels {:08x?}",
              lat, hact, vact, clk, htot, ddb, nat);
        if nat[0] != 0 && clk != 0 {
            let keep_wm = opt("i915.native_wm") == Some(0);
            for i in 0..9 { NAT_WM[i].store(nat[i], Ordering::Relaxed); }
            NAT_WM_SET.store(!keep_wm, Ordering::Relaxed);
            for (i, &l) in lat.iter().enumerate() { WM_LAT[i].store(l, Ordering::Relaxed); }
            skl_sagv_check(m, &lat, &nat);
            NATIVE_W.store(hact, Ordering::Relaxed);
            NATIVE_H.store(vact, Ordering::Relaxed);
            return Some((hact, vact));
        }
        log("kms: native: skl_compute_wm found no level 0 (or no pixel clock); falling back to the scaled GOP watermarks");
    }
    // Fallback: the GOP's, scaled by linear blocks-per-line (512-byte
    // blocks, 4 bytes/pixel); a level that no longer fits the DDB is
    // disabled with every level above it (Linux: levels are monotonic).
    let bpl_old = ((gw * 4 + 511) / 512).max(1);
    let bpl_new = (hact * 4 + 511) / 512;
    let mut off = false;
    let mut nat = [0u32; 9];
    for i in 0..9 {
        let v = ORIG_WM[i].load(Ordering::Relaxed);
        let blocks = v & 0x3FF;
        let mut nb = (blocks * bpl_new + bpl_old - 1) / bpl_old;
        if i == 0 && (v >> 31 == 0 || blocks == 0) { nb = 2 * bpl_new + 1; }
        let lines = ((v >> 14) & 0x1F).max(if i == 0 { 2 } else { 0 });
        let en = (i == 0 || v >> 31 != 0) && !(i < 8 && off);
        if !en || nb > ddb || nb > 0x3FF { if i < 8 { off = true; } continue; }
        nat[i] = 1 << 31 | (v & 1 << 30) | lines << 14 | nb;
    }
    if nat[0] == 0 {
        ilog!("kms: native: level-0 watermark ({} blocks/line) does not fit the {}-block DDB; staying at the GOP mode", bpl_new, ddb);
        return None;
    }
    let keep_wm = opt("i915.native_wm") == Some(0);
    for i in 0..9 { NAT_WM[i].store(nat[i], Ordering::Relaxed); }
    NAT_WM_SET.store(!keep_wm, Ordering::Relaxed);
    let orig: alloc::vec::Vec<u32> = ORIG_WM.iter().map(|a| a.load(Ordering::Relaxed)).collect();
    ilog!("kms: native: plane 1A watermarks GOP {:08x?} -> native {:08x?} (blocks/line {} -> {}){}",
          orig, nat, bpl_old, bpl_new, if keep_wm { "; i915.native_wm=0: keeping the GOP's" } else { "" });
    NATIVE_W.store(hact, Ordering::Relaxed);
    NATIVE_H.store(vact, Ordering::Relaxed);
    Some((hact, vact))
}

/// Set by `init` for the fake device (RAM registers: no pcode).
static FAKE_PROBE: AtomicBool = AtomicBool::new(false);
/// The pcode WM latencies (us) in use, for /proc/i915 and the log.
pub(super) static WM_LAT: [AtomicU32; 8] = [const { AtomicU32::new(0) }; 8];

const GEN6_PCODE_MAILBOX: u32 = 0x138124;
const GEN6_PCODE_DATA: u32 = 0x138128;
const GEN6_PCODE_DATA1: u32 = 0x13812C;
const GEN6_PCODE_READY: u32 = 1 << 31;
const GEN9_PCODE_READ_MEM_LATENCY: u32 = 0x6;
const GEN9_PCODE_SAGV_CONTROL: u32 = 0x21;

/// sandybridge_pcode_read/write: one mailbox transaction, 10 ms bound.
fn pcode(m: &Mmio, mbox: u32, val: u32) -> Option<u32> {
    if m.r(GEN6_PCODE_MAILBOX) & GEN6_PCODE_READY != 0 { return None; }
    m.w(GEN6_PCODE_DATA, val);
    m.w(GEN6_PCODE_DATA1, 0);
    m.w(GEN6_PCODE_MAILBOX, GEN6_PCODE_READY | mbox);
    let t0 = super::now_ns();
    loop {
        let v = m.r(GEN6_PCODE_MAILBOX);
        if v & GEN6_PCODE_READY == 0 {
            // gen7_check_mailbox_status: error code in the low byte.
            return if v & 0xFF != 0 { None } else { Some(m.r(GEN6_PCODE_DATA)) };
        }
        if super::now_ns() - t0 > 10_000_000 { return None; }
        core::hint::spin_loop();
    }
}

/// skl_read_wm_latency: levels 0..3 then 4..7, one byte each; a zero level
/// above 0 disables it and every level above; WaWmMemoryReadLatency adds
/// 2 us to every level when level 0 reads 0.
fn skl_wm_latency(m: &Mmio) -> Option<[u32; 8]> {
    let a = pcode(m, GEN9_PCODE_READ_MEM_LATENCY, 0)?;
    let b = pcode(m, GEN9_PCODE_READ_MEM_LATENCY, 1)?;
    let mut w = [0u32; 8];
    for i in 0..4 { w[i] = (a >> (8 * i)) & 0xFF; w[4 + i] = (b >> (8 * i)) & 0xFF; }
    if let Some(z) = (1..8).find(|&i| w[i] == 0) { for v in w[z..].iter_mut() { *v = 0; } }
    if w[0] == 0 { for i in 0..8 { if i > 0 && w[i] == 0 { break; } w[i] += 2; } }
    if w.iter().all(|&v| v == 0) { return None; }
    Some(w)
}

/// skl_compute_plane_wm + skl_allocate_plane_ddb's level check, for one
/// linear (not X/Y-tiled, not rotated, unscaled) 4-byte-per-pixel plane on
/// display version 9 without IPC: levels 0..7 then WM_TRANS (off: gen9 has
/// a transition WM only with IPC). `clk` pixel rate kHz, `ddb` the plane's
/// DDB blocks. Fixed16 math as Linux's (values * 65536).
pub(super) fn skl_plane_wm(lat: &[u32; 8], clk: u64, htot: u32, width: u32, ddb: u32) -> [u32; 9] {
    const CPP: u64 = 4;
    const BLK: u64 = 512;
    let mut out = [0u32; 9];
    if clk == 0 || htot == 0 || ddb == 0 { return out; }
    let htot = htot as u64;
    let bytes_per_line = width as u64 * CPP;
    // Linear: DIV_ROUND_UP(bytes, 512) + 1.
    let pbpl = (bytes_per_line + BLK - 1) / BLK + 1;
    let linetime_us = (htot * 1000 + clk - 1) / clk;
    for level in 0..8 {
        let latency = lat[level] as u64;
        if latency == 0 { break; }
        // skl_wm_method1: latency * rate * cpp / (1000 * 512), fixed16.
        let m1 = (latency * clk * CPP << 16) / (1000 * BLK);
        // skl_wm_method2: DIV_ROUND_UP(latency * rate, htotal * 1000) * pbpl.
        let m2 = ((latency * clk + htot * 1000 - 1) / (htot * 1000)) * (pbpl << 16);
        let sel = if CPP * htot / BLK < 1 && bytes_per_line / BLK < 1 { m2 }
                  else if latency >= linetime_us { m1.min(m2) } // display version 9
                  else { m1 };
        let mut blocks = ((sel + 0xFFFF) >> 16) + 1;
        if level >= 1 { blocks += 1; } // Display WA #1126 (not Y-tiled)
        let lines = (sel + (pbpl << 16) - 1) / (pbpl << 16);
        let min_ddb = blocks + (blocks + 9) / 10;
        // Bspec: lines <= 31; a level must fit the DDB (else it and every
        // level above are disabled).
        if lines > 31 || min_ddb > ddb as u64 || blocks > 0x3FF { break; }
        out[level] = 1 << 31 | (lines as u32) << 14 | blocks as u32;
    }
    out
}

/// skl_can_enable_sagv (display version 9): SAGV may stay on only if the
/// highest enabled level's latency covers the 30 us SAGV block time; else
/// Linux disables it through pcode (GEN9_SAGV_DISABLE) to avoid underruns.
/// `i915.sagv=1` leaves it alone.
fn skl_sagv_check(m: &Mmio, lat: &[u32; 8], nat: &[u32; 9]) {
    let top = (0..8).rev().find(|&i| nat[i] >> 31 != 0).unwrap_or(0);
    if lat[top] >= 30 {
        ilog!("kms: native: SAGV: top enabled WM level {} latency {} us >= 30 us block time: SAGV may stay as firmware left it", top, lat[top]);
        return;
    }
    if opt("i915.sagv") == Some(1) {
        ilog!("kms: native: SAGV: top level {} latency {} us < 30 us but i915.sagv=1: left alone (underrun risk)", top, lat[top]);
        return;
    }
    let r = pcode(m, GEN9_PCODE_SAGV_CONTROL, 0);
    ilog!("kms: native: SAGV: top level {} latency {} us < 30 us block time: disabled via pcode ({})", top, lat[top],
          if r.is_some() { "ok" } else { "pcode refused or not supported" });
}

/// After the UAPI registered: verify the GOP-left plane and register the
/// KMS seam. Only with `i915.kms=1`.
pub(super) fn init(d: &Device, env: &BootEnv) {
    if opt("i915.kms") != Some(1) { return; }
    FAKE_PROBE.store(d.fake, Ordering::Relaxed);
    let g = match d.gsm { Some(g) => g, None => { log("kms: no GGTT mapping; KMS not registered"); return; } };
    if d.gmch.ggtt_size < WIN_BASE + NSLOT as u64 * SLOT { log("kms: GGTT too small; KMS not registered"); return; }
    let m = &d.mmio;
    let (w, h) = (env.fb_width, env.fb_height);
    if d.fake {
        // RAM registers: pretend the GOP left plane 1A on the boot framebuffer.
        m.w(preg(reg::PLANE_CTL_1_A), 0x8400_0000);
        m.w(preg(reg::PLANE_STRIDE_1_A), env.fb_pitch / 64);
    } else {
        let ctl = m.r(preg(reg::PLANE_CTL_1_A));
        let (sw, sh) = hw::pipe_src(m.r(reg::pipe(reg::PIPESRC_A, PIPE)));
        let surf = m.r(preg(reg::PLANE_SURF_1_A));
        if ctl & 1 << 31 == 0 || (sw, sh) != (w, h) || d.fb_ggtt != Some(surf as u64 & !0xFFF) || w == 0 || h == 0 {
            ilog!("kms: plane 1A / pipe A not as the GOP left them for the boot framebuffer (CTL 0x{:08x}, PIPESRC {}x{}, SURF 0x{:08x}, fb {}x{} at GGTT {:?}); KMS not registered",
                  ctl, sw, sh, surf, w, h, d.fb_ggtt);
            return;
        }
        if !super::exec::ready() { log("kms: hardware execution is off (i915.exec=1 failed or absent); KMS not registered (GPU rendering only)"); return; }
    }
    if !d.fake {
        let wm: alloc::vec::Vec<u32> = (0..8).map(|i| m.r(preg(PLANE_WM_1_A) + 4 * i)).collect();
        ilog!("kms: eDP PSR_CTL=0x{:08x} PSR_STATUS=0x{:08x} PSR_DEBUG=0x{:08x} PSR2_CTL=0x{:08x}; plane 1A BUF_CFG=0x{:08x} WM={:08x?} WM_TRANS=0x{:08x}; DE pipe A IMR=0x{:08x} IIR=0x{:08x} IER=0x{:08x}",
              m.r(EDP_PSR_CTL), m.r(EDP_PSR_STATUS), m.r(EDP_PSR_DEBUG), m.r(EDP_PSR2_CTL),
              m.r(preg(PLANE_BUF_CFG_1_A)), wm, m.r(preg(PLANE_WM_TRANS_1_A)),
              m.r(DE_PIPE_A_IMR), m.r(DE_PIPE_A_IIR), m.r(DE_PIPE_A_IER));
        // Panel self refresh left on by the firmware: our flips would rely on
        // the hardware tracking alone to leave self refresh (Linux also
        // forces an exit on every frontbuffer flush). Turn it off, as
        // `i915.enable_psr=0` does, unless `i915.kms_psr=1`.
        if opt("i915.kms_psr") != Some(1) {
            for (r, what) in [(EDP_PSR2_CTL, "PSR2"), (EDP_PSR_CTL, "PSR1")] {
                let v = m.r(r);
                if v & EDP_PSR_ENABLE == 0 { continue; }
                m.w(r, v & !EDP_PSR_ENABLE);
                let t0 = super::now_ns();
                while m.r(EDP_PSR_STATUS) >> 29 != 0 && super::now_ns().wrapping_sub(t0) < 50_000_000 {}
                ilog!("kms: {} was left enabled by the firmware: disabled (PSR_STATUS now 0x{:08x}; i915.kms_psr=1 keeps it)",
                      what, m.r(EDP_PSR_STATUS));
            }
        }
        // FBC compresses the plane's frame into stolen memory and only
        // re-reads what its own tracking notices; with raw SURF flips that is
        // stale-content risk. Off unless `i915.kms_fbc=1`.
        let fbc = m.r(ILK_DPFC_CONTROL);
        ilog!("kms: FBC ILK_DPFC_CONTROL=0x{:08x} (enabled={})", fbc, fbc >> 31);
        if fbc & 1 << 31 != 0 && opt("i915.kms_fbc") != Some(1) {
            m.w(ILK_DPFC_CONTROL, fbc & !(1 << 31));
            ilog!("kms: FBC was left enabled by the firmware: disabled (i915.kms_fbc=1 keeps it)");
        }
        if opt("i915.kmsdebug") == Some(1) && m.r(DE_PIPE_A_IER) & PIPE_FIFO_UNDERRUN == 0 {
            m.w(DE_PIPE_A_IIR, PIPE_FIFO_UNDERRUN);
            m.w(DE_PIPE_A_IMR, m.r(DE_PIPE_A_IMR) & !PIPE_FIFO_UNDERRUN);
        }
    }
    DEBUG.store(opt("i915.kmsdebug") == Some(1), Ordering::Relaxed);
    MMIO.store(m.va, Ordering::Release);
    GSM_VA.store(g.va, Ordering::Release);
    GSM_ENTRIES.store(g.entries as usize, Ordering::Release);
    FAKE.store(d.fake, Ordering::Relaxed);
    ORIG_CTL.store(m.r(preg(reg::PLANE_CTL_1_A)), Ordering::Relaxed);
    ORIG_STRIDE.store(m.r(preg(reg::PLANE_STRIDE_1_A)), Ordering::Relaxed);
    ORIG_OFFSET.store(m.r(preg(reg::PLANE_OFFSET_1_A)), Ordering::Relaxed);
    ORIG_SURF.store(m.r(preg(reg::PLANE_SURF_1_A)), Ordering::Relaxed);
    ORIG_PIPESRC.store(m.r(reg::pipe(reg::PIPESRC_A, PIPE)), Ordering::Relaxed);
    ORIG_PS_CTRL.store(m.r(reg::PS_CTRL_1A), Ordering::Relaxed);
    ORIG_PS_POS.store(m.r(reg::PS_WIN_POS_1A), Ordering::Relaxed);
    ORIG_PS_SZ.store(m.r(reg::PS_WIN_SZ_1A), Ordering::Relaxed);
    ORIG_PSIZE.store(m.r(preg(reg::PLANE_SIZE_1_A)), Ordering::Relaxed);
    ORIG_PPOS.store(m.r(preg(reg::PLANE_POS_1_A)), Ordering::Relaxed);
    for i in 0..WM_LEVELS { ORIG_WM[i].store(m.r(preg(PLANE_WM_1_A) + 4 * i as u32), Ordering::Relaxed); }
    ORIG_WM[8].store(m.r(preg(PLANE_WM_TRANS_1_A)), Ordering::Relaxed);
    let (mut mw, mut mh) = (w, h);
    if !d.fake {
        if let Some((nw, nh)) = native_setup(m, w, h) { mw = nw; mh = nh; }
    }
    MODE_W.store(mw, Ordering::Release);
    MODE_H.store(mh, Ordering::Release);
    for (i, k) in ["i915.kmsdump", "i915.kmsdump2"].iter().enumerate() {
        DUMP_AT[i].store(opt(k).unwrap_or(0) as u32, Ordering::Relaxed);
    }
    if let Some(v) = opt("i915.kmsdump_div") { DUMP_DIV.store(v as u32, Ordering::Relaxed); }
    // The halves registered before the backend itself, so the DRM layer
    // never sees the backend without them (lane kmscursor).
    let vbl = super::vblank::init(d);
    let cur = super::cursor::init(d);
    crate::gpu_kms::register(&BACKEND);
    ilog!("kms: flips {}; hardware cursor {}",
          if vbl { "on real vblank (queued, armed after the render fence, event at the latching vblank; i915.vblank=0: synchronous + 60 Hz synthetic events)" }
          else { "synchronous, events on the 60 Hz synthetic vblank" },
          if cur { "on pipe A (64x64 ARGB8888; i915.hwcursor=0: composited)" } else { "off (composited by the compositor)" });
    if (mw, mh) != (w, h) {
        ilog!("kms: reporting the panel's native {}x{}@{} (GOP mode {}x{}): the first flip fastsets pipe A (PIPESRC, scaler off); console_restore returns to the GOP state (i915.native=0 keeps the GOP mode)",
              mw, mh, REFRESH.load(Ordering::Relaxed), w, h);
    }
    ilog!("==== G4a KMS registered: card0 scans out {}x{} on pipe A plane 1A (the GOP mode, no modeset; the pipe scaler keeps the panel mode) by PLANE_SURF flips from a {}x{} MiB GGTT window at 0x{:x}; modifiers LINEAR{}; kms_uncached={} kms_clflush={} ppat_cached={} kms_psr={} kmsdebug={} kmsdump={}/{} ====",
          w, h, NSLOT, SLOT >> 20, WIN_BASE, if d.fake { " (fake device: nothing reaches a screen)" } else { "" },
          opt("i915.kms_uncached").unwrap_or(1), opt("i915.kms_clflush").unwrap_or(2), opt("i915.ppat_cached").unwrap_or(4),
          opt("i915.kms_psr").unwrap_or(0), debug() as u8,
          DUMP_AT[0].load(Ordering::Relaxed), DUMP_AT[1].load(Ordering::Relaxed));
}
