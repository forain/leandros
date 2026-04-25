//! Interrupt Descriptor Table (IDT) — exception and IRQ handlers.

use core::mem::size_of;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct IdtEntry {
    offset_low:  u16,
    selector:    u16,
    ist:         u8,
    type_attr:   u8,
    offset_mid:  u16,
    offset_high: u32,
    _reserved:   u32,
}

impl IdtEntry {
    fn new(handler: usize, selector: u16, ist: u8, type_attr: u8) -> Self {
        Self {
            offset_low:  handler as u16,
            selector,
            ist,
            type_attr,
            offset_mid:  (handler >> 16) as u16,
            offset_high: (handler >> 32) as u32,
            _reserved:   0,
        }
    }
}

#[repr(C, align(16))]
struct Idt([IdtEntry; 256]);

static mut IDT: Idt = Idt([IdtEntry {
    offset_low: 0, selector: 0, ist: 0, type_attr: 0,
    offset_mid: 0, offset_high: 0, _reserved: 0,
}; 256]);

#[repr(C, packed)]
struct IdtPointer { limit: u16, base: u64 }

/// Interrupt stack frame pushed by the CPU on exception entry (x86-64).
#[repr(C)]
pub struct InterruptStackFrame {
    pub ip:    u64,
    pub cs:    u64,
    pub flags: u64,
    pub sp:    u64,
    pub ss:    u64,
}

