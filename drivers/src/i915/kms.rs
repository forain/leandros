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

/// The scanout GGTT window: 16 slots of 32 MiB from 1 GiB (clear of the GOP
/// surface at 0, the CPU aperture, exec's LRC slots and the G2 window at
/// the top of the GGTT). 32 MiB slots are 256 KiB aligned (gen9 linear).
const WIN_BASE: u64 = 1 << 30;
const SLOT: u64 = 32 << 20;
const NSLOT: usize = 16;

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
static FLIPS: AtomicU32 = AtomicU32::new(0);
static LATE: AtomicU32 = AtomicU32::new(0);

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
fn program(ctl: u32, stride: u32, offset: u32, surf: u32, wait: bool) {
    let m = mmio();
    if m.va == 0 { return; }
    m.w(preg(reg::PLANE_CTL_1_A), ctl);
    m.w(preg(reg::PLANE_STRIDE_1_A), stride);
    m.w(preg(reg::PLANE_OFFSET_1_A), offset);
    m.w(preg(reg::PLANE_SURF_1_A), surf);
    if !wait || FAKE.load(Ordering::Relaxed) { return; }
    for _ in 0..50 {
        if m.r(preg(PLANE_SURFLIVE_1_A)) & !0xFFF == surf & !0xFFF { return; }
        sched::kthread::msleep(1);
    }
    if LATE.fetch_add(1, Ordering::Relaxed) < 4 {
        ilog!("kms: flip to GGTT 0x{:08x} not latched after 50 ms (SURFLIVE=0x{:08x})", surf, m.r(preg(PLANE_SURFLIVE_1_A)));
    }
}

/// Scanout back to the GOP surface (the console's). Lock-free: MMIO writes
/// only, safe from the panic path.
pub fn console_restore() {
    if ON_SCREEN.swap(0, Ordering::AcqRel) == 0 { return; }
    program(ORIG_CTL.load(Ordering::Relaxed), ORIG_STRIDE.load(Ordering::Relaxed),
            ORIG_OFFSET.load(Ordering::Relaxed), ORIG_SURF.load(Ordering::Relaxed), false);
}

fn mode() -> (u32, u32, u32) { (MODE_W.load(Ordering::Acquire), MODE_H.load(Ordering::Acquire), 60) }

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
        program(ORIG_CTL.load(Ordering::Relaxed), ORIG_STRIDE.load(Ordering::Relaxed),
                ORIG_OFFSET.load(Ordering::Relaxed), ORIG_SURF.load(Ordering::Relaxed), true);
    }
    let fb = FBS.lock()[slot].take();
    let Some(fb) = fb else { return };
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
    if !uapi::kms_bo_wait(fb.obj, 1_000_000_000) { return Err(err(ETIME)); }
    let first = fb.offset as u64 + src_y as u64 * fb.pitch as u64;
    flush_for_display(fb.phys + first, mh as u64 * fb.pitch as u64);
    let ctl = (ORIG_CTL.load(Ordering::Relaxed) & !(0xF << 24 | 0x7 << 10)) | 1 << 31 | 0x4 << 24; // RGB 8888, linear
    let surf = (ggtt_of(slot) + fb.offset as u64) as u32;
    ON_SCREEN.store(token, Ordering::Release);
    program(ctl, fb.pitch / 64, src_y << 16 | src_x, surf, true);
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
    MMIO.store(m.va, Ordering::Release);
    GSM_VA.store(g.va, Ordering::Release);
    GSM_ENTRIES.store(g.entries as usize, Ordering::Release);
    FAKE.store(d.fake, Ordering::Relaxed);
    ORIG_CTL.store(m.r(preg(reg::PLANE_CTL_1_A)), Ordering::Relaxed);
    ORIG_STRIDE.store(m.r(preg(reg::PLANE_STRIDE_1_A)), Ordering::Relaxed);
    ORIG_OFFSET.store(m.r(preg(reg::PLANE_OFFSET_1_A)), Ordering::Relaxed);
    ORIG_SURF.store(m.r(preg(reg::PLANE_SURF_1_A)), Ordering::Relaxed);
    MODE_W.store(w, Ordering::Release);
    MODE_H.store(h, Ordering::Release);
    crate::gpu_kms::register(&BACKEND);
    ilog!("==== G4a KMS registered: card0 scans out {}x{} on pipe A plane 1A (the GOP mode, no modeset; the pipe scaler keeps the panel mode) by PLANE_SURF flips from a {}x{} MiB GGTT window at 0x{:x}; modifiers LINEAR{} ====",
          w, h, NSLOT, SLOT >> 20, WIN_BASE, if d.fake { " (fake device: nothing reaches a screen)" } else { "" });
}
