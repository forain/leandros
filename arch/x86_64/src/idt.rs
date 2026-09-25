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

        // Per-exception handlers with correct vector numbers. Every fault a
        // user program can commit goes through a `fault_stub_N` asm entry
        // (full `UserFrame`, signal delivery on the way out — see
        // `fault_common`); NMI, #DF and #MC are not the task's doing and keep
        // the print-and-halt handlers.
        IDT.0[0]  = IdtEntry::new(fault_stub_0  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[1]  = IdtEntry::new(fault_stub_1  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[2]  = IdtEntry::new(exc_nmi as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[3]  = IdtEntry::new(fault_stub_3  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[4]  = IdtEntry::new(fault_stub_4  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[5]  = IdtEntry::new(fault_stub_5  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[6]  = IdtEntry::new(fault_stub_6  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[7]  = IdtEntry::new(fault_stub_7  as *const () as usize, 0x08, 0, 0x8E);
        // Vector 8 = double fault — uses IST1 (dedicated stack in TSS).
        IDT.0[8]  = IdtEntry::new(exc_df  as *const () as usize, 0x08, 1, 0x8E);
        IDT.0[10] = IdtEntry::new(fault_stub_10 as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[11] = IdtEntry::new(fault_stub_11 as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[12] = IdtEntry::new(fault_stub_12 as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[13] = IdtEntry::new(fault_stub_13 as *const () as usize, 0x08, 0, 0x8E);
        // Vector 14 = page fault — CR2 is read inside `fault_common`.
        IDT.0[14] = IdtEntry::new(fault_stub_14 as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[16] = IdtEntry::new(fault_stub_16 as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[17] = IdtEntry::new(fault_stub_17 as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[18] = IdtEntry::new(exc_mc  as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[19] = IdtEntry::new(fault_stub_19 as *const () as usize, 0x08, 0, 0x8E);

        // Vector 32 = IRQ0 (8253/8254 timer after PIC remapping) and vector
        // 0x40 = reschedule IPI: full-frame stubs, so a return to ring 3
        // delivers pending signals (see `irq_common`).
        IDT.0[32] = IdtEntry::new(irq_stub_32 as *const () as usize, 0x08, 0, 0x8E);
        // Vector 33 = IRQ1 (PS/2 keyboard).
        IDT.0[33] = IdtEntry::new(keyboard_irq as *const () as usize, 0x08, 0, 0x8E);
        IDT.0[0x40] = IdtEntry::new(irq_stub_64 as *const () as usize, 0x08, 0, 0x8E);
        // Vector 0x41 = virtio-gpu control-queue completion (MSI-X entry 0;
        // `drivers::virtio_gpu::MSIX_VECTOR_CTRLQ`). Reap-and-wake only; it
        // never needs the ring-3 signal-delivery path the tick/resched stubs take.
        IDT.0[0x41] = IdtEntry::new(gpu_irq as *const () as usize, 0x08, 0, 0x8E);
        // Vector 0xFD = TLB shootdown IPI (remote TLB invalidation).
        IDT.0[0xFD] = IdtEntry::new(tlb_shootdown_irq as *const () as usize, 0x08, 0, 0x8E);

        load();
    }
}

/// Load the (shared) IDT on the calling CPU.
///
/// The IDT contents are CPU-independent, but the `lidt` register is per-CPU:
/// every AP must call this before enabling interrupts.
pub unsafe fn load() {
    #[cfg(target_arch = "x86_64")]
    {
        let ptr = IdtPointer {
            limit: (size_of::<Idt>() - 1) as u16,
            base:  core::ptr::addr_of!(IDT) as u64,
        };
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
fn _from_user(frame: &InterruptStackFrame) -> bool {
    frame.cs & 0x3 == 3
}

// POSIX signal numbers (same values as `sched/src/signal.rs` uses; they are
// architecture-independent).
const SIGILL:  u32 = 4;
const SIGTRAP: u32 = 5;
const SIGBUS:  u32 = 7;
const SIGFPE:  u32 = 8;
const SIGSEGV: u32 = 11;

/// The signal a user-mode CPU exception corresponds to, by vector.
///
/// Every fault handler below used to kill the task with `exit_group(1)`,
/// which `waitpid` reports as a clean exit with code 1 — indistinguishable
/// from a program that chose to `return 1`. The per-vector mapping (Linux's
/// `arch/x86/kernel/traps.c`) is what lets a shell say "Illegal instruction"
/// where it means it and "Floating point exception" where it means that.
///
/// `const fn` because each handler is generated by a macro that has the
/// vector as a literal: the call folds to a constant at every site.
const fn fault_signal(vector: u64) -> u32 {
    match vector {
        0  => SIGFPE,   // #DE divide error
        1  => SIGTRAP,  // #DB debug
        3  => SIGTRAP,  // #BP breakpoint
        4  => SIGSEGV,  // #OF overflow (Linux: do_overflow -> SIGSEGV)
        5  => SIGSEGV,  // #BR bound range exceeded
        6  => SIGILL,   // #UD invalid opcode
        7  => SIGILL,   // #NM device not available (no lazy-FPU path here)
        10 => SIGSEGV,  // #TS invalid TSS
        11 => SIGBUS,   // #NP segment not present
        12 => SIGBUS,   // #SS stack-segment fault
        13 => SIGSEGV,  // #GP general protection
        14 => SIGSEGV,  // #PF page fault
        16 => SIGFPE,   // #MF x87 floating-point error
        17 => SIGBUS,   // #AC alignment check
        18 => SIGBUS,   // #MC machine check
        19 => SIGFPE,   // #XF SIMD floating-point exception
        // NMI (2), #DF (8) and the 0xFE catch-all are not faults the task can
        // be said to have *committed*; killing it is still the only option
        // once it happened in ring 3, and SIGSEGV is the honest generic.
        _  => SIGSEGV,
    }
}

/// `si_code` and `si_addr` for a user-mode exception, alongside the signal.
///
/// Linux's `arch/x86/kernel/traps.c` conventions: a page fault reports the
/// faulting address with SEGV_MAPERR (not present) or SEGV_ACCERR (protection
/// violation, error-code bit 0); #UD/#DE/#BP report the faulting RIP; #GP and
/// the segment faults have no meaningful address and use SI_KERNEL.
const fn fault_siginfo(vector: u64, error_code: u64, cr2: u64, rip: u64) -> (u32, i32, usize) {
    let sig = fault_signal(vector);
    match vector {
        14 => (sig, if error_code & 1 != 0 { sched::SEGV_ACCERR } else { sched::SEGV_MAPERR }, cr2 as usize),
        0  => (sig, sched::FPE_INTDIV,  rip as usize),
        16 | 19 => (sig, sched::FPE_FLTINV, rip as usize),
        6  | 7  => (sig, sched::ILL_ILLOPC, rip as usize),
        3  => (sig, sched::TRAP_BRKPT, rip as usize),
        1  => (sig, sched::TRAP_TRACE, rip as usize),
        17 => (sig, sched::BUS_ADRALN, 0),
        _  => (sig, sched::SI_KERNEL, 0),
    }
}

/// Generate a print-and-halt handler for exceptions that are never a user
/// task's own fault (NMI, #DF, #MC, the catch-all). Taken from ring 3 they
/// still kill the task — there is nothing else to do — but they never reach
/// a user signal handler.
macro_rules! fatal_no_err_handler {
    ($name:ident, $vector:expr) => {
        #[cfg(target_arch = "x86_64")]
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            fatal_exception(&frame, $vector, 0);
        }
    }
}

/// Error-code variant of [`fatal_no_err_handler`].
macro_rules! fatal_with_err_handler {
    ($name:ident, $vector:expr) => {
        #[cfg(target_arch = "x86_64")]
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, error_code: u64) {
            fatal_exception(&frame, $vector, error_code);
        }
    }
}

#[cfg(target_arch = "x86_64")]
fn fatal_exception(frame: &InterruptStackFrame, vector: u64, error_code: u64) -> ! {
    let from_user = (frame.cs & 3) != 0;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags)); }
        serial_str(b"user fault vec="); serial_hex64(vector);
        serial_str(b" RIP=0x"); serial_hex64(frame.ip);
        serial_str(b" CS=0x"); serial_hex64(frame.cs);
        serial_str(b" RSP=0x"); serial_hex64(frame.sp);
        serial_str(b" err=0x"); serial_hex64(error_code);
        serial_str(b": task killed\r\n");
        sched::exit_group_signal(fault_signal(vector));
    }
    print_exception(frame, vector, error_code);
    loop { unsafe { core::arch::asm!("hlt", options(nomem, nostack)); } }
}

fatal_no_err_handler!(exc_nmi, 2);        // NMI
fatal_with_err_handler!(exc_df, 8);       // #DF Double Fault
fatal_no_err_handler!(exc_mc, 18);        // #MC Machine Check
fatal_no_err_handler!(exc_misc, 0xFE);    // catch-all for other vectors

// ── Fault entry stubs: full UserFrame + signal delivery ──────────────────────
//
// A user-mode CPU exception is the only way a task reaches a SIGSEGV/SIGBUS/
// SIGILL/SIGFPE/SIGTRAP handler, and running one needs exactly what the
// SYSCALL path has: the complete user register file in a `UserFrame` on the
// kernel stack (`check_and_deliver_signals` snapshots it into the signal
// frame and rewrites rip/rsp/rdi/rsi/rdx to enter the handler) and a pop +
// `iretq` epilogue that honours the rewritten frame. The `extern
// "x86-interrupt"` handlers these stubs replace saw only the five words the
// CPU pushes and could therefore do nothing but kill the task.
//
// Layout built here is byte-for-byte `sched::context::UserFrame` — the same
// push order as `syscall_entry` in syscall.rs, so `sched::signal::x86_64`
// needs no second frame type. The CPU pushes `[err] rip cs rflags rsp ss`;
// vectors without an error code first push a 0 so the two shapes coincide,
// and `xchg r11, [rsp]` then swaps the error code out of the frame and the
// user r11 into its slot in one instruction (r11 is otherwise the only
// register the stub would have to save before it had a scratch register).
//
// Alignment: on a ring-3 → ring-0 fault the CPU aligns RSP to 16 before the
// 5-word frame, so `frame + err` = 48 bytes and the 14 pushes = 112 bytes
// leave RSP ≡ 0 (mod 16) — `call` then lands `fault_common` at the SysV
// RSP+8 alignment. Same-privilege (kernel-mode) faults are aligned the same
// way by the CPU.
//
// GS: from ring 3, `swapgs` on entry as the other handlers do, and the
// migration-proof `restore_user_gs` on exit (see syscall.rs). A kernel-mode
// fault leaves GS alone — the kernel does not use it, and the syscall exit
// that eventually follows restores the user invariant itself.
macro_rules! fault_stub {
    ($name:literal, $vector:literal, $push_zero:literal) => {
        core::arch::global_asm!(concat!(r#"
.section .text, "ax", @progbits
.global "#, $name, r#"
.type   "#, $name, r#", @function
"#, $name, r#":
    "#, $push_zero, r#"
    xchg  r11, [rsp]          // r11 = error code; user r11 -> frame slot
    push  rcx
    push  rax
    push  rdi
    push  rsi
    push  rdx
    push  r8
    push  r9
    push  r10
    push  rbx
    push  rbp
    push  r12
    push  r13
    push  r14
    push  r15
    mov   rdx, r11            // arg3 = error code
    mov   esi, "#, $vector, r#" // arg2 = vector
    jmp   fault_common_asm
"#));
    };
}

fault_stub!("fault_stub_0",  0,  "push 0");   // #DE Divide Error
fault_stub!("fault_stub_1",  1,  "push 0");   // #DB Debug
fault_stub!("fault_stub_3",  3,  "push 0");   // #BP Breakpoint
fault_stub!("fault_stub_4",  4,  "push 0");   // #OF Overflow
fault_stub!("fault_stub_5",  5,  "push 0");   // #BR Bound Range
fault_stub!("fault_stub_6",  6,  "push 0");   // #UD Invalid Opcode
fault_stub!("fault_stub_7",  7,  "push 0");   // #NM Device Not Available
fault_stub!("fault_stub_10", 10, "");         // #TS Invalid TSS (err)
fault_stub!("fault_stub_11", 11, "");         // #NP Segment Not Present (err)
fault_stub!("fault_stub_12", 12, "");         // #SS Stack-Segment Fault (err)
fault_stub!("fault_stub_13", 13, "");         // #GP General Protection (err)
fault_stub!("fault_stub_14", 14, "");         // #PF Page Fault (err)
fault_stub!("fault_stub_16", 16, "push 0");   // #MF x87 FPE
fault_stub!("fault_stub_17", 17, "");         // #AC Alignment Check (err)
fault_stub!("fault_stub_19", 19, "push 0");   // #XF SIMD FPE

core::arch::global_asm!(r#"
.section .text, "ax", @progbits
.global fault_common_asm
.type   fault_common_asm, @function
fault_common_asm:
    // rsi = vector, rdx = error code (set by the stub); rdi = frame.
    mov   rdi, rsp
    // UserFrame.cs is the 17th word: 15 GPR slots + rip.
    test  qword ptr [rsp + 128], 3
    jz    1f
    // ── from ring 3 ──
    swapgs
    call  fault_common
    // Deliver the fault signal `fault_common` queued (and anything else
    // pending) — builds the signal frame and redirects this UserFrame.
    mov   rdi, rsp
    call  check_and_deliver_signals
    call  restore_user_gs
    jmp   2f
1:
    // ── from ring 0 (kernel-mode fault on a demand-paged user address) ──
    call  fault_common
2:
    pop   r15
    pop   r14
    pop   r13
    pop   r12
    pop   rbp
    pop   rbx
    pop   r10
    pop   r9
    pop   r8
    pop   rdx
    pop   rsi
    pop   rdi
    pop   rax
    pop   rcx
    pop   r11
    iretq
"#);

extern "C" {
    fn fault_stub_0();  fn fault_stub_1();  fn fault_stub_3();  fn fault_stub_4();
    fn fault_stub_5();  fn fault_stub_6();  fn fault_stub_7();  fn fault_stub_10();
    fn fault_stub_11(); fn fault_stub_12(); fn fault_stub_13(); fn fault_stub_14();
    fn fault_stub_16(); fn fault_stub_17(); fn fault_stub_19();
    fn irq_stub_32();   fn irq_stub_64();
}

// ── IRQ entry stubs: full UserFrame + signal delivery on return to ring 3 ────
//
// The timer tick and the reschedule IPI used to be `extern "x86-interrupt"`
// handlers, which see only the five words the CPU pushes and can therefore
// never run `check_and_deliver_signals`. On this arch a signal was thus only
// ever delivered on a syscall or fault return: a thread spinning in user
// mode with no syscalls could not be killed at all — `kill -9` of such a
// process left it running forever (killmt `spin_all`), and a SIGKILL on a
// threaded process only reached the threads that happened to trap. AArch64
// has always delivered on its IRQ return path (`exc_el1_irq` in
// exception_asm.s), so this closes an arch asymmetry, not a design choice.
//
// Same frame layout and GS handling as the fault stubs above; the fake
// error-code push keeps the shapes identical. `irq_common` runs the
// device/IPI work and the preemption check; a fatal signal found on the way
// out ends in `exit_group` exactly as it does after a syscall.
fault_stub!("irq_stub_32", 32, "push 0");  // LAPIC timer
fault_stub!("irq_stub_64", 64, "push 0");  // reschedule IPI (0x40)

/// Common fault handler behind every `fault_stub_N`.
///
/// Returning resumes the interrupted context through the frame: either the
/// fault was serviced (demand paging), or `sched::fault_signal` queued the
/// signal for a user handler and the stub's `check_and_deliver_signals`
/// redirects the frame into it. Everything else ends here — a user fault
/// with no handler kills the group, a kernel fault halts for triage.
///
/// Page faults: error code bit 0 (P) 0 = not-present (demand-paging path),
/// 1 = protection violation — also routed through `handle_page_fault` so a
/// write to a read-only CoW page can be promoted instead of killing the
/// task. Bit 1 = write. Kernel-mode faults on a *user* address are kernel/
/// server code dereferencing a demand-paged user pointer (lazy heap, CoW, a
/// never-touched exec image page); the servers run synchronously in the
/// calling task's context, so its address space is the right one. Faults
/// that need a *file read* must never be taken while filesystem locks are
/// held — the syscall layer prefaults every user buffer it forwards into the
/// VFS to guarantee that; this path is the safety net for everything else.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
extern "C" fn fault_common(frame: *mut sched::context::UserFrame, vector: u64, error_code: u64) {
    // IRQs routed through the fault stubs (see `irq_stub_*`): EOI, drive
    // the tick, preempt if asked, and return through the common epilogue,
    // which delivers pending signals when the frame is a ring-3 one.
    if vector == 32 || vector == 0x40 {
        super::apic::eoi();
        if vector == 32 {
            if sched::pcsample::ENABLED {
                let f = unsafe { &*frame };
                sched::pcsample::sample(f.rip, f.cs & 3 != 0);
            }
            super::timer::on_tick();
        }
        sched::preempt_check();
        return;
    }
    let frame = unsafe { &mut *frame };
    let from_user = frame.cs & 3 != 0;

    let cr2: u64;
    let cr3: u64;
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) cr2, options(nomem, nostack));
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    }

    if vector == 14 {
        const USER_VA_LIMIT: u64 = 0x0000_8000_0000_0000;
        let is_write = error_code & 2 != 0;
        if (from_user || cr2 < USER_VA_LIMIT) && sched::handle_page_fault(cr2 as usize, is_write) {
            return; // fault handled — resume the interrupted instruction
        }
    }

    if !from_user {
        let isf = InterruptStackFrame {
            ip: frame.rip, cs: frame.cs, flags: frame.rflags, sp: frame.rsp, ss: frame.ss,
        };
        print_exception(&isf, vector, error_code);
        if vector == 14 { serial_str(b"CR2=0x"); serial_hex64(cr2); serial_str(b"\r\n"); }
        // The rest of the machine state a kernel-mode fault needs to be
        // diagnosed from one serial dump: which CPU and task, which page
        // table, the general registers (a corrupt pointer is usually still
        // sitting in one of them), and the page-table walk of the faulting
        // address so "not present" can be told apart from "wrong CR3".
        serial_str(b"CR3=0x");   serial_hex64(cr3);
        serial_str(b" CPU=0x");  serial_hex64(unsafe { sched::cpu_id() } as u64);
        serial_str(b" PID=0x");  serial_hex64(sched::current_pid() as u64);
        serial_str(b"\r\n");
        serial_str(b"RAX=0x"); serial_hex64(frame.rax); serial_str(b" RBX=0x"); serial_hex64(frame.rbx);
        serial_str(b" RCX=0x"); serial_hex64(frame.rcx); serial_str(b" RDX=0x"); serial_hex64(frame.rdx); serial_str(b"\r\n");
        serial_str(b"RSI=0x"); serial_hex64(frame.rsi); serial_str(b" RDI=0x"); serial_hex64(frame.rdi);
        serial_str(b" RBP=0x"); serial_hex64(frame.rbp); serial_str(b" R8 =0x"); serial_hex64(frame.r8);  serial_str(b"\r\n");
        serial_str(b"R9 =0x"); serial_hex64(frame.r9);  serial_str(b" R10=0x"); serial_hex64(frame.r10);
        serial_str(b" R11=0x"); serial_hex64(frame.r11); serial_str(b" R12=0x"); serial_hex64(frame.r12); serial_str(b"\r\n");
        serial_str(b"R13=0x"); serial_hex64(frame.r13); serial_str(b" R14=0x"); serial_hex64(frame.r14);
        serial_str(b" R15=0x"); serial_hex64(frame.r15); serial_str(b"\r\n");
        if vector == 14 {
            serial_str(b"page-table walk of CR2 in CR3:\r\n");
            unsafe { super::paging::debug_walk_pte((cr3 & !0xFFF) as usize, cr2 as usize); }
        }
        // A few words of the kernel stack: the return addresses in them are
        // the only backtrace a release kernel has.
        serial_str(b"stack:");
        for i in 0..24u64 {
            let p = (frame.rsp + i * 8) as *const u64;
            if (p as u64) < 0xFFFF_8000_0000_0000 { break; }
            if i % 4 == 0 { serial_str(b"\r\n  "); }
            serial_hex64(unsafe { p.read_volatile() }); serial_str(b" ");
        }
        serial_str(b"\r\n");
        loop { unsafe { core::arch::asm!("hlt", options(nomem, nostack)); } }
    }

    // A user fault with a handler installed: queue SIGSEGV/SIGBUS/… with its
    // siginfo and let the stub deliver it. Silent on purpose — a program
    // that handles its own faults (GC barriers, stack probes, siglongjmp
    // recovery) must not spam the console on each one.
    let (sig, si_code, si_addr) = fault_siginfo(vector, error_code, cr2, frame.rip);
    if sched::fault_signal(sig, si_code, si_addr) {
        return;
    }

    if vector == 14 {
        serial_str(b"user page fault RIP=0x"); serial_hex64(frame.rip);
        serial_str(b" CR2=0x"); serial_hex64(cr2);
        serial_str(b" CR3=0x"); serial_hex64(cr3);
        serial_str(b" err=0x"); serial_hex64(error_code);
        serial_str(b": task killed\r\n");
        unsafe { super::paging::debug_walk_pte((cr3 & !0xFFF) as usize, cr2 as usize); }
    } else {
        serial_str(b"user fault vec="); serial_hex64(vector);
        serial_str(b" RIP=0x"); serial_hex64(frame.rip);
        serial_str(b" CS=0x"); serial_hex64(frame.cs);
        serial_str(b" RSP=0x"); serial_hex64(frame.rsp);
        serial_str(b" err=0x"); serial_hex64(error_code);
        serial_str(b": task killed\r\n");
    }
    sched::exit_group_signal(sig);
}

// Non-x86 stubs (satisfy the compiler on other targets).
#[cfg(not(target_arch = "x86_64"))]
extern "C" fn exc_misc(_frame: InterruptStackFrame) { loop {} }

// The timer IRQ (vector 32, APIC timer at 100 Hz) and the reschedule IPI
// (vector 0x40, sent by `sched::trigger_preempt`; the sender already set this
// CPU's `PREEMPT_NEEDED` slot) enter through `irq_stub_32`/`irq_stub_64` and
// are handled at the top of `fault_common`. An idle CPU parked in `sti; hlt`
// is woken by the interrupt itself and re-picks from the run queue when the
// handler returns.

/// TLB shootdown IPI handler — vector 0xFD.
///
/// Flushes this CPU's TLB (CR3 reload) and acknowledges the initiator.
/// Must NOT reschedule: the initiator is spin-waiting for the ack and the
/// flush must complete on this CPU before any user memory is touched again.
#[cfg(target_arch = "x86_64")]
extern "x86-interrupt" fn tlb_shootdown_irq(frame: InterruptStackFrame) {
    let from_user = (frame.cs & 3) != 0;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags)); }
    }

    unsafe {
        core::arch::asm!(
            "mov {tmp}, cr3",
            "mov cr3, {tmp}",
            tmp = out(reg) _,
            options(nostack)
        );
    }
    super::apic::eoi();
    super::paging::tlb_shootdown_ack();

    if from_user {
        // No reschedule happens here, but keep the exit path uniform.
        unsafe { super::syscall::restore_user_gs(); }
    }
}

#[cfg(not(target_arch = "x86_64"))]
extern "C" fn tlb_shootdown_irq(_frame: InterruptStackFrame) {}

/// Keyboard IRQ handler — PS/2 keyboard at IRQ 1 (vector 33).
#[cfg(target_arch = "x86_64")]
extern "x86-interrupt" fn keyboard_irq(frame: InterruptStackFrame) {
    let from_user = (frame.cs & 3) != 0;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags)); }
    }

    super::apic::eoi();
    super::keyboard::on_irq();

    if from_user {
        unsafe { super::syscall::restore_user_gs(); }
    }
}

/// virtio-gpu control-queue completion — MSI-X vector 0x41.
///
/// EOI first, then the driver's reaper (`drivers::virtio_gpu::virtio_gpu_msix_isr`,
/// reached by symbol the way `arch_serial_putc` is, since this crate cannot
/// name `drivers`). The reaper only try-locks and never sleeps, so no
/// reschedule happens here and the exit path is the keyboard handler's.
#[cfg(target_arch = "x86_64")]
extern "x86-interrupt" fn gpu_irq(frame: InterruptStackFrame) {
    extern "C" { fn virtio_gpu_msix_isr(); }
    let from_user = (frame.cs & 3) != 0;
    if from_user {
        unsafe { core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags)); }
    }

    super::apic::eoi();
    unsafe { virtio_gpu_msix_isr(); }

    if from_user {
        unsafe { super::syscall::restore_user_gs(); }
    }
}

#[cfg(not(target_arch = "x86_64"))]
extern "C" fn gpu_irq(_frame: InterruptStackFrame) {}

#[cfg(not(target_arch = "x86_64"))]
extern "C" fn keyboard_irq(_frame: InterruptStackFrame) {
    // No-op.
}
