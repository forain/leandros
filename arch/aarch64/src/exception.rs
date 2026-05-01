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
///
/// Stores `kst` in TPIDR_EL1, which the EL0 exception entry stubs reload into
/// SP before saving any registers.  This mirrors x86-64's TSS.rsp0 update and
/// ensures that each user task gets a fresh kernel stack on every exception,
/// regardless of what SP_EL1 happened to be before the EL0→EL1 transition.
///
/// Called from `sched::run()` before every `cpu_switch_to` into a user task.
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
        extern "C" { fn arch_serial_putc(ch: u8); }

        // Debug: Print what we're setting VBAR_EL1 to
        let vector_addr: u64;
        core::arch::asm!(
            "adr {}, __exception_vectors",
            out(reg) vector_addr,
            options(nostack)
        );

        let debug_msg = b"[EXCEPTION] Setting VBAR_EL1 to: 0x";
        for &b in debug_msg { arch_serial_putc(b); }
        for shift in (0..16).rev() {
            let nibble = (vector_addr >> (shift * 4)) & 0xF;
            let ch = if nibble < 10 { b'0' + nibble as u8 } else { b'A' + (nibble - 10) as u8 };
            arch_serial_putc(ch);
        }
        let debug_nl = b"\r\n";
        for &b in debug_nl { arch_serial_putc(b); }

        // Debug: Check current exception level and system registers
        #[cfg(target_arch = "aarch64")]
        let el_level: u64 = {
            let current_el: u64;
            core::arch::asm!("mrs {}, CurrentEL", out(reg) current_el);
            (current_el >> 2) & 0x3
        };
        #[cfg(not(target_arch = "aarch64"))]
        let el_level: u64 = 1;

        let debug_el = b"[EXCEPTION] Running at exception level: EL";
        for &b in debug_el { arch_serial_putc(b); }
        arch_serial_putc(b'0' + el_level as u8);
        for &b in debug_nl { arch_serial_putc(b); }

        // Check DAIF (interrupt masks) - these might be blocking exceptions
        let daif: u64;
        core::arch::asm!("mrs {}, DAIF", out(reg) daif);
        let debug_daif = b"[EXCEPTION] DAIF register: 0x";
        for &b in debug_daif { arch_serial_putc(b); }
        for shift in (0..2).rev() {
            let nibble = (daif >> (shift * 4)) & 0xF;
            let ch = if nibble < 10 { b'0' + nibble as u8 } else { b'A' + (nibble - 10) as u8 };
            arch_serial_putc(ch);
        }
        for &b in debug_nl { arch_serial_putc(b); }

        core::arch::asm!(
            "adr x0, __exception_vectors",
            "msr VBAR_EL1, x0",
            "isb",
            options(nostack)
        );

        // Debug: Read back VBAR_EL1 to verify it was set
        let vbar_readback: u64;
        core::arch::asm!(
            "mrs {}, VBAR_EL1",
            out(reg) vbar_readback,
            options(nostack)
        );

        let debug_readback = b"[EXCEPTION] VBAR_EL1 readback: 0x";
        for &b in debug_readback { arch_serial_putc(b); }
        for shift in (0..16).rev() {
            let nibble = (vbar_readback >> (shift * 4)) & 0xF;
            let ch = if nibble < 10 { b'0' + nibble as u8 } else { b'A' + (nibble - 10) as u8 };
            arch_serial_putc(ch);
        }
        let debug_nl2 = b"\r\n";
        for &b in debug_nl2 { arch_serial_putc(b); }

        if vector_addr == vbar_readback {
            let debug_ok = b"[EXCEPTION] VBAR_EL1 setup SUCCESS!\r\n";
            for &b in debug_ok { arch_serial_putc(b); }
        } else {
            let debug_fail = b"[EXCEPTION] VBAR_EL1 setup FAILED!\r\n";
            for &b in debug_fail { arch_serial_putc(b); }
        }

        // Debug: Check the actual memory contents at the exception vector table
        let debug_vector_mem = b"[EXCEPTION] Vector table memory at 0x";
        for &b in debug_vector_mem { arch_serial_putc(b); }
        for shift in (0..16).rev() {
            let nibble = (vector_addr >> (shift * 4)) & 0xF;
            let ch = if nibble < 10 { b'0' + nibble as u8 } else { b'A' + (nibble - 10) as u8 };
            arch_serial_putc(ch);
        }
        let debug_colon = b":\r\n";
        for &b in debug_colon { arch_serial_putc(b); }

        // Read first 32 bytes of exception vector table and display as hex
        let vec_ptr = vector_addr as *const u32;
        for i in 0..8 {
            let word = vec_ptr.add(i).read_volatile();
            let debug_word = b"  0x";
            for &b in debug_word { arch_serial_putc(b); }
            for shift in (0..8).rev() {
                let nibble = (word >> (shift * 4)) & 0xF;
                let ch = if nibble < 10 { b'0' + nibble as u8 } else { b'A' + (nibble - 10) as u8 };
                arch_serial_putc(ch);
            }
            if i % 2 == 1 {
                let debug_nl = b"\r\n";
                for &b in debug_nl { arch_serial_putc(b); }
            } else {
                arch_serial_putc(b' ');
            }
        }

        // Check EL1h Sync vector at offset 0x200 (critical for undefined instruction)
        let debug_el1_msg = b"[EXCEPTION] EL1h Sync vector at offset 0x200:\r\n";
        for &b in debug_el1_msg { arch_serial_putc(b); }
        let el1_sync_ptr = (vector_addr + 0x200) as *const u32;
        for i in 0..4 {
            let word = el1_sync_ptr.add(i).read_volatile();
            let debug_word = b"  0x";
            for &b in debug_word { arch_serial_putc(b); }
            for shift in (0..8).rev() {
                let nibble = (word >> (shift * 4)) & 0xF;
                let ch = if nibble < 10 { b'0' + nibble as u8 } else { b'A' + (nibble - 10) as u8 };
                arch_serial_putc(ch);
            }
            if i % 2 == 1 {
                let debug_nl = b"\r\n";
                for &b in debug_nl { arch_serial_putc(b); }
            } else {
                arch_serial_putc(b' ');
            }
        }

        // Also check EL0 Sync vector at offset 0x400 (critical for svc #0)
        let debug_el0_msg = b"[EXCEPTION] EL0 Sync vector at offset 0x400:\r\n";
        for &b in debug_el0_msg { arch_serial_putc(b); }
        let el0_sync_ptr = (vector_addr + 0x400) as *const u32;
        for i in 0..4 {
            let word = el0_sync_ptr.add(i).read_volatile();
            let debug_word = b"  0x";
            for &b in debug_word { arch_serial_putc(b); }
            for shift in (0..8).rev() {
                let nibble = (word >> (shift * 4)) & 0xF;
                let ch = if nibble < 10 { b'0' + nibble as u8 } else { b'A' + (nibble - 10) as u8 };
                arch_serial_putc(ch);
            }
            if i % 2 == 1 {
                let debug_nl = b"\r\n";
                for &b in debug_nl { arch_serial_putc(b); }
            } else {
                arch_serial_putc(b' ');
            }
        }
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
    ldr x9, =exc_el1_sync
    br x9
    .balign 128
    // EL1t IRQ
    ldr x9, =exc_irq
    br x9
    .balign 128
    // EL1t FIQ
    ldr x9, =exc_unexpected
    br x9
    .balign 128
    // EL1t SError
    ldr x9, =exc_unexpected
    br x9
    .balign 128

    // EL1h Sync  (SP_EL1) — 0x200
    b exc_el1_sync_nearby
    .balign 128
    // EL1h IRQ   — 0x280
    ldr x9, =exc_irq
    br x9
    .balign 128
    // EL1h FIQ
    ldr x9, =exc_unexpected
    br x9
    .balign 128
    // EL1h SError
    ldr x9, =exc_unexpected
    br x9
    .balign 128

    // EL0-64 Sync — 0x400
    ldr x9, =exc_el0_sync_impl
    br x9
    .balign 128
    // EL0-64 IRQ
    ldr x9, =exc_el0_irq
    br x9
    .balign 128
    // EL0-64 FIQ
    ldr x9, =exc_unexpected
    br x9
    .balign 128
    // EL0-64 SError
    ldr x9, =exc_unexpected
    br x9
    .balign 128

    // EL0-32 (AArch32) — 0x600 — not supported
    ldr x9, =exc_unexpected
    br x9
    .balign 128
    ldr x9, =exc_unexpected
    br x9
    .balign 128
    ldr x9, =exc_unexpected
    br x9
    .balign 128
    ldr x9, =exc_unexpected
    br x9
    .balign 128

