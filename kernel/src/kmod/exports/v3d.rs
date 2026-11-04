//! kapi exports for the `v3d` module (Pi 5 V3D tristate). Always linked (the table is fixed); every body
//! answers -19 / 0 unless this is a Pi 5 build with V3D as `y` or `m`.
use super::*;

#[cfg(all(target_arch = "aarch64", feature = "rpi5", any(feature = "drv-v3d", feature = "mod-v3d")))]
mod real {
    pub use drivers::drm_device_interface as drm;
}

macro_rules! v3d_gate {
    ($real:block, $stub:expr) => {{
        #[cfg(all(target_arch = "aarch64", feature = "rpi5", any(feature = "drv-v3d", feature = "mod-v3d")))]
        { $real }
        #[cfg(not(all(target_arch = "aarch64", feature = "rpi5", any(feature = "drv-v3d", feature = "mod-v3d"))))]
        { $stub }
    }};
}

#[no_mangle]
pub extern "C" fn kapi_v3d_bo_hold(h: u32, open_id: u32) -> i64 { v3d_gate!({ real::drm::v3d_bo_hold(h, open_id) }, { let _ = (h, open_id); -19 }) }
#[no_mangle]
pub extern "C" fn kapi_v3d_bo_unref(obj: u32) { v3d_gate!({ real::drm::v3d_bo_unref(obj) }, { let _ = obj; }) }
#[no_mangle]
pub extern "C" fn kapi_v3d_bo_fence(h: u32, open_id: u32, f: u64) { v3d_gate!({ real::drm::v3d_bo_fence(h, open_id, f) }, { let _ = (h, open_id, f); }) }
#[no_mangle]
pub extern "C" fn kapi_v3d_queue_wait(deadline: u64) -> i32 { v3d_gate!({ real::drm::v3d_queue_wait(deadline) as i32 }, { let _ = deadline; 1 }) }
#[no_mangle]
pub extern "C" fn kapi_v3d_fence_alloc() -> u64 { v3d_gate!({ real::drm::v3d_alloc_fence_id() }, { 0 }) }
#[no_mangle]
pub extern "C" fn kapi_v3d_fence_done(f: u64) -> i32 { v3d_gate!({ real::drm::v3d_fence_signalled(f) as i32 }, { let _ = f; 1 }) }
#[no_mangle]
pub extern "C" fn kapi_v3d_fence_retire(f: u64) { v3d_gate!({ real::drm::syncobj_retire_fence(f) }, { let _ = f; }) }
#[no_mangle]
pub extern "C" fn kapi_v3d_va_alloc(bytes: u64) -> u64 { v3d_gate!({ real::drm::v3d_kernel_va_alloc(bytes).unwrap_or(0) }, { let _ = bytes; 0 }) }
#[no_mangle]
pub extern "C" fn kapi_v3d_va_free(va: u64) { v3d_gate!({ real::drm::v3d_kernel_va_free(va) }, { let _ = va; }) }

/// Hand over the module's `KV3dOps` (permanent: the module has no `exit`). -22 bad size, -17 already set.
#[no_mangle]
pub extern "C" fn kapi_v3d_register(owner: KmodId, ops: *const u8, size: usize) -> i32 {
    v3d_gate!({
        use drivers::v3d_class as c;
        if ops.is_null() || size < core::mem::size_of::<c::KV3dOps>() { return -22; }
        if c::registered().is_some() { return -17; }
        let t: &'static c::KV3dOps = unsafe { &*(ops as *const c::KV3dOps) };
        if !c::register(t) { return -22; }
        crate::kmod::registry::note_registration(owner, RegKind::V3D, 0);
        0
    }, { let _ = (owner, ops, size); -19 })
}

/// Latch the backend armed and pick the scanout sink (what `kenv::v3d::arm` does at =y). -22 with no table.
#[no_mangle]
pub extern "C" fn kapi_v3d_arm(cmdline: *const u8, len: usize) -> i32 {
    v3d_gate!({
        if drivers::v3d_class::registered().is_none() { return -22; }
        let cl = if cmdline.is_null() { "" } else { core::str::from_utf8(unsafe { core::slice::from_raw_parts(cmdline, len) }).unwrap_or("") };
        real::drm::v3d_arm_hw();
        drivers::v3d_scanout::register(cl);
        0
    }, { let _ = (cmdline, len); -19 })
}

/// One firmware property call, held under the kernel's firmware lock. Reply words copied to `out`; count or -errno.
#[no_mangle]
pub extern "C" fn kapi_mbox_property(words: *const u32, n: usize, out: *mut u32, out_n: usize) -> i64 {
    v3d_gate!({
        if words.is_null() || out.is_null() || n == 0 { return -22; }
        let w = unsafe { core::slice::from_raw_parts(words, n) };
        let o = unsafe { core::slice::from_raw_parts_mut(out, out_n) };
        match drivers::rpi_misc::fw_raw(w, o) { Ok(c) => c as i64, Err(e) => e as i64 }
    }, { let _ = (words, n, out, out_n); -19 })
}

#[no_mangle]
pub extern "C" fn kapi_pm_base() -> u64 { v3d_gate!({ drivers::rpi_pm::base().unwrap_or(0) as u64 }, { 0 }) }

/// GIC SPI `intid` for the module (`kapi_irq_request_line` always takes the PCI INTx instead).
#[no_mangle]
pub extern "C" fn kapi_irq_request_spi(owner: KmodId, intid: u32, handler: kmod_abi::KIrqHandler, ctx: usize) -> i64 {
    v3d_gate!({
        let (hf, hc) = match crate::kmod::lower::wrap_irq(owner, handler, ctx) { Some(x) => x, None => return -19 };
        match drivers::irq::request_irq(drivers::irq::IrqSource::Line(intid), hf, hc) {
            Ok(h) => {
                crate::kmod::lower::hold_irq(owner, intid as u64, h);
                crate::kmod::registry::note_registration(owner, RegKind::IRQ, intid as u64);
                intid as i64
            }
            Err(_) => -16,
        }
    }, { let _ = (owner, intid, handler, ctx); -19 })
}

#[no_mangle]
pub extern "C" fn kapi_irq_line_state(id: u32) -> u32 {
    v3d_gate!({
        extern "C" { fn arch_irq_line_state(id: u32) -> u32; }
        unsafe { arch_irq_line_state(id) }
    }, { let _ = id; 0 })
}
