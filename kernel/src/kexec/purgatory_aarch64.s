// kexec purgatory (aarch64). Copied into a page outside every destination
// range and entered at that page's physical address through an identity
// map in TTBR0 (exec_aarch64.rs), MMU and caches on, EL1, DAIF masked.
// Position independent, no stack, no FP/SIMD (the kernel is softfloat).
//
//   x0 = first indirection page (phys)    x1 = entry of the new kernel
//   x2 = FDT (phys)                       x3 = (start, end) pairs to clean
//   x4 = number of pairs
//
// 1. TTBR1 walks off: the old kernel's tables may be overwritten below.
// 2. Walk the Linux-style list (DESTINATION 1, INDIRECTION 2, DONE 4,
//    SOURCE 8) and copy each source page to the cursor.
// 3. dc civac every line of the destination footprint (BSS and FDT
//    included: the new kernel writes BSS and its first page tables with
//    the caches off), then invalidate the I-cache.
// 4. SCTLR_EL1 = reset value (MMU, caches, WXN, alignment all off), PAN
//    cleared, and the arm64 Image protocol: x0 = FDT, x1..x3 = 0.

    .section .text.kexec_purgatory, "ax"
    .balign 16
    .globl kexec_purgatory_start
    .globl kexec_purgatory_end
kexec_purgatory_start:
    mov     x8, x1
    mov     x7, x2

    mrs     x9, tcr_el1
    orr     x9, x9, #(1 << 23)          // EPD1
    msr     tcr_el1, x9
    isb
    tlbi    vmalle1
    dsb     nsh
    isb

    mov     x9, xzr                     // copy cursor
    mov     x10, x0                     // next list word
1:  ldr     x11, [x10], #8
    and     x12, x11, #0xfffffffffffff000
    tbnz    x11, #0, 2f
    tbnz    x11, #1, 3f
    tbnz    x11, #2, 5f
    tbnz    x11, #3, 4f
    b       1b
2:  mov     x9, x12                     // DESTINATION
    b       1b
3:  mov     x10, x12                    // INDIRECTION
    b       1b
4:  mov     x13, #4096                  // SOURCE
6:  ldp     x14, x15, [x12], #16
    stp     x14, x15, [x9], #16
    subs    x13, x13, #16
    b.ne    6b
    b       1b

5:  mrs     x13, ctr_el0                // DONE: clean + invalidate
    ubfx    x13, x13, #16, #4           // DminLine (log2 words)
    mov     x14, #4
    lsl     x14, x14, x13               // line bytes
    sub     x15, x14, #1
7:  cbz     x4, 9f
    ldp     x16, x17, [x3], #16
    bic     x16, x16, x15
8:  dc      civac, x16
    add     x16, x16, x14
    cmp     x16, x17
    b.lo    8b
    sub     x4, x4, #1
    b       7b
9:  dsb     sy
    ic      ialluis
    dsb     sy
    isb

    movz    x13, #0x0800                // SCTLR_EL1 reset: RES1, MMU/C/I off
    movk    x13, #0x30d0, lsl #16
    msr     sctlr_el1, x13
    isb
    .inst   0xd500409f                  // msr pan, #0
    ic      ialluis
    tlbi    vmalle1
    dsb     ish
    isb

    mov     x0, x7
    mov     x1, xzr
    mov     x2, xzr
    mov     x3, xzr
    br      x8

// Secondary CPUs: same identity-mapped entry, then MMU/caches off, VBAR 0,
// and PSCI CPU_OFF (x1 = 0 HVC, 1 SMC). Leaving the CPU in reset-like state
// matters under HVF, where a later CPU_ON does not reset SCTLR_EL1/VBAR_EL1:
// with the old kernel's MMU-on SCTLR the new kernel's AP stub faults at its
// first (physical) instruction.
    .balign 16
    .globl kexec_purgatory_off
kexec_purgatory_off:
    mov     x9, x1
    dsb     sy
    movz    x13, #0x0800
    movk    x13, #0x30d0, lsl #16
    msr     sctlr_el1, x13
    isb
    ic      ialluis
    tlbi    vmalle1
    dsb     ish
    isb
    msr     vbar_el1, xzr
    msr     ttbr0_el1, xzr
    isb
    movz    x0, #0x0002                 // PSCI CPU_OFF
    movk    x0, #0x8400, lsl #16
    cbnz    x9, 1f
    hvc     #0
    b       2f
1:  .inst   0xd4000003                  // smc #0
2:  wfi
    b       2b
kexec_purgatory_end:
