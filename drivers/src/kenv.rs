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

/// Pi 5 board services of the Wi-Fi chip (SDIO2 pin mux, WL_REG_ON, factory MAC). The module build
/// answers the same functions through `kapi_pi5_pins` and `kapi_board_mac`.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod pi5 {
    use alloc::string::String;
    use crate::bcm2712_gpio::{self as gpio, Block, Chip, GROUP_SDIO2_WL_ON, WL_ON};
    use crate::bcm2712_id;

    /// Mux gpio28 to GPIO and gpio30..35 to sd2 with the dts pulls. Returns the log text; an unknown
    /// stepping leaves the mux untouched. Writes only registers whose value changes.
    pub fn sdio2_pinmux() -> String {
        let st = bcm2712_id::stepping().stepping;
        if bcm2712_id::pin_rows(st).is_none() {
            return String::from("pinmux=skip(unknown stepping)");
        }
        let mut before = 0u32; // packed fsel nibbles of pins 30..35 as found
        for (i, p) in (30..=35).enumerate() {
            before |= (gpio::pin_get_fsel(Block::Main, p).unwrap_or(0) & 0xF) << (i as u32 * 4);
        }
        match gpio::pin_apply(Block::Main, &GROUP_SDIO2_WL_ON) {
            Ok(w) => alloc::format!("pinmux={:?} sd2-fsel-before={:06x} writes={}", st, before, w),
            Err(e) => alloc::format!("pinmux=fail({:?})", e),
        }
    }
    pub fn wl_reg_on(on: bool) { let _ = gpio::gpio_set_output(Chip::Gio, WL_ON, on); }
    /// The WL_ON line is a readable output (the gio block is mapped).
    pub fn wl_reg_on_ready() -> bool { gpio::gpio_is_output(Chip::Gio, WL_ON).is_ok() }
    /// Factory Wi-Fi MAC from the board OTP.
    pub fn wifi_mac() -> Option<[u8; 6]> { crate::bcm2712_otp::wifi_mac() }
}

/// Power-management hook seam. The built-in build keeps the hooks in a drivers table that the kernel's
/// pm2 walks per phase; the module build registers through `kapi_pm_register2` (per-owner, dropped
/// again when init fails).
pub mod pm {
    use alloc::{string::String, vec::Vec};
    /// Same value as `kmod_abi::phase::WIFI` (pm2 asserts it).
    pub const PHASE_WIFI: u32 = 5;
    /// A quiesce hook returns its log line.
    pub type ShutdownFn = fn() -> Option<String>;
    static HOOKS: spin::Mutex<Vec<(&'static str, u32, ShutdownFn)>> = spin::Mutex::new(Vec::new());
    /// Register `name`'s phase-1 shutdown hook; a second registration of the same name replaces it.
    pub fn register_shutdown(name: &'static str, phase: u32, f: ShutdownFn) {
        let mut h = HOOKS.lock();
        h.retain(|e| e.0 != name);
        h.push((name, phase, f));
    }
    pub fn unregister(name: &str) { HOOKS.lock().retain(|e| e.0 != name); }
    /// The shutdown hooks of `phase`, in registration order.
    pub fn shutdown_hooks(phase: u32) -> Vec<ShutdownFn> {
        HOOKS.lock().iter().filter(|e| e.1 == phase).map(|e| e.2).collect()
    }
}

/// What the HDMI audio hardware code (`hdmi_audio::hw`) needs from the kernel; the module build
/// (modules/hdmi-audio) supplies the same names over the kapi exports.
#[cfg(all(target_arch = "aarch64", feature = "rpi5", feature = "hdmi-audio"))]
pub mod hdmi {
    pub use crate::bcm2712_dma40 as dma;
    pub use crate::bcm2712_dma40::dcache_clean_inval;
    pub use crate::hdmi_probe::decode::acr_n_cts;
    pub use crate::hdmi_probe::{PORT_ACTIVE, PORT_HDMI_MODE, PORT_HPD, PORT_VALID, PROBE_DONE, PROBE_NOT_RUN, PROBE_SKIPPED, SINK_DVI, SINK_HDMI};
    pub use crate::pci::serial_debug;
    pub use ::mm::{buddy, phys_to_virt};
    pub const TX_BUF_BYTES: usize = crate::snd::TX_BUF_BYTES;

