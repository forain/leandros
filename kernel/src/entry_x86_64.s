// x86-64 Dual-boot entry point — kernel/src/entry_x86_64.s
//
// Supports both Limine and Multiboot2/QEMU direct kernel loading:
//
// Limine mode:
//   • CPU is in 64-bit long mode, CPL 0.
//   • Interrupts disabled (RFLAGS.IF = 0).
//   • A flat GDT is loaded: null @ 0x00, 64-bit code @ 0x08, data @ 0x10.
//   • Paging is active; identity map + HHDM are set up by Limine.
//   • RSP is NOT guaranteed — we set it up below.
//   • Boot information is NOT in registers; use boot::limine request structs.
//
// Multiboot2 mode (QEMU -kernel):
//   • CPU is in 32-bit protected mode or 64-bit long mode.
//   • EAX contains magic number 0x36D76289.
//   • EBX contains physical address of multiboot2 info structure.
//   • We transition to 64-bit mode if needed and parse multiboot2 info.
//
// We must:
//   1. Set up a 64-bit stack.
//   2. Zero the BSS.
//   3. Call kernel_main(boot_info_addr)  [0 = Limine, non-zero = Multiboot2].
//
// Ref: Limine Boot Protocol §entry-point, Multiboot2 Specification

// ── Multiboot2 Header ────────────────────────────────────────────────────────

    .section .multiboot2_header, "a"
multiboot2_header_start:
    .long 0xE85250D6                    // Magic number
    .long 0                             // Architecture (i386)
    .long multiboot2_header_end - multiboot2_header_start  // Header length
    .long -(0xE85250D6 + 0 + (multiboot2_header_end - multiboot2_header_start))  // Checksum

    // Information request tag
    .short 1                            // Type: information request
    .short 0                            // Flags
    .long 20                            // Size
    .long 4                             // Basic memory info
    .long 6                             // Memory map

    // Entry address tag (optional - use ELF entry point)
    .short 3                            // Type: entry address
    .short 0                            // Flags
    .long 12                            // Size
    .long _start                        // Entry point address

    // End tag
    .short 0                            // Type: end
    .short 0                            // Flags
    .long 8                             // Size
multiboot2_header_end:

// ── PVH ELF Note removed due to linker issues with high addresses ──────────

// ── Code Section ─────────────────────────────────────────────────────────────

    .section .text
    .code64
    .globl _start
    _start:





    cli

    // ── Detect boot protocol ──────────────────────────────────────────────────
    // Multiboot2: EAX = 0x36D76289, EBX = info struct physical address
    // Limine:     EAX = anything else, EBX = don't care
    mov     r15, rbx            // Preserve multiboot2 info address in r15
    cmp     eax, 0x36D76289
    jne     .Llimine_boot

    // Multiboot2 detected - save info address for kernel_main
    mov     r14, rbx            // r14 = multiboot2 info address
    jmp     .Lcommon_setup

.Llimine_boot:
    // Limine detected - use 0 to signal Limine mode
    xor     r14, r14            // r14 = 0 (Limine mode)

.Lcommon_setup:
    // ── 64-bit stack ──────────────────────────────────────────────────────────
    // Use the stack defined in Rust
    lea     rsp, [rip + EARLY_STACK + 0x10000]

    // Debug: Write 'X' to COM1 to show we reached assembly entry
    mov     al, 'X'
    mov     dx, 0x3F8
    out     dx, al

    // ── Zero BSS ──────────────────────────────────────────────────────────────
    lea     rdi, [rip + __bss_start]
    lea     rcx, [rip + __bss_end]
    sub     rcx, rdi
    xor     rax, rax
    rep stosb

    // Debug: Write 'Y' after stack and BSS setup
    mov     al, 'Y'
    mov     dx, 0x3F8
    out     dx, al

    // ── Call kernel_main(boot_info_addr) ──────────────────────────────────────
    mov     rdi, r14
    call    kernel_main

.Lhalt:
    hlt
    jmp     .Lhalt
