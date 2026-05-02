//! AArch64 exception vector table and handlers.
//!
//! The table must be 2 KiB aligned (VBAR_EL1 requirement).
//! Each of the 16 vector slots is 128 bytes; we branch to out-of-line
//! handlers so the slots only hold a single `b` each.
//!
//! Slots wired up:
//!   EL1h Sync  (0x200) — kernel fault → panic with ESR/ELR
//!   EL1h IRQ   (0x280) — device/timer interrupt → irq_dispatch
//!   EL0-64 Sync (0x400) — SVC #0 → syscall_dispatch
//!   Everything else    → exc_unexpected (panic)
//!
//! Also provides `ret_to_user`: the trampoline used by the scheduler when
//! entering a user-space task for the first time (or after a syscall).

/// Set the kernel stack top for EL0→EL1 exception entry on the current CPU.
#[no_mangle]
pub unsafe extern "C" fn arch_set_kernel_stack(kst: u64) {
    core::arch::asm!(
        "msr tpidr_el1, {k}",
        k = in(reg) kst,
        options(nostack)
    );
}

/// Install VBAR_EL1 pointing at our vector table.
pub fn init() {
    unsafe {
        core::arch::asm!(
            "adr x0, __exception_vectors",
            "msr VBAR_EL1, x0",
            "isb",
            options(nostack)
        );
    }
}

