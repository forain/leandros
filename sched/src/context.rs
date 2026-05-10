//! CPU context save/restore — the foundation of context switching.
//!
//! `cpu_switch_to(old, new)` saves callee-saved registers into `*old` and
//! restores them from `*new`, transferring execution to the new task.

#[cfg(target_arch = "x86_64")]
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuContext {
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rsp: u64,
    pub fs_base: u64,
}

#[cfg(target_arch = "aarch64")]
#[repr(C)]
#[derive(Debug, Default, Clone, Copy)]
pub struct CpuContext {
    pub gregs: [u64; 12],
    pub sp:    u64,
    pub tpidr_el0: u64,
}

/// Saved CPU state on exception entry.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct UserFrame {
    #[cfg(target_arch = "x86_64")]
    pub r15: u64,
    #[cfg(target_arch = "x86_64")]
    pub r14: u64,
    #[cfg(target_arch = "x86_64")]
    pub r13: u64,
    #[cfg(target_arch = "x86_64")]
    pub r12: u64,
    #[cfg(target_arch = "x86_64")]
    pub rbp: u64,
    #[cfg(target_arch = "x86_64")]
    pub rbx: u64,
    #[cfg(target_arch = "x86_64")]
    pub r11: u64,
    #[cfg(target_arch = "x86_64")]
    pub r10: u64,
    #[cfg(target_arch = "x86_64")]
    pub r9:  u64,
    #[cfg(target_arch = "x86_64")]
    pub r8:  u64,
    #[cfg(target_arch = "x86_64")]
    pub rax: u64,
    #[cfg(target_arch = "x86_64")]
    pub rcx: u64,
    #[cfg(target_arch = "x86_64")]
    pub rdx: u64,
    #[cfg(target_arch = "x86_64")]
    pub rsi: u64,
    #[cfg(target_arch = "x86_64")]
    pub rdi: u64,
    #[cfg(target_arch = "x86_64")]
    pub rip: u64,
    #[cfg(target_arch = "x86_64")]
    pub cs:  u64,
    #[cfg(target_arch = "x86_64")]
    pub rflags: u64,
    #[cfg(target_arch = "x86_64")]
    pub rsp: u64,
    #[cfg(target_arch = "x86_64")]
    pub ss:  u64,

    #[cfg(target_arch = "aarch64")]
    pub x:        [u64; 31],
    #[cfg(target_arch = "aarch64")]
    pub sp_el0:   u64,
    #[cfg(target_arch = "aarch64")]
    pub elr_el1:  u64,
    #[cfg(target_arch = "aarch64")]
    pub spsr_el1: u64,
    #[cfg(target_arch = "aarch64")]
    pub pt:       u64,
}

impl UserFrame {
    pub const SIZE: usize = core::mem::size_of::<Self>();
}

