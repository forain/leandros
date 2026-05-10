ENTRY(_start_pvh)
OUTPUT_FORMAT(elf64-x86-64)

PHDRS
{
    note PT_NOTE FLAGS(0);
    kernel PT_LOAD FLAGS(7);
}

SECTIONS {
    . = 0x100000;

    .note.pvh : {
        KEEP(*(.note.pvh))
    } :note

    .text : {
        __text_start = .;
        KEEP(*(.header))
        KEEP(*(.text.boot))
        *(.text*)
        . = ALIGN(4K);
        __text_end = .;
    } :kernel

    .rodata ALIGN(4K) : {
        __rodata_start = .;
        *(.rodata*)
        . = ALIGN(4K);
        __rodata_end = .;
    } :kernel

    .data ALIGN(4K) : {
        __data_start = .;
        *(.data*)
        . = ALIGN(8);
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
        . = ALIGN(4K);
        __bss_start = .;
        *(.bss*)
        . = ALIGN(8);
        __bss_end = .;
    } :kernel

    __end = .;
    __kernel_end = .;

    /DISCARD/ : {
        *(.comment*)
        *(.eh_frame*)
        *(.gcc_except_table*)
        *(.rel.eh_frame*)
    }
}
