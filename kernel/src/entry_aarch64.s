// AArch64 bare-metal entry point supporting Limine — kernel/src/entry_aarch64.s

.section ".text.boot", "ax", @progbits
.globl _start
_start:
    // ── PRESERVE ARGUMENTS IMMEDIATELY ──
    // x0 holds the DTB pointer on direct (-kernel) boot; preserve across the
    // EL2->EL1 drop below (eret does not clobber GPRs).
    mov     x19, x0
    mov     x20, x1

    // ── Drop to EL1 if entered at EL2 ────────────────────────────────────────
    // QEMU `-kernel` on `virt` with `-cpu max` (EL2 implemented, secure=off)
    // enters at EL2.  The rest of this entry — and the kernel — only programs
    // EL1 system registers, so we must hand off to EL1 first.  Limine already
    // enters at EL1, so CurrentEL gates this and leaves that path untouched.
    mrs     x4, CurrentEL
    lsr     x4, x4, #2
    and     x4, x4, #3
    cmp     x4, #2
    b.ne    .Lat_el1                // not EL2 → already where we want to be

    // Record the EL2 entry: QEMU registers its PSCI emulation on the SMC
    // conduit when the guest boots at EL2 (HVC otherwise), and smp.rs must
    // use the matching instruction for CPU_ON.  boot_entered_el2 lives in
    // .data (not .bss) so the BSS-zero loop below can't wipe it.
    adrp    x5, boot_entered_el2
    add     x5, x5, :lo12:boot_entered_el2
    mov     x6, #1
    str     x6, [x5]

    // Let EL1 read the physical/virtual counter without trapping to EL2,
    // otherwise the timer init later faults into nonexistent EL2 vectors.
    mrs     x4, cnthctl_el2
    orr     x4, x4, #3              // EL1PCTEN | EL1PCEN
    msr     cnthctl_el2, x4
    msr     cntvoff_el2, xzr        // zero virtual counter offset

    movz    x4, #0x8000, lsl #16    // HCR_EL2.RW (bit 31): EL1 executes AArch64
    msr     hcr_el2, x4

    movz    x4, #0x0800             // SCTLR_EL1 reset: RES1 bits set, MMU off
    movk    x4, #0x30d0, lsl #16
    msr     sctlr_el1, x4

    mov     x4, #0x3c5              // SPSR_EL2: return to EL1h, DAIF masked
    msr     spsr_el2, x4
    adr     x4, .Lat_el1            // PC-relative: physical addr (MMU still off)
    msr     elr_el2, x4
    isb
    eret

.Lat_el1:
    // Force SP_EL1
    msr     SPSel, #1
    isb
    
    // Check if MMU is on
    mrs     x4, sctlr_el1
    tst     x4, #1
    b.ne    .Llimine_entry

    // ── Direct Boot Path (MMU is OFF) ────────────────────────────────────────

    // 0. Zero BSS *now*, before building page tables.
    //    early_pgtables lives in .bss; once we install it as the live page
    //    table we must never zero it again.  adrp/add is PC-relative, so with
    //    the MMU off it yields the *physical* BSS addresses we can write here.
    //    (The Limine path zeroes BSS later instead — there early_pgtables is
    //    unused, so clearing it is harmless.)
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
.Ldirect_bss:
    cmp     x0, x1
    b.ge    .Ldirect_bss_done
    str     xzr, [x0], #8
    b       .Ldirect_bss
.Ldirect_bss_done:

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

    // Transition to high virtual address — land *after* the BSS-zero loop so
    // we don't wipe the page tables we just installed.
    ldr     x4, =.Lsetup_stack
    br      x4

.Llimine_entry:
    // ── Zero BSS (Limine path only) ──────────────────────────────────────────
    // Limine supplies its own page tables, so clearing all of .bss here (which
    // includes early_pgtables) is safe.
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

.Lsetup_stack:
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