impl CpuContext {
    pub const fn zeroed() -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            Self { rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0, rsp: 0, fs_base: 0 }
        }
        #[cfg(target_arch = "aarch64")]
        {
            Self { gregs: [0; 12], sp: 0, tpidr_el0: 0 }
        }
    }

    pub fn new_task(entry: usize, stack_top: usize) -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            let mut c = Self::zeroed();
            c.rsp = (stack_top - 8) as u64;
            unsafe { (c.rsp as *mut u64).write(entry as u64); }
            c
        }
        #[cfg(target_arch = "aarch64")]
        {
            let mut c = Self::zeroed();
            c.gregs[11] = entry as u64;
            c.sp = stack_top as u64;
            c
        }
    }

    pub fn new_user_task(user_entry: usize, user_sp: usize, kernel_stack_top: usize) -> Self {
        Self::new_user_task_with_pt(user_entry, user_sp, kernel_stack_top, 0)
    }

    pub fn new_user_task_with_pt(user_entry: usize, user_sp: usize, kernel_stack_top: usize, page_table: usize) -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            let _ = page_table;
            extern "C" { fn iret_to_user(); }
            // iretq frame: RIP, CS, RFLAGS, RSP, SS (5 * 8 bytes)
            // Plus return address for cpu_switch_to (8 bytes)
            let frame = kernel_stack_top.wrapping_sub(6 * 8);
            unsafe {
                let p = frame as *mut u64;
                p.add(0).write(iret_to_user as *const () as u64);
                p.add(1).write(user_entry as u64);
                p.add(2).write(0x23);
                p.add(3).write(0x202);
                p.add(4).write(user_sp as u64);
                p.add(5).write(0x1B);
            }
            let mut c = Self::zeroed();
            c.rsp = frame as u64;
            c
        }

        #[cfg(target_arch = "aarch64")]
        {
            extern "C" { fn ret_to_user(); }
            // Correct for 288-byte alignment in exception_asm.s
            let frame_aligned_size = 288usize;
            let frame = kernel_stack_top.wrapping_sub(frame_aligned_size);
            unsafe {
                let p = frame as *mut UserFrame;
                (*p).x = [0u64; 31];
                (*p).sp_el0 = user_sp as u64;
                (*p).elr_el1 = user_entry as u64;
                (*p).spsr_el1 = 0x0u64;
                (*p).pt = page_table as u64;
            }
            let mut c = Self::zeroed();
            c.gregs[11] = ret_to_user as *const () as u64;
            c.sp = frame as u64;
            c
        }
    }
}

extern "C" {
    pub fn cpu_switch_to(old: *mut CpuContext, new: *const CpuContext);
}

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(r#"
.global cpu_switch_to
.type   cpu_switch_to, @function
cpu_switch_to:
    stp  x19, x20, [x0, #0]
    stp  x21, x22, [x0, #16]
    stp  x23, x24, [x0, #32]
    stp  x25, x26, [x0, #48]
    stp  x27, x28, [x0, #64]
    stp  x29, x30, [x0, #80]
    mov  x9, sp
    str  x9, [x0, #96]
    mrs  x9, tpidr_el0
    str  x9, [x0, #104]

    ldp  x19, x20, [x1, #0]
    ldp  x21, x22, [x1, #16]
    ldp  x23, x24, [x1, #32]
    ldp  x25, x26, [x1, #48]
    ldp  x27, x28, [x1, #64]
    ldp  x29, x30, [x1, #80]
    ldr  x9, [x1, #96]
    mov  sp, x9
    ldr  x9, [x1, #104]
    msr  tpidr_el0, x9

    ret
.size cpu_switch_to, .-cpu_switch_to
"#);

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(r#"
.section .text
.global cpu_switch_to
.type   cpu_switch_to, @function
cpu_switch_to:
    mov   ecx, 0xC0000100
    rdmsr
    shl   rdx, 32
    or    rax, rdx
    mov   [rdi + 56], rax

    mov   [rdi + 0],  rbx
    mov   [rdi + 8],  rbp
    mov   [rdi + 16], r12
    mov   [rdi + 24], r13
    mov   [rdi + 32], r14
    mov   [rdi + 40], r15
    mov   [rdi + 48], rsp

    mov   rbx, [rsi + 0]
    mov   rbp, [rsi + 8]
    mov   r12, [rsi + 16]
    mov   r13, [rsi + 24]
    mov   r14, [rsi + 32]
    mov   r15, [rsi + 40]
    mov   rsp, [rsi + 48]

    mov   rax, [rsi + 56]
    mov   rdx, rax
    shr   rdx, 32
    mov   ecx, 0xC0000100
    wrmsr

    ret
.size cpu_switch_to, .-cpu_switch_to

.section .text
.global iret_to_user
.type   iret_to_user, @function
iret_to_user:
    mov ax, 0x1B
    mov ds, ax
    mov es, ax
    iretq
.size iret_to_user, .-iret_to_user
"#);
