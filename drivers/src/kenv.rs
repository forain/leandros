//! Kernel-environment seam. The built-in build re-exports the real kernel crates; the module
//! build of `virtio_rng` (modules/virtio-rng) supplies its own `kenv` over the kapi shim.
pub use ::mm;
pub use ::sched;
pub use crate::pci;
pub use crate::irq;
pub use crate::netdev;

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
}
/// Raw serial line (the module build routes this through `kapi_log`).
pub fn serial_raw(s: &str) { unsafe { serial_print_str_raw(s.as_ptr(), s.len()); } }
