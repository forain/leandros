ENTRY(_start)
OUTPUT_FORMAT("elf64-littleaarch64", "elf64-littleaarch64", "elf64-littleaarch64")

KERNEL_VIRT = 0xffff800000000000;
KERNEL_PHYS = 0x40080000;

SECTIONS {
    . = KERNEL_VIRT + KERNEL_PHYS;

    .text : AT(KERNEL_PHYS) {
        __text_start = .;
        /* The entry stub must be the first thing in the image.
         *
         * QEMU's -kernel loads this as an ELF and branches to e_entry, so the
         * position of _start inside .text never mattered there. The Raspberry
         * Pi firmware does not parse ELF: it copies the flat binary produced by
         * objcopy to `kernel_address` and branches to its first byte. With
         * .text.boot left to fall out of the generic *(.text*) glob below,
         * _start landed 0xbf8 bytes in and the firmware jumped into
         * __exception_vectors instead — a silent hang with no output at all,
         * since nothing had configured VBAR_EL1 or the stack yet.
         *
         * KEEP so --gc-sections cannot drop it: nothing references _start. */
        KEEP(*(.text.boot))
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
        . = ALIGN(8);
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
