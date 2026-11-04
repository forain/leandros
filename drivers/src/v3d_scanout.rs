//! `pi-scanout`: the Raspberry Pi 5 `gpu_kms::KmsBackend` for v3d buffers.
//!
//! Mesa's v3d is a single-device driver: a GBM scanout surface is a v3d BO whose GEM handle goes
//! straight to ADDFB2. This backend wraps such a handle in a framebuffer (holding a reference on
//! the BO until RMFB or the creator's close) and puts it on screen through a pluggable `Sink`:
//!
//!   * `Headless` (opt-in, `v3d.headless=WxH`): a RAM surface. A present copies the BO into it;
//!     the frame count and a CRC are logged and read back from `/proc/v3d_headless`, the surface
//!     itself from `/proc/v3d_headless_fb` (raw XRGB8888, `W*4` pitch). No monitor is needed, so
//!     the compositor can run on the real GPU and be checked over SSH.
//!   * `FwCopy`: `kms_v3d::present`, a stride copy into the firmware framebuffer (needs a monitor
//!     at power-on for the mailbox to grant one).
//!   * `HvsPlane` (`hvs_plane`, the default when firmware channel 0 runs a matching one-plane list;
//!     `hvs.plane=0` opts out): the BO is scanned out directly, vblank paced, no copy. A frame it
//!     cannot show goes through `FwCopy`.
//!
//! Nothing registers when no sink exists, so ADDFB2 of a v3d BO fails instead of looking usable.
//! A present first waits for the BO's last job: the GPU is not coherent with the copy.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

use crate::drm_device_interface as dri;
use crate::gpu_kms::{self, KmsBackend, KmsFbReq};
use crate::{kms_v3d, DriverError};

/// Where presented pixels go. `present` gets the visible window of the BO (physical address of its
/// first visible pixel, size, pitch); the BO is idle and CPU-visible.
pub struct Sink {
    pub name: &'static str,
    pub mode: fn() -> (u32, u32, u32),
    pub present: fn(src_phys: u64, w: u32, h: u32, pitch: u32) -> Result<(), DriverError>,
    /// Zero-copy scanout of the BO (`obj`, window at `phys`). `Err`: this frame cannot be shown
    /// that way; the backend then puts it on screen with `FWCOPY`.
    pub flip: Option<fn(obj: u32, phys: u64, w: u32, h: u32, pitch: u32) -> Result<(), DriverError>>,
}

struct Fb { obj: u32, phys: u64, w: u32, h: u32, pitch: u32 }

static FBS: Mutex<BTreeMap<u32, Fb>> = Mutex::new(BTreeMap::new());
static NEXT_TOKEN: AtomicU32 = AtomicU32::new(1);
static SINK: AtomicPtr<Sink> = AtomicPtr::new(core::ptr::null_mut());
static WAIT_TIMEOUTS: AtomicU64 = AtomicU64::new(0);

fn sink() -> Option<&'static Sink> {
    let p = SINK.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

fn log(s: &str) { crate::pci::serial_debug(s); }

// ---- Headless sink ------------------------------------------------------------------------

static H_PHYS: AtomicUsize = AtomicUsize::new(0);
static H_W: AtomicU32 = AtomicU32::new(0);
static H_H: AtomicU32 = AtomicU32::new(0);
static H_FRAMES: AtomicU64 = AtomicU64::new(0);

fn headless_surface() -> Option<&'static [u8]> {
    let phys = H_PHYS.load(Ordering::Acquire);
    if phys == 0 { return None; }
    let len = H_W.load(Ordering::Relaxed) as usize * H_H.load(Ordering::Relaxed) as usize * 4;
    Some(unsafe { core::slice::from_raw_parts(mm::phys_to_virt(phys) as *const u8, len) })
}