    /// Boot-probe facts for hdmi0 plus the SoC stepping.
    pub struct Sink { pub state: u32, pub flags: u32, pub pix_khz: u32, pub sink: u32, pub d0: bool }
    pub fn sink() -> Sink {
        let (state, flags, pix_khz, sink) = crate::hdmi_plat::sink_info();
        Sink { state, flags, pix_khz, sink, d0: crate::hdmi_plat::is_d0() }
    }
    pub fn monotonic_ns() -> u64 { ::sched::monotonic_ns() }
    /// Uncached device mapping; 0 on failure.
    pub fn ioremap_uc(phys: u64, len: usize) -> usize {
        ::mm::kva::ioremap(phys, len, ::mm::kva::CacheMode::Uncached).unwrap_or(0)
    }
    pub fn iounmap(va: usize, _len: usize) { let _ = ::mm::kva::iounmap(va); }
    /// Fallback SError checkpoint when the boot has not installed one.
    pub fn serror() -> Option<u64> { None }
}

/// What the V3D driver (`drivers/src/v3d`) needs from the kernel. The built-in build answers with the
/// kernel's own functions; the module build (K3) answers the same names through `kapi_mbox_property`,
/// `kapi_pm_base`, `kapi_v3d_arm`, `kapi_v3d_va_alloc/free` and the existing irq/dma/fence exports.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub mod v3d {
    pub use crate::irq::{request_irq, IrqReturn, IrqSource};
    pub use crate::pci::serial_debug;
    pub use crate::rpi_mailbox::MBOX_BASE_PHYS;

    pub mod mm { pub use ::mm::{buddy, kva, phys_to_virt}; }
    pub mod sched { pub use ::sched::monotonic_ns; }

    /// One firmware property-tag call (`kapi_mbox_property`).
    ///
    /// # Safety
    /// Boot CPU only, nothing else touching the mailbox; `hhdm` as for `rpi_mailbox::property_call`.
    pub unsafe fn mbox_property(hhdm: usize, words: &[u32]) -> Result<&'static [u32], crate::rpi_mailbox::MboxError> {
        crate::rpi_mailbox::property_call(hhdm, words)
    }
    /// Record the ioremapped mailbox window (`kapi_mbox_property` owns it in the module build).
    pub fn mbox_set_virt(virt: usize) { crate::rpi_mailbox::set_mbox_virt(virt) }
    /// Shared PM window (`kapi_pm_base`).
    pub fn pm_base() -> Option<usize> { crate::rpi_pm::base() }
    /// Register the kexec stop hook (`kapi_dma_quiesce_register`).
    pub fn dma_quiesce_register(name: &'static str, f: fn() -> bool) { super::dma_quiesce_register(name, f) }
    /// Kernel-owned GPU VA (`kapi_v3d_va_alloc` / `kapi_v3d_va_free`).
    pub fn va_alloc(bytes: u64) -> Option<u64> { crate::drm_device_interface::v3d_kernel_va_alloc(bytes) }
    pub fn va_free(va: u64) { crate::drm_device_interface::v3d_kernel_va_free(va) }
    /// Tell the DRM layer the hardware is up (`kapi_v3d_arm`).
    pub fn arm() { crate::drm_device_interface::v3d_arm_hw() }
    pub fn alloc_fence_id() -> u64 { crate::drm_device_interface::v3d_alloc_fence_id() }
    pub fn fence_signalled(f: u64) -> bool { crate::drm_device_interface::v3d_fence_signalled(f) }
    pub fn syncobj_retire_fence(f: u64) { crate::drm_device_interface::syncobj_retire_fence(f) }
}
