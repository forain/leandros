//! SoC temperature, for fan curves and reporting. On the Raspberry Pi 5 it is
//! the BCM2712 AVS monitor (firmware GET_TEMPERATURE as fallback,
//! `rpi_misc.rs`); elsewhere there is no sensor yet and the API answers None.
//!
//! `/proc/thermal` and the aarch64 `/proc/cpufreq` are built by the VFS server
//! through the `arch_thermal_report` / `arch_cpufreq_report` hooks below.

/// SoC temperature in millidegrees Celsius, if the board has a sensor.
pub fn soc_millicelsius() -> Option<i32> {
    #[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
    { crate::rpi_misc::soc_millicelsius() }
    #[cfg(not(all(target_arch = "aarch64", feature = "rpi5")))]
    { None }
}

/// `/proc/thermal`: one `<zone>: <temp_mC>` line (Linux `thermal_zone0/temp` units).
#[no_mangle]
pub extern "C" fn arch_thermal_report(buf: *mut u8, cap: usize) -> usize {
    if buf.is_null() || cap == 0 { return 0; }
    let out = unsafe { core::slice::from_raw_parts_mut(buf, cap) };
    let mut b = crate::rpi_misc_logic::Buf::new(out);
    match soc_millicelsius() {
        Some(t) => { b.s("soc: ").i(t as i64).s(" (").mc(t).s(" C)\n"); }
        None => { b.s("thermal: no sensor\n"); }
    }
    b.n
}

/// `/proc/cpufreq` on aarch64 (x86_64 has its own in `arch_x86_64::cpufreq`).
#[cfg(target_arch = "aarch64")]
#[no_mangle]
pub extern "C" fn arch_cpufreq_report(buf: *mut u8, cap: usize) -> usize {
    if buf.is_null() || cap == 0 { return 0; }
    let out = unsafe { core::slice::from_raw_parts_mut(buf, cap) };
    #[cfg(feature = "rpi5")]
    { crate::rpi_misc::cpufreq_report(out) }
    #[cfg(not(feature = "rpi5"))]
    {
        let mut b = crate::rpi_misc_logic::Buf::new(out);
        b.s("cpufreq: no driver\n");
        b.n
    }
}
