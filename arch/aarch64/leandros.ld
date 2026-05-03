/* AArch64 linker script for QEMU -machine virt
 *
 * QEMU virt DRAM starts at 0x40000000.  We link the kernel at 0xffffffff40080000
 * (higher-half mapping) which Limine maps to physical 0x40080000.
 * The kernel entry (_start) must be the very first byte executed.
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

KERNEL_OFFSET = 0xFFFFFFFF80000000;

SECTIONS
{
    . = KERNEL_OFFSET;

    . += SIZEOF_HEADERS;

    /* Force the zero page to be part of a segment by creating a
     * dummy section in the zero page.
     * Limine will map the segment with the lowest vaddr value at
     * higher half even if the segment has a higher vaddr.
     * As such without the zero page being part of a segment, the
     * kernel would be loaded at an offset from the expected
     * location. As the redox kernel is not currently relocatable,
     * this would result in a crash.
     */
    .dummy ALIGN(8) : AT(ADDR(.dummy) - KERNEL_OFFSET) {}

    . = ALIGN(4096);

    /* ── Boot stub — must execute before any other .text ─────────────────── */
    .text.boot : AT(ADDR(.text.boot) - KERNEL_OFFSET) {
        KEEP(*(.text.boot))
    }

    /* ── Normal code ─────────────────────────────────────────────────────── */
    .text : AT(ADDR(.text) - KERNEL_OFFSET) {
        *(.text .text.*)
    }

    /* ── Initialised read-write data ─────────────────────────────────────── */
    .data ALIGN(4096) : AT(ADDR(.data) - KERNEL_OFFSET) {
        *(.data .data.*)
        /* ── Limine request structures ────────────────────────────────────────── */
        . = ALIGN(8);
        KEEP(*(.limine_reqs_start))
        KEEP(*(.limine_reqs))
        KEEP(*(.limine_reqs_end))

        __data_end = .;
    }

    .bss ALIGN(4096) : AT(ADDR(.bss) - KERNEL_OFFSET) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        /* Kernel stack: 1 MiB, 16-byte aligned (increased for large Task struct operations) */
        . = ALIGN(16);
        . += 0x100000;
        __stack_top = .;
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