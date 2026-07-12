// AArch64 bare-metal entry point supporting Limine — kernel/src/entry_aarch64.s

.section ".text.boot", "ax", @progbits
.globl _start
_start:
    // ── Park non-primary cores ────────────────────────────────────────────────
    // Unlike QEMU `-machine virt` (only CPU 0 ever executes _start; APs stay
    // PSCI-powered-off until arch/aarch64/src/smp.rs explicitly calls
    // cpu_on() much later), QEMU `-M raspi4b` releases all 4 cores
    // simultaneously at reset with identical initial PC — confirmed via QMP
    // register dumps showing multiple cores concurrently executing this
    // file with garbage register state (a translation fault on a wild
    // pointer, varying between runs — the signature of unsynchronized
    // concurrent BSS-zero/page-table-build races, not a fixed bad address).
    // Every step below assumes single-threaded execution by one core.
    //
    // Gated on MPIDR_EL1.Aff0 (bits [7:0], the same field
    // arch/aarch64/src/smp.rs's arch_cpu_id()/record_own_mpidr() already use
    // as the logical CPU index) — confirmed via QEMU's GDB stub reading each
    // vCPU's VMPIDR_EL2 as 0/1/2/3 respectively before any of our code runs.
    // An earlier version of this gate used a first-core-wins LDAXR/STLXR
    // election instead, reasoning that QEMU's raspi4b MPIDR encoding was
    // unconfirmed; that turned out to livelock all 4 vCPUs under QEMU TCG
    // (confirmed via GDB stub single-stepping: the same LDAXR/STLXR sequence
    // completes correctly for one core stepped in isolation but never
    // resolves under real concurrent execution, with or without
    // `-accel tcg,thread=multi`) — a QEMU TCG exclusive-monitor quirk under
    // 4-way contention, not a logic bug. MPIDR reads are purely local to
    // each core (no shared memory, no contention possible even in
    // principle), so this is the more robust choice now that the encoding
    // is confirmed.
    //
    // Real RPi5 firmware may already gate this like `virt` does —
    // unconfirmed without hardware — so this is unconditional and harmless
    // wherever only one core ever reaches here. SMP bring-up is out of scope
    // for the raspi4b QEMU test target (a stepping stone for the sdhci
    // driver, not a hardware target) — parked cores here are never released.
    mrs     x4, mpidr_el1
    and     x4, x4, #0xFF
    cbz     x4, _start_primary

park_secondary_core:
    wfe
    b       park_secondary_core

_start_primary:
    // ── PRESERVE ARGUMENTS IMMEDIATELY ──
    // x0 holds the DTB pointer on direct (-kernel) boot; preserve across the
    // EL2->EL1 drop below (eret does not clobber GPRs).
    mov     x19, x0
    mov     x20, x1

    // ── Drop to EL2 if entered at EL3 ────────────────────────────────────────
    // QEMU `-M raspi4b` resets the boot CPU into EL3 (confirmed via QMP
    // `info registers` at a halted boot: PC == _start, PSTATE decodes to
    // EL3h) rather than EL2 like `-machine virt` does (no EL3 implemented
    // there, secure=off). Real RPi5 firmware may already drop to EL2 before
    // jumping here like `virt` does — unconfirmed without hardware — so
    // this is a one-time, unconditional drop straight into the existing,
    // already-tested EL2->EL1 logic below, not a parallel code path: it is
    // a no-op everywhere CurrentEL is never observed to be 3.
    mrs     x4, CurrentEL
    lsr     x4, x4, #2
    and     x4, x4, #3
    cmp     x4, #3
    b.ne    check_el2                // not EL3 → fall through to the EL2 check

    mov     x5, #(1 << 0)            // SCR_EL3.NS: EL2/EL1/EL0 are Non-secure
    orr     x5, x5, #(1 << 8)        // SCR_EL3.HCE: HVC enabled at lower ELs
    orr     x5, x5, #(1 << 10)       // SCR_EL3.RW: EL2 executes AArch64
    msr     scr_el3, x5

    mov     x5, #0x3c9               // SPSR_EL3: return to EL2h, DAIF masked
    msr     spsr_el3, x5
    adr     x5, check_el2            // physical address (MMU off) of the EL2 check below
    msr     elr_el3, x5
    isb
    eret

    // ── Drop to EL1 if entered at EL2 ────────────────────────────────────────
    // QEMU `-kernel` on `virt` with `-cpu max` (EL2 implemented, secure=off)
    // enters at EL2.  The rest of this entry — and the kernel — only programs
    // EL1 system registers, so we must hand off to EL1 first.  Limine already
    // enters at EL1, so CurrentEL gates this and leaves that path untouched.
check_el2:
    mrs     x4, CurrentEL
    lsr     x4, x4, #2
    and     x4, x4, #3
    cmp     x4, #2
    b.ne    at_el1                  // not EL2 → already where we want to be

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

    // Check if we are running at a virtual address (MMU ON)
    adr     x4, at_el1
    tbz     x4, #63, el2_mmu_off    // if bit 63 is 0, MMU is already off

    // MMU is ON: Translate virtual at_el1 to physical address using KERNEL_ADDR_REQUEST
    adrp    x0, KERNEL_ADDR_REQUEST
    add     x0, x0, :lo12:KERNEL_ADDR_REQUEST
    ldr     x1, [x0, #40]             // Response pointer
    cbz     x1, translation_failed
    ldr     x2, [x1, #8]              // physical_base
    ldr     x3, [x1, #16]             // virtual_base
    sub     x4, x4, x3                // at_el1 offset from kernel base
    add     x4, x4, x2                // physical address of at_el1
    b       el2_mmu_off

translation_failed:
    b       translation_failed

el2_mmu_off:
    // Configure SCTLR_EL1 with MMU OFF
    movz    x5, #0x0800             // SCTLR_EL1 reset: RES1 bits set, MMU off
    movk    x5, #0x30d0, lsl #16
    msr     sctlr_el1, x5

    mov     x5, #0x3c5              // SPSR_EL2: return to EL1h, DAIF masked
    msr     spsr_el2, x5
    msr     elr_el2, x4             // x4 contains physical address of at_el1
    isb
    eret

at_el1:
    // Force SP_EL1
    msr     SPSel, #1
    isb
    
    // Check if MMU is on
    mrs     x4, sctlr_el1
    tst     x4, #1
    b.ne    limine_entry

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
direct_bss:
    cmp     x0, x1
    b.ge    direct_bss_done
    str     xzr, [x0], #8
    b       direct_bss
direct_bss_done:

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
clear_pgt:
    str     xzr, [x6], #8
    subs    x5, x5, #8
    b.ne    clear_pgt

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
    ldr     x4, =setup_stack
    br      x4

limine_entry:
    // FP/SIMD on before any Rust code runs.
    mrs     x2, cpacr_el1
    orr     x2, x2, #(3 << 20)
    msr     cpacr_el1, x2
    isb


    // ── Zero BSS (Limine path only) ──────────────────────────────────────────
    // Limine supplies its own page tables, so clearing all of .bss here (which
    // includes early_pgtables) is safe.
    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
bss_loop:
    cmp     x0, x1
    b.ge    bss_done
    str     xzr, [x0], #8
    b       bss_loop
bss_done:

setup_stack:
    // Set up initial stack
    adrp    x1, EARLY_STACK
    add     x1, x1, :lo12:EARLY_STACK
    mov     x2, #0x10000
    add     x1, x1, x2
    mov     sp, x1

    // Call kernel_main
    mov     x0, x19
    ldr     x1, kernel_main_val
    blr     x1

.align 3
kernel_main_val:
    .quad kernel_main

halt:
    wfe
    b       halt
