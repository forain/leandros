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

    // Load CR3 from the clean page-table base in edx *before* any diagnostic
    // runs: the serial diagnostics use `mov dx, 0x3f8`, which overwrites the
    // low 16 bits of edx. Doing this after a diagnostic loaded CR3 with
    // 0x000203f8 — whose reserved bits make the paging-enable `mov cr0` fault,
    // leaving the CPU in 32-bit mode.
    mov cr3, edx

    // Diagnostic: 'p'
    mov dx, 0x3f8
    mov al, 0x70
    out dx, al

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
    mov dx, 0x3f8
    mov al, 0x4d
    out dx, al

    // 6. Jump to 64-bit
    // Diagnostic: 'J'
    mov dx, 0x3f8
    mov al, 0x4a
    out dx, al

    // Far-return into the 64-bit code segment at .Ltarget. Derive its runtime
    // address from the current PC plus the assembler-computed distance to
    // .Ltarget — a hand-counted offset silently breaks when the intervening
    // instructions change, and an absolute relocation overflows the
    // higher-half-linked Limine build that shares this code.
    call 1f
1:  pop eax
    // eax = runtime address of label 1 (the pop). Add the byte distance from
    // there to .Ltarget to get .Ltarget's runtime address. That distance is
    // pop(1) + add(3) + push 0x08(2) + push eax(1) + retf(1) = 8 bytes; it must
    // be recomputed if the instructions between label 1 and .Ltarget change.
    // (LLVM's assembler won't fold `.Ltarget - 1b` into an immediate, and an
    // absolute reference overflows the higher-half-linked Limine build.)
    add eax, 8

    push 0x08
    push eax
    retf

    .code64
.Ltarget:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax

    // Diagnostic: '6'
    mov dx, 0x3f8
    mov al, 0x36
    out dx, al
    
    // Enable SSE/AVX and FSGSBASE (CR4 bit 16).
    // Clear CR0.EM (bit 2) and CR0.TS (bit 3), set CR0.MP (bit 1). The mask
    // must be full-width: 0xFFFFFFF3 sign-extends to 0xFFFF...FFF3, clearing
    // only EM+TS. A 16-bit mask like 0xFFFB would also clear CR0.PG (bit 31),
    // disabling paging and dropping the CPU out of 64-bit mode mid-trampoline.
    mov rax, cr0
    and rax, -13        // ~0xC: clear EM(bit2)+TS(bit3), preserve all other bits
    or rax, 0x2
    mov cr0, rax
    mov rax, cr4
    or rax, (3 << 9) | (1 << 16)
    mov cr4, rax

    // Diagnostic: '!'
    mov al, 0x21
    out dx, al

    // Jump to the kernel body via its absolute (higher-half) address. The
    // direct-boot linker places _start_common and the rest of the kernel at
    // KERNEL_OFFSET while this trampoline runs from low physical memory, so a
    // RIP-relative LEA can't reach it. Load the linker-resolved 64-bit address
    // from a nearby .quad instead (LLVM's assembler rejects `movabs sym`).
    // (Harmless in the Limine build, where this trampoline is never executed.)
    mov rcx, [rip + .Lstart_common_ptr]
    mov eax, edi
    mov rbx, rsi
    jmp rcx

    .align 8
.Lstart_common_ptr:
    .quad _start_common

.section .text
    .globl _start_common
    .code64
    .align 16
_start_common:
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
