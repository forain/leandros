// AArch64 Exception Vector Table — arch/aarch64/src/exception_asm.s

.macro ventry label
    .align 7
    b \label
.endm

.section ".text", "ax"
.align 11
.globl __exception_vectors
__exception_vectors:
    // Current EL with SP0 (should not happen)
    ventry exc_unexpected
    ventry exc_unexpected
    ventry exc_unexpected
    ventry exc_unexpected

    // Current EL with SPx
    ventry exc_el1_sync
    ventry exc_el1_irq
    ventry exc_unexpected
    ventry exc_unexpected

    // Lower EL using AArch64
    ventry exc_el0_sync
    ventry exc_el0_irq
    ventry exc_unexpected
    ventry exc_unexpected

    // Lower EL using AArch32
    ventry exc_unexpected
    ventry exc_unexpected
    ventry exc_unexpected
    ventry exc_unexpected

// ── Exception Handlers ────────────────────────────────────────────────────────

exc_el1_sync:
    // Save minimal state on current stack (SP_EL1)
    sub  sp, sp, #288
    stp  x0, x1, [sp, #0]
    stp  x2, x3, [sp, #16]
    mrs  x0, esr_el1
    mrs  x1, elr_el1
    bl   exc_el1_sync_handler
    b    .

exc_el0_sync:
    // 1. We are in EL1. SP is already SP_EL1.
    // However, SP_EL1 might not have been initialized if this is the first entry?
    // No, scheduler sets it.
    // But we MUST ensure we use the kernel stack designated for this CPU.
    // We store the kernel stack top in tpidr_el1.
    
    msr  tpidrro_el0, x9
    mrs  x9, tpidr_el1
    // If tpidr_el1 is 0, we are in early boot, just use current SP.
    cbz  x9, 1f
    
    // Swap SP with the one in tpidr_el1 if we came from EL0.
    // Wait! If we came from EL0, SP is already SP_EL1 (because of SPSel=1).
    // So we just need to ensure SP is at the top of the kernel stack.
    mov  sp, x9
1:  mrs  x9, tpidrro_el0

    // 2. Save full frame
    sub  sp, sp, #288
    stp  x0, x1, [sp, #0]
    stp  x2, x3, [sp, #16]
    stp  x4, x5, [sp, #32]
    stp  x6, x7, [sp, #48]
    stp  x8, x9, [sp, #64]
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
    
    mrs  x21, sp_el0
    mrs  x22, elr_el1
    mrs  x23, spsr_el1
    mrs  x24, ttbr0_el1
    stp  x21, x22, [sp, #248]
    stp  x23, x24, [sp, #264]

    // 3. Dispatch syscall or fault
    mrs  x0, esr_el1
    lsr  x1, x0, #26
    cmp  x1, #0x15           // EC 0x15 = SVC AArch64
    b.eq .Lsyscall

    // Fault
    mov  x1, x22             // elr
    mov  x2, sp              // frame
    bl   exc_el0_sync_handler
    b    ret_to_user

exc_el1_irq:
    sub  sp, sp, #288
    stp  x0, x1, [sp, #0]
    stp  x2, x3, [sp, #16]
    stp  x4, x5, [sp, #32]
    stp  x6, x7, [sp, #48]
    stp  x8, x9, [sp, #64]
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
    
    mrs  x21, sp_el0
    mrs  x22, elr_el1
    mrs  x23, spsr_el1
    mrs  x24, ttbr0_el1
    stp  x21, x22, [sp, #248]
    stp  x23, x24, [sp, #264]

    mov  x0, sp
    bl   exc_el1_irq_handler

    b    ret_to_user

exc_el0_irq:
    msr  tpidrro_el0, x9
    mrs  x9, tpidr_el1
    cbz  x9, 1f
    mov  sp, x9
1:  mrs  x9, tpidrro_el0

    sub  sp, sp, #288
    stp  x0, x1, [sp, #0]
    stp  x2, x3, [sp, #16]
    stp  x4, x5, [sp, #32]
    stp  x6, x7, [sp, #48]
    stp  x8, x9, [sp, #64]
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
    
    mrs  x21, sp_el0
    mrs  x22, elr_el1
    mrs  x23, spsr_el1
    mrs  x24, ttbr0_el1
    stp  x21, x22, [sp, #248]
    stp  x23, x24, [sp, #264]

    mov  x0, sp
    bl   exc_el0_irq_handler

    b    ret_to_user

.Lsyscall:
    // Syscall ABI: x8=nr, x0-x5=args. Return in x0.
    // Regs x0-x5 are already saved in frame.
    // We pass (nr, a0, a1, a2, a3, a4, a5, frame) to syscall_dispatch.
    mov  x0, x8
    ldp  x1, x2, [sp, #0]
    ldp  x3, x4, [sp, #16]
    ldp  x5, x6, [sp, #32]
    mov  x7, sp
    bl   syscall_dispatch
    str  x0, [sp, #0]        // result to frame.x0

.globl ret_to_user
ret_to_user:
    // 1. Restore state
    ldp  x21, x22, [sp, #248] // SP_EL0, ELR_EL1
    ldp  x23, x24, [sp, #264] // SPSR_EL1, PAGE_TABLE
    msr  sp_el0,   x21
    msr  elr_el1,  x22
    msr  spsr_el1, x23
    
    // Switch address space if needed
    cbz  x24, 1f
    msr  ttbr0_el1, x24
    tlbi vmalle1
    dsb  sy
    isb
1:
    ldp  x0, x1, [sp, #0]
    ldp  x2, x3, [sp, #16]
    ldp  x4, x5, [sp, #32]
    ldp  x6, x7, [sp, #48]
    ldp  x8, x9, [sp, #64]
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
    add  sp, sp, #288
    eret

.globl ret_to_user_fork
ret_to_user_fork:
    mov  x0, #0
    str  x0, [sp, #0]
    b    ret_to_user

.globl arch_execve_return
arch_execve_return:
    // x0=entry, x1=sp
    msr  elr_el1, x0
    msr  sp_el0,  x1
    mov  x0, #0
    msr  spsr_el1, x0
    // Ensure ttbr0 is set (caller should have set it, but we can do it here too)
    eret

exc_unexpected:
    mrs  x0, esr_el1
    mrs  x1, elr_el1
    bl   exc_unexpected_handler
    b    .
