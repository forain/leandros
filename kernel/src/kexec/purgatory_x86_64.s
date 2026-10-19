// kexec purgatory (x86_64). Copied into a page below 4 GiB, outside every
// destination range, and entered at that page's physical address in 64-bit
// mode through an identity map (exec_x86_64.rs), IF = 0, on the BSP.
// Position independent; its stack is the tail of its own page.
//
//   rdi = first indirection page (phys)   rsi = entry (_start_pvh, < 4 GiB)
//   rdx = multiboot2 MBI (phys, < 1 MiB)
//
// 1. Own GDT (null, 0x08 flat 32-bit code, 0x10 flat data, 0x18 64-bit
//    code) and a null IDT: an NMI or fault from here on is a triple fault,
//    never a jump into memory being overwritten.
// 2. Walk the Linux-style list (DESTINATION 1, INDIRECTION 2, DONE 4,
//    SOURCE 8) and copy each source page to the cursor.
// 3. CR4.PCIDE/PGE off, far return to 32-bit compatibility mode, CR0.PG
//    off (leaves long mode), EFER.LME off, CR4 = PAE only, CR3 = 0.
// 4. Multiboot2 hand-off: EAX = 0x36d76289, EBX = MBI, flat segments,
//    paging off, jump to the entry.

    .section .text.kexec_purgatory_x86, "ax"
    .balign 16
    .globl kexec_purgatory_x86_start
    .globl kexec_purgatory_x86_end
    .code64
kexec_purgatory_x86_start:
    cld
    lea     rsp, [rip + kpx_stack_top]
    mov     r12, rsi                    // entry
    mov     r13, rdx                    // MBI

    lea     rax, [rip + kpx_gdt]
    mov     qword ptr [rip + kpx_gdtr + 2], rax
    lgdt    [rip + kpx_gdtr]
    lidt    [rip + kpx_idtr]
    mov     eax, 0x10
    mov     ds, ax
    mov     es, ax
    mov     ss, ax
    mov     fs, ax
    mov     gs, ax

    mov     r9, rdi                     // next list word
    xor     r8d, r8d                    // copy cursor
1:  mov     rax, qword ptr [r9]
    add     r9, 8
    mov     rbx, rax
    and     rbx, -4096
    test    al, 1
    jnz     2f
    test    al, 2
    jnz     3f
    test    al, 4
    jnz     5f
    test    al, 8
    jnz     4f
    jmp     1b
2:  mov     r8, rbx                     // DESTINATION
    jmp     1b
3:  mov     r9, rbx                     // INDIRECTION
    jmp     1b
4:  mov     rsi, rbx                    // SOURCE: one page
    mov     rdi, r8
    mov     ecx, 512
    rep movsq
    mov     r8, rdi
    jmp     1b

5:  mov     rax, cr4                    // DONE
    and     rax, -0x20081               // ~(PCIDE | PGE)
    mov     cr4, rax
    mov     ebx, r13d
    mov     ebp, r12d
    lea     rax, [rip + kpx_compat32]
    push    0x08
    push    rax
    .byte   0x48, 0xcb                  // retfq

    .code32
kpx_compat32:
    mov     eax, 0x10
    mov     ds, ax
    mov     es, ax
    mov     ss, ax
    mov     fs, ax
    mov     gs, ax
    mov     eax, cr0
    and     eax, 0x7fffffff             // PG off: long mode deactivated
    mov     cr0, eax
    jmp     6f
6:  mov     ecx, 0xC0000080             // EFER.LME off (NXE kept)
    rdmsr
    and     eax, 0xFFFFFEFF
    wrmsr
    mov     eax, 0x20                   // CR4 = PAE only
    mov     cr4, eax
    xor     eax, eax
    mov     cr3, eax
    xor     ecx, ecx
    xor     edx, edx
    xor     esi, esi
    xor     edi, edi
    mov     eax, 0x36d76289
    jmp     ebp

    .code64
    .balign 8
kpx_gdt:
    .quad   0
    .quad   0x00cf9a000000ffff
    .quad   0x00cf92000000ffff
    .quad   0x00af9a000000ffff
kpx_gdtr:
    .word   31
    .quad   0
kpx_idtr:
    .word   0
    .quad   0
    .balign 16
    .space  256
kpx_stack_top:
kexec_purgatory_x86_end:
