ENTRY(_start)
OUTPUT_FORMAT("elf64-littleaarch64", "elf64-littleaarch64", "elf64-littleaarch64")

PHDRS
{
    kernel PT_LOAD FLAGS(7); /* R W E */
}

KERNEL_OFFSET = 0xffffffff80000000;
PHYS_OFFSET   = 0x40080000;

SECTIONS
{
    . = KERNEL_OFFSET + PHYS_OFFSET;

    .text : AT(ADDR(.text) - KERNEL_OFFSET) {
        __text_start = .;
        KEEP(*(.text.boot))
        *(.text .text.*)
        __text_end = .;
    } :kernel

    . = ALIGN(4K);

    .rodata : AT(ADDR(.rodata) - KERNEL_OFFSET) {
        __rodata_start = .;
        *(.rodata .rodata.*)
        __rodata_end = .;
    } :kernel

    . = ALIGN(4K);

    /* ── Limine Revision 6 Requests ───────────────────────────────────────── */
    .limine_reqs : AT(ADDR(.limine_reqs) - KERNEL_OFFSET) {
        . = ALIGN(8);
        KEEP(*(.limine_reqs_start))
        KEEP(*(.limine_reqs))
        KEEP(*(.limine_reqs_end))
    } :kernel

    . = ALIGN(4K);

    .data : AT(ADDR(.data) - KERNEL_OFFSET) {
        __data_start = .;
        *(.data .data.*)
        *(.got .got.*)
        __data_end = .;
    } :kernel

    .bss : AT(ADDR(.bss) - KERNEL_OFFSET) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    } :kernel

    /DISCARD/ : {
        *(.comment*)
        *(.eh_frame*)
    }
}
