ENTRY(_start_pvh)
OUTPUT_FORMAT(elf64-x86-64)

/* The kernel body runs in the higher half (like the Limine build) so its
 * mappings live in PML4[256..512] and are shared into every process page
 * table.  The PVH/multiboot entry, however, must be a low 32-bit physical
 * address, so the trampoline (.text.pvh + boot headers) is linked and loaded
 * low.  The trampoline sets up paging and jumps to the high _start_common. */
KERNEL_OFFSET = 0xffffffff80000000;

PHDRS
{
    boot PT_LOAD   FLAGS(7);  /* low 32-bit trampoline + headers */
    note PT_NOTE   FLAGS(0);
    high PT_LOAD   FLAGS(7);  /* higher-half kernel body */
}

SECTIONS {
    /* ── Low trampoline (physical == virtual) ─────────────────────────────── */
    /* 32 MiB, not 1 MiB: OVMF keeps ACPI NVS / firmware ranges at
     * 0x800000-0x1780000 (and the HP EliteBook a hole at 0xC00000-0x1022000),
     * which a kexec destination must not touch. The image (~26 MiB incl. BSS)
     * ends far below the direct initrd at 0x10000000 and inside the
     * trampoline's 0-2 GiB map. main.rs derives the base from _start_pvh. */
    . = 0x2000000;

    .boot : {
        KEEP(*(.header))      /* multiboot1 / multiboot2 headers */
        KEEP(*(.text.pvh))    /* _start_pvh + .Ltarget (32->64 trampoline) */
    } :boot

    .note.pvh : {
        KEEP(*(.note.pvh))
    } :note

    . = ALIGN(4K);
    __boot_phys_end = .;

    /* ── Higher-half kernel body ──────────────────────────────────────────── */
    . = KERNEL_OFFSET + __boot_phys_end;

    .text : AT(__boot_phys_end) {
        *(.text.boot)
        *(.text .text.*)
    } :high

    . = ALIGN(4K);
    .rodata : AT(ADDR(.rodata) - KERNEL_OFFSET) {
        *(.rodata .rodata.*)
    } :high

    . = ALIGN(8);
    .note.gnu.build-id : AT(ADDR(.note.gnu.build-id) - KERNEL_OFFSET) {
        __build_id_start = .;
        KEEP(*(.note.gnu.build-id))
        __build_id_end = .;
    } :high

    . = ALIGN(8);
    .ksymtab : AT(ADDR(.ksymtab) - KERNEL_OFFSET) {
        __ksymtab_start = .;
        KEEP(*(.ksymtab))
        __ksymtab_end = .;
    } :high

    . = ALIGN(4K);
    .data : AT(ADDR(.data) - KERNEL_OFFSET) {
        *(.data .data.*)
        . = ALIGN(8);
        KEEP(*(.limine_reqs_start))
        KEEP(*(.limine_reqs))
        KEEP(*(.limine_reqs_end))
    } :high

    .bss : AT(ADDR(.bss) - KERNEL_OFFSET) {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    } :high

    __kernel_end = .;

    /DISCARD/ : {
        *(.comment*)
        *(.eh_frame*)
        *(.gcc_except_table*)
        *(.rel.eh_frame*)
        *(.note.gnu*)
    }

    ASSERT(__kernel_end - KERNEL_OFFSET < 0x10000000, "x86 direct image must stay below 256 MiB (kmodwin unshare)")
}