// ── Exception handlers (placed immediately after vectors for direct branch) ──
// At this point we should be at offset 0x800 from vector table base

exc_el1_sync_nearby:
    // Save x0 first
    stp  x0, x1, [sp, #-16]!

    // DEBUG: Print character directly to UART to show we entered exc_el1_sync
    mov  x0, #'Y'  // Y for EL1 sync
    mov  x1, #0x09000000  // QEMU virt UART base address
    str  w0, [x1]  // Write directly to UART data register

    // Skip the undefined instruction by advancing ELR_EL1 by 4 bytes
    mrs  x0, elr_el1
    add  x0, x0, #4
    msr  elr_el1, x0

    // Restore x0, x1
    ldp  x0, x1, [sp], #16

    // Return from exception
    eret


exc_irq_nearby:
    // DEBUG: Print character to show we entered exc_irq
    mov  x9, #'J'  // J for IRQ
    bl   arch_serial_putc
    bl   irq_dispatch
    b .

exc_el0_irq_nearby:
    // DEBUG: Print character to show we entered exc_el0_irq
    mov  x9, #'I'  // I for IRQ
    bl   arch_serial_putc
    b .

exc_unexpected_nearby:
    // DEBUG: Print character to show we entered exc_unexpected
    mov  x9, #'Y'  // Y for unexpected nearby
    mov  x10, #0x09000000
    str  w9, [x10]
    b .

// ── EL0 sync handler (syscalls and faults from userspace) ───────────────────
exc_el0_sync_impl:
    // DEBUG: Print 'E' to show we entered EL0 sync handler
    stp  x0, x1, [sp, #-16]!  // Save x0, x1 first
    mov  x0, #'E'
    mov  x1, #0x09000000
    str  w0, [x1]
    ldp  x0, x1, [sp], #16    // Restore x0, x1

    // Save all registers for syscall/fault handling
    stp  x0,  x1,  [sp, #-16]!
    stp  x2,  x3,  [sp, #-16]!
    stp  x4,  x5,  [sp, #-16]!
    stp  x6,  x7,  [sp, #-16]!
    stp  x8,  x9,  [sp, #-16]!
    stp  x10, x11, [sp, #-16]!
    stp  x12, x13, [sp, #-16]!
    stp  x14, x15, [sp, #-16]!
    stp  x16, x17, [sp, #-16]!
    stp  x18, x19, [sp, #-16]!
    stp  x20, x21, [sp, #-16]!
    stp  x22, x23, [sp, #-16]!
    stp  x24, x25, [sp, #-16]!
    stp  x26, x27, [sp, #-16]!
    stp  x28, x29, [sp, #-16]!
    str  x30,      [sp, #-8]!

    // Check ESR_EL1 to determine exception type
    mrs  x0, esr_el1
    mrs  x1, elr_el1

    // Extract Exception Class (EC) from ESR_EL1[31:26]
    lsr  x2, x0, #26
    and  x2, x2, #0x3F

    // Check if it's SVC (exception class 0x15 = 21)
    cmp  x2, #21
    b.eq el0_syscall_handler

    // Not a syscall - route to fault handler
    bl   exc_el0_fault_handler
    b    el0_sync_return

el0_syscall_handler:
    // DEBUG: Print 'S' to show we entered syscall handler
    mov  x9, #'S'
    mov  x10, #0x09000000
    str  w9, [x10]

    // Get syscall arguments from saved registers on stack
    // Stack layout (from bottom to top): x0,x1 x2,x3 x4,x5 x6,x7 x8,x9 ...
    ldr  x0, [sp, #120]  // x8 (syscall number) at offset 8*15 = 120
    ldr  x1, [sp, #128]  // x0 (arg0) at offset 8*16 = 128
    ldr  x2, [sp, #112]  // x2 (arg1) at offset 8*14 = 112
    ldr  x3, [sp, #104]  // x3 (arg2) at offset 8*13 = 104
    ldr  x4, [sp, #96]   // x4 (arg3) at offset 8*12 = 96
    ldr  x5, [sp, #88]   // x5 (arg4) at offset 8*11 = 88
    ldr  x6, [sp, #80]   // x6 (arg5) at offset 8*10 = 80
    mov  x7, sp          // Frame pointer as last argument

    // DEBUG: Print 'D' before calling syscall_dispatch
    mov  x9, #'D'
    mov  x10, #0x09000000
    str  w9, [x10]

    // Call syscall_dispatch(number, a0, a1, a2, a3, a4, a5, frame_ptr)
    bl   syscall_dispatch

    // DEBUG: Print 'R' after syscall_dispatch returns
    mov  x9, #'R'
    mov  x10, #0x09000000
    str  w9, [x10]

    // Store return value in saved x0 slot
    str  x0, [sp, #128]

el0_sync_return:
    // Restore all registers in reverse order
    ldr  x30,      [sp], #8
    ldp  x28, x29, [sp], #16
    ldp  x26, x27, [sp], #16
    ldp  x24, x25, [sp], #16
    ldp  x22, x23, [sp], #16
    ldp  x20, x21, [sp], #16
    ldp  x18, x19, [sp], #16
    ldp  x16, x17, [sp], #16
    ldp  x14, x15, [sp], #16
    ldp  x12, x13, [sp], #16
    ldp  x10, x11, [sp], #16
    ldp  x8,  x9,  [sp], #16
    ldp  x6,  x7,  [sp], #16
    ldp  x4,  x5,  [sp], #16
    ldp  x2,  x3,  [sp], #16
    ldp  x0,  x1,  [sp], #16

    // Return to userspace
    eret

// ── Macro: reload SP_EL1 from TPIDR_EL1 on EL0 entry ────────────────────────
//
// TPIDR_EL1 holds the current task's kernel stack top, written by
// arch_set_kernel_stack() before each cpu_switch_to.
//
// Technique: temporarily stash x9 in sp_el0 (user SP register, which the
// hardware preserves separately and restores on eret), load TPIDR_EL1 into
// x9, move it into sp, then recover x9 from sp_el0.  This keeps the user's
// sp_el0 intact (we restore it before any saves touch the stack).
.macro reload_kernel_sp
    msr  sp_el0, x9           // stash x9; preserves user SP_EL0 value below
    mrs  x9, tpidr_el1        // x9 = kernel stack top (0 if never set)
    cbz  x9, 1f               // skip reload if not set yet (early boot)
    mov  sp, x9               // reset SP to kernel stack top
1:  mrs  x9, sp_el0           // restore x9; user's sp_el0 is back in sp_el0
.endm

// ── EL0-64 IRQ — save caller-saved regs, reload KSP, dispatch, eret ──────────
exc_el0_irq:
    // DEBUG: Print character to show we entered exc_el0_irq
    mov  x9, #'I'  // I for IRQ
    bl   arch_serial_putc

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

// ── EL1h IRQ — save caller-saved regs, dispatch, restore, eret ───────────────
// Does NOT reload the kernel SP (already on the correct EL1 stack).
exc_irq:
    // DEBUG: Print character to show we entered exc_irq
    mov  x9, #'J'  // J for EL1 IRQ
    bl   arch_serial_putc

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

    bl   irq_dispatch           // Rust handler; may call sched::timer_tick_irq

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
    // DEBUG: Print character to show we entered exc_el1_sync
    mov  x9, #'Y'  // Y for EL1 sync
    bl   arch_serial_putc

    mrs  x0, esr_el1
    mrs  x1, elr_el1
    bl   exc_el1_sync_handler   // panics

// ── EL0-64 synchronous exception (SVC / user fault) ───────────────────────
//
// Saves the COMPLETE user register state into a UserFrame on the kernel stack:
//
//   UserFrame layout (272 bytes, 16-byte aligned):
//   [sp+  0]: x0        [sp+  8]: x1
//   [sp+ 16]: x2        [sp+ 24]: x3
//   [sp+ 32]: x4        [sp+ 40]: x5
//   [sp+ 48]: x6        [sp+ 56]: x7
//   [sp+ 64]: x8        [sp+ 72]: x9
//   [sp+ 80]: x10       [sp+ 88]: x11
//   [sp+ 96]: x12       [sp+104]: x13
//   [sp+112]: x14       [sp+120]: x15
//   [sp+128]: x16       [sp+136]: x17
//   [sp+144]: x18       [sp+152]: x19
//   [sp+160]: x20       [sp+168]: x21
//   [sp+176]: x22       [sp+184]: x23
//   [sp+192]: x24       [sp+200]: x25
//   [sp+208]: x26       [sp+216]: x27
//   [sp+224]: x28       [sp+232]: x29
//   [sp+240]: x30
//   [sp+248]: sp_el0
//   [sp+256]: elr_el1   (user PC after SVC)
//   [sp+264]: spsr_el1
//
// syscall_dispatch receives 8 arguments:
//   x0=number, x1=a0, x2=a1, x3=a2, x4=a3, x5=a4, x6=a5, x7=frame_ptr
exc_el0_sync:
    // IMMEDIATE DEBUG: Print character to show we entered exc_el0_sync
    mov  x9, #'$'  // $ for exc_el0_sync (very distinctive!)
    bl   arch_serial_putc

    reload_kernel_sp
    // Allocate UserFrame (272 bytes).
    sub  sp, sp, #272
    // Save x0-x29 as 15 pairs.
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
    // Save x30 alone (odd register after 15 pairs).
    str  x30,      [sp, #240]
    // Save system registers (x9 is already saved at [sp+72]).
    mrs  x9, sp_el0
    str  x9,       [sp, #248]
    mrs  x9, elr_el1
    str  x9,       [sp, #256]
    mrs  x9, spsr_el1
    str  x9,       [sp, #264]

    // Determine exception class.
    mrs  x9, esr_el1
    lsr  x9, x9, #26
    cmp  x9, #0x15           // EC 0x15 = SVC AArch64
    b.ne exc_el0_fault

    // SVC: before: x0=a0, x1=a1, x2=a2, x3=a3, x4=a4, x5=a5, x8=number
    // Build syscall_dispatch(number, a0, a1, a2, a3, a4, a5, frame_ptr).
    // Rearrange high-to-low to avoid overwriting live sources:
    //   x7 = sp (frame_ptr, captured before x5 is overwritten)
    //   x6 = a5 (from x5)
    //   x5 = a4 (from x4)
    //   x4 = a3 (from x3)
    //   x3 = a2 (from x2)
    //   x2 = a1 (from x1)
    //   x1 = a0 (from x0)
    //   x0 = syscall number (from x8)
    mov  x7,  sp
    mov  x6,  x5
    mov  x5,  x4
    mov  x4,  x3
    mov  x3,  x2
    mov  x2,  x1
    mov  x1,  x0
    mov  x0,  x8
    bl   syscall_dispatch    // result in x0
    // Store return value into saved-x0 slot so eret restores it.
    str  x0,  [sp, #0]

    // Check and deliver any pending signals before returning to user space.
    // x0 = frame_ptr = sp (points to the UserFrame we just saved).
    // check_and_deliver_signals may modify the frame in-place to redirect
    // execution to a signal handler.
    mov  x0,  sp
    bl   check_and_deliver_signals

    b    exc_el0_return

exc_el0_fault:
    mrs  x0, esr_el1
    mrs  x1, elr_el1
    bl   exc_el0_fault_handler

exc_el0_return:
    // Restore system registers (x9 as scratch; its saved value is at [sp+72]).
    ldr  x9, [sp, #264]
    msr  spsr_el1, x9
    ldr  x9, [sp, #256]
    msr  elr_el1,  x9
    ldr  x9, [sp, #248]
    msr  sp_el0,   x9
    // Restore all GPRs.
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
    // DEBUG: Print character to show we entered exc_unexpected
    mov  x9, #'X'  // X for unexpected (more distinctive)
    mov  x10, #0x09000000
    str  w9, [x10]

    mrs  x0, esr_el1
    mrs  x1, elr_el1
    bl   exc_unexpected_handler  // panics

// ── ret_to_user — first entry into a new user-space task ──────────────────
//
// Called via cpu_switch_to (x30 = ret_to_user in the task's CpuContext).
// The kernel stack contains 4 words built by CpuContext::new_user_task:
//   [sp+0]:  SP_EL0   (user stack pointer)
//   [sp+8]:  ELR_EL1  (user entry point)
//   [sp+16]: SPSR_EL1 (0 = EL0t, all interrupts unmasked)
//   [sp+24]: PAGE_TABLE (user page table root, set by scheduler)
.global ret_to_user
.type   ret_to_user, %function
ret_to_user:
    // Debug: Direct UART write to show we reached ret_to_user
    mov  x0, #'R'
    mov  x1, #0x09000000
    str  w0, [x1]

    // Load user context from stack
    ldr  x0, [sp], #8
    msr  sp_el0,   x0

    ldr  x0, [sp], #8
    msr  elr_el1,  x0

    ldr  x0, [sp], #8
    msr  spsr_el1, x0

    // Load page table from stack frame and switch to it
    ldr  x0, [sp], #8      // Load page table from 4th word
    cbz  x0, 1f            // if page table is 0, skip switch
    msr  ttbr0_el1, x0     // switch to user page table
    isb                    // ensure page table switch completes
1:

    // Debug: Direct UART write before eret
    mov  x0, #'!'
    mov  x1, #0x09000000
    str  w0, [x1]

    mov  x0, #0             // fork / spawn returns 0 in child / new task
    eret

// ── ret_to_user_fork — resume a forked child from a full UserFrame ─────────
//
// Called via cpu_switch_to (x30 = ret_to_user_fork) when the child task is
// scheduled for the first time.  SP points to a UserFrame (272 bytes) that
// was copied from the parent at fork time, with x[0] already zeroed.
//
// UserFrame layout (matches exc_el0_sync save sequence above):
//   [sp+  0..239]: x0-x30 (31 × 8 bytes)
//   [sp+248]:      sp_el0
//   [sp+256]:      elr_el1
//   [sp+264]:      spsr_el1
.global ret_to_user_fork
.type   ret_to_user_fork, %function
ret_to_user_fork:
    // Restore system registers using x9 as scratch.
    ldr  x9, [sp, #264]
    msr  spsr_el1, x9
    ldr  x9, [sp, #256]
    msr  elr_el1,  x9
    ldr  x9, [sp, #248]
    msr  sp_el0,   x9
    // Restore GPRs (x9 restored from stack after its use as scratch above).
    ldr  x0,       [sp, #0]    // x0 = 0 (fork returns 0 in child)
    ldp  x1,  x2,  [sp, #8]
    ldp  x3,  x4,  [sp, #24]
    ldp  x5,  x6,  [sp, #40]
    ldp  x7,  x8,  [sp, #56]
    ldp  x9,  x10, [sp, #72]
    ldp  x11, x12, [sp, #88]
    ldp  x13, x14, [sp, #104]
    ldp  x15, x16, [sp, #120]
    ldp  x17, x18, [sp, #136]
    ldp  x19, x20, [sp, #152]
    ldp  x21, x22, [sp, #168]
    ldp  x23, x24, [sp, #184]
    ldp  x25, x26, [sp, #200]
    ldp  x27, x28, [sp, #216]
    ldp  x29, x30, [sp, #232]
    add  sp,  sp,  #272
    eret

// ── arch_execve_return — drop into user space at a new entry point ─────────
//
// Called from sched::replace_address_space after the new address space is
// installed.  Never returns.
//
// Arguments (AAPCS64):
//   x0 = entry   — virtual address of the new process entry point
//   x1 = user_sp — user stack pointer
.global arch_execve_return
.type   arch_execve_return, %function
arch_execve_return:
    msr  elr_el1,  x0       // user entry point
    msr  sp_el0,   x1       // user stack pointer
    msr  spsr_el1, xzr      // EL0t, all interrupts unmasked
    dsb  sy
    isb
    // Zero all general-purpose registers so the new process starts clean.
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

// ── IRQ dispatch table ────────────────────────────────────────────────────────
//
// Handlers are registered at init time (single-CPU, interrupts disabled) and
// read-only from IRQ context, so no lock is needed.

pub const MAX_IRQS: usize = 1020;

static mut IRQ_HANDLERS: [Option<fn(u32)>; MAX_IRQS] = [None; MAX_IRQS];

/// Register a handler for the given GIC IRQ ID.
///
/// # Safety
/// Must be called before the corresponding IRQ is unmasked (typically during
/// driver init with interrupts disabled).  IRQ context must never call this.
pub unsafe fn register_irq(id: u32, handler: fn(u32)) {
    if (id as usize) < MAX_IRQS {
        IRQ_HANDLERS[id as usize] = Some(handler);
    }
}

// ── Rust-side handlers ────────────────────────────────────────────────────────

/// Dispatch an IRQ: acknowledge via GIC, route to the correct handler, EOI.
///
/// Called from `exc_irq` with caller-saved registers already stacked.
/// Must NOT acquire any spin locks (those could be held by interrupted code).
#[no_mangle]
unsafe extern "C" fn irq_dispatch() {
    let iar = super::gic::ack();
    let id  = super::gic::irq_id(iar);
    if id == super::gic::SPURIOUS { return; }

    if id == 30 {
        // PPI #30 = EL1 physical timer.
        super::timer::on_tick();
    } else if (id as usize) < MAX_IRQS {
        if let Some(handler) = IRQ_HANDLERS[id as usize] {
            handler(id);
        }
    }

    super::gic::eoi(iar);

    // After acknowledging the interrupt, check if the scheduler wants to
    // preempt the current task.  We are still in exception context here, but
    // yield_now() saves the task's callee-saved registers via cpu_switch_to
    // and returns normally when the task is resumed; the exc_irq asm epilogue
    // then restores caller-saved registers and issues eret as usual.
    sched::preempt_check();
}

/// EL1 synchronous exception — always a kernel bug; panic with diagnostics.
#[no_mangle]
unsafe extern "C" fn exc_el1_sync_handler(esr: u64, elr: u64) {
    extern "C" { fn arch_serial_putc(b: u8); }
    let msg = b"\n[EXCEPTION] EL1 Sync! ESR=";
    for &b in msg { arch_serial_putc(b); }
    // Just loop for life confirmation
    loop { core::hint::spin_loop(); }
}

/// Print detailed information about data abort exceptions for debugging
unsafe fn print_detailed_data_abort_info(esr: u64, elr: u64, far: u64) {
    extern "C" { fn arch_serial_putc(b: u8); }

    let ec = (esr >> 26) & 0x3F;  // Exception Class
    let iss = esr & 0x1FFFFFF;    // Instruction Specific Syndrome

    // Print basic info
    let msg = b"[CYANOS] DETAILED DATA ABORT:\r\n";
    for &b in msg { arch_serial_putc(b); }

    // Exception Class
    let ec_prefix = b"EC=0x";
    for &b in ec_prefix { arch_serial_putc(b); }

    let ec_hex = ((ec >> 4) & 0xF, ec & 0xF);
    for nibble in [ec_hex.0, ec_hex.1] {
        let c = if nibble < 10 { b'0' + nibble as u8 } else { b'A' + nibble as u8 - 10 };
        arch_serial_putc(c);
    }

    if ec == 0x20 {
        let msg = b" (Instruction Abort, EL0)\r\n";
        for &b in msg { arch_serial_putc(b); }
    } else if ec == 0x21 {
        let msg = b" (Instruction Abort, current EL)\r\n";
        for &b in msg { arch_serial_putc(b); }
    } else if ec == 0x24 {
        let msg = b" (Data Abort, EL0)\r\n";
        for &b in msg { arch_serial_putc(b); }
    } else if ec == 0x25 {
        let msg = b" (Data Abort, current EL)\r\n";
        for &b in msg { arch_serial_putc(b); }
    } else {
        let msg = b" (Unknown exception class)\r\n";
        for &b in msg { arch_serial_putc(b); }
    }

    // For data/instruction aborts, decode fault status
    if ec == 0x24 || ec == 0x25 || ec == 0x20 || ec == 0x21 {
        let dfsc = iss & 0x3F;  // Data/Instruction Fault Status Code (bits 5:0)
        let wnr = (iss >> 6) & 1;  // Write not Read (bit 6, data aborts only)
        let s1ptw = (iss >> 7) & 1;  // Stage 1 translation table walk (bit 7)

        // Print DFSC manually to avoid type mismatch issues
        let dfsc_prefix = b"DFSC=0x";
        for &b in dfsc_prefix { arch_serial_putc(b); }

        let dfsc_hex = ((dfsc >> 4) & 0xF, dfsc & 0xF);
        for nibble in [dfsc_hex.0, dfsc_hex.1] {
            let c = if nibble < 10 { b'0' + nibble as u8 } else { b'A' + nibble as u8 - 10 };
            arch_serial_putc(c);
        }

        if dfsc >= 0b000000 && dfsc <= 0b000011 {
            let msg = b" (Address size fault)\r\n";
            for &b in msg { arch_serial_putc(b); }
        } else if dfsc >= 0b000100 && dfsc <= 0b000111 {
            let msg = b" (Translation fault)\r\n";
            for &b in msg { arch_serial_putc(b); }
        } else if dfsc >= 0b001001 && dfsc <= 0b001011 {
            let msg = b" (Access flag fault)\r\n";
            for &b in msg { arch_serial_putc(b); }
        } else if dfsc >= 0b001101 && dfsc <= 0b001111 {
            let msg = b" (Permission fault)\r\n";
            for &b in msg { arch_serial_putc(b); }
        } else if dfsc == 0b010000 {
            let msg = b" (Synchronous External abort)\r\n";
            for &b in msg { arch_serial_putc(b); }
        } else if dfsc == 0b010001 {
            let msg = b" (Synchronous Tag Check Fault)\r\n";
            for &b in msg { arch_serial_putc(b); }
        } else if dfsc == 0b100001 {
            let msg = b" (Alignment fault)\r\n";
            for &b in msg { arch_serial_putc(b); }
        } else if dfsc == 0b110000 {
            let msg = b" (TLB conflict abort)\r\n";
            for &b in msg { arch_serial_putc(b); }
        } else {
            let msg = b" (Unknown fault)\r\n";
            for &b in msg { arch_serial_putc(b); }
        }

        if ec == 0x24 || ec == 0x25 {  // Data aborts
            if wnr == 1 {
                let wnr_msg = b"WnR=1 (Write access)\r\n";
                for &b in wnr_msg { arch_serial_putc(b); }
            } else {
                let wnr_msg = b"WnR=0 (Read access)\r\n";
                for &b in wnr_msg { arch_serial_putc(b); }
            }
        }

        if s1ptw == 1 {
            let s1ptw_msg = b"S1PTW=1 (Stage 1 translation table walk)\r\n";
            for &b in s1ptw_msg { arch_serial_putc(b); }
        }
    }

    // Print registers in hex
    print_hex_value(b"ESR_EL1=0x", esr);
    print_hex_value(b"ELR_EL1=0x", elr);
    print_hex_value(b"FAR_EL1=0x", far);
}

/// Print a hex value to serial
unsafe fn print_hex_value(prefix: &[u8], value: u64) {
    extern "C" { fn arch_serial_putc(b: u8); }

    for &b in prefix { arch_serial_putc(b); }

    for i in (0..16).rev() {
        let nibble = ((value >> (i * 4)) & 0xF) as u8;
        let c = if nibble < 10 { b'0' + nibble } else { b'A' + nibble - 10 };
        arch_serial_putc(c);
    }

    arch_serial_putc(b'\r');
    arch_serial_putc(b'\n');
}

/// EL0 fault (non-SVC) — attempt demand-paging, then kill on unhandled faults.
///
/// EC values that indicate a translation or access-flag fault (i.e. "page not
/// present") from EL0:
///   0x20 — Instruction Abort from EL0 (EL0 Inst Abort)
///   0x21 — Instruction Abort from EL0 (EL0 Inst Abort, current EL)  [unused]
///   0x24 — Data Abort from EL0
///   0x25 — Data Abort from EL0 (current EL)                          [unused]
///
/// IFSR/DFSR LSB (ISS[5:0]) == 0b0001xx / 0b0010xx indicate translation
/// faults at levels 1–3.  We delegate all EL0 aborts to the VMM demand-paging
/// path; if it declines (no matching lazy VMA) we kill the task.
#[no_mangle]
unsafe extern "C" fn exc_el0_fault_handler(esr: u64, elr: u64) {
    // Debug: Print that we caught a userspace fault
    let debug_fault = b"[DEBUG] EL0 fault! ESR=0x";
    for &b in debug_fault { arch_serial_putc(b); }
    for i in (0..16).rev() {
        let nibble = ((esr >> (i * 4)) & 0xF) as u8;
        let ch = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
        arch_serial_putc(ch);
    }
    let debug_elr = b" ELR=0x";
    for &b in debug_elr { arch_serial_putc(b); }
    for i in (0..16).rev() {
        let nibble = ((elr >> (i * 4)) & 0xF) as u8;
        let ch = if nibble < 10 { b'0' + nibble } else { b'a' + nibble - 10 };
        arch_serial_putc(ch);
    }
    let debug_newline = b"\r\n";
    for &b in debug_newline { arch_serial_putc(b); }

    let ec = (esr >> 26) & 0x3F;  // Exception Class

    // Data Abort (0x24) or Instruction Abort (0x20) from EL0.
    let is_abort = ec == 0x24 || ec == 0x20;

    if is_abort {
        // FAR_EL1 holds the faulting virtual address for aborts.
        #[cfg(target_arch = "aarch64")]
        let far: u64 = {
            let far: u64;
            core::arch::asm!("mrs {}, far_el1", out(reg) far, options(nomem, nostack));
            far
        };
        #[cfg(not(target_arch = "aarch64"))]
        let far: u64 = 0;

        if sched::handle_page_fault(far as usize) {
            // Fault handled by the demand-paging path — resume the task.
            return;
        }
    }

    // Unhandled fault — print a brief serial diagnostic then kill the task.
    extern "C" { fn arch_serial_putc(b: u8); }
    let msg = b"EL0 fault: task killed\r\n";
    for &b in msg { arch_serial_putc(b); }
    let _ = elr;
    sched::exit(1);
}

/// Unexpected vector — should never fire; panic with diagnostics.
#[no_mangle]
unsafe extern "C" fn exc_unexpected_handler(esr: u64, elr: u64) {
    panic!("unexpected exception: ESR={:#010x} ELR={:#010x}", esr, elr);
}

/// AArch64 syscall entry — legacy wrapper; the asm vector calls `syscall_dispatch`
/// directly.  Kept for documentation; matches the 8-parameter (+ frame_ptr) signature.
#[no_mangle]
pub unsafe extern "C" fn syscall_entry_aarch64(
    number:    usize,
    a0: usize, a1: usize, a2: usize,
    a3: usize, a4: usize, a5: usize,
    frame_ptr: usize,
) -> isize {
    extern "C" {
        fn syscall_dispatch(
            number:    usize,
            a0: usize, a1: usize, a2: usize,
            a3: usize, a4: usize, a5: usize,
            frame_ptr: usize,
        ) -> isize;
    }
    syscall_dispatch(number, a0, a1, a2, a3, a4, a5, frame_ptr)
}

