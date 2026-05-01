ENTRY(kstart)
OUTPUT_FORMAT(elf64-x86-64)

KERNEL_OFFSET = 0xFFFFFFFF80000000;

SECTIONS {
    . = KERNEL_OFFSET;

    . += SIZEOF_HEADERS;

    /* Force the zero page to be part of a segment by creating a
     * dummy section in the zero page.
     * Limine will map the segment with the lowest vaddr value at
     * 0xFFFFFFFF80000000 even if the segment has a higher vaddr.
     * As such without the zero page being part of a segment, the
     * kernel would be loaded at an offset from the expected
     * location. As the redox kernel is not currently relocatable,
     * this would result in a crash. A similar issue likely exists
     * with multiboot/multiboot2 and the paddr of the segment.
     */
    .dummy : AT(ADDR(.dummy) - KERNEL_OFFSET) {}

    .text ALIGN(4K) : AT(ADDR(.text) - KERNEL_OFFSET) {
        __text_start = .;
        *(.text*)
    }

    .data ALIGN(4K) : AT(ADDR(.data) - KERNEL_OFFSET) {
        *(.data*)
        /* ── Limine request structures ────────────────────────────────────────── */
        . = ALIGN(8);
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
        . = ALIGN(4K);
        __bss_start = .;
        *(.bss*)
        *(COMMON)
        /* Kernel stack: 1 MiB, 16-byte aligned (increased for large Task struct operations) */
        . = ALIGN(16);
        . += 0x100000;
        __stack_top = .;
        . = ALIGN(8);
        __bss_end = .;
    }

    __end = .;

    /DISCARD/ : {
        *(.comment*)
        *(.eh_frame*)
        *(.gcc_except_table*)
        *(.note*)
        *(.rel.eh_frame*)
    }
}