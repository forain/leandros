//! AArch64 exception vector table and handlers.
//!
//! The table must be 2 KiB aligned (VBAR_EL1 requirement).
//! Each of the 16 vector slots is 128 bytes; we branch to out-of-line
//! handlers so the slots only hold a single `b handler`.
//!
//! Spec: ARMv8-A Architecture Reference Manual §D1.10 (Exception Handling)

core::arch::global_asm!(include_str!("exception_asm.s"));

extern "C" {
    fn serial_print_str_raw(ptr: *const u8, len: usize);
    fn print_hex(n: usize);
    fn print_number(n: u32);
}

fn serial_print_str(s: &str) {
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

#[allow(dead_code)]
extern "C" {
    /// Save all registers and call the appropriate Rust handler.
    fn exc_vector_table();
}

/// Initialize the exception vector table by setting `VBAR_EL1`.
pub fn init() {
    unsafe {
        let table_addr = exc_vector_table_ptr();
        core::arch::asm!("msr vbar_el1, {}", in(reg) table_addr);
    }
}

fn exc_vector_table_ptr() -> usize {
    extern "C" {
        static __exception_vectors: u8;
    }
    core::ptr::addr_of!(__exception_vectors) as usize
}

/// Updates the per-CPU kernel stack pointer used on EL0 exception entry.
#[no_mangle]
pub unsafe extern "C" fn arch_set_kernel_stack(kst: u64) {
    core::arch::asm!("msr tpidr_el1, {}", in(reg) kst);
}

// ── Exception Frame ──────────────────────────────────────────────────────────

// Use the definition from sched::context to ensure consistency
pub use sched::context::UserFrame;

// ── Sync Exception Handlers ──────────────────────────────────────────────────

fn handle_irq(_frame: *mut UserFrame) {
    let iar = super::gic::ack();
    let irq_id = super::gic::irq_id(iar);

    if irq_id == 27 || irq_id == 30 {
        // Virtual or Physical Timer
        super::timer::on_tick();
    } else if irq_id == 33 {
        // PL011 UART
        while let Some(b) = unsafe { super::uart::getc() } {
            evdev_server::push_event(0, 1 /* EV_KEY */, b as u16, 2);
            evdev_server::push_event(0, 0 /* EV_SYN */, 0 /* SYN_REPORT */, 0);
        }
        unsafe { super::uart::clear_irq(); }
    } else if irq_id != super::gic::SPURIOUS {
        serial_print_str("\n[EXC] Unhandled IRQ ");
        unsafe { print_number(irq_id); }
        serial_print_str("\n");
    }

    super::gic::eoi(iar);
    sched::preempt_check();
}

#[no_mangle]
unsafe extern "C" fn exc_el1_irq_handler(frame: *mut UserFrame) {
    handle_irq(frame);
}

#[no_mangle]
unsafe extern "C" fn exc_el0_irq_handler(frame: *mut UserFrame) {
    handle_irq(frame);
}

#[no_mangle]
unsafe extern "C" fn exc_el1_sync_handler(esr: u64, elr: u64) {
    serial_print_str("\n[EXC] EL1 Sync Fault! ESR=");
    print_hex(esr as usize);
    serial_print_str(" ELR=");
    print_hex(elr as usize);
    serial_print_str("\n");
    loop { core::hint::spin_loop(); }
}

#[no_mangle]
unsafe extern "C" fn exc_el0_sync_handler(esr: u64, elr: u64, frame: *mut UserFrame) {
    let ec = (esr >> 26) & 0x3F;
    if ec == 0x15 {
        serial_print_str("[EXC] Unexpected syscall in Rust handler\n");
    } else {
        serial_print_str("\n[EXC] EL0 Fault! PID=");
        print_number(sched::current_pid());
        serial_print_str(" ESR=");
        print_hex(esr as usize);
        serial_print_str(" EC=");
        print_hex(ec as usize);
        serial_print_str(" DFSC=");
        print_hex((esr & 0x3F) as usize);
        serial_print_str(" ELR=");
        print_hex(elr as usize);
        
        // Print instruction at ELR if possible
        if elr >= 0x200000 && elr < 0x80000000 {
            // We need to be in the same address space to read this!
            // But wait! We are in EL1, we can't easily read TTBR0 memory if it's not mapped in EL1.
            // However, we are identity mapped for the first 1GB in some boot paths.
            // For now, let's just print the ESR/ELR and try to deduce.
            let _instr_ptr = elr as *const u32;
        }
        
        // Print some regs from frame
        serial_print_str("\n[EXC] x0=");
        print_hex((*frame).x[0] as usize);
        serial_print_str(" x1=");
        print_hex((*frame).x[1] as usize);
        serial_print_str(" sp=");
        print_hex((*frame).sp_el0 as usize);
        serial_print_str("\n");
        
        sched::exit(1);
    }
}

#[no_mangle]
unsafe extern "C" fn exc_unexpected_handler(esr: u64, elr: u64) {
    serial_print_str("\n[EXC] Unexpected Exception! ESR=");
    print_hex(esr as usize);
    serial_print_str(" ELR=");
    print_hex(elr as usize);
    serial_print_str("\n");
    loop { core::hint::spin_loop(); }
}
