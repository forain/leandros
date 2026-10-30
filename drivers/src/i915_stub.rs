//! Stand-in for `drivers::i915` when the `i915` feature (CONFIG_I915) is off.
//! Same public / crate-visible API as `i915/mod.rs`: `run` does nothing, so no
//! GPU is probed and no DRM node is registered; the log and timing helpers
//! that `acpi_power` shares keep working.

use alloc::string::String;

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
    fn klog_write_only(s: *const u8, len: usize);
    fn arch_monotonic_ns() -> u64;
}

/// What the kernel knows about the boot framebuffer.
pub struct BootEnv {
    pub fb_phys: u64,
    pub fb_width: u32,
    pub fb_height: u32,
    pub fb_pitch: u32,
}

pub(crate) fn emit(s: &str) { unsafe { serial_print_str_raw(s.as_ptr(), s.len()); } }
pub(crate) fn emit_logonly(s: &str) { unsafe { klog_write_only(s.as_ptr(), s.len()); } }
pub(crate) fn now_ns() -> u64 { unsafe { arch_monotonic_ns() } }

/// Bounded busy-wait; falls back to an iteration cap if the clock is stuck.
pub(crate) fn delay_us(us: u64) {
    let start = now_ns();
    for _ in 0..20_000_000u32 {
        if now_ns().wrapping_sub(start) >= us * 1000 { return; }
        core::hint::spin_loop();
    }
}

pub fn run(_env: &BootEnv) {}
pub fn kexec_quiesce() -> alloc::vec::Vec<String> { alloc::vec::Vec::new() }
pub fn pm_suspend() -> Result<String, String> { Ok(String::new()) }
pub fn pm_resume() -> Result<String, String> { Ok(String::new()) }
pub fn proc_text() -> Option<String> { crate::drm_class::proc_text() }
pub fn set_tmpfs_census(f: fn() -> Option<[u64; 9]>) { crate::drm_class::set_tmpfs_census(f) }
pub fn prime_dedup(obj: u32) -> bool { crate::drm_class::prime_dedup(obj) }
pub fn ptecheck_fd_to_handle(fd: i32, open_id: u32, obj: Option<u32>, handle: u32) { crate::drm_class::ptecheck_fd_to_handle(fd, open_id, obj, handle) }
pub fn ptecheck_dmabuf_mmap(obj: u32, off: u64, frames: &[usize]) { crate::drm_class::ptecheck_dmabuf_mmap(obj, off, frames) }

pub mod backlight {
    pub fn get() -> Option<(u32, u32)> { None }
    pub fn set(_duty: u32) -> bool { false }
}
