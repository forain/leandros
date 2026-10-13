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
const PLANE_SURFLIVE_1_A: u32 = 0x701AC;
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
const PLANE_WM_1_A: u32 = 0x70240;
const PLANE_WM_TRANS_1_A: u32 = 0x70268;
const PLANE_BUF_CFG_1_A: u32 = 0x7027C;

/// The scanout GGTT window: 16 slots of 32 MiB from 1 GiB (clear of the GOP
/// surface at 0, the CPU aperture, exec's LRC slots and the G2 window at
/// the top of the GGTT). 32 MiB slots are 256 KiB aligned (gen9 linear).
const WIN_BASE: u64 = 1 << 30;
const SLOT: u64 = 32 << 20;
const NSLOT: usize = 16;

const WM_LEVELS: usize = 8;

const FOURCC_XR24: u32 = 0x3432_5258;
const FOURCC_AR24: u32 = 0x3432_5241;

static MMIO: AtomicUsize = AtomicUsize::new(0);
static GSM_VA: AtomicUsize = AtomicUsize::new(0);
static GSM_ENTRIES: AtomicUsize = AtomicUsize::new(0);
static FAKE: AtomicBool = AtomicBool::new(false);
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
static VBL_START: AtomicU32 = AtomicU32::new(0);
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
static UNDERRUNS: AtomicU32 = AtomicU32::new(0);
static RESTORES: AtomicU32 = AtomicU32::new(0);
static DBG_LINES: AtomicU32 = AtomicU32::new(0);

fn debug() -> bool { DEBUG.load(Ordering::Relaxed) }

/// Rate limit for debug lines: the first 64, then one in 256.
fn dbg_ok() -> bool {
    let n = DBG_LINES.fetch_add(1, Ordering::Relaxed);
    n < 64 || n % 256 == 0
}

/// Pipe A FIFO underruns latched since the last call (kmsdebug only).
fn underruns() -> u32 {
    if !debug() || FAKE.load(Ordering::Relaxed) { return 0; }
    let m = mmio();
    if m.r(DE_PIPE_A_IIR) & PIPE_FIFO_UNDERRUN == 0 { return 0; }
    m.w(DE_PIPE_A_IIR, PIPE_FIFO_UNDERRUN);
    UNDERRUNS.fetch_add(1, Ordering::Relaxed) + 1
}

#[derive(Clone, Copy)]
struct Fb { obj: u32, phys: u64, pages: u64, pitch: u32, offset: u32, width: u32, height: u32 }

static FBS: spin::Mutex<[Option<Fb>; NSLOT]> = spin::Mutex::new([None; NSLOT]);

fn mmio() -> Mmio { Mmio { va: MMIO.load(Ordering::Acquire) } }
fn preg(a: u32) -> u32 { reg::plane(a, PIPE, PL) }

fn gsm_write(idx: u64, pte: u64) {
    let va = GSM_VA.load(Ordering::Acquire);
    if va == 0 || idx >= GSM_ENTRIES.load(Ordering::Relaxed) as u64 { return; }
    unsafe { core::ptr::write_volatile((va + idx as usize * 8) as *mut u64, pte) }
}

fn ggtt_of(slot: usize) -> u64 { WIN_BASE + slot as u64 * SLOT }

/// Write back the CPU/GPU LLC lines of `[phys, phys + len)` so the display
/// engine (which does not snoop the LLC) reads what was rendered.
fn flush_for_display(phys: u64, len: u64) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        let va = mm::phys_to_virt(phys as usize);
        let mut p = va & !63;
        let end = va + len as usize;
        while p < end {
            core::arch::asm!("clflush [{0}]", in(reg) p, options(nostack, preserves_flags));
            p += 64;
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
    if ON_SCREEN.swap(0, Ordering::AcqRel) == 0 { return; }
    // A counter only (lock-free, panic-safe); kmsdebug's flip lines report it.
    RESTORES.fetch_add(1, Ordering::Relaxed);
    let _ = program(ORIG_CTL.load(Ordering::Relaxed), ORIG_STRIDE.load(Ordering::Relaxed),
            ORIG_OFFSET.load(Ordering::Relaxed), ORIG_SURF.load(Ordering::Relaxed), false, false);
}

fn mode() -> (u32, u32, u32) { (MODE_W.load(Ordering::Acquire), MODE_H.load(Ordering::Acquire), REFRESH.load(Ordering::Relaxed)) }

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
    let (mw, mh, _) = mode();
    if req.width < mw || req.height < mh || req.width > 8192 || req.height > 8192 { return Err(EINVAL); }
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

