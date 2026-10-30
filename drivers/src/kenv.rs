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

/// Sleep the calling task until `ns` (monotonic).
pub fn sleep_until(ns: u64) { crate::usb_storage::sleep_until(ns) }
pub use crate::nvidia_probe::{cmdline_known, cmdline_opt, cmdline_word};

/// Hand a radio table to the kernel's Wi-Fi class (the module build calls `kapi_wifi_register`).
#[cfg(feature = "wifi-stack")]
pub fn wifi_register(ops: &crate::wifi_radio::KWifiRadioOps, ctx: usize, legacy: *const crate::wifi_radio::KLegacy) -> i32 {
    crate::wifi_class::register_raw(ops, ctx, legacy)
}
