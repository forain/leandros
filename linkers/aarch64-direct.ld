ENTRY(_start)
OUTPUT_FORMAT("elf64-littleaarch64", "elf64-littleaarch64", "elf64-littleaarch64")

KERNEL_VIRT = 0xffff800000000000;
KERNEL_PHYS = 0x40080000;

SECTIONS {
    . = KERNEL_VIRT + KERNEL_PHYS;

    .text : AT(KERNEL_PHYS) {
        __text_start = .;
        KEEP(*(.boot))
        . = ALIGN(2048);
        __exception_vectors = .;
        *(.text*)
        . = ALIGN(4096);
        __text_end = .;
    }

    .rodata : AT(ADDR(.rodata) - KERNEL_VIRT) {
        __rodata_start = .;
        *(.rodata*)
        . = ALIGN(4096);
        __rodata_end = .;
    }

    .data : AT(ADDR(.data) - KERNEL_VIRT) {
        __data_start = .;
        *(.data*)
        . = ALIGN(8);
        KEEP(*(.limine_requests_start))
        KEEP(*(.limine_requests))
        KEEP(*(.limine_requests_end))
        . = ALIGN(4096);
        __data_end = .;
    }

    .bss : AT(ADDR(.bss) - KERNEL_VIRT) {
        __bss_start = .;
        *(.bss*)
        . = ALIGN(8);
        __bss_end = .;
        . = ALIGN(4096);
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
