//! Stub second pipe (lane lapdisp C1): `i915.fake=2 i915.kms=1 i915.pipeb_stub=1`.
//!
//! Registers a `KmsPipe2` whose modeset always succeeds and whose flips
//! complete from a 60 Hz kernel thread, so the DRM core's two-CRTC paths run
//! in QEMU. No hardware is touched.

use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use sched::ksync::KSemaphore;

use crate::gpu_kms::{KmsPipe2, KmsTiming};
use crate::DriverError;

use super::{ilog, now_ns, opt};

static TICK: KSemaphore = KSemaphore::new(0);
/// The pending event cookie (0: none).
static PENDING: AtomicU32 = AtomicU32::new(0);
static FRAME: AtomicU32 = AtomicU32::new(0);
/// Packed mode: w<<32 | h<<8 ... kept as three words; 0 width = off.
static MODE_W: AtomicU32 = AtomicU32::new(0);
static MODE_H: AtomicU32 = AtomicU32::new(0);
static MODE_R: AtomicU32 = AtomicU32::new(0);
static FLIPS: AtomicU64 = AtomicU64::new(0);

static PIPE: KmsPipe2 = KmsPipe2 {
    name: "stub",
    modeset,
    disable,
    queue,
    event,
    busy,
    wait_idle,
    cursor: None,
    mode,
    dumb_ok: true,
};

fn modeset(conn_idx: usize, t: &KmsTiming) -> Result<(), DriverError> {
    let tot = t.h[3] as u64 * t.v[3] as u64;
    let r = if tot > 0 { ((t.clock_khz as u64 * 1000 + tot / 2) / tot) as u32 } else { 60 };
    MODE_W.store(t.h[0] as u32, Ordering::Release);
    MODE_H.store(t.v[0] as u32, Ordering::Release);
    MODE_R.store(r, Ordering::Release);
    ilog!("drm: CRTC 2 modeset {}x{}@{} conn 0x{:x}", t.h[0], t.v[0], r, crate::gpu_kms::EXT_CONN_BASE as usize + conn_idx);
    Ok(())
}

fn disable() {
    MODE_W.store(0, Ordering::Release);
    ilog!("drm: CRTC 2 disabled");
}

fn queue(_token: u32, _sx: u32, _sy: u32, ev: u32) -> Result<(), DriverError> {
    FLIPS.fetch_add(1, Ordering::Relaxed);
    event(ev);
    Ok(())
}

fn event(ev: u32) {
    if ev != 0 { PENDING.store(ev, Ordering::Release); }
}

fn busy() -> bool { PENDING.load(Ordering::Acquire) != 0 }

fn wait_idle(timeout_ns: u64) -> bool {
    let t0 = now_ns();
    while busy() {
        if now_ns().wrapping_sub(t0) > timeout_ns { return false; }
        let _ = TICK.down_timeout(1_000_000);
    }
    true
}

fn mode() -> Option<(u32, u32, u32)> {
    let w = MODE_W.load(Ordering::Acquire);
    if w == 0 { None } else { Some((w, MODE_H.load(Ordering::Acquire), MODE_R.load(Ordering::Acquire))) }
}

fn worker() {
    loop {
        let _ = TICK.down_timeout(16_666_667);
        let seq = FRAME.fetch_add(1, Ordering::AcqRel) + 1;
        let ev = PENDING.swap(0, Ordering::AcqRel);
        if ev != 0 { crate::drm_device_interface::ext_event_complete(ev, seq, now_ns()); }
    }
}

pub(super) fn init(d: &super::Device) {
    if !d.fake || opt("i915.pipeb_stub") != Some(1) { return; }
    if sched::kthread::spawn("i915-pipeb-stub", worker).is_none() {
        ilog!("drm: pipeb stub: could not start the vblank thread");
        return;
    }
    crate::gpu_kms::register_pipe2(&PIPE);
    ilog!("drm: 2 CRTCs (pipe B: stub)");
}
