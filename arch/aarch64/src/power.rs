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

/// Stop this CPU for good (`machine_halt`): mask interrupts, wait forever.
pub fn halt_forever() -> ! {
    loop {
        #[cfg(target_arch = "aarch64")]
        unsafe { core::arch::asm!("msr daifset, #0xf", "wfi", options(nomem, nostack)); }
        #[cfg(not(target_arch = "aarch64"))]
        core::hint::spin_loop();
    }
}
