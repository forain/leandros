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

/// Uncached (Device) MMIO mapping of a physical range.
pub fn ioremap_uc(pa: u64, len: usize) -> Option<usize> { ::mm::kva::ioremap(pa, len, ::mm::kva::CacheMode::Uncached) }
/// Factory Ethernet MAC of the board (Pi 5 OTP); the module build calls `kapi_board_mac`.
pub fn board_eth_mac() -> Option<[u8; 6]> { crate::bcm2712_otp::eth_mac() }
/// The recorded boot DTB; the module build calls `kapi_dtb_blob`.
pub fn dtb_blob() -> Option<&'static [u8]> { unsafe { boot::device_tree::recorded_dtb() } }
/// Attach `h` to RP1 interrupt source `n` for good (the module build calls `kapi_rp1_irq_request`).
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub fn rp1_irq_request(n: u32, level: bool, h: crate::irq::IrqHandler, ctx: usize) -> bool {
    crate::rp1::request_irq(n, level, h, ctx).map(core::mem::forget).is_ok()
}
#[cfg(not(all(target_arch = "aarch64", feature = "rpi5")))]
pub fn rp1_irq_request(_n: u32, _level: bool, _h: crate::irq::IrqHandler, _ctx: usize) -> bool { false }
/// Register a kexec stop hook for a bus master (device stage); the module build calls `kapi_dma_quiesce_register`.
pub fn dma_quiesce_register(name: &'static str, f: fn() -> bool) {
    crate::dma_shutdown::register(name, crate::dma_shutdown::Stage::Device, f)
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

pub use crate::kdiag::{emit, emit_logonly, now_ns, delay_us};

/// DRM device-interface seam (fences, sync objects, ext events, offsets, stats).
pub mod drm {
    pub use crate::drm_device_interface::{
        ext_event_complete, ext_event_stats, ext_present_stats, gpu_fence_irq_hint, proc_prime,
        syncobj_exists, syncobj_in_fences, syncobj_install, wait_fences,
    };
    pub use crate::drm_offset::{offset_for, release as offset_release};
}

/// KMS backend registration seam.
pub mod kms {
    pub use crate::gpu_kms::*;
}

/// DRM uapi backend registration seam.
pub mod uapi {
    pub use crate::gpu_uapi::*;
    /// Install the backend mmap-pin hook (sys_mmap page pin for driver tokens).
    pub fn set_mmap_pin(f: fn(usize, usize, u32) -> Option<alloc::vec::Vec<usize>>) {
        *crate::drm_device_interface::BACKEND_MMAP_PIN.lock() = Some(f);
    }
}

/// HPD uevent to userspace (evdev hotplug node).
pub fn hotplug(minor: u8) { ::evdev_server::hp_drm_change(minor) }
