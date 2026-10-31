//! HDMI audio at the kernel side of the platform registry (kmodpi H2): the boot-probe facts a
//! module reads, the announcement of `leandros,pi5-hdmi-audio`, and the built-in claimer. Kept apart
//! from `hdmi_audio/` because that file is also compiled into the module (modules/hdmi-audio).

/// Platform-registry compatible of the HDMI audio function; the boot announces it once the hdmi
/// probe is done. The built-in driver claims it ([`register`]); with `CONFIG_HDMI_AUDIO=m` the
/// module's `of:` alias loads instead.
pub const COMPATS: &[&str] = &["leandros,pi5-hdmi-audio"];

/// Built-in claimer (`CONFIG_HDMI_AUDIO=y`): the PipeWire backend probes the sink itself, so this
/// only marks the device as owned (no autoload).
#[cfg(feature = "hdmi-audio")]
pub fn register() {
    use core::sync::atomic::{AtomicBool, Ordering};
    static ONCE: AtomicBool = AtomicBool::new(false);
    if !ONCE.swap(true, Ordering::AcqRel) { crate::platform::register(COMPATS, |_, _| true); }
}

/// Announce the HDMI audio function through the platform registry once the boot probe is done.
/// True when a driver (built in or module) claimed it.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub fn announce() -> bool {
    #[cfg(feature = "hdmi-audio")] register();
    let d = crate::platform::PlatDev { base: 0x10_7c72_0000, size: 0x100, ..Default::default() };
    crate::platform::announce(COMPATS[0], d)
}

/// Boot-probe facts for hdmi0 as `(state, port_flags, pix_khz, edid_sink)`; all 0 off the rpi5 build.
/// Independent of the `hdmi-audio` feature: the probe is also what an out-of-tree audio module reads.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub fn sink_info() -> (u32, u32, u32, u32) {
    use crate::hdmi_probe as p;
    use core::sync::atomic::Ordering::Relaxed;
    (p::state(), p::PORT_FLAGS[0].load(Relaxed), p::PORT_PIX_KHZ[0].load(Relaxed), p::EDID_SINK.load(Relaxed))
}
#[cfg(not(all(target_arch = "aarch64", feature = "rpi5")))]
pub fn sink_info() -> (u32, u32, u32, u32) { (0, 0, 0, 0) }

/// True on a D0 (not C0) BCM2712; the audio FIFO threshold layout differs.
#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub fn is_d0() -> bool { crate::bcm2712_id::stepping().stepping != crate::bcm2712_id::Stepping::C0 }
#[cfg(not(all(target_arch = "aarch64", feature = "rpi5")))]
pub fn is_d0() -> bool { false }
