//! AArch64 architecture support (ARMv8-A).

#![no_std]

pub mod exception;
pub mod gic;
pub mod mmu;
pub mod paging;
pub mod smp;
pub mod timer;
pub mod uart;

/// Returns the current CPU's logical index (0 for BSP, etc.).
#[no_mangle]
pub extern "C" fn cpu_id() -> usize {
    unsafe { smp::arch_cpu_id() }
}

/// Initialise AArch64 hardware.
///
/// Call order matters:
///   0. MAIR_EL1 — memory attribute indices must be set before the MMU is used
///   1. MMU — identity mapping; must come after MAIR and before caches/coherency
///   2. exception vectors (VBAR_EL1)
///   3. GIC distributor + CPU interface
///   4. generic timer — arms the countdown and unmasks IRQs
pub fn init(info: &boot::BootInfo) {
    unsafe {
        // 1. Initialise exception vectors early to catch any faults during MMU setup.
        exception::init();

        // 2. Initialise MMU (identity + higher-half) and architectural registers (MAIR, TCR, CPACR)
        mmu::enable_identity(info);
    }

    // 3. Initialise peripherals
    gic::init();
    timer::init();

    // Validate that the generic timer frequency was set by firmware.
    // CNTFRQ_EL0 must be non-zero and within a plausible range.
    // RPi5: 54 MHz.  QEMU virt: 62.5 MHz.  Typical range: 1–250 MHz.
    let freq = timer::freq();
    if freq == 0 {
        panic!("arch::init: CNTFRQ_EL0 == 0 — firmware did not set the \
                generic timer frequency; check device tree /timer or \
                firmware version");
    }
    const MIN_FREQ: u64 = 1_000_000;    // 1 MHz — no credible board is slower
    const MAX_FREQ: u64 = 250_000_000;  // 250 MHz — generous upper bound
    if freq < MIN_FREQ || freq > MAX_FREQ {
        panic!("arch::init: CNTFRQ_EL0 out of range (plausible 1–250 MHz)");
    }
}
