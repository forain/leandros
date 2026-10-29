// Test module (aarch64). MODNAME and the build-id file (BUILDID_BIN) come from testblob.sh.
.section .kmod.info,"a"
.balign 8
hdr:
    .ascii "LKMOD\0\0\1"
    .long 1                   // abi_version
    .long 128                 // header_size
    .long 183                 // machine
    .long 0                   // flags
    .incbin BUILDID_BIN       // build_id[20]
    .long 20
    .quad TEST_SLOT
    .quad 0x40000
    .quad kmod_init
    .quad 0                   // exit
    .quad 0                   // ids
    .long 0, 0
name0:
    .ascii MODNAME
    .fill 32 - (. - name0), 1, 0

.section .rodata,"a"
msg: .ascii "hello"
.set msglen, . - msg

.text
.globl kmod_init
kmod_init:
    stp x29, x30, [sp, #-32]!
    mov x29, sp
    str x19, [sp, #16]
    mov x19, x0
    mov w0, #1
    adrp x1, msg
    add x1, x1, :lo12:msg
    mov x2, #msglen
    bl kapi_log               // far call: needs lld's absolute long thunk
    mov x0, x19
    bl kapi_ktest_ping
    ldr x19, [sp, #16]
    ldp x29, x30, [sp], #32
    cmp w0, #0
    csel w0, wzr, w0, ge      // ping >= 0 -> 0, else propagate the error
    ret
