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

// Secondary CPUs: entered at a second copy of this blob in the AP park page
// (info.rs AP_PARK_OFF, the last page of the FDT block, which the next
// kernel's /memory leaves out), then MMU/caches off, VBAR 0, and PSCI
// CPU_OFF (x1 = 0 HVC, 1 SMC). Leaving the CPU in reset-like state matters
// under HVF (below): its CPU_ON resets nothing.
//
// PSCI CPU_OFF never returns, so the code after the call only runs on QEMU's
// HVF backend (11.1 included), whose CPU_OFF loses the next CPU_ON: the HVC
// exit syncs the vCPU into env (vcpu_dirty = true), and the exit's closing
// flush_cpu_state pushes it straight back and clears vcpu_dirty, so when a
// later CPU_ON's arm_set_cpu_on_async_work does cpu_reset + pc = entry +
// x0 = context in env, nothing pushes that to Hypervisor.framework. The vCPU
// wakes here, after the hvc, with x0 = 0 (CPU_OFF's "return"). It only
// wakes once CPU_ON was accepted for it, so the next kernel writes
// (MPIDR | 1 << 63, entry, context) into the mailbox at +AP_MAILBOX_OFF of
// this page (`/chosen/leandros,kexec-ap-mailbox`) and cleans it to PoC
// before every CPU_ON; the CPU (MMU off: uncached reads) takes the record
// for its own MPIDR and branches there as CPU_ON would have. TCG, KVM and
// firmware never get here.
//
// Park mode (x2 = 1, the rpi5 default; exec_aarch64.rs): no CPU_OFF at all.
// The Pi 5's TF-A accepts CPU_ON for a CPU it powered off but never starts
// it again, so the CPU stays here, on as far as the firmware knows; the next
// kernel's CPU_ON returns ALREADY_ON and the mailbox below starts it. Real
// cores keep their caches here, so they are cleaned and invalidated by
// set/way first: no dirty line of the old kernel may land on the new image
// later, and no stale clean line may be hit once the next kernel turns this
// CPU's MMU on. Either way, a CPU stores 1 at +AP_ARRIVED_OFF + 8 * x3
// (x3 = its dense index) once it no longer touches old-kernel memory; the
// kexec'ing CPU waits for that before the purgatory overwrites anything.
    .balign 16
    .globl kexec_purgatory_off
kexec_purgatory_off:
    mov     x17, x1                     // 1 = SMC conduit
    mov     x15, x2                     // 1 = park, no CPU_OFF
    mov     x16, x3                     // dense CPU index
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
    cbz     x15, 20f
    // Clean + invalidate the data/unified cache levels up to LoC by set/way.
    mrs     x0, clidr_el1
    ubfx    x3, x0, #24, #3             // LoC
    lsl     x3, x3, #1
    mov     x1, #4                      // levels 1-2 only: the A76's own L1/L2
    cmp     x3, x1                      // (the DSU L3 is shared and coherent;
    csel    x3, x1, x3, hi              // set/way there would race cpu0)
    cbz     x3, 20f
    mov     x10, xzr                    // level << 1
10: add     x2, x10, x10, lsr #1        // 3 * level
    lsr     x1, x0, x2
    and     x1, x1, #7                  // cache type at this level
    cmp     x1, #2
    b.lt    15f                         // none or I-cache only
    msr     csselr_el1, x10
    isb
    mrs     x1, ccsidr_el1
    and     x2, x1, #7
    add     x2, x2, #4                  // log2(line bytes)
    ubfx    x4, x1, #3, #10             // ways - 1
    clz     w5, w4                      // way field shift
    ubfx    x7, x1, #13, #15            // sets - 1
11: mov     x9, x4
12: lsl     x6, x9, x5
    orr     x11, x10, x6
    lsl     x6, x7, x2
    orr     x11, x11, x6
    dc      cisw, x11
    subs    x9, x9, #1
    b.ge    12b
    subs    x7, x7, #1
    b.ge    11b
15: add     x10, x10, #2
    cmp     x3, x10
    b.gt    10b
    msr     csselr_el1, xzr
    dsb     sy
    isb
20: adr     x11, kexec_purgatory_start
    add     x11, x11, #0x900            // info.rs AP_ARRIVED_OFF
    mov     x12, #1
    str     x12, [x11, x16, lsl #3]     // arrived: MMU off, caches flushed
    dsb     sy
    cbnz    x15, 2f
    movz    x0, #0x0002                 // PSCI CPU_OFF
    movk    x0, #0x8400, lsl #16
    cbnz    x17, 1f
    hvc     #0
    b       2f
1:  .inst   0xd4000003                  // smc #0
2:  mrs     x10, mpidr_el1              // CPU_OFF returned (HVF) or park mode
    movz    x11, #0xffff                // Aff0..Aff2 | Aff3 << 32
    movk    x11, #0x00ff, lsl #16
    movk    x11, #0x00ff, lsl #32
    and     x10, x10, x11
    orr     x10, x10, #(1 << 63)
    adr     x11, kexec_purgatory_start
    add     x11, x11, #0x800            // info.rs AP_MAILBOX_OFF
3:  ldr     x12, [x11]
    cmp     x12, x10
    b.ne    4f
    ldr     x13, [x11, #8]
    ldr     x0, [x11, #16]
    mov     x1, xzr
    mov     x2, xzr
    mov     x3, xzr
    ic      iallu
    dsb     nsh
    isb
    br      x13
4:  wfe
    b       3b
kexec_purgatory_end:
