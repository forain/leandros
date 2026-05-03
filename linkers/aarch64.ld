ENTRY(_start)
OUTPUT_FORMAT("elf64-littleaarch64", "elf64-littleaarch64", "elf64-littleaarch64")

PHDRS
{
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    . = 0xffffffff80000000;

    .text : {
        __text_start = .;
        KEEP(*(.text.boot))
        *(.text .text.*)
        __text_end = .;
    } :text

    . = ALIGN(4K);

    .rodata : {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.gcc_except_table*)
        __rodata_end = .;
    } :rodata

    . = ALIGN(4K);

    .data : {
        __data_start = .;
        *(.data .data.*)
        *(.got .got.*)
        *(.data.rel.ro .data.rel.ro.*)
        *(.tdata .tdata.*)

        /* ── Limine request structures ────────────────────────────────────────── */
        . = ALIGN(8);
        KEEP(*(.limine_reqs_start))
        KEEP(*(.limine_reqs))
        KEEP(*(.limine_reqs_end))

        __data_end = .;
    } :data

    .bss : {
        . = ALIGN(8);
        PROVIDE(__bss_start = .);
        *(.bss .bss.*)
        *(.tbss .tbss.*)
        *(COMMON)
        . = ALIGN(8);
        PROVIDE(__bss_end = .);
    } :data

    __kernel_end = .;

    /DISCARD/ : {
        *(.comment*)
        *(.eh_frame*)
        *(.note*)
    }
}
