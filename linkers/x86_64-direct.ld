ENTRY(_start)
OUTPUT_FORMAT(elf64-x86-64)

SECTIONS {
    . = 0x100000;  /* Physical address 1MB for direct boot */

    .multiboot2_header ALIGN(8) : {
        *(.multiboot2_header*)
    }

    .text ALIGN(4K) : {
        __text_start = .;
        *(.text*)
    }

    .rodata ALIGN(4K) : {
        __text_end = .;
        __rodata_start = .;
        *(.rodata*)
        __altcode_start = .;
        KEEP(*(.altcode*))
        __altcode_end = .;
        . = ALIGN(8);
        __altrelocs_start = .;
        KEEP(*(.altrelocs*))
        __altrelocs_end = .;
        __altfeatures_start = .;
        KEEP(*(.altfeatures*))
        __altfeatures_end = .;
    }

    .data ALIGN(4K) : {
        __rodata_end = .;
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
    }

    __end = .;
    __kernel_end = .;

    /DISCARD/ : {
        *(.comment*)
        *(.eh_frame*)
        *(.gcc_except_table*)
        *(.note*)
        *(.rel.eh_frame*)
    }
}