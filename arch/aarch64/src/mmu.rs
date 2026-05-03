//! AArch64 Memory Management Unit (MMU) initialization.

use boot::BootInfo;

/// Minimal initialization: Enable FPU/SIMD and ensure correct stack selection.
/// We keep Limine's MMU configuration (TCR, MAIR, TTBR1) for stability.
pub unsafe fn enable_identity(_boot_info: &BootInfo) {
    // 1. Enable FP/SIMD access (CPACR_EL1.FPEN = 0b11)
    let mut cpacr: u64;
    core::arch::asm!("mrs {}, cpacr_el1", out(reg) cpacr);
    cpacr |= 3 << 20;
    core::arch::asm!("msr cpacr_el1, {}", "isb", in(reg) cpacr);

    // 2. Ensure we are using SP_EL1 for the kernel
    core::arch::asm!("msr SPSel, #1", "isb");

    // 3. Set MAIR_EL1
    // Index 0: Normal Memory, Outer/Inner Write-Back (0xFF)
    // Index 1: Device-nGnRE (0x04)
    // Index 2: Device-nGnRnE (0x00)
    // Index 3: Normal Non-Cacheable (0x44)
    let mair: u64 = 0xFF | (0x04 << 8) | (0x00 << 16) | (0x44 << 24);
    core::arch::asm!("msr mair_el1, {}", "isb", in(reg) mair);
}
