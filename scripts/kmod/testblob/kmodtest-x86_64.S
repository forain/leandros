# Test module (x86_64). MODNAME and the build-id file (BUILDID_BIN) come from testblob.sh.
.intel_syntax noprefix
.section .kmod.info,"a"
.balign 8
hdr:
    .ascii "LKMOD\0\0\1"
    .long 1
    .long 128
    .long 62
    .long 0
    .incbin BUILDID_BIN
    .long 20
    .quad TEST_SLOT
    .quad 0x40000
    .quad kmod_init
    .quad 0
    .quad 0
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
    push rbx
    mov rbx, rdi
    mov edi, 1
    lea rsi, [rip + msg]
    mov edx, msglen
    call kapi_log
    mov rdi, rbx
    call kapi_ktest_ping
    pop rbx
    test eax, eax
    jns 1f
    ret
1:  xor eax, eax
    ret