fn present(token: u32, src_x: u32, src_y: u32, _rects: Option<&[(i32, i32, i32, i32)]>) -> Result<(), DriverError> {
    let slot = token.wrapping_sub(1) as usize;
    let fb = FBS.lock().get(slot).copied().flatten().ok_or(err(EINVAL))?;
    let (mw, mh, _) = mode();
    if src_x + mw > fb.width || src_y + mh > fb.height { return Err(err(EINVAL)); }
    // Implicit sync: the frame's rendering must be done (bounded: a hung
    // context is reset by hangcheck long before this gives up).
    let t0 = super::now_ns();
    if !uapi::kms_bo_wait(fb.obj, 1_000_000_000) { return Err(err(ETIME)); }
    let t1 = super::now_ns();
    let first = fb.offset as u64 + src_y as u64 * fb.pitch as u64;
    flush_for_display(fb.phys + first, mh as u64 * fb.pitch as u64);
    let ctl = (ORIG_CTL.load(Ordering::Relaxed) & !(0xF << 24 | 0x7 << 10)) | 1 << 31 | 0x4 << 24; // RGB 8888, linear
    let surf = (ggtt_of(slot) + fb.offset as u64) as u32;
    let prev = ON_SCREEN.swap(token, Ordering::AcqRel);
    let t2 = super::now_ns();
    let native = NATIVE_W.load(Ordering::Relaxed) != 0;
    let was = NATIVE_ON.load(Ordering::Acquire);
    let latched = program(ctl, fb.pitch / 64, src_y << 16 | src_x, surf, true, native);
    if native && !was {
        let m = mmio();
        ilog!("kms: fastset to native {}x{} (fastset #{}): PIPESRC 0x{:08x} PS_CTRL1 0x{:08x} PS_WIN_SZ1 0x{:08x} PLANE_SIZE 0x{:08x} latched={} underruns {}",
              mw, mh, FASTSETS.load(Ordering::Relaxed), m.r(reg::pipe(reg::PIPESRC_A, PIPE)), m.r(reg::PS_CTRL_1A),
              m.r(reg::PS_WIN_SZ_1A), m.r(preg(reg::PLANE_SIZE_1_A)), latched, underruns());
    }
    if debug() {
        let t3 = super::now_ns();
        let ur = underruns();
        let slow = t1 - t0 > 20_000_000 || !latched || prev == 0;
        if slow || ur != 0 || dbg_ok() {
            let m = mmio();
            let (pres, perr, vtr) = crate::drm_device_interface::ext_present_stats();
            ilog!("kms: flip #{} fb {} (prev {}) fence {} us flush {} us latch {} us{} FRMCOUNT {} underruns {} console-restores {} presents/errors/VT-repaints {}/{}/{} PSR_STATUS 0x{:08x}",
                  FLIPS.load(Ordering::Relaxed) + 1, token, prev, (t1 - t0) / 1000, (t2 - t1) / 1000, (t3 - t2) / 1000,
                  if latched { "" } else { " NOT LATCHED" }, m.r(reg::pipe(reg::PIPE_FRMCOUNT_A, PIPE)),
                  UNDERRUNS.load(Ordering::Relaxed), RESTORES.load(Ordering::Relaxed), pres, perr, vtr, m.r(EDP_PSR_STATUS));
        }
    }
    if FLIPS.fetch_add(1, Ordering::Relaxed) == 0 {
        ilog!("kms: first flip: fb {} ({}x{} pitch {}) at GGTT 0x{:08x}", token, fb.width, fb.height, fb.pitch, surf);
    }
    Ok(())
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
    // Watermarks: the GOP's, scaled by linear blocks-per-line (512-byte
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

/// After the UAPI registered: verify the GOP-left plane and register the
/// KMS seam. Only with `i915.kms=1`.
pub(super) fn init(d: &Device, env: &BootEnv) {
    if opt("i915.kms") != Some(1) { return; }
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
    crate::gpu_kms::register(&BACKEND);
    if (mw, mh) != (w, h) {
        ilog!("kms: reporting the panel's native {}x{}@{} (GOP mode {}x{}): the first flip fastsets pipe A (PIPESRC, scaler off); console_restore returns to the GOP state (i915.native=0 keeps the GOP mode)",
              mw, mh, REFRESH.load(Ordering::Relaxed), w, h);
    }
    ilog!("==== G4a KMS registered: card0 scans out {}x{} on pipe A plane 1A (the GOP mode, no modeset; the pipe scaler keeps the panel mode) by PLANE_SURF flips from a {}x{} MiB GGTT window at 0x{:x}; modifiers LINEAR{}; kms_uncached={} kms_psr={} kmsdebug={} ====",
          w, h, NSLOT, SLOT >> 20, WIN_BASE, if d.fake { " (fake device: nothing reaches a screen)" } else { "" },
          opt("i915.kms_uncached").unwrap_or(1), opt("i915.kms_psr").unwrap_or(0), debug() as u8);
}
