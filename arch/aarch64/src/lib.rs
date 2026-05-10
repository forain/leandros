//! AArch64 architecture support (ARMv8-A).

#![no_std]

pub mod exception;
pub mod gic;
pub mod mmu;
pub mod paging;
pub mod smp;
pub mod timer;
pub mod uart;

#[no_mangle]
pub unsafe extern "C" fn arch_flush_cache_range(addr: usize, len: usize) {
    let mut curr = addr & !63;
    let end = addr + len;
    while curr < end {
        core::arch::asm!("dc cvau, {}", in(reg) curr, options(nostack));
        curr += 64;
    }
    core::arch::asm!("dsb ish", "isb", options(nostack));
}

pub fn init(boot_info: &boot::BootInfo) {
    unsafe {
        // Enable SIMD/FP (set CPACR_EL1.FPEN to 0b11)
        let mut cpacr: u64;
        core::arch::asm!("mrs {}, cpacr_el1", out(reg) cpacr);
        cpacr |= 3 << 20;
        core::arch::asm!("msr cpacr_el1, {}", in(reg) cpacr);
        core::arch::asm!("isb");

        // Initialize exception vectors FIRST so we can catch early faults
        exception::init();

        // Limine Base Revision 1+ (Revision 6) does not map MMIO in HHDM.
        // We must map critical devices explicitly into our current page tables.
        let ttbr1: usize;
        core::arch::asm!("mrs {}, ttbr1_el1", out(reg) ttbr1);
        let root = ttbr1 as *mut u64;

        let device_flags = paging::PageDescFlags::VALID | paging::PageDescFlags::AF | paging::PageDescFlags::INNER_SHR | paging::PageDescFlags::ATTR_DEV;
        
        // Map UART (physical 0x09000000) to its HHDM address
        let uart_virt = if boot_info.hhdm_offset != 0 {
            0x09000000 + boot_info.hhdm_offset as usize
        } else {
            0x09000000
        };
        paging::map_4k(root, uart_virt, 0x09000000, device_flags);

        // Map GIC Distributor and CPU interface to their HHDM addresses
        let gicd_virt = if boot_info.hhdm_offset != 0 {
            gic::GICD_BASE + boot_info.hhdm_offset as usize
        } else {
            gic::GICD_BASE
        };
        let gicc_virt = if boot_info.hhdm_offset != 0 {
            gic::GICC_BASE + boot_info.hhdm_offset as usize
        } else {
            gic::GICC_BASE
        };
        paging::map_4k(root, gicd_virt, gic::GICD_BASE, device_flags);
        paging::map_4k(root, gicc_virt, gic::GICC_BASE, device_flags);

        // Flush TLB to ensure the new mappings are active.
        core::arch::asm!("tlbi vmalle1", "dsb ish", "isb", options(nostack));

        // Initialize UART: use HHDM mapping
        uart::set_base(uart_virt);
        if boot_info.uart_base != 0 {
            uart::reinit(uart_virt);
        } else {
            // Standard init if needed, but set_base is already done
            uart::init();
        }

        // Initialize GIC (needs virtual addresses; helpers use mm::phys_to_virt)
        gic::init();

        // Ensure timer is in a sane state
        init_timer();
    }
}

/// Early timer check/init.
fn init_timer() {
    let freq: u64;
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) freq);
    }
    // Frequency should be non-zero.
    const MIN_FREQ: u64 = 1_000_000;    // 1 MHz — no credible board is slower
    const MAX_FREQ: u64 = 250_000_000;  // 250 MHz — generous upper bound
    if freq < MIN_FREQ || freq > MAX_FREQ {
        // Don't panic, just log if possible
    }
}
