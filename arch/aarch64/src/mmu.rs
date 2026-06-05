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

    // 4. Configure TCR_EL1
    // T1SZ = 16 (48-bit VA for TTBR1)
    // T0SZ = 16 (48-bit VA for TTBR0)
    // TG1  = 0b10 (4KB granule for TTBR1)
    // TG0  = 0b00 (4KB granule for TTBR0)
    // IPS  = 0b010 (36-bit PA, up to 64GB) or check ID_AA64MMFR0_EL1.PARange
    let mut tcr: u64 = (16 << 0) | (16 << 16); // T0SZ, T1SZ
    tcr |= (0b00 << 14); // TG0 = 4KB
    tcr |= (0b10 << 30); // TG1 = 4KB
    tcr |= (0b010 << 32); // IPS = 36-bit (good for virt machine)
    
    // SH0/SH1 = 0b11 (Inner Shareable)
    // ORGN0/IRGN0 = 0b01 (Normal WB/WA)
    // ORGN1/IRGN1 = 0b01 (Normal WB/WA)
    tcr |= (0b11 << 12) | (0b11 << 28); // SH0, SH1
    tcr |= (0b01 << 8) | (0b01 << 10);  // IRGN0, ORGN0
    tcr |= (0b01 << 24) | (0b01 << 26); // IRGN1, ORGN1
    
    core::arch::asm!("msr tcr_el1, {}", "isb", in(reg) tcr);
}
