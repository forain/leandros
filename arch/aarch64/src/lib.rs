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

/// Monotonic nanoseconds since boot, for kernel crates that sit below this one
/// in the dependency graph and so cannot call `timer::monotonic_ns` directly —
/// `evdev-server` stamps input events with it. Same contract as the function it
/// forwards to: whole ticks plus a CNTVCT_EL0 fraction, never decreasing, and
/// callable from IRQ context (two atomic loads and an `mrs`, no locks, no user
/// memory).
#[no_mangle]
pub extern "C" fn arch_monotonic_ns() -> u64 {
    timer::monotonic_ns()
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
        
        // Map UART to its HHDM address
        let uart_phys = uart::BASE;
        let uart_virt = if boot_info.hhdm_offset != 0 {
            uart_phys + boot_info.hhdm_offset as usize
        } else {
            uart_phys
        };
        
        // We need to map it in the current page table.
        // paging::map_4k uses mm::phys_to_virt which is now set.
        paging::map_4k(root_phys as *mut u64, uart_virt, uart_phys, device_flags);

        // Switch to the HHDM virtual address immediately so serial output works
        // before the TLB flush. New mappings are visible to the page table walker
        // on TLB miss without an explicit invalidation.
        uart::set_base(uart_virt);
        uart::reinit(uart_virt);

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
            let fb_start_phys = boot_info.framebuffer_base as usize & !4095;
            // Mirror kernel_main's pitch fallback (main.rs's `pitch_bytes` calc):
            // some firmware DTBs omit or zero the `stride` property, in which case
            // the console falls back to width*4 bytes/row when it *draws*. If this
            // mapping used the raw (possibly zero) DTB pitch instead, it would map
            // far fewer pages than the console later writes into, and the very
            // first fb_putc/clear would walk off the mapped region into an
            // unhandled data abort — silently hanging with a black screen and no
            // console output at all.
            let effective_pitch = if (boot_info.framebuffer_pitch as usize) < boot_info.framebuffer_width as usize * 4 {
                boot_info.framebuffer_width as usize * 4
            } else {
                boot_info.framebuffer_pitch as usize
            };
            let fb_size = effective_pitch * boot_info.framebuffer_height as usize;
            let fb_end_phys = (boot_info.framebuffer_base as usize + fb_size + 4095) & !4095;
            let num_pages = (fb_end_phys - fb_start_phys) / 4096;
            let fb_flags = paging::PageDescFlags::VALID | paging::PageDescFlags::AF | paging::PageDescFlags::INNER_SHR | paging::PageDescFlags::ATTR_NOCACHE;
            
            crate::uart::serial_print_str("[ARCH] Mapping framebuffer 0x");
            crate::uart::print_hex(boot_info.framebuffer_base as usize);
            crate::uart::serial_print_str(" size=0x");
            crate::uart::print_hex(fb_size);
            crate::uart::serial_print_str("\n");

            for i in 0..num_pages {
                let offset = i * 4096;
                let virt = 0xFFFF_A000_0000_0000 + offset;
                let phys = fb_start_phys + offset;
                if !paging::map_4k(root_phys as *mut u64, virt, phys, fb_flags) {
                    crate::uart::serial_print_str("[ARCH] Failed to map framebuffer page at 0x");
                    crate::uart::print_hex(virt);
                    crate::uart::serial_print_str("\n");
                }
            }
        }

        // Flush TLB to ensure the new mappings are active.
        core::arch::asm!("tlbi vmalle1", "dsb ish", "isb", options(nostack));

        // Initialize exception vectors
        exception::init();

        // Initialize GIC (needs virtual addresses; helpers use mm::phys_to_virt)
        gic::init();

        // Ensure timer is in a sane state
        init_timer();

        // Bring up secondary CPUs last: page tables, GIC distributor and the
        // buddy allocator are all ready now.  MPIDRs that don't exist (fewer
        // cores than MAX_APS) fail CPU_ON harmlessly.  The APs initialise
        // their banked GIC/timer state and park in sched::ap_entry until the
        // BSP calls sched::run().
        //
        // Skipped on raspi4b: unlike `virt` (APs stay PSCI-powered-off until
        // this call) and presumably real RPi5 hardware, QEMU's raspi4b board
        // releases all cores simultaneously at reset (see
        // kernel/src/entry_aarch64.s's park loop) — APs 1-3 are already
        // alive, just parked in our own WFE loop, not powered off. A CPU_ON
        // SMC on an already-running core should return PSCI's ALREADY_ON
        // error harmlessly per spec, but empirically (confirmed via GDB
        // stub register dumps: the BSP gets stuck at PC=0x200/EL3h with
        // X00 still holding the CPU_ON function ID) this board's SMC
        // dispatch hangs instead. SMP is out of scope for this QEMU-only
        // sdhci-driver test target — single-CPU operation is sufficient.
        #[cfg(not(feature = "raspi4b"))]
        smp::smp_init(&[1, 2, 3, 4, 5, 6, 7]);
    }
}

/// Map a physical MMIO range into the kernel (TTBR1) page tables with device memory
/// attributes (nGnRE, MAIR index 1). Must be called after mm::init_with_map so
/// the buddy allocator can supply intermediate page table pages.
///
/// `phys` and `size` need not be page-aligned; the function rounds up internally.
/// The mapping appears at `phys + hhdm_offset` in the kernel address space.
pub unsafe fn map_mmio_range(phys: usize, size: usize, hhdm_offset: usize) {
    let ttbr1: usize;
    core::arch::asm!("mrs {}, ttbr1_el1", out(reg) ttbr1);
    let root_phys = (ttbr1 & 0x0000_FFFF_FFFF_F000) as *mut u64;

    let device_flags = paging::PageDescFlags::VALID
        | paging::PageDescFlags::AF
        | paging::PageDescFlags::INNER_SHR
        | paging::PageDescFlags::ATTR_DEV;

    let virt = phys + hhdm_offset;
    paging::map_range(root_phys, virt, phys, size, device_flags);

    // DSB + ISB to ensure page table writes are ordered before any subsequent
    // load/store that targets the newly-mapped region.
    core::arch::asm!("dsb ish", "isb", options(nostack));
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
    unsafe { timer::init(); }
}
