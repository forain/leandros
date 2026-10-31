/* Module link script. Inputs: --defsym=SLOT_BASE, --defsym=SLOT_SIZE.
   Three PT_LOADs (R: header+rodata, RX: text, RW: data+bss), page aligned.
   .got/.plt/.init_array/TLS are NOT discarded: any content fails the link (ASSERT). */
ENTRY(kmod_init)
PHDRS {
    hdr  PT_LOAD FLAGS(4);
    text PT_LOAD FLAGS(5);
    data PT_LOAD FLAGS(6);
}
SECTIONS {
    . = SLOT_BASE;
    .kmod.info : { KEEP(*(.kmod.info)) } :hdr
    .kmod.ids  : { KEEP(*(.kmod.ids)) } :hdr
    .kmod.ofstr : { KEEP(*(.kmod.ofstr)) } :hdr
    .kmod.exit : { KEEP(*(.kmod.exit)) } :hdr
    .rodata : { *(.rodata .rodata.*) } :hdr
    . = ALIGN(4096);
    .text : { *(.text .text.*) } :text
    . = ALIGN(4096);
    .data : { *(.data .data.*) } :data
    .bss : { *(.bss .bss.*) *(COMMON) } :data

    .got      : { *(.got) *(.got.plt) *(.igot*) }
    .plt      : { *(.plt) *(.plt.*) *(.iplt) }
    .init_array : { *(.init_array*) *(.ctors*) }
    .fini_array : { *(.fini_array*) *(.dtors*) }
    .tdata    : { *(.tdata .tdata.*) }
    .tbss     : { *(.tbss .tbss.*) *(.tcommon) }
    ASSERT(SIZEOF(.got) == 0, "module needs a GOT (non-static relocation)")
    ASSERT(SIZEOF(.plt) == 0, "module needs a PLT")
    ASSERT(SIZEOF(.init_array) == 0, "module has .init_array/.ctors (static constructors)")
    ASSERT(SIZEOF(.fini_array) == 0, "module has .fini_array/.dtors")
    ASSERT(SIZEOF(.tdata) == 0 && SIZEOF(.tbss) == 0, "module uses thread-local storage")
    ASSERT(. <= SLOT_BASE + SLOT_SIZE, "module outgrew its slot reservation")
    /DISCARD/ : { *(.comment) *(.note*) *(.eh_frame*) *(.interp) *(.llvm_addrsig) *(.llvm.*) *(.ARM.attributes) *(.riscv.attributes) *(.symtab_shndx) }
}
