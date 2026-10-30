//! Kernel diagnostics shared by drivers and ACPI power code: serial/klog emit, monotonic clock,
//! bounded busy-wait. (Formerly in `i915`, which re-exports them through `kenv`.)

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
    fn klog_write_only(s: *const u8, len: usize);
    fn arch_monotonic_ns() -> u64;
}

pub fn emit(s: &str) { unsafe { serial_print_str_raw(s.as_ptr(), s.len()); } }
pub fn emit_logonly(s: &str) { unsafe { klog_write_only(s.as_ptr(), s.len()); } }
pub fn now_ns() -> u64 { unsafe { arch_monotonic_ns() } }

/// Bounded busy-wait; falls back to an iteration cap if the clock is stuck.
pub fn delay_us(us: u64) {
    let start = now_ns();
    for _ in 0..20_000_000u32 {
        if now_ns().wrapping_sub(start) >= us * 1000 { return; }
        core::hint::spin_loop();
    }
}
