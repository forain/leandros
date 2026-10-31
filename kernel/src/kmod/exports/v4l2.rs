//! kapi exports: V4L2 camera, tick callbacks
use super::*;

// ── uvc-class exports (L7a): V4L2 camera registration, USB class drivers, tick callbacks ──

use kmod_abi::{KCameraOps, KTickFn, KUsbDetachFn, KUsbIf, KUsbProbeFn, KmodDevId, BUS_USB};

/// Register a camera with the V4L2 layer; it appears as /dev/video0 candidate at once.
/// 0 ok, -22 bad table or description.
#[no_mangle]
pub extern "C" fn kapi_v4l2_register(owner: KmodId, ops: *const KCameraOps, ctx: usize) -> i32 {
    if ops.is_null() { return -22; }
    // Modules are permanent, so the table lives as long as the kernel.
    let ops: &'static KCameraOps = unsafe { &*ops };
    let r = crate::syscall::v4l2::register_camera(owner, ops, ctx);
    if r == 0 { crate::kmod::registry::note_registration(owner, RegKind::V4L2, ctx as u64); }
    r
}

pub(super) fn tick_tramp(p: usize) {
    let b = unsafe { &*(p as *const (crate::kmod::gate::ModFn<KTickFn>, usize)) };
    let c = b.1;
    b.0.with(|f| f(c));
}

/// Run `f(ctx)` on every USB timer tick (interrupt context, must not block). 0 ok.
#[no_mangle]
pub extern "C" fn kapi_tick_register(owner: KmodId, f: KTickFn, ctx: usize) -> i32 {
    let owner = crate::kmod::gate::owner(owner);
    let gate = match crate::kmod::gate::get(owner) { Some(g) => g, None => return -19 };
    let mf = crate::kmod::gate::ModFn::new(gate, crate::kmod::gate::Class::Lower, f);
    let p = Box::into_raw(Box::new((mf, ctx))) as usize;
    crate::kmod::registry::note_registration(owner, RegKind::TICK, p as u64);
    usb::class::tick_register(owner, tick_tramp, p);
    0
}

