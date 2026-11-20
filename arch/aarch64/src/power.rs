//! Machine power-off and reset through PSCI — the arch half of
//! `kernel_power_off` / `kernel_restart` (kernel/src/power.rs), as Linux's
//! `psci_sys_poweroff` / `psci_sys_reset` (drivers/firmware/psci). On QEMU
//! `virt` (TCG and HVF alike) SYSTEM_OFF makes QEMU exit and SYSTEM_RESET
//! resets the machine through the firmware.

/// PSCI 0.2 function IDs (SMC32 convention; these take no arguments).
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;
const PSCI_SYSTEM_RESET: u64 = 0x8400_0009;

/// PSCI SYSTEM_OFF. Returns only if the firmware refused (no PSCI).
///
/// # Safety
/// EL1; the machine stops.
pub unsafe fn system_off() {
    #[cfg(all(target_arch = "aarch64", not(feature = "raspi4b")))]
    { let _ = crate::smp::psci_call(PSCI_SYSTEM_OFF, 0, 0, 0); }
}

/// PSCI SYSTEM_RESET. Returns only if the firmware refused (no PSCI).
///
/// # Safety
/// EL1; the machine restarts.
pub unsafe fn system_reset() {
    #[cfg(all(target_arch = "aarch64", not(feature = "raspi4b")))]
    { let _ = crate::smp::psci_call(PSCI_SYSTEM_RESET, 0, 0, 0); }
}

/// PSCI 1.1 SYSTEM_RESET2 (SMC64) with reset_type 0 = SYSTEM_WARM_RESET and
/// `cookie`, if PSCI_FEATURES reports it. The Raspberry Pi 5 kernel passes
/// the `reboot N` partition this way (raspberrypi/linux "firmware/psci: Pass
/// given partition number through": "Pi 5 uses BL31 as its armstub file, so
/// the reset goes via PSCI"). Returns if unsupported or refused.
///
/// # Safety
/// EL1; the machine restarts.
pub unsafe fn system_reset2_warm(cookie: u64) {
    #[cfg(all(target_arch = "aarch64", not(feature = "raspi4b")))]
    {
        const PSCI_FEATURES: u64 = 0x8400_000A;
        const PSCI_SYSTEM_RESET2_64: u64 = 0xC400_0012;
        if crate::smp::psci_call(PSCI_FEATURES, PSCI_SYSTEM_RESET2_64, 0, 0) < 0 { return; }
        let _ = crate::smp::psci_call(PSCI_SYSTEM_RESET2_64, 0, cookie, 0);
    }
    #[cfg(not(all(target_arch = "aarch64", not(feature = "raspi4b"))))]
    let _ = cookie;
}

/// Stop this CPU for good (`machine_halt`): mask interrupts, wait forever.
pub fn halt_forever() -> ! {
    loop {
        #[cfg(target_arch = "aarch64")]
        unsafe { core::arch::asm!("msr daifset, #0xf", "wfi", options(nomem, nostack)); }
        #[cfg(not(target_arch = "aarch64"))]
        core::hint::spin_loop();
    }
}
