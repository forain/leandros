//! Kernel-environment seam. The built-in build re-exports the real kernel crates; the module
//! build of `virtio_rng` (modules/virtio-rng) supplies its own `kenv` over the kapi shim.
pub use ::mm;
pub use ::sched;
pub use crate::pci;
pub use crate::irq;
pub use crate::netdev;
pub use crate::DriverError;

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
}
/// Raw serial line (the module build routes this through `kapi_log`).
pub fn serial_raw(s: &str) { unsafe { serial_print_str_raw(s.as_ptr(), s.len()); } }

/// Park until `deadline` (`monotonic_ns`). Task context that may sleep only.
pub fn sleep_until(deadline: u64) {
    while sched::monotonic_ns() < deadline {
        sched::block_on_poll_prepare_masked(deadline, sched::POLL_MASK_TIMED_ONLY);
        if sched::monotonic_ns() >= deadline {
            sched::block_on_poll_cancel();
        } else {
            sched::block_on_poll_commit();
        }
    }
}

pub use crate::nvidia_probe::{cmdline_known, cmdline_opt, cmdline_word};

/// Input seam: the built-in feeds the evdev server directly; the module build goes through the
/// `kapi_input_*` exports. `register` claims the fixed evdev node of the same index.
pub mod input {
    /// The built-in owns both nodes outright; the module build claims them through `kapi_input_register`.
    pub fn claim(_node: u32) -> bool { true }
    pub fn push_event(node: u32, ty: u16, code: u16, value: i32) { ::evdev_server::push_event(node, ty, code, value) }
    pub fn set_device_caps(node: u32, rel: &[u8], key: &[u8]) { ::evdev_server::set_device_caps(node, rel, key) }
    pub fn register_stats_hook(f: fn(&mut dyn core::fmt::Write)) { ::tty_server::vt::register_stats_hook(f) }
    pub fn vt_handoff_stats() -> (u64, u64) { crate::drm_device_interface::vt_handoff_stats() }
}

/// Hand a radio table to the kernel's Wi-Fi class (the module build calls `kapi_wifi_register`).
#[cfg(feature = "wifi-stack")]
pub fn wifi_register(ops: &crate::wifi_radio::KWifiRadioOps, ctx: usize, legacy: *const crate::wifi_radio::KLegacy) -> i32 {
    crate::wifi_class::register_raw(ops, ctx, legacy)
}
