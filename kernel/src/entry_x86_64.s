// x86-64 Entry point supporting Limine, Multiboot, and PVH — kernel/src/entry_x86_64.s

    .section .header, "a"
    .align 8
multiboot2_header:
    .long 0xE85250D6                // Magic
    .long 0                         // Architecture (i386)
    .long 24                        // Header length
    .long 0x17ADAF12                // Checksum
    .short 0
    .short 0
    .long 8
multiboot2_header_end:

    .align 4
multiboot_header:
    .long 0x1BADB002                // Magic
    .long 0x00000003                // Flags: ALIGN | MEMINFO
    .long -(0x1BADB002 + 0x00000003) // Checksum

    .section .note.pvh, "a"
    .align 4
pvh_note:
    .long 4             // Name size
    .long 8             // Desc size
    .long 18            // Type (XEN_ELFNOTE_PHYS32_ENTRY)
    .asciz "Xen"        // Name
    .long _start_pvh    // Entry point physical address
    .long 0

.section .text.boot, "ax"
    .globl _start
    .code64
_start:
    cli
    xor eax, eax
    jmp _start_common

.section .text.pvh, "ax"
    .code32
    .align 16
    .globl _start_pvh
_start_pvh:
    cli
    cld

    // Initialize serial port 0x3f8
    mov dx, 0x3f9
    xor al, al
    out dx, al
    mov dx, 0x3fb
    mov al, 0x80
    out dx, al
    mov dx, 0x3f8
    mov al, 0x01 // 115200 baud
    out dx, al
    mov dx, 0x3f9
    xor al, al
    out dx, al
    mov dx, 0x3fb
    mov al, 0x03 // 8N1
    out dx, al
    
    // Diagnostic: 'P'
    mov dx, 0x3f8
    mov al, 0x50
    out dx, al

    // 0. Temporary stack
    mov esp, 0x90000

    // Preserve start info
    mov edi, eax // magic
    mov esi, ebx // info

    // 1. Discover physical base and load GDT
    mov eax, 0x10000
    mov dword ptr [eax + 0], 0
    mov dword ptr [eax + 4], 0
    mov dword ptr [eax + 8], 0x0000ffff // 64-bit code
    mov dword ptr [eax + 12], 0x00af9a00
    mov dword ptr [eax + 16], 0x0000ffff // 32-bit data
    mov dword ptr [eax + 20], 0x00cf9200
    
    sub esp, 8
    mov word ptr [esp], 23 
    mov [esp + 2], eax
    lgdt [esp]
    add esp, 8

    // Load data segments
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax

    // Diagnostic: 'G'
    mov dx, 0x3f8
    mov al, 0x47
    out dx, al

    // 2. PAE
    mov eax, cr4
    or eax, 0x20
    mov cr4, eax

    // 3. Paging structures at 0x20000
    mov edx, 0x20000
    push edi
    mov edi, edx
    xor eax, eax
    mov ecx, 1024 * 8
    rep stosd
    pop edi
    
    lea eax, [edx + 0x1003]
    mov [edx], eax
    mov [edx + 2048], eax
    mov [edx + 4088], eax
    
    lea eax, [edx + 0x2003]
    mov [edx + 4096], eax
    add eax, 0x1000
    mov [edx + 4104], eax
    
    lea eax, [edx + 0x2003]
    mov [edx + 4096 + 510 * 8], eax
    add eax, 0x1000
    mov [edx + 4096 + 511 * 8], eax

    push ebp
    lea ebp, [edx + 8192]
    mov eax, 0x00000083
    mov ecx, 1024
.Lmap_2gb_v11:
    mov [ebp], eax
    mov dword ptr [ebp + 4], 0
    add eax, 0x200000
    add ebp, 8
    loop .Lmap_2gb_v11
    pop ebp

    // Diagnostic: 'p'
    mov dx, 0x3f8
    mov al, 0x70
    out dx, al

    mov cr3, edx

    // Diagnostic: '3'
    mov al, 0x33
    out dx, al

    // 4. EFER.LME
    mov ecx, 0xC0000080
    rdmsr
    or eax, 0x100
    wrmsr

    // Diagnostic: 'E'
    mov dx, 0x3f8
    mov al, 0x45
    out dx, al

    // 5. Enable Paging
    mov eax, cr0
    or eax, 0x80000001
    mov cr0, eax

    // Diagnostic: 'M'
    mov al, 0x4d
    out dx, al

    // 6. Jump to 64-bit
    call 1f
1:  pop eax
    add eax, 17 // Distance from 1: to .Ltarget
    
    // Diagnostic: 'J'
    mov dx, 0x3f8
    mov al, 0x4a
    out dx, al
    
    // Jump to 64-bit mode using far return
    push 0x08
    push eax
    retf

    .align 16
    .code64
.Ltarget:
    // Diagnostic: '6'
    mov dx, 0x3f8
    mov al, 0x36
    out dx, al

    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    
    // Enable SSE/AVX
    mov rax, cr0
    and rax, 0xFFFB
    or rax, 0x2
    mov cr0, rax
    mov rax, cr4
    or rax, (3 << 9)
    mov cr4, rax

    // Diagnostic: '!'
    mov al, 0x21
    out dx, al

    lea rcx, [rip + _start_common]
    mov eax, edi
    mov rbx, rsi
    jmp rcx

.section .text
    .globl _start_common
    .code64
    .align 16
_start_common:
    // Diagnostic: 'C'
    mov dx, 0x3f8
    mov al, 0x43
    out dx, al

    // ── Zero BSS ─────────────────────────────────────────────────────────────
    lea rdi, [rip + __bss_start]
    lea rcx, [rip + __bss_end]
    cmp rdi, rcx
    jge .Lbss_done
    sub rcx, rdi
    xor rax, rax
    rep stosb
.Lbss_done:

    // ── Set up stack ─────────────────────────────────────────────────────────
    lea rsp, [rip + EARLY_STACK + 0x10000]

    // Limine/PVH detection
    cmp eax, 0x36D76289
    je .Lpvh_boot
    xor rdi, rdi
    jmp .Lcontinue
.Lpvh_boot:
    mov rdi, rbx
.Lcontinue:
    call kernel_main

    .section .data
    .align 4096
    
    .globl _start_common_phys_ptr
_start_common_phys_ptr:
    .long 0
    .globl _start_multiboot_phys
_start_multiboot_phys:
    .long 0
    .globl _start_pvh_phys
_start_pvh_phys:
    .long 0
