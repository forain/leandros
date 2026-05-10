// AArch64 bare-metal entry point supporting Limine — kernel/src/entry_aarch64.s

.section ".boot", "ax", @progbits
.globl _start
_start:
    // ── Diagnostic: 'A' ──
    mov     x0, #0x09000000
    mov     x1, #0x41
    str     w1, [x0]

    // ── PRESERVE ARGUMENTS IMMEDIATELY ──
    mov     x19, x0
    mov     x20, x1

    // Force SP_EL1
    msr     SPSel, #1
    isb
    
    // Check if MMU is on
    mrs     x4, sctlr_el1
    tst     x4, #1
    b.ne    .Llimine_entry

    // ── Direct Boot Path (MMU is OFF) ────────────────────────────────────────
    
    // Diagnostic: 'D'
    mov     x0, #0x09000000
    mov     x1, #0x44
    str     w1, [x0]

    // 1. MAIR
    mov     x4, #0x04FF
    msr     mair_el1, x4

    // 2. TCR: 48-bit VA, 4KB granule
    ldr     x4, =0x280100010
    msr     tcr_el1, x4
    isb

    // 3. Setup temporary page tables using early_pgtables
    adrp    x0, early_pgtables
    add     x0, x0, :lo12:early_pgtables
    
    mov     x5, #32768
    mov     x6, x0
.Lclear_pgt:
    str     xzr, [x6], #8
    subs    x5, x5, #8
    b.ne    .Lclear_pgt

    // Level 0 (PGD)
    mov     x5, #0x1003
    add     x5, x0, x5
    str     x5, [x0, #0]
    str     x5, [x0, #2048]
    str     x5, [x0, #4088]

    // Level 1 (PUD)
    add     x6, x0, #0x1000
    ldr     x5, =0x0000000000000705 // 0..1GB (Device)
    str     x5, [x6, #0]
    ldr     x5, =0x0000000040000701 // 1..2GB (Normal)
    str     x5, [x6, #8]
    ldr     x5, =0x0000000080000701 // 2..3GB (Normal)
    str     x5, [x6, #16]
    ldr     x5, =0x00000000C0000701 // 3..4GB (Normal)
    str     x5, [x6, #24]

    // Set page tables
    msr     ttbr0_el1, x0
    msr     ttbr1_el1, x0
    isb

    // Invalidate TLB
    tlbi    vmalle1
    dsb     nsh
    isb

    // 4. Enable MMU and SIMD
    mrs     x4, sctlr_el1
    orr     x4, x4, #1
    msr     sctlr_el1, x4
    
    mrs     x5, cpacr_el1
    orr     x5, x5, #(3 << 20)
    msr     cpacr_el1, x5
    isb

    // Transition to high virtual address
    ldr     x4, =1f
    br      x4
1:
    // Diagnostic: 'H'
    mov     x0, #0x09000000
    mov     x1, #0x48
    str     w1, [x0]

.Llimine_entry:
    // ── Zero BSS ─────────────────────────────────────────────────────────────
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
.Lbss_loop:
    cmp     x0, x1
    b.ge    .Lbss_done
    str     xzr, [x0], #8
    b       .Lbss_loop
.Lbss_done:

    // Set up initial stack
    adrp    x1, EARLY_STACK
    add     x1, x1, :lo12:EARLY_STACK
    mov     x2, #0x10000
    add     x1, x1, x2
    mov     sp, x1

    // Call kernel_main
    mov     x0, x19
    ldr     x1, .Lkernel_main_val
    blr     x1

.align 3
.Lkernel_main_val:
    .quad kernel_main

.Lhalt:
    wfe
    b       .Lhalt
