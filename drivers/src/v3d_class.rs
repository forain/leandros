//! `KV3dOps`: the narrow table through which core DRM code reaches the V3D driver.
//!
//! Core (`drm_device_interface`) keeps the BO table, VA allocator, PRIME, syncobj and the QEMU
//! stub. The driver (Pi 5 V3D) owns MMU/queue/IRQ. Core never names `crate::v3d::*`; it asks
//! `ops()`. With nothing registered (V3D=n, QEMU, failed probe) every call site falls through to
//! the stub path, so the answers equal the pre-table behaviour.
//!
//! The table is `repr(C)` and size-gated so a later `v3d.kmod` can register it through
//! `kapi_v3d_register`. Today the `=y` build registers the same table statically.
//! Codes: `submit` returns `>= 0` or `-errno` and its `arg` is a KERNEL pointer to a copy of the ioctl struct; `map_bo` 0 mapped, 1 driver inactive (skip), `< 0` failure.

use core::sync::atomic::{AtomicPtr, Ordering};

#[repr(C)]
pub struct KV3dOps {
    /// `size_of::<KV3dOps>()` as the registrant built it; core refuses a smaller table.
    pub size: usize,
    /// Real hardware queue is up (probe + MMU ok).
    pub active: extern "C" fn() -> bool,
    /// Back a GPU VA with PTEs.
    pub map_bo: extern "C" fn(va: u64, phys: u64, bytes: u64) -> i32,
    /// Drop the PTEs before the VA span and pages are reused.
    pub unmap_bo: extern "C" fn(va: u64, bytes: u64),
    /// `DRM_V3D_PARAM`: true and `*out` set when the driver answers.
    pub param: extern "C" fn(param: u32, out: *mut u64) -> bool,
    /// SUBMIT_CL / SUBMIT_TFU / SUBMIT_CSD; `nr` is the ioctl command word.
    pub submit: extern "C" fn(nr: u32, arg: usize, open_id: u32) -> i64,
    /// Release BO references of jobs whose fences signalled.
    pub reap: extern "C" fn(),
    /// Tick-context poll: IRQ fallback, hang timer, dependency re-checks.
    pub tick: extern "C" fn(),
    /// Task-context bounded wait for `fence`; false on timeout.
    pub wait_bo: extern "C" fn(fence: u64, timeout_ns: u64) -> bool,
}

static OPS: AtomicPtr<KV3dOps> = AtomicPtr::new(core::ptr::null_mut());

/// Register the driver table (`=y` at probe, `kapi_v3d_register` for a module). Last wins.
pub fn register(t: &'static KV3dOps) -> bool {
    if t.size < core::mem::size_of::<KV3dOps>() { return false; }
    OPS.store(t as *const KV3dOps as *mut KV3dOps, Ordering::Release);
    true
}

/// Drop the table (module unload; unused by the built-in driver).
pub fn unregister() { OPS.store(core::ptr::null_mut(), Ordering::Release); }

/// The registered table, whether or not the hardware came up.
#[inline]
pub fn registered() -> Option<&'static KV3dOps> {
    let p = OPS.load(Ordering::Acquire);
    if p.is_null() { None } else { Some(unsafe { &*p }) }
}

/// The table, only while the real queue is up: the gate every core call site uses.
#[inline]
pub fn ops() -> Option<&'static KV3dOps> {
    registered().filter(|o| (o.active)())
}

/// Map a `submit` result code onto the core error type.
pub fn submit_result(r: i64) -> Result<usize, crate::DriverError> {
    use crate::DriverError as E;
    if r >= 0 { return Ok(r as usize); }
    Err(match -r {
        22 => E::InvalidParameter,
        2 => E::NotFound,
        16 => E::Busy,
        5 => E::Io,
        e => E::Errno(e as i32),
    })
}