pub fn init() {
    unsafe {
        // Default: catch-all for vectors 0-31.
        for i in 0..32usize {
            IDT.0[i] = IdtEntry::new(exc_misc as *const () as usize, 0x08, 0, 0x8E);
        }

        // Per-exception handlers with correct vector numbers.
        IDT.0[0]  = IdtEntry::new(exc_de  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[1]  = IdtEntry::new(exc_db  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[2]  = IdtEntry::new(exc_nmi as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[3]  = IdtEntry::new(exc_bp  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[4]  = IdtEntry::new(exc_of  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[5]  = IdtEntry::new(exc_br  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[6]  = IdtEntry::new(exc_ud  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[7]  = IdtEntry::new(exc_nm  as *const () as usize, 0x08, 0, 0x8E);
        // Vector 8 = double fault — uses IST1 (dedicated stack in TSS).
        IDT.0[8]  = IdtEntry::new(exc_df  as *const () as usize, 0x08, 1, 0x8E);
        IDT.0[10] = IdtEntry::new(exc_ts  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[11] = IdtEntry::new(exc_np  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[12] = IdtEntry::new(exc_ss  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[13] = IdtEntry::new(exc_gp  as *const () as usize, 0x08, 0, 0x8E);
        // Vector 14 = page fault — needs CR2 in addition to error code.
        IDT.0[14] = IdtEntry::new(page_fault as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[16] = IdtEntry::new(exc_mf  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[17] = IdtEntry::new(exc_ac  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[18] = IdtEntry::new(exc_mc  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[19] = IdtEntry::new(exc_xf  as *const () as usize, 0x08, 0, 0x8E);

        // Vector 32 = IRQ0 (8253/8254 timer after PIC remapping).
        IDT.0[32] = IdtEntry::new(timer_irq as *const () as usize, 0x08, 0, 0x8E);

        #[cfg(target_arch = "x86_64")]
        let ptr = IdtPointer {
            limit: (size_of::<Idt>() - 1) as u16,
            base:  core::ptr::addr_of!(IDT) as u64,
        };
        #[cfg(target_arch = "x86_64")]
        core::arch::asm!("lidt [{}]", in(reg) &ptr, options(nostack));
    }
}

// ── Minimal serial output for exception dumps ─────────────────────────────────
// Direct port I/O to COM1 (0x3F8) avoids any dependency on the drivers crate.

#[cfg(target_arch = "x86_64")]
fn serial_byte(b: u8) {
    unsafe {
        // Spin on LSR.THRE (bit 5) — transmit-holding-register empty.
        loop {
            let lsr: u8;
            core::arch::asm!(
                "in al, dx", out("al") lsr, in("dx") 0x3F8u16 + 5,
                options(nomem, nostack)
            );
            if lsr & 0x20 != 0 { break; }
        }
        core::arch::asm!(
            "out dx, al", in("dx") 0x3F8u16, in("al") b,
            options(nomem, nostack)
        );
    }
}

#[cfg(target_arch = "x86_64")]
fn serial_str(s: &[u8]) {
    for &b in s { serial_byte(b); }
}

/// Print a u64 as 16 hex digits.
#[cfg(target_arch = "x86_64")]
fn serial_hex64(v: u64) {
    const HEX: &[u8] = b"0123456789ABCDEF";
    let mut buf = [0u8; 16];
    for i in 0..16 {
        buf[15 - i] = HEX[((v >> (i * 4)) & 0xF) as usize];
    }
    serial_str(&buf);
}

// ── Exception entry point shared by all handlers ──────────────────────────────

#[cfg(target_arch = "x86_64")]
fn print_exception(frame: &InterruptStackFrame, vector: u64, error_code: u64) {
    serial_str(b"\r\n*** KERNEL EXCEPTION ***\r\n");
    serial_str(b"Vector=0x");   serial_hex64(vector);     serial_str(b"\r\n");
    serial_str(b"ErrCode=0x");  serial_hex64(error_code); serial_str(b"\r\n");
    serial_str(b"RIP=0x");      serial_hex64(frame.ip);   serial_str(b"\r\n");
    serial_str(b"CS=0x");       serial_hex64(frame.cs);   serial_str(b"\r\n");
    serial_str(b"RFLAGS=0x");   serial_hex64(frame.flags);serial_str(b"\r\n");
    serial_str(b"RSP=0x");      serial_hex64(frame.sp);   serial_str(b"\r\n");
    serial_str(b"SS=0x");       serial_hex64(frame.ss);   serial_str(b"\r\n");
}

// ── Exception handlers ────────────────────────────────────────────────────────

/// Returns true if the exception was taken from ring 3 (user mode).
#[cfg(target_arch = "x86_64")]
#[inline]
fn from_user(frame: &InterruptStackFrame) -> bool {
    frame.cs & 0x3 == 3
}

/// Generate a named exception handler that doesn't take an error code.
/// Each exception gets its own function so the actual vector number is known.
macro_rules! fault_no_err_handler {
    ($name:ident, $vector:expr) => {
        #[cfg(target_arch = "x86_64")]
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            if from_user(&frame) {
                serial_str(b"user fault (no errcode): task killed\r\n");
                sched::exit(1);
            } else {
                print_exception(&frame, $vector, 0);
                loop { unsafe { core::arch::asm!("hlt", options(nomem, nostack)); } }
            }
        }
    }
}

/// Generate a named exception handler that takes an error code.
macro_rules! fault_with_err_handler {
    ($name:ident, $vector:expr) => {
        #[cfg(target_arch = "x86_64")]
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, error_code: u64) {
            if from_user(&frame) {
                serial_str(b"user fault (errcode): task killed\r\n");
                let _ = error_code;
                sched::exit(1);
            } else {
                print_exception(&frame, $vector, error_code);
                loop { unsafe { core::arch::asm!("hlt", options(nomem, nostack)); } }
            }
        }
    }
}

fault_no_err_handler!(exc_de,  0);   // #DE Divide Error
fault_no_err_handler!(exc_db,  1);   // #DB Debug
fault_no_err_handler!(exc_nmi, 2);   // NMI
fault_no_err_handler!(exc_bp,  3);   // #BP Breakpoint
fault_no_err_handler!(exc_of,  4);   // #OF Overflow
fault_no_err_handler!(exc_br,  5);   // #BR Bound Range
fault_no_err_handler!(exc_ud,  6);   // #UD Invalid Opcode
fault_no_err_handler!(exc_nm,  7);   // #NM Device Not Available
fault_with_err_handler!(exc_df,  8); // #DF Double Fault
fault_with_err_handler!(exc_ts, 10); // #TS Invalid TSS
fault_with_err_handler!(exc_np, 11); // #NP Segment Not Present
fault_with_err_handler!(exc_ss, 12); // #SS Stack-Segment Fault
fault_with_err_handler!(exc_gp, 13); // #GP General Protection
fault_no_err_handler!(exc_mf, 16);   // #MF x87 FPE
fault_with_err_handler!(exc_ac, 17); // #AC Alignment Check
fault_no_err_handler!(exc_mc, 18);   // #MC Machine Check
fault_no_err_handler!(exc_xf, 19);   // #XF SIMD FPE
fault_no_err_handler!(exc_misc, 0xFE); // catch-all for other vectors

/// Page fault handler — also reads CR2 (faulting virtual address).
///
/// Error code bit 0 (P): 0 = not-present, 1 = protection violation.
///
/// For user-mode not-present faults we first try the demand-paging path.
/// If that succeeds the handler returns normally and execution resumes.
/// All other user faults kill the task; kernel faults halt.
#[cfg(target_arch = "x86_64")]
extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error_code: u64) {
    let cr2: u64;
    unsafe { core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack)); }

    if from_user(&frame) {
        // Bit 0 of the error code: 0 = page not present (translation fault).
        // Try demand paging before giving up.
        if error_code & 1 == 0 && sched::handle_page_fault(cr2 as usize) {
            return; // fault handled — resume user task
        }
        serial_str(b"user page fault CR2=0x"); serial_hex64(cr2);
        serial_str(b" err=0x"); serial_hex64(error_code);
        serial_str(b": task killed\r\n");
        sched::exit(1);
    } else {
        print_exception(&frame, 14, error_code);
        serial_str(b"CR2=0x"); serial_hex64(cr2); serial_str(b"\r\n");
        loop { unsafe { core::arch::asm!("hlt", options(nomem, nostack)); } }
    }
}

// Non-x86 stubs (satisfy the compiler on other targets).
#[cfg(not(target_arch = "x86_64"))]
extern "C" fn exc_misc(_frame: InterruptStackFrame) { loop {} }
#[cfg(not(target_arch = "x86_64"))]
extern "C" fn page_fault(_frame: InterruptStackFrame, _error_code: u64) { loop {} }

/// Timer IRQ handler — APIC timer at 100 Hz.
///
/// Sends LAPIC EOI, drives the scheduler tick, then checks if the running
/// task should be preempted.  `sched::preempt_check()` calls `yield_now()`
/// if needed; the `iretq` epilogue then resumes the correct task.
#[cfg(target_arch = "x86_64")]
extern "x86-interrupt" fn timer_irq(_frame: InterruptStackFrame) {
    super::apic::eoi();
    super::timer::on_tick();
    sched::preempt_check();
}

#[cfg(not(target_arch = "x86_64"))]
extern "C" fn timer_irq(_frame: InterruptStackFrame) {
    // No-op: timer module is only present on x86_64.
}
