/* AArch64 linker script for Raspberry Pi 5
 *
 * The RPi5 firmware (config.txt: kernel=leandros.elf) reads ELF PT_LOAD
 * segments and loads the kernel at the address specified by the ELF header.
 * The standard RPi load address is 0x80000; firmware maps the first 2 GiB
 * of DRAM starting at physical 0x0.
 *
 * Symbol contract with assembly / Rust:
 *   _start            — entry point (in .text.boot)
 *   __bss_start       — start of zero-initialised data
 *   __bss_end         — end of BSS (8-byte aligned)
 *   __stack_top       — initial SP (grows downward)
 *   __kernel_end      — first byte past the kernel image
 *   __exception_vectors — AArch64 vector table (arch/aarch64/src/exception.rs)
 */

ENTRY(_start)

KERNEL_BASE = 0x80000;    /* RPi5: standard ELF load address */

SECTIONS
{
    . = KERNEL_BASE;

    /* ── Boot stub — must execute before any other .text ─────────────────── */
    .text.boot : {
        KEEP(*(.text.boot))
    }

    /* ── Normal code ─────────────────────────────────────────────────────── */
    .text : ALIGN(4096) {
        *(.text .text.*)
    }

    /* ── Read-only data ───────────────────────────────────────────────────── */
    .rodata : ALIGN(4096) {
        *(.rodata .rodata.*)
    }

    /* ── Initialised read-write data ─────────────────────────────────────── */
    .data : ALIGN(4096) {
        *(.data .data.*)
    }

    /* ── 64 KiB kernel stack (in BSS so it does not bloat the ELF) ───────── */
    .stack : ALIGN(16) {
        __stack_bottom = .;
        . += 0x10000;           /* 64 KiB */
        __stack_top = .;
    }

    /* ── Zero-initialised data ────────────────────────────────────────────── */
    .bss : ALIGN(4096) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    }

    . = ALIGN(4096);
    __kernel_end = .;

    /* Discard DWARF frame unwinding tables — we don't unwind in the kernel */
    /DISCARD/ : {
        *(.eh_frame .eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
        *(.ARM.exidx*)
    }
}