fn headless_init(w: u32, h: u32) -> bool {
    let bytes = w as usize * h as usize * 4;
    let order = (bytes.div_ceil(4096)).next_power_of_two().trailing_zeros() as usize;
    let Some(phys) = mm::buddy::alloc(order) else { return false };
    unsafe { core::ptr::write_bytes(mm::phys_to_virt(phys) as *mut u8, 0, bytes) };
    H_W.store(w, Ordering::Relaxed);
    H_H.store(h, Ordering::Relaxed);
    H_PHYS.store(phys, Ordering::Release);
    true
}

fn headless_mode() -> (u32, u32, u32) {
    (H_W.load(Ordering::Relaxed), H_H.load(Ordering::Relaxed), 60)
}

fn headless_present(src_phys: u64, w: u32, h: u32, pitch: u32) -> Result<(), DriverError> {
    let (sw, sh, _) = headless_mode();
    let phys = H_PHYS.load(Ordering::Acquire);
    if phys == 0 || src_phys == 0 { return Err(DriverError::NotFound); }
    let (cw, ch) = kms_v3d::clip(w, h, sw, sh);
    let row = cw as usize * 4;
    let src_len = (ch as usize).saturating_sub(1) * pitch as usize + row;
    let dst_pitch = sw as usize * 4;
    let dst_len = sh as usize * dst_pitch;
    let src_va = mm::phys_to_virt(src_phys as usize) as usize;
    kms_v3d::dc_range(src_va, src_len, false);
    unsafe {
        let src = core::slice::from_raw_parts(src_va as *const u8, src_len);
        let dst = core::slice::from_raw_parts_mut(mm::phys_to_virt(phys) as *mut u8, dst_len);
        kms_v3d::stride_copy(src, pitch as usize, dst, dst_pitch, row, ch as usize);
    }
    let n = H_FRAMES.fetch_add(1, Ordering::Relaxed) + 1;
    if kms_v3d::flip_log_due(n) {
        let crc = headless_surface().map_or(0, |s| kms_v3d::crc32(0, s));
        log(&alloc::format!("[kms] pi-scanout headless frame {} {}x{} pitch={} crc={:08x}\n", n, w, h, pitch, crc));
    }
    Ok(())
}

static HEADLESS: Sink = Sink { name: "headless", mode: headless_mode, present: headless_present, flip: None };

// ---- FwCopy sink --------------------------------------------------------------------------

fn fw_mode() -> (u32, u32, u32) {
    crate::framebuffer::get_hardware_fb_info().map_or((0, 0, 60), |(_, w, h, _)| (w, h, 60))
}

static FWCOPY: Sink = Sink { name: "fwcopy", mode: fw_mode, present: kms_v3d::present, flip: None };

// ---- HvsPlane sink ------------------------------------------------------------------------

fn hvs_never_copy(_src_phys: u64, _w: u32, _h: u32, _pitch: u32) -> Result<(), DriverError> { Err(DriverError::Unsupported) }

static HVS: Sink = Sink { name: "hvs", mode: fw_mode, present: hvs_never_copy, flip: Some(crate::hvs_plane::flip) };

// ---- The KmsBackend -----------------------------------------------------------------------

const MOD_LINEAR: u64 = 0;
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

fn fb_create(req: &KmsFbReq, open_id: u32) -> Option<Result<u32, DriverError>> {
    let bo = dri::v3d_scanout_ref(req.handle, open_id)?;
    let mod_ok = req.modifier.map_or(true, |m| m == MOD_LINEAR || m == MOD_INVALID);
    if !mod_ok || !kms_v3d::fb_acceptable(req.fourcc, req.width, req.height, req.pitch, req.offset, bo.size) {
        dri::v3d_scanout_unref(bo.obj);
        return Some(Err(DriverError::InvalidParameter));
    }
    let token = NEXT_TOKEN.fetch_add(1, Ordering::Relaxed);
    FBS.lock().insert(token, Fb { obj: bo.obj, phys: bo.phys, w: req.width, h: req.height, pitch: req.pitch });
    Some(Ok(token))
}

