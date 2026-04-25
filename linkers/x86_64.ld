ENTRY(_start)
OUTPUT_FORMAT(elf64-x86-64)

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R E */
    rodata PT_LOAD FLAGS(4); /* R   */
    data   PT_LOAD FLAGS(6); /* RW  */
}

SECTIONS
{
    . = 0xffffffff80000000;

    .text : {
        KEEP(*(.limine_requests))
        __text_start = .;
        *(.text .text.*)
        __text_end = .;
    } :text

    . = ALIGN(0x1000);

    .rodata : {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.gcc_except_table*)
        __rodata_end = .;
    } :rodata

    . = ALIGN(0x1000);

    .data : {
        __data_start = .;
        *(.data .data.*)
        *(.got .got.*)
        __data_end = .;
    } :data

    .bss : {
        PROVIDE(__bss_start = .);
        *(.bss .bss.*)
        *(.tbss .tbss.*)
        *(COMMON)
        PROVIDE(__bss_end = .);
    } :data

    /DISCARD/ : {
        *(.comment*)
        *(.eh_frame*)
        *(.note*)
        *(.multiboot2_header*)
    }
}
