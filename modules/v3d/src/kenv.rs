//! The `kenv::v3d` names the shared driver source expects, over kmod-kapi.
pub mod v3d {
    use alloc::vec::Vec;
    use core::cell::UnsafeCell;
    use kmod_kapi::v3d as sys;
    pub use kmod_kapi::irq::{IrqReturn, IrqSource};
    pub use kmod_kapi::mm::{buddy, kva, phys_to_virt};
    pub use kmod_kapi::pci::serial_debug;
    pub use crate::DriverError;

    pub const MBOX_BASE_PHYS: usize = 0x10_7C01_3880;
    pub mod mm { pub use kmod_kapi::mm::{buddy, kva, phys_to_virt}; }
    pub mod sched { pub use kmod_kapi::sched::monotonic_ns; }

    #[derive(Debug, Clone, Copy)]
    pub enum MboxError { Err(i64) }

    struct Reply(UnsafeCell<[u32; 36]>);
    unsafe impl Sync for Reply {}
    static REPLY: Reply = Reply(UnsafeCell::new([0; 36]));
    static REPLY_LOCK: spin::Mutex<()> = spin::Mutex::new(());

    /// Firmware call through `kapi_mbox_property` (the kernel's lock); `hhdm` is unused. The returned
    /// slice is a static buffer: callers use it before the next call (probe is single-threaded).
    pub unsafe fn mbox_property(_hhdm: usize, words: &[u32]) -> Result<&'static [u32], MboxError> {
        let _g = REPLY_LOCK.lock();
        let buf = &mut *REPLY.0.get();
        match sys::mbox_property(words, buf) {
            n if n >= 0 => Ok(&buf[..(n as usize).min(buf.len())]),
            e => Err(MboxError::Err(e)),
        }
    }
    /// The kernel owns the mailbox window.
    pub fn mbox_set_virt(_virt: usize) {}
    pub fn pm_base() -> Option<usize> { sys::pm_base() }

    pub fn dma_quiesce_register(name: &'static str, f: fn() -> bool) {
        extern "C" fn q(ctx: usize) -> bool {
            let f: fn() -> bool = unsafe { core::mem::transmute(ctx) };
            f()
        }
        let _ = kmod_kapi::plat::dma_quiesce_register(name, kmod_kapi::plat::quiesce_stage::DEVICE, q, f as usize);
    }

    pub fn va_alloc(bytes: u64) -> Option<u64> { sys::va_alloc(bytes) }
    pub fn va_free(va: u64) { sys::va_free(va) }
    pub fn arm(cmdline: &str, _serror: crate::v3d::SerrorCheck) { let _ = sys::arm(cmdline); }
    pub fn register_ops(t: &'static crate::v3d_class::KV3dOps) {
        let _ = sys::register(t as *const _ as *const u8, core::mem::size_of::<crate::v3d_class::KV3dOps>());
    }
    pub fn irq_line_state(id: u32) -> u32 { sys::irq_line_state(id) }
    pub fn alloc_fence_id() -> u64 { sys::fence_alloc() }
    pub fn fence_signalled(f: u64) -> bool { sys::fence_done(f) }
    pub fn syncobj_retire_fence(f: u64) { sys::fence_retire(f) }
    pub fn yield_now(_why: &'static str) { kmod_kapi::sys_common::yield_now() }

    /// GIC SPI interrupt (`kapi_irq_request_spi`); `IrqSource::Line(n)` is the intid.
    pub fn request_irq(src: IrqSource, h: kmod_kapi::irq::IrqHandler, ctx: usize) -> Result<(), DriverError> {
        let IrqSource::Line(n) = src;
        // SAFETY: IrqReturn is repr(u32) 0/1, same shape as KIrqHandler's u32.
        let raw: kmod_abi::KIrqHandler = unsafe { core::mem::transmute(h) };
        sys::irq_request_spi(n, raw, ctx).map(|_| ()).map_err(|e| if e == -16 { DriverError::Busy } else { DriverError::NotFound })
    }

    pub fn copy_from_user(src: usize, buf: &mut [u8]) -> bool { kmod_kapi::sys_common::copy_from_user(buf, src).is_ok() }
    /// Append the pending fences of in-sync `h`; -22 (no fence yet) counts as satisfied, -2 is ENOENT.
    pub fn syncobj_deps(h: u32, open_id: u32, out: &mut Vec<u64>) -> Result<(), DriverError> {
        let mut cap = 8usize;
        loop {
            let mut v = alloc::vec![0u64; cap];
            match kmod_kapi::drm::syncobj_in_fences(h, open_id, 0, &mut v) {
                -22 => return Ok(()),
                -2 => return Err(DriverError::NotFound),
                n if n < 0 => return Err(DriverError::Errno(-n as i32)),
                n if n as usize > cap => { cap = n as usize; }
                n => { out.extend_from_slice(&v[..n as usize]); return Ok(()); }
            }
        }
    }
    pub fn syncobj_exists(h: u32, open_id: u32) -> bool { kmod_kapi::drm::syncobj_exists(h, open_id) }
    pub fn syncobj_install(h: u32, open_id: u32, fence: u64) { let _ = kmod_kapi::drm::syncobj_install(h, open_id, 0, fence); }
    pub fn bo_hold(h: u32, open_id: u32) -> i64 { sys::bo_hold(h, open_id) }
    pub fn bo_unref(obj: u32) { sys::bo_unref(obj) }
    pub fn bo_fence(h: u32, open_id: u32, f: u64) { sys::bo_fence(h, open_id, f) }
    pub fn queue_wait(deadline: u64) -> bool { sys::queue_wait(deadline) }
}