// ── Vector table (2 KiB aligned) ──────────────────────────────────────────

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(r#"
.section .text
.balign 2048
.global __exception_vectors
__exception_vectors:
    // EL1t Sync  (SP_EL0) — 0x000
    b exc_el1_sync
    .balign 128
    // EL1t IRQ
    b exc_irq
    .balign 128
    // EL1t FIQ
    b exc_unexpected
    .balign 128
    // EL1t SError
    b exc_unexpected
    .balign 128

    // EL1h Sync  (SP_EL1) — 0x200
    b exc_el1_sync
    .balign 128
    // EL1h IRQ   — 0x280
    b exc_irq
    .balign 128
    // EL1h FIQ
    b exc_unexpected
    .balign 128
    // EL1h SError
    b exc_unexpected
    .balign 128

    // EL0-64 Sync — 0x400
    b exc_el0_sync
    .balign 128
    // EL0-64 IRQ
    b exc_el0_irq
    .balign 128
    // EL0-64 FIQ
    b exc_unexpected
    .balign 128
    // EL0-64 SError
    b exc_unexpected
    .balign 128

    // EL0-32 (AArch32) — 0x600 — not supported
    b exc_unexpected
    .balign 128
    b exc_unexpected
    .balign 128
    b exc_unexpected
    .balign 128
    b exc_unexpected
    .balign 128

// ── Exception handlers ───────────────────────────────────────────────────────

// ── EL1h IRQ — save caller-saved regs, dispatch, restore, eret ───────────────
exc_irq:
    // Save all caller-saved registers (x0-x17, x29=fp, x30=lr).
    stp  x29, x30, [sp, #-16]!
    stp  x0,  x1,  [sp, #-16]!
    stp  x2,  x3,  [sp, #-16]!
    stp  x4,  x5,  [sp, #-16]!
    stp  x6,  x7,  [sp, #-16]!
    stp  x8,  x9,  [sp, #-16]!
    stp  x10, x11, [sp, #-16]!
    stp  x12, x13, [sp, #-16]!
    stp  x14, x15, [sp, #-16]!
    stp  x16, x17, [sp, #-16]!

    bl   irq_dispatch           // Rust handler

    ldp  x16, x17, [sp], #16
    ldp  x14, x15, [sp], #16
    ldp  x12, x13, [sp], #16
    ldp  x10, x11, [sp], #16
    ldp  x8,  x9,  [sp], #16
    ldp  x6,  x7,  [sp], #16
    ldp  x4,  x5,  [sp], #16
    ldp  x2,  x3,  [sp], #16
    ldp  x0,  x1,  [sp], #16
    ldp  x29, x30, [sp], #16
    eret

// ── EL1h synchronous exception (kernel fault) ─────────────────────────────
exc_el1_sync:
    mrs  x0, esr_el1
    mrs  x1, elr_el1
    bl   exc_el1_sync_handler   // panics

// ── Macro: reload SP_EL1 from TPIDR_EL1 on EL0 entry ────────────────────────
.macro reload_kernel_sp
    msr  tpidrro_el0, x9      // stash x9 in tpidrro_el0 (safe scratch)
    mrs  x9, tpidr_el1        // x9 = kernel stack top
    cbz  x9, 1f               // skip if not set
    mov  sp, x9               // reset SP
1:  mrs  x9, tpidrro_el0      // restore x9
.endm

// ── EL0-64 IRQ ─────────────────────────────────────────────────────────────
exc_el0_irq:
    reload_kernel_sp
    stp  x29, x30, [sp, #-16]!
    stp  x0,  x1,  [sp, #-16]!
    stp  x2,  x3,  [sp, #-16]!
    stp  x4,  x5,  [sp, #-16]!
    stp  x6,  x7,  [sp, #-16]!
    stp  x8,  x9,  [sp, #-16]!
    stp  x10, x11, [sp, #-16]!
    stp  x12, x13, [sp, #-16]!
    stp  x14, x15, [sp, #-16]!
    stp  x16, x17, [sp, #-16]!

    bl   irq_dispatch

    ldp  x16, x17, [sp], #16
    ldp  x14, x15, [sp], #16
    ldp  x12, x13, [sp], #16
    ldp  x10, x11, [sp], #16
    ldp  x8,  x9,  [sp], #16
    ldp  x6,  x7,  [sp], #16
    ldp  x4,  x5,  [sp], #16
    ldp  x2,  x3,  [sp], #16
    ldp  x0,  x1,  [sp], #16
    ldp  x29, x30, [sp], #16
    eret

// ── EL0-64 synchronous exception (SVC / user fault) ───────────────────────
exc_el0_sync:
    reload_kernel_sp
    sub  sp, sp, #272
    stp  x0,  x1,  [sp, #0]
    stp  x2,  x3,  [sp, #16]
    stp  x4,  x5,  [sp, #32]
    stp  x6,  x7,  [sp, #48]
    stp  x8,  x9,  [sp, #64]
    stp  x10, x11, [sp, #80]
    stp  x12, x13, [sp, #96]
    stp  x14, x15, [sp, #112]
    stp  x16, x17, [sp, #128]
    stp  x18, x19, [sp, #144]
    stp  x20, x21, [sp, #160]
    stp  x22, x23, [sp, #176]
    stp  x24, x25, [sp, #192]
    stp  x26, x27, [sp, #208]
    stp  x28, x29, [sp, #224]
    str  x30,      [sp, #240]
    mrs  x9, sp_el0
    str  x9,       [sp, #248]
    mrs  x9, elr_el1
    str  x9,       [sp, #256]
    mrs  x9, spsr_el1
    str  x9,       [sp, #264]

    mrs  x9, esr_el1
    lsr  x9, x9, #26
    cmp  x9, #0x15           // EC 0x15 = SVC AArch64
    b.ne exc_el0_fault

    mov  x7,  sp
    mov  x6,  x5
    mov  x5,  x4
    mov  x4,  x3
    mov  x3,  x2
    mov  x2,  x1
    mov  x1,  x0
    mov  x0,  x8
    bl   syscall_dispatch
    str  x0,  [sp, #0]

    mov  x0,  sp
    bl   check_and_deliver_signals
    b    exc_el0_return

exc_el0_fault:
    mrs  x0, esr_el1
    mrs  x1, elr_el1
    bl   exc_el0_fault_handler

exc_el0_return:
    ldr  x9, [sp, #264]
    msr  spsr_el1, x9
    ldr  x9, [sp, #256]
    msr  elr_el1,  x9
    ldr  x9, [sp, #248]
    msr  sp_el0,   x9
    ldp  x0,  x1,  [sp, #0]
    ldp  x2,  x3,  [sp, #16]
    ldp  x4,  x5,  [sp, #32]
    ldp  x6,  x7,  [sp, #48]
    ldp  x8,  x9,  [sp, #64]
    ldp  x10, x11, [sp, #80]
    ldp  x12, x13, [sp, #96]
    ldp  x14, x15, [sp, #112]
    ldp  x16, x17, [sp, #128]
    ldp  x18, x19, [sp, #144]
    ldp  x20, x21, [sp, #160]
    ldp  x22, x23, [sp, #176]
    ldp  x24, x25, [sp, #192]
    ldp  x26, x27, [sp, #208]
    ldp  x28, x29, [sp, #224]
    ldr  x30,      [sp, #240]
    add  sp,  sp,  #272
    eret

// ── Unexpected exception ──────────────────────────────────────────────────
exc_unexpected:
    mrs  x0, esr_el1
    mrs  x1, elr_el1
    bl   exc_unexpected_handler

// ── ret_to_user ────────────────────────────────────────────────────────────
.global ret_to_user
.type   ret_to_user, %function
ret_to_user:
    // Ensure we are using SP_EL1 for the kernel
    msr  SPSel, #1

    // Load user context from stack (in 16-byte aligned pairs)
    ldp  x0, x1, [sp], #16    // x0 = SP_EL0, x1 = ELR_EL1
    msr  sp_el0,   x0
    msr  elr_el1,  x1
    ldp  x0, x1, [sp], #16    // x0 = SPSR_EL1, x1 = page_table
    msr  spsr_el1, x0
    cbz  x1, 1f
    msr  ttbr0_el1, x1
    dsb  sy
    isb
    tlbi vmalle1
    dsb  sy
    isb
1:  
    mov  x0, #0
    eret

.global ret_to_user_fork
.type   ret_to_user_fork, %function
ret_to_user_fork:
    ldr  x9, [sp, #264]
    msr  spsr_el1, x9
    ldr  x9, [sp, #256]
    msr  elr_el1,  x9
    ldr  x9, [sp, #248]
    msr  sp_el0,   x9
    ldp  x0,  x1,  [sp, #0]
    ldp  x2,  x3,  [sp, #16]
    ldp  x4,  x5,  [sp, #32]
    ldp  x6,  x7,  [sp, #48]
    ldp  x8,  x9,  [sp, #64]
    ldp  x10, x11, [sp, #80]
    ldp  x12, x13, [sp, #96]
    ldp  x14, x15, [sp, #112]
    ldp  x16, x17, [sp, #128]
    ldp  x18, x19, [sp, #144]
    ldp  x20, x21, [sp, #160]
    ldp  x22, x23, [sp, #176]
    ldp  x24, x25, [sp, #192]
    ldp  x26, x27, [sp, #208]
    ldp  x28, x29, [sp, #224]
    ldr  x30,      [sp, #240]
    add  sp,  sp,  #272
    eret

.global arch_execve_return
.type   arch_execve_return, %function
arch_execve_return:
    msr  elr_el1,  x0
    msr  sp_el0,   x1
    msr  spsr_el1, xzr
    dsb  sy
    isb
    mov  x0,  xzr
    mov  x1,  xzr
    mov  x2,  xzr
    mov  x3,  xzr
    mov  x4,  xzr
    mov  x5,  xzr
    mov  x6,  xzr
    mov  x7,  xzr
    mov  x8,  xzr
    mov  x9,  xzr
    mov  x10, xzr
    mov  x11, xzr
    mov  x12, xzr
    mov  x13, xzr
    mov  x14, xzr
    mov  x15, xzr
    mov  x16, xzr
    mov  x17, xzr
    mov  x18, xzr
    mov  x19, xzr
    mov  x20, xzr
    mov  x21, xzr
    mov  x22, xzr
    mov  x23, xzr
    mov  x24, xzr
    mov  x25, xzr
    mov  x26, xzr
    mov  x27, xzr
    mov  x28, xzr
    mov  x29, xzr
    mov  x30, xzr
    eret
"#);

pub const MAX_IRQS: usize = 1020;
static mut IRQ_HANDLERS: [Option<fn(u32)>; MAX_IRQS] = [None; MAX_IRQS];
pub unsafe fn register_irq(id: u32, handler: fn(u32)) {
    if (id as usize) < MAX_IRQS { IRQ_HANDLERS[id as usize] = Some(handler); }
}

#[no_mangle]
unsafe extern "C" fn irq_dispatch() {
    let iar = super::gic::ack();
    let id  = super::gic::irq_id(iar);
    if id == super::gic::SPURIOUS { return; }
    if id == 27 { super::timer::on_tick(); }
    else if (id as usize) < MAX_IRQS {
        if let Some(handler) = IRQ_HANDLERS[id as usize] { handler(id); }
    }
    super::gic::eoi(iar);
    sched::preempt_check();
}

#[no_mangle]
unsafe extern "C" fn exc_el1_sync_handler(esr: u64, elr: u64) {
    let far: u64;
    core::arch::asm!("mrs {}, far_el1", out(reg) far);
    print_detailed_data_abort_info(esr, elr, far);
    loop { core::hint::spin_loop(); }
}

unsafe fn print_detailed_data_abort_info(esr: u64, elr: u64, far: u64) {
    print_hex_value(b"[LEANDROS] ESR_EL1=0x", esr);
    print_hex_value(b"[LEANDROS] ELR_EL1=0x", elr);
    print_hex_value(b"[LEANDROS] FAR_EL1=0x", far);
}

unsafe fn print_hex_value(prefix: &[u8], value: u64) {
    extern "C" { fn serial_print_bytes(ptr: *const u8, len: usize); }
    serial_print_bytes(prefix.as_ptr(), prefix.len());
    for i in (0..16).rev() {
        let nibble = ((value >> (i * 4)) & 0xF) as u8;
        let c = if nibble < 10 { b'0' + nibble } else { b'A' + nibble - 10 };
        serial_print_bytes(&c, 1);
    }
    let nl = b"\r\n";
    serial_print_bytes(nl.as_ptr(), 2);
}

#[no_mangle]
unsafe extern "C" fn exc_el0_fault_handler(esr: u64, elr: u64) {
    let ec = (esr >> 26) & 0x3F;
    if ec == 0x24 || ec == 0x20 {
        let far: u64;
        core::arch::asm!("mrs {}, far_el1", out(reg) far, options(nomem, nostack));
        if sched::handle_page_fault(far as usize) { return; }
    }
    print_hex_value(b"EL0 fault! ESR=0x", esr);
    print_hex_value(b"ELR=0x", elr);
    sched::exit(1);
}

#[no_mangle]
unsafe extern "C" fn exc_unexpected_handler(esr: u64, elr: u64) {
    print_hex_value(b"Unexpected! ESR=0x", esr);
    print_hex_value(b"ELR=0x", elr);
    loop { core::hint::spin_loop(); }
}
