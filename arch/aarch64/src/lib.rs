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

#[no_mangle]
pub extern "C" fn arch_interrupt_save() -> usize {
    let daif: usize;
    unsafe {
        core::arch::asm!("mrs {}, daif", out(reg) daif);
        core::arch::asm!("msr daifset, #2"); // Disable IRQ
    }
    daif
}

#[no_mangle]
pub extern "C" fn arch_interrupt_restore(flags: usize) {
    unsafe {
        core::arch::asm!("msr daif, {}", in(reg) flags);
    }
}

pub fn init(boot_info: &boot::BootInfo) {
    unsafe {
        // Initialize MMU features and memory attributes (MAIR_EL1)
        mmu::enable_identity(boot_info);

        // Limine Base Revision 1+ (Revision 6) does not map MMIO in HHDM.
        // We must map critical devices explicitly into our current page tables.
        let ttbr1: usize;
        core::arch::asm!("mrs {}, ttbr1_el1", out(reg) ttbr1);
        let root_phys = ttbr1 & 0x0000_FFFF_FFFF_F000;

        let device_flags = paging::PageDescFlags::VALID | paging::PageDescFlags::AF | paging::PageDescFlags::INNER_SHR | paging::PageDescFlags::ATTR_DEV;
        
        // Map UART (physical 0x09000000) to its HHDM address
        let uart_phys = 0x09000000;
        let uart_virt = if boot_info.hhdm_offset != 0 {
            uart_phys + boot_info.hhdm_offset as usize
        } else {
            uart_phys
        };
        
        // We need to map it in the current page table.
        // paging::map_4k uses mm::phys_to_virt which is now set.
        paging::map_4k(root_phys as *mut u64, uart_virt, uart_phys, device_flags);

        // Map GIC Distributor and CPU interface to their HHDM addresses
        let gicd_phys = gic::GICD_BASE;
        let gicc_phys = gic::GICC_BASE;
        let gicd_virt = if boot_info.hhdm_offset != 0 {
            gicd_phys + boot_info.hhdm_offset as usize
        } else {
            gicd_phys
        };
        let gicc_virt = if boot_info.hhdm_offset != 0 {
            gicc_phys + boot_info.hhdm_offset as usize
        } else {
            gicc_phys
        };
        paging::map_4k(root_phys as *mut u64, gicd_virt, gicd_phys, device_flags);
        paging::map_4k(root_phys as *mut u64, gicc_virt, gicc_phys, device_flags);
        if boot_info.framebuffer_base != 0 {
            let fb_size = boot_info.framebuffer_pitch as usize * boot_info.framebuffer_height as usize;
            let num_pages = (fb_size + 4095) / 4096;
            let fb_flags = paging::PageDescFlags::VALID | paging::PageDescFlags::AF | paging::PageDescFlags::INNER_SHR | paging::PageDescFlags::ATTR_NOCACHE;
            
            crate::uart::serial_print_str("[ARCH] Mapping framebuffer 0x");
            crate::uart::print_hex(boot_info.framebuffer_base as usize);
            crate::uart::serial_print_str(" size=0x");
            crate::uart::print_hex(fb_size);
            crate::uart::serial_print_str("\n");

            for i in 0..num_pages {
                let offset = i * 4096;
                let virt = boot_info.framebuffer_base as usize + boot_info.hhdm_offset as usize + offset;
                let phys = boot_info.framebuffer_base as usize + offset;
                if !paging::map_4k(root_phys as *mut u64, virt, phys, fb_flags) {
                    crate::uart::serial_print_str("[ARCH] Failed to map framebuffer page at 0x");
                    crate::uart::print_hex(virt);
                    crate::uart::serial_print_str("\n");
                }
            }
        }

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

        // Initialize exception vectors
        exception::init();

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
