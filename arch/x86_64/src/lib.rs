//! x86-64 architecture support.

#![no_std]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]

pub mod gdt;
pub mod idt;
pub mod keyboard;
pub mod paging;
#[cfg(target_arch = "x86_64")]
pub mod apic;
#[cfg(target_arch = "x86_64")]
pub mod pic;
#[cfg(target_arch = "x86_64")]
pub mod smp;
#[cfg(target_arch = "x86_64")]
pub mod syscall;
#[cfg(target_arch = "x86_64")]
pub mod timer;

#[no_mangle]
pub unsafe extern "C" fn arch_flush_cache_range(_addr: usize, _len: usize) {
    // x86-64 is coherent for framebuffer writes usually, or we use NO_CACHE.
    // If we wanted to be absolutely sure, we could use CLFLUSH, but it's slow.
}

/// Initialise x86-64 hardware: GDT, IDT, APIC, APIC timer, SYSCALL.
///
/// Init order matters:
///   1. GDT  — segments must be valid before IDT exceptions fire.
///   2. IDT  — exception/IRQ handlers must exist before APIC unmasks.
///   3. SSE  — enable OSFXSR/OSXMMEXCPT before any FPU/SSE use.
///   4. APIC — masks 8259 PIC, enables LAPIC; must precede timer init.
///   5. Timer — programs APIC timer (calibration uses PIT ch2 briefly).
///   6. SYSCALL — LSTAR/STAR/SFMASK, independent of interrupt routing.
pub fn init(info: &boot::BootInfo) {
    gdt::init();
    idt::init();
    #[cfg(target_arch = "x86_64")]
    unsafe {
        enable_sse();
        apic::set_hhdm_offset(info.hhdm_offset);

        // Limine Base Revision 1+ (Revision 6) does not map MMIO in HHDM.
        // We must map the LAPIC explicitly into our current page tables.
        // Since we swapped init order, mm is now initialized, and we can
        // use HHDM to access page tables.
        let apic_msr = apic::rdmsr(0x1B); // IA32_APIC_BASE_MSR
        let phys_base = apic_msr & 0x0000_FFFF_FFFF_F000; // APIC_BASE_MASK
        let virt_base = (phys_base as u64 + info.hhdm_offset) as usize;

        let cr3: usize;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
        let root = cr3 & !0xFFF;

        paging::map_4k(root, virt_base, phys_base as usize, 
            paging::PageTableFlags::PRESENT | paging::PageTableFlags::WRITABLE | paging::PageTableFlags::NO_CACHE);

        // Also map the framebuffer if present, as Limine might not have mapped it in HHDM.
        if info.framebuffer_base != 0 {
            let fb_size = info.framebuffer_pitch as usize * info.framebuffer_height as usize;
            let num_pages = (fb_size + 4095) / 4096;
            for i in 0..num_pages {
                let offset = i * 4096;
                let virt = info.framebuffer_base as usize + info.hhdm_offset as usize + offset;
                let phys = info.framebuffer_base as usize + offset;
                if !paging::map_4k(root, virt, phys,
                    paging::PageTableFlags::PRESENT | paging::PageTableFlags::WRITABLE | paging::PageTableFlags::NO_CACHE) {
                    // This might happen if we hit a huge page that we can't split yet.
                }
            }
            // Flush TLB to ensure the new mappings are active.
            core::arch::asm!("mov rax, cr3", "mov cr3, rax", out("rax") _);
        }

        apic::init();
    }
    #[cfg(target_arch = "x86_64")]
    unsafe { timer::init(); }
    #[cfg(target_arch = "x86_64")]
    syscall::init();
}

/// Enable SSE/SSE2 instructions in the CPU.
///
/// Must be called before any code path that uses XMM registers or
/// FXSAVE/FXRSTOR.  The context-switch assembly (`cpu_switch_to`) saves and
/// restores XMM0-XMM15 via `movdqu`, which requires CR4.OSFXSR=1.
///
/// Without this:
///   - CR4.OSFXSR=0  → `movdqu` raises #UD (Invalid Opcode, vector 6).
///   - CR0.TS=1      → any FPU/SSE access raises #NM (Device Not Available).
#[cfg(target_arch = "x86_64")]
unsafe fn enable_sse() {
    use core::arch::asm;
    let mut cr0: u64;
    asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
    cr0 &= !((1u64 << 2) | (1u64 << 3)); // clear EM (bit 2) and TS (bit 3)
    asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));

    let mut cr4: u64;
    asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
    cr4 |= (1u64 << 9) | (1u64 << 10); // set OSFXSR (bit 9) and OSXMMEXCPT (bit 10)
    asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
}

/// Returns the ID of the current CPU.
///
/// Placeholder implementation: always returns 0 (BSP).
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn cpu_id() -> usize {
    0
}

/// x86_64 serial output for early debugging.
///
/// Uses 16550 UART at COM1 (0x3F8).
#[cfg(target_arch = "x86_64")]
pub unsafe fn putc(c: u8) {
    use core::arch::asm;

    // Wait for transmit holding register to be empty (bit 5 of LSR)
    loop {
        let lsr: u8;
        asm!("in al, dx", out("al") lsr, in("dx") 0x3FDu16, options(nomem, nostack));
        if lsr & 0x20 != 0 { break; }
    }

    // Send the character
    asm!("out dx, al", in("dx") 0x3F8u16, in("al") c, options(nomem, nostack));
}

/// Compatibility wrapper for serial output.
pub fn arch_serial_putc(c: u8) {
    unsafe { putc(c); }
}

#[no_mangle]
pub extern "C" fn arch_interrupt_save() -> usize {
    let rflags: usize;
    unsafe {
        core::arch::asm!("pushfq", "pop {}", out(reg) rflags);
        core::arch::asm!("cli");
    }
    rflags
}

#[no_mangle]
pub extern "C" fn arch_interrupt_restore(flags: usize) {
    if flags & (1 << 9) != 0 {
        unsafe { core::arch::asm!("sti"); }
    }
}

/// x86_64 serial input.
///
/// Returns Some(byte) if a character is available in the UART RX FIFO.
#[cfg(target_arch = "x86_64")]
pub unsafe fn serial_read_byte() -> Option<u8> {
    use core::arch::asm;
    let lsr: u8;
    asm!("in al, dx", out("al") lsr, in("dx") 0x3FDu16, options(nomem, nostack));
    
    if lsr & 0x01 != 0 {
        let b: u8;
        asm!("in al, dx", out("al") b, in("dx") 0x3F8u16, options(nomem, nostack));
        Some(b)
    } else {
        None
    }
}

/// Returns true if the UART RX FIFO is not empty.
#[cfg(target_arch = "x86_64")]
pub unsafe fn serial_has_data() -> bool {
    use core::arch::asm;
    let lsr: u8;
    asm!("in al, dx", out("al") lsr, in("dx") 0x3FDu16, options(nomem, nostack));
    lsr & 0x01 != 0
}
