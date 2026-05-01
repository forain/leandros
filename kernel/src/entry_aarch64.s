// AArch64 bare-metal entry point — kernel/src/entry_aarch64.s
//
// Supports two boot environments:
//   QEMU -machine virt : enters at EL1 directly.
//   Raspberry Pi 5     : firmware (VideoCore / TF-A) enters at EL2.
//
// On Limine:
//   Enters at EL1 with MMU enabled and kernel mapped at 0xffffffff80000000.

.section ".text.boot", "ax", @progbits
.globl _start
_start:
    // Debug: _start reached - write 'B' IMMEDIATELY
    mov     x20, 0x09000000         // QEMU virt UART base
    mov     w21, #'B'
    str     w21, [x20]

    // ── Enable FP / SIMD (NEON) ──────────────────────────────────────────────
    // CPACR_EL1.FPEN (bits 20-21) = 0b11: do not trap FP/SIMD instructions.
    mov     x1, #(3 << 20)
    msr     cpacr_el1, x1
    isb

    // ── Zero the BSS section ──────────────────────────────────────────────────
    // Debug: Starting BSS clear - write 'F'
    mov     w21, #'F'
    str     w21, [x20]

    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
    
    cmp     x0, x1
    b.ge    .Lbss_done
    
.Lbss_loop:
    strb    wzr, [x0], #1
    cmp     x0, x1
    b.lt    .Lbss_loop

.Lbss_done:
    dsb     sy
    isb

    // Debug: BSS cleared - write 'G'
    mov     w21, #'G'
    str     w21, [x20]

    // ── Set up initial stack (SP_EL1) ─────────────────────────────────────────
    adrp    x1, EARLY_STACK
    add     x1, x1, :lo12:EARLY_STACK
    mov     x2, #0x10000            // 64 KiB
    add     x1, x1, x2
    mov     sp, x1

    // Debug: Stack set up - write 'E'
    mov     w21, #'E'
    str     w21, [x20]

    // ── Call kernel_main(dtb_ptr: usize) ─────────────────────────────────────
    // Debug: Write 'A' after basic setup before kernel_main
    mov     w1, #'A'
    str     w1, [x20]

    // Preserve x0 (boot info addr) just in case
    mov     x19, x0
    
    // Call into Rust using PC-relative address
    adrp    x1, kernel_main
    add     x1, x1, :lo12:kernel_main
    
    // Debug: Write 'J' to indicate we are about to jump to the address in x1
    mov     w2, #'J'
    str     w2, [x20]
    
    // Restore x0 before jump
    mov     x0, x19
    blr     x1

.Lhalt:
    wfe
    b       .Lhalt

// ── Exception Vectors ────────────────────────────────────────────────────────
// Minimal table to catch early boot faults.

.section ".text", "ax", @progbits
.align 11                           // 2^11 = 2048 = 2 KiB alignment
.Llocal_exception_vectors:

// Current EL, SP0
.align 7; b .Lexc_halt_sync        // Synchronous
.align 7; b .Lexc_halt_irq         // IRQ
.align 7; b .Lexc_halt_fiq         // FIQ
.align 7; b .Lexc_halt_serror      // SError

// Current EL, SPx
.align 7; b .Lexc_halt_sync        // Synchronous
.align 7; b .Lexc_halt_irq         // IRQ
.align 7; b .Lexc_halt_fiq         // FIQ
.align 7; b .Lexc_halt_serror      // SError

// Lower EL, AArch64
.align 7; b .Lexc_halt_sync        // Synchronous
.align 7; b .Lexc_halt_irq         // IRQ
.align 7; b .Lexc_halt_fiq         // FIQ
.align 7; b .Lexc_halt_serror      // SError

// Lower EL, AArch32
.align 7; b .Lexc_halt_sync        // Synchronous
.align 7; b .Lexc_halt_irq         // IRQ
.align 7; b .Lexc_halt_fiq         // FIQ
.align 7; b .Lexc_halt_serror      // SError

.Lexc_halt_sync:
    mov     x0, 0x09000000
    mov     w1, #'S'
    str     w1, [x0]
    b .Lexc_park

.Lexc_halt_irq:
    mov     x0, 0x09000000
    mov     w1, #'I'
    str     w1, [x0]
    b .Lexc_park

.Lexc_halt_fiq:
    mov     x0, 0x09000000
    mov     w1, #'F'
    str     w1, [x0]
    b .Lexc_park

.Lexc_halt_serror:
    mov     x0, 0x09000000
    mov     w1, #'R'
    str     w1, [x0]
    b .Lexc_park

.Lexc_park:
    wfe
    b       .Lexc_park
