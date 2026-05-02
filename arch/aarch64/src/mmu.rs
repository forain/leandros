//! AArch64 Memory Management Unit (MMU) initialization.

use boot::BootInfo;

#[repr(C, align(4096))]
struct PageTable([u64; 512]);

// ── Static Page Tables ───────────────────────────────────────────────────────
// We use these for the initial identity and kernel mappings.
static mut ID_L0: PageTable = PageTable([0; 512]);
static mut ID_L1: PageTable = PageTable([0; 512]);

/// Enable the AArch64 MMU with identity and higher-half kernel mappings.
pub unsafe fn enable_identity(boot_info: &BootInfo) {
    // ── L1 block descriptor attribute words ──────────────────────────────────
    // Normal WB/WA inner-shareable (MAIR index 0): 0x701
    let normal: u64 = 0b01 | (0b000 << 2) | (0b11 << 8) | (1 << 10);
    // Device nGnRnE non-shareable (MAIR index 1): 0x405
    let _device: u64 = 0b01 | (0b001 << 2) | (0b00 << 8) | (1 << 10);

    // ── Populate L1 table ─────────────────────────────────────────────────────
    // 1. Identity map the first 1GB (where UART at 0x09000000 lives).
    ID_L1.0[0] = (0u64 << 30) | normal;
    
    // 2. Determine physical address of the kernel.
    let mut kernel_phys_base = 0usize;
    
    if let Some(resp) = boot::limine::KERNEL_ADDR_REQUEST.response() {
        kernel_phys_base = resp.physical_base as usize;
    } else {
        // Fallback: scan memory regions for the kernel.
        for region in boot_info.memory_regions() {
            if region.base >= 0x100000 && region.length >= 0x100000 {
                kernel_phys_base = region.base as usize;
                break;
            }
        }
    }
    if kernel_phys_base == 0 { kernel_phys_base = 0x599cb000; } // QEMU fallback

    // Round kernel physical base down to 1GB alignment for L1 block mapping.
    let kernel_phys_aligned = kernel_phys_base as u64 & !0x3FFFFFFF;

    // 3. Map kernel at high virtual address (0xffffffff80000000).
    // 0xffffffff80000000 is index 510 in L1.
    // Ensure it is EXECUTABLE by NOT setting PXN/UXN bits.
    ID_L1.0[510] = kernel_phys_aligned | normal;
    ID_L1.0[511] = (kernel_phys_aligned + 0x40000000) | normal;

    // Determine physical addresses of the tables themselves.
    let virt_to_phys = |v: usize| {
        v - 0xffffffff80000000 + kernel_phys_base
    };

    let l1_phys = virt_to_phys(core::ptr::addr_of!(ID_L1) as usize) as u64;
    let l0_phys = virt_to_phys(core::ptr::addr_of!(ID_L0) as usize) as u64;

    // L0 entry 0: identity map [0, 512GB) via L1
    ID_L0.0[0] = l1_phys | 0b11; // Table descriptor

    // L0 entry 511: higher-half map [-512GB, 0) via same L1
    ID_L0.0[511] = l1_phys | 0b11;

    // ── TCR_EL1 (Translation Control Register) ───────────────────────────────
    let tcr: u64 = (25 << 0) | (3 << 12) | (1 << 10) | (1 << 8) | (0 << 14) |
                   (25 << 16) | (3 << 28) | (1 << 26) | (1 << 24) | (2 << 30);
    
    core::arch::asm!("msr tcr_el1, {}", in(reg) tcr);
    core::arch::asm!("isb");

    // ── TTBR0_EL1 and TTBR1_EL1 ──────────────────────────────────────────────
    core::arch::asm!("msr ttbr0_el1, {}", in(reg) l0_phys);
    core::arch::asm!("msr ttbr1_el1, {}", in(reg) l0_phys);
    core::arch::asm!("isb");

    // ── Enable MMU ───────────────────────────────────────────────────────────
    let mut sctlr: u64;
    core::arch::asm!("mrs {}, sctlr_el1", out(reg) sctlr);
    sctlr |= (1 << 0) | (1 << 2) | (1 << 12); // M, C, I bits
    core::arch::asm!("msr sctlr_el1, {}", in(reg) sctlr);
    core::arch::asm!("isb");
}
