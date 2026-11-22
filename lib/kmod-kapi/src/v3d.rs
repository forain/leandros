//! V3D core services for the `v3d` module (Pi 5): BO / fence / VA seams, table registration, board access.
use kmod_abi::plat::sys_v3d as sys;

pub fn bo_hold(h: u32, open_id: u32) -> i64 { unsafe { sys::kapi_v3d_bo_hold(h, open_id) } }
pub fn bo_unref(obj: u32) { unsafe { sys::kapi_v3d_bo_unref(obj) } }
pub fn bo_fence(h: u32, open_id: u32, f: u64) { unsafe { sys::kapi_v3d_bo_fence(h, open_id, f) } }
/// True: a signal is pending, give up.
pub fn queue_wait(deadline: u64) -> bool { unsafe { sys::kapi_v3d_queue_wait(deadline) != 0 } }
pub fn fence_alloc() -> u64 { unsafe { sys::kapi_v3d_fence_alloc() } }
pub fn fence_done(f: u64) -> bool { unsafe { sys::kapi_v3d_fence_done(f) != 0 } }
pub fn fence_retire(f: u64) { unsafe { sys::kapi_v3d_fence_retire(f) } }
pub fn va_alloc(bytes: u64) -> Option<u64> { match unsafe { sys::kapi_v3d_va_alloc(bytes) } { 0 => None, v => Some(v) } }
pub fn va_free(va: u64) { unsafe { sys::kapi_v3d_va_free(va) } }
/// `ops` must be a `'static` `KV3dOps`; its size is passed for core's check.
pub fn register(ops: *const u8, size: usize) -> i32 { unsafe { sys::kapi_v3d_register(crate::owner(), ops, size) } }
pub fn arm(cmdline: &str) -> i32 { unsafe { sys::kapi_v3d_arm(cmdline.as_ptr(), cmdline.len()) } }
/// Reply word count or -errno.
pub fn mbox_property(words: &[u32], out: &mut [u32]) -> i64 {
    unsafe { sys::kapi_mbox_property(words.as_ptr(), words.len(), out.as_mut_ptr(), out.len()) }
}
pub fn pm_base() -> Option<usize> { match unsafe { sys::kapi_pm_base() } { 0 => None, v => Some(v as usize) } }
pub fn irq_request_spi(intid: u32, h: kmod_abi::KIrqHandler, ctx: usize) -> Result<u32, i64> {
    match unsafe { sys::kapi_irq_request_spi(crate::owner(), intid, h, ctx) } { l if l >= 0 => Ok(l as u32), e => Err(e) }
}
pub fn irq_line_state(id: u32) -> u32 { unsafe { sys::kapi_irq_line_state(id) } }
