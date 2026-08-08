//! x86-64 architecture support.

#![no_std]
#![cfg_attr(target_arch = "x86_64", feature(abi_x86_interrupt))]

pub mod gdt;
pub mod idt;
pub mod ioapic;
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
///
/// IA32_PAT comes before all of it: it needs nothing but `rdmsr`/`wrmsr` and
/// the I/O-port UART, and everything after it — the LAPIC and framebuffer
/// mappings below, and every AP `smp_init` starts at the end — must already see
/// the finished value.
pub fn init(info: &boot::BootInfo) {
    #[cfg(target_arch = "x86_64")]
    unsafe { paging::init_pat_bsp(); }

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
        }

        // Flush TLB to ensure the new mappings are active.
        core::arch::asm!("mov rax, cr3", "mov cr3, rax", out("rax") _);

        apic::init();
        
        ioapic::init(info.hhdm_offset, root);
        // Route IRQ 1 (keyboard) -> APIC ID 0, Vector 33
        ioapic::set_irq(1, 0, 33);
    }
    #[cfg(target_arch = "x86_64")]
    unsafe { timer::init(); }
    #[cfg(target_arch = "x86_64")]
    syscall::init();

    // Remember the boot CR3 as the canonical kernel page table.  The
    // scheduler resets every CPU to it after running a task so a freed task
    // root is never left loaded (see paging::arch_load_kernel_page_table).
    #[cfg(target_arch = "x86_64")]
    unsafe { paging::capture_kernel_root(); }

    // Bring up the Application Processors last: everything they inherit
    // (page tables, LAPIC mapping, timer calibration, IDT contents) is ready
    // now.  They initialise their per-CPU state in smp::sched_ap_entry and
    // then park in sched::ap_entry until the BSP calls sched::run().
    #[cfg(target_arch = "x86_64")]
    unsafe { smp::smp_init(sched::MAX_CPUS - 1); }
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
pub(crate) unsafe fn enable_sse() {
    use core::arch::asm;
    let mut cr0: u64;
    asm!("mov {}, cr0", out(reg) cr0, options(nomem, nostack));
    cr0 &= !((1u64 << 2) | (1u64 << 3)); // clear EM (bit 2) and TS (bit 3)
    asm!("mov cr0, {}", in(reg) cr0, options(nomem, nostack));

    let mut cr4: u64;
    asm!("mov {}, cr4", out(reg) cr4, options(nomem, nostack));
    cr4 |= (1u64 << 9) | (1u64 << 10) | (1u64 << 16); // OSFXSR, OSXMMEXCPT, FSGSBASE
    asm!("mov cr4, {}", in(reg) cr4, options(nomem, nostack));
}

/// Returns the ID of the current CPU (LAPIC-derived).
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn cpu_id() -> usize {
    unsafe { smp::arch_cpu_id() }
}

