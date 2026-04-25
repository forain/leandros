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
pub fn init(_info: &boot::BootInfo) {
    // MAIR_EL1: index 0 = normal WB/WA memory (0xFF),
    //           index 1 = device nGnRnE memory   (0x00).
    unsafe {
        core::arch::asm!(
            "msr MAIR_EL1, {v}",
            "isb",
            v = in(reg) 0x00FFu64,
            options(nostack)
        );

        // Enable FP/SIMD access via CPACR_EL1.FPEN (bits 21:20) = 0b11
        // This prevents "Access to SVE, Advanced SIMD, or floating-point functionality trapped" exceptions
        let mut cpacr: u64;
        core::arch::asm!("mrs {}, CPACR_EL1", out(reg) cpacr, options(nostack, nomem));
        cpacr |= 0b11 << 20;  // FPEN = 0b11 (no trapping of FP/SIMD at EL0 and EL1)
        core::arch::asm!(
            "msr CPACR_EL1, {}",
            "isb",
            in(reg) cpacr,
            options(nostack)
        );

        // Initialise PL011 UART for early debug output.
        uart::init();
        // Enable the MMU with a 4 GiB identity mapping.  MAIR must be written
        // first so the translation table walks use the correct memory attributes.
        mmu::enable_identity();
    }
    exception::init();
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