fn fb_destroy(token: u32) {
    let fb = FBS.lock().remove(&token);
    if let Some(fb) = fb { dri::v3d_scanout_unref(fb.obj); }
}

fn present(token: u32, src_x: u32, src_y: u32, _rects: Option<&[(i32, i32, i32, i32)]>) -> Result<(), DriverError> {
    let (obj, phys, w, h, pitch) = {
        let g = FBS.lock();
        let fb = g.get(&token).ok_or(DriverError::NotFound)?;
        (fb.obj, fb.phys, fb.w, fb.h, fb.pitch)
    };
    let sink = sink().ok_or(DriverError::NotFound)?;
    if src_x >= w || src_y >= h { return Err(DriverError::InvalidParameter); }
    // The GPU writes the BO behind the CPU's back: copy only once its last job retired.
    if !dri::v3d_scanout_wait(obj, 200_000_000) {
        let n = WAIT_TIMEOUTS.fetch_add(1, Ordering::Relaxed) + 1;
        if kms_v3d::flip_log_due(n) { log("[kms] pi-scanout: BO job wait timed out, presenting anyway\n"); }
    }
    let off = src_y as u64 * pitch as u64 + src_x as u64 * 4;
    if let Some(flip) = sink.flip {
        if flip(obj, phys + off, w - src_x, h - src_y, pitch).is_ok() { return Ok(()); }
        // This frame (or the HVS) cannot scan out directly: show the firmware framebuffer again
        // and copy into it.
        crate::hvs_plane::restore();
        return (FWCOPY.present)(phys + off, w - src_x, h - src_y, pitch);
    }
    (sink.present)(phys + off, w - src_x, h - src_y, pitch)
}

fn mode() -> (u32, u32, u32) {
    sink().map_or((0, 0, 60), |s| (s.mode)())
}

static BACKEND: KmsBackend = KmsBackend {
    name: "pi-scanout",
    mode,
    modifiers: &[MOD_LINEAR],
    fb_create,
    fb_destroy,
    present,
    console_restore: Some(crate::hvs_plane::restore),
};

/// Choose a sink and register the backend. Called once, when the v3d probe armed the GPU.
pub fn register(cmdline: &str, serror_check: fn() -> Option<u64>) {
    let chosen: Option<&'static Sink> = if let Some((w, h)) = kms_v3d::parse_headless(cmdline) {
        if headless_init(w, h) { Some(&HEADLESS) } else { None }
    } else if crate::hvs_plane::init(cmdline, serror_check).is_some() {
        Some(&HVS)
    } else if crate::framebuffer::get_hardware_fb_info().is_some() {
        Some(&FWCOPY)
    } else {
        None
    };
    let Some(s) = chosen else {
        log("[kms] pi-scanout: no sink (no firmware framebuffer, no v3d.headless=WxH): KMS not registered\n");
        return;
    };
    SINK.store(s as *const Sink as *mut Sink, Ordering::Release);
    gpu_kms::register(&BACKEND);
    let (w, h, hz) = (s.mode)();
    log(&alloc::format!("[kms] pi-scanout registered sink={} {}x{}@{}\n", s.name, w, h, hz));
}

/// `/proc/v3d_headless` (`dump` false: `frames= crc=` line) and `/proc/v3d_headless_fb` (`dump`
/// true: the raw surface). `None` unless the Headless sink is the active one.
pub fn proc_headless(dump: bool) -> Option<Vec<u8>> {
    let s = headless_surface()?;
    if dump { return Some(s.to_vec()); }
    let (w, h, _) = headless_mode();
    let line = alloc::format!("sink=headless {}x{} frames={} crc={:08x}\n",
        w, h, H_FRAMES.load(Ordering::Relaxed), kms_v3d::crc32(0, s));
    Some(String::from(line).into_bytes())
}
