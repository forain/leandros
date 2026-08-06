//! CPU context save/restore — the foundation of context switching.
//!
//! `cpu_switch_to(old, new)` saves callee-saved registers into `*old` and
//! restores them from `*new`, transferring execution to the new task.

#[cfg(target_arch = "x86_64")]
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct CpuContext {
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rsp: u64,
    pub fs_base: u64,
    /// FXSAVE64 area (x87 + XMM0-15 + MXCSR), 16-byte aligned at offset 64.
    ///
    /// Preemptive switches (timer tick / resched IPI) interrupt a thread at
    /// an arbitrary instruction where *every* XMM register may be live —
    /// the SysV "caller-saved" convention only protects them across calls
    /// the compiler can see. Without saving this state here, two SSE-using
    /// threads scheduled on the same CPU silently corrupt each other's
    /// vector registers (first seen as garbage pointers in MAME, whose
    /// main + sound threads both run SSE memcpy concurrently).
    pub fpu: [u8; 512],
}

#[cfg(target_arch = "aarch64")]
#[repr(C, align(16))]
#[derive(Debug, Clone, Copy)]
pub struct CpuContext {
    pub gregs: [u64; 12],
    pub sp:    u64,
    pub tpidr_el0: u64,
    /// Full FP/SIMD state (q0-q31), 16-byte aligned at offset 112 — same
    /// preemption rationale as the x86_64 `fpu` field above.
    pub vregs: [u8; 512],
    pub fpsr:  u64,
    pub fpcr:  u64,
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
    pub r10: u64,
    #[cfg(target_arch = "x86_64")]
    pub r9:  u64,
    #[cfg(target_arch = "x86_64")]
    pub r8:  u64,
    #[cfg(target_arch = "x86_64")]
    pub rdx: u64,
    #[cfg(target_arch = "x86_64")]
    pub rsi: u64,
    #[cfg(target_arch = "x86_64")]
    pub rdi: u64,
    #[cfg(target_arch = "x86_64")]
    pub rax: u64,
    #[cfg(target_arch = "x86_64")]
    pub rcx: u64,
    #[cfg(target_arch = "x86_64")]
    pub r11: u64,
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
            // FXRSTOR64 validates the area, so a fresh task needs sane
            // defaults: x87 FCW = 0x037F (all exceptions masked, 64-bit
            // precision), MXCSR = 0x1F80 (all SSE exceptions masked).
            // An all-zero MXCSR would UNMASK every SSE exception and the
            // first float op after the first switch would raise #XM.
            let mut fpu = [0u8; 512];
            fpu[0] = 0x7F; fpu[1] = 0x03;   // FCW
            fpu[24] = 0x80; fpu[25] = 0x1F; // MXCSR
            Self { rbx: 0, rbp: 0, r12: 0, r13: 0, r14: 0, r15: 0, rsp: 0, fs_base: 0, fpu }
        }
        #[cfg(target_arch = "aarch64")]
        {
            Self { gregs: [0; 12], sp: 0, tpidr_el0: 0, vregs: [0; 512], fpsr: 0, fpcr: 0 }
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

/// Read this CPU's *live* TLS-base register directly, rather than trusting
/// any `Task`-side shadow copy.
///
/// `Task::tls_base` (see `set_fs_base`) is only kept in sync on x86-64,
/// where `arch_prctl(ARCH_SET_FS)` traps into the kernel. AArch64 has no
/// such syscall — musl's startup code sets TPIDR_EL0 with a plain `msr`
/// from EL0, which the kernel never observes — so `Task::tls_base` stays 0
/// for the whole lifetime of an AArch64 process while the hardware register
/// is very much set. `cpu_switch_to`'s own save/restore (above) proves the
/// register itself is always the source of truth: it round-trips the live
/// value on every switch instead of reading back a bookkeeping field.
/// Callers that need "what TLS base does the *currently running* task have
/// right now" (e.g. clone()'s vfork-style fallback, which inherits the
/// caller's TLS when CLONE_SETTLS is absent) must ask the register, not the
/// Task struct.
#[cfg(target_arch = "x86_64")]
pub fn current_tls_base() -> u64 {
    let lo: u32;
    let hi: u32;
    unsafe {
        core::arch::asm!(
            "rdmsr",
            in("ecx") 0xC0000100u32,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
    }
    ((hi as u64) << 32) | (lo as u64)
}

#[cfg(target_arch = "aarch64")]
pub fn current_tls_base() -> u64 {
    let v: u64;
    unsafe {
        core::arch::asm!("mrs {}, tpidr_el0", out(reg) v, options(nomem, nostack, preserves_flags));
    }
    v
}

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(r#"
// The kernel itself is built with -neon,-fp-armv8 (targets/
// aarch64-unknown-kernel.json) so that no kernel code can ever lower through a
// q register and land on the interrupted thread's vector state. This routine is
// the one deliberate exception: it *saves and restores* userspace FP/SIMD, so it
// needs those instructions. Enable the extension for this block only and drop
// back to the plain baseline at the end, so the relaxation cannot leak into any
// assembly emitted after it.
.arch armv8-a+fp+simd
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

    // Full FP/SIMD state: a preempted thread can have any of q0-q31 live
    // (AAPCS only makes v8-v15 callee-saved across visible calls). vregs
    // starts at offset 112, fpsr/fpcr at 624/632 (see CpuContext).
    stp  q0,  q1,  [x0, #112]
    stp  q2,  q3,  [x0, #144]
    stp  q4,  q5,  [x0, #176]
    stp  q6,  q7,  [x0, #208]
    stp  q8,  q9,  [x0, #240]
    stp  q10, q11, [x0, #272]
    stp  q12, q13, [x0, #304]
    stp  q14, q15, [x0, #336]
    stp  q16, q17, [x0, #368]
    stp  q18, q19, [x0, #400]
    stp  q20, q21, [x0, #432]
    stp  q22, q23, [x0, #464]
    stp  q24, q25, [x0, #496]
    stp  q26, q27, [x0, #528]
    stp  q28, q29, [x0, #560]
    stp  q30, q31, [x0, #592]
    mrs  x9, fpsr
    str  x9, [x0, #624]
    mrs  x9, fpcr
    str  x9, [x0, #632]

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

    ldp  q0,  q1,  [x1, #112]
    ldp  q2,  q3,  [x1, #144]
    ldp  q4,  q5,  [x1, #176]
    ldp  q6,  q7,  [x1, #208]
    ldp  q8,  q9,  [x1, #240]
    ldp  q10, q11, [x1, #272]
    ldp  q12, q13, [x1, #304]
    ldp  q14, q15, [x1, #336]
    ldp  q16, q17, [x1, #368]
    ldp  q18, q19, [x1, #400]
    ldp  q20, q21, [x1, #432]
    ldp  q22, q23, [x1, #464]
    ldp  q24, q25, [x1, #496]
    ldp  q26, q27, [x1, #528]
    ldp  q28, q29, [x1, #560]
    ldp  q30, q31, [x1, #592]
    ldr  x9, [x1, #624]
    msr  fpsr, x9
    ldr  x9, [x1, #632]
    msr  fpcr, x9

    ret
.size cpu_switch_to, .-cpu_switch_to
.arch armv8-a
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

    // Full x87/SSE state: live XMM registers of a preempted thread must
    // survive the switch (see CpuContext::fpu). Area is 16-byte aligned.
    fxsave64  [rdi + 64]
    fxrstor64 [rsi + 64]

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
    // Establish this CPU's user-mode GS invariant (KERNEL_GS_BASE = per-CPU
    // syscall block, GS_BASE = 0) before the first entry into user space.
    // Defined in arch-x86_64 (syscall.rs); resolved at kernel link time.
    call restore_user_gs
    mov ax, 0x1B
    mov ds, ax
    mov es, ax
    iretq
.size iret_to_user, .-iret_to_user
"#);
