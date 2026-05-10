ENTRY(_start)
OUTPUT_FORMAT(elf64-x86-64)

KERNEL_OFFSET = 0xffffffff80000000;
PHYS_OFFSET   = 0x100000; /* 1MB */

PHDRS
{
    kernel PT_LOAD FLAGS(7); /* R W E */
}

SECTIONS
{
    . = KERNEL_OFFSET + PHYS_OFFSET;

    .text : AT(ADDR(.text) - KERNEL_OFFSET) {
        *(.header)
        *(.text.boot)
        *(.text.pvh)
        *(.text .text.*)
    } :kernel

    .rodata : AT(ADDR(.rodata) - KERNEL_OFFSET) {
        *(.rodata .rodata.*)
    } :kernel

    .data : AT(ADDR(.data) - KERNEL_OFFSET) {
        *(.data .data.*)
        . = ALIGN(8);
        KEEP(*(.limine_reqs_start))
        KEEP(*(.limine_reqs))
        KEEP(*(.limine_reqs_end))
    } :kernel

    .bss : AT(ADDR(.bss) - KERNEL_OFFSET) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        __bss_end = .;
    } :kernel

    /DISCARD/ : {
        *(.note.pvh)
        *(.comment*)
        *(.eh_frame*)
        *(.note*)
    }
}
