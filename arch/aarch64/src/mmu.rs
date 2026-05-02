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
    
    extern "C" { fn arch_serial_putc(c: u8); }
    arch_serial_putc(b'O');
    arch_serial_putc(b'K');
    arch_serial_putc(b'!');
}