/// x86_64 serial output for early debugging.
///
/// Uses 16550 UART at COM1 (0x3F8).
#[cfg(target_arch = "x86_64")]
pub unsafe fn putc(c: u8) {
    use core::arch::asm;
    use core::sync::atomic::Ordering::Relaxed;

    // Wait for the transmit holding register to be empty (LSR bit 5) — with a
    // DEADLINE, and it has to keep one.
    //
    // `putc` runs in IRQ context: the timer tick's 0.5 Hz diagnostics census,
    // panic paths and `pci::serial_debug` all reach it. QEMU's 16550 withholds
    // LSR.THRE for exactly as long as its chardev back end refuses the byte
    // (`hw/char/serial.c:serial_xmit` installs a G_IO_OUT watch on EAGAIN and
    // returns without setting THRE), so a serial consumer that stops reading
    // used to park this loop *forever* inside the timer IRQ handler — freezing
    // TICK_COUNT, the scheduler tick and `virtio_keyboard::poll_events` on this
    // CPU. Measured cost of that wedge: 1086 of 1200 injected pointer frames
    // dropped by the host for want of a posted eventq buffer, which is the loss
    // a rate ladder had previously read as the input path starving under load.
    //
    // It takes no load at all to provoke: a host that merely holds the serial
    // socket open and stops reading is enough. Measured on one boot, 60
    // pointer moves/s, three phases differing in nothing else -- consumer
    // parked 9.5% of frames delivered, consumer reading 100%, no consumer
    // attached 100% (QEMU discards output when nobody is connected, so it never
    // back-pressures). artifacts/m15_serial_stall.py is that measurement.
    //
    // THE CONTRACT IS: console output may be lost, an interrupt handler may not
    // be stalled. Two parts to keeping it cheap —
    //   * the wait is bounded by the cycle counter, not by an iteration count,
    //     because one `in al, dx` costs ~1 us against a real UART and a full
    //     exit to host userspace (~10 us) against an emulated one; an iteration
    //     bound safe for the first is a ~100 ms stall in the second.
    //   * once the wait expires, TX_WEDGED latches, and while it is set each
    //     later byte costs a single LSR probe instead of a whole deadline. The
    //     first probe that finds THRE clears it. So a back-pressured console
    //     costs one deadline per episode, not one per byte.
    if TX_WEDGED.load(Relaxed) {
        let lsr: u8;
        asm!("in al, dx", out("al") lsr, in("dx") 0x3FDu16, options(nomem, nostack));
        if lsr & 0x20 == 0 {
            UART_TX_DROPPED.fetch_add(1, Relaxed);
            return;
        }
        TX_WEDGED.store(false, Relaxed);
    } else {
        let deadline = rdtsc_raw().wrapping_add(UART_TX_WAIT_CYCLES);
        loop {
            let lsr: u8;
            asm!("in al, dx", out("al") lsr, in("dx") 0x3FDu16, options(nomem, nostack));
            if lsr & 0x20 != 0 { break; }
            if rdtsc_raw().wrapping_sub(deadline) < (1u64 << 63) {
                TX_WEDGED.store(true, Relaxed);
                UART_TX_DROPPED.fetch_add(1, Relaxed);
                return;
            }
            core::hint::spin_loop();
        }
    }

    // Send the character
    asm!("out dx, al", in("dx") 0x3F8u16, in("al") c, options(nomem, nostack));
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
unsafe fn rdtsc_raw() -> u64 {
    let lo: u32;
    let hi: u32;
    core::arch::asm!("rdtsc", out("eax") lo, out("edx") hi,
                     options(nomem, nostack, preserves_flags));
    ((hi as u64) << 32) | (lo as u64)
}

/// How long `putc` will wait for the UART transmitter before giving the byte
/// up, in TSC cycles. Deliberately a raw cycle count and not a calibrated
/// interval: this runs before (and independently of) timer calibration. ~7 ms
/// at 3 GHz, ~20 ms on a 1 GHz part — either way orders of magnitude above the
/// ~87 us a real 16550 needs at 115200 baud, and short enough that a wedged
/// host back end cannot eat a scheduling quantum's worth of ticks.
pub const UART_TX_WAIT_CYCLES: u64 = 20_000_000;

/// Latched when a `putc` wait expires; cleared by the first later probe that
/// finds the transmitter free. Keeps a back-pressured console at one probe per
/// byte instead of one full deadline per byte.
static TX_WEDGED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Console bytes dropped because the UART transmitter never reported itself
/// empty in time. Non-zero means console output was traded away to keep this
/// CPU's interrupt handler making progress.
pub static UART_TX_DROPPED: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

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

/// Monotonic nanoseconds since boot, for kernel crates that sit below this one
/// in the dependency graph and so cannot call `timer::monotonic_ns` directly —
/// `evdev-server` stamps input events with it. Same contract as the function it
/// forwards to: whole ticks plus a TSC fraction scaled by the PIT-measured
/// cycles-per-tick, never decreasing, and callable from IRQ context (two atomic
/// loads and an `rdtsc`, no locks, no user memory).
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn arch_monotonic_ns() -> u64 {
    timer::monotonic_ns()
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
