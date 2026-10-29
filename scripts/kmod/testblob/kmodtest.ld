/* Test module link: header page first, then separate R / RX / RW PT_LOADs. */
ENTRY(kmod_init)
PHDRS {
    hdr  PT_LOAD FLAGS(4);
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}
SECTIONS {
    . = TEST_SLOT;
    .kmod.info : { KEEP(*(.kmod.info)) } :hdr
    .rodata : { *(.rodata .rodata.*) } :hdr
    . = ALIGN(4096);
    .text : { *(.text .text.*) } :text
    . = ALIGN(4096);
    .data : { *(.data .data.*) *(.bss .bss.*) *(COMMON) } :data
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.got*) *(.plt*) *(.interp) }
}
