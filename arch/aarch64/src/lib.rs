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

        let mut ttbr1: u64;
        core::arch::asm!("mrs {}, ttbr1_el1", out(reg) ttbr1);
        let root = (ttbr1 & 0x0000_FFFF_FFFF_F000) as *mut u64;

        let device_flags = paging::PageDescFlags::VALID | paging::PageDescFlags::AF | 
                           paging::PageDescFlags::INNER_SHR | paging::PageDescFlags::ATTR_DEV;

        // Map UART in HHDM
        let uart_base = if info.uart_base != 0 { info.uart_base as usize } else { uart::BASE };
        if !paging::map_4k(root, uart_base + info.hhdm_offset as usize, uart_base, device_flags) {
            extern "C" { fn serial_write_byte(b: u8); }
            serial_write_byte(b'U'); serial_write_byte(b'F');
        }

        // Map GICD in HHDM (64KB)
        for i in 0..16 {
            let phys = gic::GICD_BASE + i * 4096;
            if !paging::map_4k(root, phys + info.hhdm_offset as usize, phys, device_flags) {
                extern "C" { fn serial_write_byte(b: u8); }
                serial_write_byte(b'G'); serial_write_byte(b'D');
            }
        }

        // Map GICC in HHDM (64KB)
        for i in 0..16 {
            let phys = gic::GICC_BASE + i * 4096;
            if !paging::map_4k(root, phys + info.hhdm_offset as usize, phys, device_flags) {
                extern "C" { fn serial_write_byte(b: u8); }
                serial_write_byte(b'G'); serial_write_byte(b'C');
            }
        }

        // Also map the framebuffer if present, as Limine might not have mapped it in HHDM.
        if info.framebuffer_base != 0 {
            let fb_size = info.framebuffer_pitch as usize * info.framebuffer_height as usize;
            let num_pages = (fb_size + 4095) / 4096;
            for i in 0..num_pages {
                let offset = i * 4096;
                let virt = info.framebuffer_base as usize + info.hhdm_offset as usize + offset;
                let phys = info.framebuffer_base as usize + offset;
                
                // Use ATTR_NOCACHE for framebuffer to ensure writes hit the screen while allowing efficient access.
                let flags = paging::PageDescFlags::VALID | paging::PageDescFlags::AF | 
                            paging::PageDescFlags::INNER_SHR | paging::PageDescFlags::ATTR_NOCACHE;

                if !paging::map_4k(root, virt, phys, flags) {
                    // This might happen if we hit a huge page that we can't split yet.
                }
            }
        }
        
        // Flush TLB to ensure the new mappings are active.
        paging::arch_tlb_shootdown_all();
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
