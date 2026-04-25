//! CPU context save/restore — the foundation of context switching.
//!
//! `cpu_switch_to(old, new)` saves callee-saved registers into `*old` and
//! restores them from `*new`, transferring execution to the new task.
//!
//! AArch64: saves x19-x28, x29(fp), x30(lr), SP into `CpuContext`.
//! x86-64:  pushes rbx/rbp/r12-r15 onto the task stack, then stores rsp.

/// Architecture-specific saved context for one schedulable task.
///
/// Only callee-saved state is stored here; the task is responsible for
/// saving caller-saved registers before any blocking call.
///
/// FPU/SIMD state is always saved eagerly on every context switch.
/// This is simpler than lazy-FPU (trap-on-use) and correct for all workloads.
#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CpuContext {
    /// x19, x20, x21, x22, x23, x24, x25, x26, x27, x28, x29(fp), x30(lr)
    /// Offsets: 0..96 (12 × 8 bytes)
    pub gregs: [u64; 12],
    /// SP_EL1 — offset 96.
    pub sp: u64,
    /// Padding to 16-byte-align fpregs for `stp q` instructions — offset 104.
    pub _pad: u64,
    // ── AArch64 FP/SIMD (FEAT_FP + FEAT_AdvSIMD, mandatory from ARMv8.0) ────
    /// SIMD/FP registers Q0-Q31, each 128 bits wide.
    /// Offset: 112.  Total: 32 × 16 = 512 bytes.
    pub fpregs: [u128; 32],
    /// FPCR (floating-point control register) — offset 624.
    pub fpcr: u64,
    /// FPSR (floating-point status register) — offset 632.
    pub fpsr: u64,
    /// TPIDR_EL0 — user-space thread-pointer register — offset 640.
    /// Used by musl/pthreads as the TLS base pointer.
    pub tpidr_el0: u64,
    /// Padding to maintain 8-byte struct alignment — offset 648.
    pub _pad2: u64,
}

/// On all non-AArch64 targets (x86-64 and future ports).
#[cfg(not(target_arch = "aarch64"))]
#[derive(Clone, Copy)]
#[repr(C)]
pub struct CpuContext {
    /// Saved kernel stack pointer.
    /// rbx, rbp, r12–r15 are pushed onto the task's stack before rsp is saved.
    /// Offset: 0.
    pub rsp: u64,
    // ── x86-64 SSE/AVX state ─────────────────────────────────────────────────
    /// XMM0-XMM15 (128-bit each).  Offset: 8.  Total: 16 × 16 = 256 bytes.
    pub xmm: [u128; 16],
    /// MXCSR (SSE control/status).  Offset: 264.
    pub mxcsr: u32,
    /// Padding — offset 268.
    pub _pad: u32,
    /// FS.base — thread-local storage pointer for musl/pthreads — offset 272.
    /// Saved/restored via RDMSR/WRMSR on MSR_FS_BASE (0xC000_0100).
    pub fs_base: u64,
}

impl CpuContext {
    /// A zeroed context, suitable as the initial scheduler idle context.
    pub const fn zeroed() -> Self {
        #[cfg(target_arch = "aarch64")]
        { Self {
            gregs: [0u64; 12], sp: 0, _pad: 0,
            fpregs: [0u128; 32], fpcr: 0, fpsr: 0,
            tpidr_el0: 0, _pad2: 0,
        } }
        #[cfg(not(target_arch = "aarch64"))]
        // mxcsr 0x1F80 = default SSE control: all exceptions masked, round-to-nearest
        { Self { rsp: 0, xmm: [0u128; 16], mxcsr: 0x1F80, _pad: 0, fs_base: 0 } }
    }

    /// Build a context for a brand-new kernel-mode task.
    ///
    /// On the first `cpu_switch_to` into this context:
    /// - AArch64: `ret` jumps to `entry` (pre-loaded into x30/lr).
    /// - x86-64:  `ret` pops `entry` from the pre-built stack frame.
    pub fn new_task(entry: usize, stack_top: usize) -> Self {
        #[cfg(target_arch = "aarch64")]
        {
            let mut c = Self::zeroed();
            c.gregs[11] = entry as u64;  // x30 (lr) = entry point
            c.sp = stack_top as u64;
            c
        }

        #[cfg(not(target_arch = "aarch64"))]
        {
            // Pre-build a stack frame that cpu_switch_to will pop on first entry.
            // Layout from rsp (low) → stack_top (high):
            //   rsp+0:  rbx = 0   (first pop)
            //   rsp+8:  rbp = 0
            //   rsp+16: r12 = 0
            //   rsp+24: r13 = 0
            //   rsp+32: r14 = 0
            //   rsp+40: r15 = 0
            //   rsp+48: entry     (ret target — popped last)
            let frame = stack_top.wrapping_sub(7 * 8);
            unsafe {
                let p = frame as *mut u64;
                p.add(0).write(0);
                p.add(1).write(0);
                p.add(2).write(0);
                p.add(3).write(0);
                p.add(4).write(0);
                p.add(5).write(0);
                p.add(6).write(entry as u64);
            }
            let mut c = Self::zeroed();
            c.rsp = frame as u64;
            c
        }
    }

    /// Build a context for a new user-mode task.
    ///
    /// **AArch64**: `cpu_switch_to` loads x30 = `ret_to_user` and branches there
    /// via `ret`.  `ret_to_user` pops SP_EL0/ELR_EL1/SPSR_EL1 and `eret`s to EL0.
    ///
    /// **x86-64**: `cpu_switch_to` pops callee-saved regs, then `ret`s to
    /// `iret_to_user`, which executes `iretq` into the IRET frame below it.
    ///
    /// AArch64 kernel stack layout (from `kernel_stack_top - 24`):
    ///   [ksp+0]:  SP_EL0   = user stack pointer
    ///   [ksp+8]:  ELR_EL1  = user entry point
    ///   [ksp+16]: SPSR_EL1 = 0 (EL0t, all interrupts unmasked)
    ///
    /// x86-64 kernel stack layout (from `kernel_stack_top - 96`):
    ///   [ksp+0..40]: callee-saved regs = 0 (rbx, rbp, r12-r15)
    ///   [ksp+48]:    iret_to_user (ret target)
    ///   [ksp+56]:    user RIP
    ///   [ksp+64]:    user CS  = 0x23
    ///   [ksp+72]:    user RFLAGS = 0x202 (IF set)
    ///   [ksp+80]:    user RSP
    ///   [ksp+88]:    user SS  = 0x1B
    pub fn new_user_task(user_entry: usize, user_sp: usize, kernel_stack_top: usize) -> Self {
        #[cfg(target_arch = "x86_64")]
        {
            extern "C" { fn iret_to_user(); }
            // Frame is 12 words (96 bytes) below stack top.
            // Layout: 6 × callee-saved zeros | iret_to_user | IRET frame (5 words)
            let frame = kernel_stack_top.wrapping_sub(12 * 8);
            unsafe {
                let p = frame as *mut u64;
                p.add(0).write(0);                      // rbx
                p.add(1).write(0);                      // rbp
                p.add(2).write(0);                      // r12
                p.add(3).write(0);                      // r13
                p.add(4).write(0);                      // r14
                p.add(5).write(0);                      // r15
                p.add(6).write(iret_to_user as u64);    // ret target → iretq
                p.add(7).write(user_entry as u64);      // IRET: user RIP
                p.add(8).write(0x23);                   // IRET: user CS  (DPL 3, 64-bit)
                p.add(9).write(0x202);                  // IRET: RFLAGS (IF=1)
                p.add(10).write(user_sp as u64);        // IRET: user RSP
                p.add(11).write(0x1B);                  // IRET: user SS  (DPL 3)
            }
            let mut c = Self::zeroed();
            c.rsp = frame as u64;
            c
        }

        #[cfg(target_arch = "aarch64")]
        {
            extern "C" { fn ret_to_user(); }
            // Build the frame on the kernel stack for ret_to_user
            // Frame: 4 × 8 bytes = SP_EL0, ELR_EL1, SPSR_EL1, PAGE_TABLE
            let frame = kernel_stack_top.wrapping_sub(4 * 8);
            unsafe {
                let p = frame as *mut u64;
                p.add(0).write(user_sp as u64);     // SP_EL0
                p.add(1).write(user_entry as u64);  // ELR_EL1
                p.add(2).write(0x0u64);             // SPSR_EL1 = EL0t, DAIF unmasked (allows syscalls!)
                p.add(3).write(0x0u64);             // PAGE_TABLE placeholder - will be set by scheduler
            }
            let mut c = Self::zeroed();
            c.gregs[11] = ret_to_user as *const () as u64;  // x30 (lr) = ret_to_user
            c.sp = frame as u64;  // SP points to the frame
            c
        }
    }
}

/// Full user-register frame saved by the AArch64 EL0 synchronous exception
/// handler at the top of the kernel stack on every EL0→EL1 transition.
///
/// Layout matches the `sub sp, sp, #272` frame in `exc_el0_sync`:
///
/// | Offset | Field      | Description                         |
/// |--------|------------|-------------------------------------|
/// |   0    | x[0..=30]  | General-purpose registers x0–x30    |
/// |  248   | sp_el0     | User stack pointer at exception entry|
/// |  256   | elr_el1    | User PC (return address after SVC)  |
/// |  264   | spsr_el1   | User PSTATE                         |
///
/// Total size: 272 bytes (17 × 16 — 16-byte aligned).
#[cfg(target_arch = "aarch64")]
#[repr(C)]
pub struct UserFrame {
    /// General-purpose registers x0–x30 (31 × 8 = 248 bytes).
    pub x:        [u64; 31],
    /// User stack pointer saved by the EL0 exception stub.
    pub sp_el0:   u64,
    /// ELR_EL1: user-space return address (instruction after SVC).
    pub elr_el1:  u64,
    /// SPSR_EL1: saved user PSTATE.
    pub spsr_el1: u64,
}

#[cfg(target_arch = "aarch64")]
impl UserFrame {
    /// Byte size of the frame — must match the `sub sp, sp, #272` in asm.
    pub const SIZE: usize = core::mem::size_of::<Self>();
}

extern "C" {
    /// Switch CPU from `old` context to `new` context.
    ///
    /// Saves callee-saved registers into `*old` and restores from `*new`.
    /// Returns in the execution context of `new`.
    ///
    /// # Safety
    /// Both pointers must be valid, non-null, and aligned.
    /// `new` must have been initialised by a prior `cpu_switch_to` or
    /// by `CpuContext::new_task`.
    pub fn cpu_switch_to(old: *mut CpuContext, new: *const CpuContext);

    /// Switch CPU from `old` context to `new` context with page table switch.
    ///
    /// Like `cpu_switch_to` but also switches to the specified page table
    /// atomically during the context switch to avoid race conditions.
    ///
    /// # Parameters
    /// - `old`: Context to save current state into
    /// - `new`: Context to restore from
    /// - `page_table`: Physical address of new page table root (0 = no change)
    ///
    /// # Safety
    /// Same as `cpu_switch_to`. `page_table` must be a valid physical address.
    pub fn cpu_switch_to_with_pt(old: *mut CpuContext, new: *const CpuContext, page_table: usize);
}

// ─── AArch64 context switch ────────────────────────────────────────────────

#[cfg(target_arch = "aarch64")]
core::arch::global_asm!(r#"
.global cpu_switch_to
.type   cpu_switch_to, %function
cpu_switch_to:
    // x0 = *mut CpuContext (old)
    // x1 = *const CpuContext (new)

    // Debug: Direct UART write 'A' to show we entered cpu_switch_to
    stp  x0, x1, [sp, #-16]!
    mov  x0, #'A'
    mov  x1, #0x09000000
    str  w0, [x1]
    ldp  x0, x1, [sp], #16

    // ── save outgoing integer registers ─────────────────────────────────────
    stp  x19, x20, [x0, #0]
    stp  x21, x22, [x0, #16]
    stp  x23, x24, [x0, #32]
    stp  x25, x26, [x0, #48]
    stp  x27, x28, [x0, #64]
    stp  x29, x30, [x0, #80]    // fp (x29) and lr (x30)
    mov  x9,  sp
    str  x9,  [x0, #96]

    // ── save outgoing FP/SIMD state ─────────────────────────────────────────
    mrs  x9,  fpcr
    mrs  x10, fpsr
    str  x9,  [x0, #624]        // fpcr
    str  x10, [x0, #632]        // fpsr

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

    // save TLS register
    mrs  x10, tpidr_el0
    str  x10, [x0, #640]

    // Debug: Direct UART write 'B' to show we finished saving
    stp  x0, x1, [sp, #-16]!
    mov  x0, #'B'
    mov  x1, #0x09000000
    str  w0, [x1]
    ldp  x0, x1, [sp], #16

    // ── restore incoming integer registers ──────────────────────────────────
    ldp  x19, x20, [x1, #0]
    ldp  x21, x22, [x1, #16]
    ldp  x23, x24, [x1, #32]
    ldp  x25, x26, [x1, #48]
    ldp  x27, x28, [x1, #64]
    ldp  x29, x30, [x1, #80]    // x30 = return addr or entry point
    ldr  x9,  [x1, #96]
    mov  sp,  x9

    // ── restore incoming FP/SIMD state ──────────────────────────────────────
    ldr  x9,  [x1, #624]        // fpcr
    ldr  x10, [x1, #632]        // fpsr
    msr  fpcr, x9
    msr  fpsr, x10

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

    // restore TLS register
    ldr  x10, [x1, #640]
    msr  tpidr_el0, x10

    // Debug: Direct UART write 'C' to show we're about to ret
    stp  x0, x1, [sp, #-16]!
    mov  x0, #'C'
    mov  x1, #0x09000000
    str  w0, [x1]
    ldp  x0, x1, [sp], #16

    ret                          // branch to x30

// ─── AArch64 context switch with page table switch ────────────────────────────
.global cpu_switch_to_with_pt
.type   cpu_switch_to_with_pt, %function
cpu_switch_to_with_pt:
    // x0 = *mut CpuContext (old)
    // x1 = *const CpuContext (new)
    // x2 = page_table (physical address, 0 = no change)

    // Debug: Direct UART write 'D' to show we entered cpu_switch_to_with_pt
    stp  x0, x1, [sp, #-16]!
    mov  x0, #'D'
    mov  x1, #0x09000000
    str  w0, [x1]
    ldp  x0, x1, [sp], #16

    // ── save outgoing integer registers ─────────────────────────────────────
    stp  x19, x20, [x0, #0]
    stp  x21, x22, [x0, #16]
    stp  x23, x24, [x0, #32]
    stp  x25, x26, [x0, #48]
    stp  x27, x28, [x0, #64]
    stp  x29, x30, [x0, #80]    // fp (x29) and lr (x30)
    mov  x9,  sp
    str  x9,  [x0, #96]

    // ── save outgoing FP/SIMD state ─────────────────────────────────────────
    mrs  x9,  fpcr
    mrs  x10, fpsr
    str  x9,  [x0, #624]        // fpcr
    str  x10, [x0, #632]        // fpsr

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

    // save TLS register
    mrs  x10, tpidr_el0
    str  x10, [x0, #640]

    // ── switch page table if needed ─────────────────────────────────────────
    // Debug: Direct UART write 'P' before page table switch
    stp  x0, x1, [sp, #-16]!
    mov  x0, #'P'
    mov  x1, #0x09000000
    str  w0, [x1]
    ldp  x0, x1, [sp], #16

    cbz  x2, 1f                 // if page_table == 0, skip page table switch
    msr  ttbr0_el1, x2          // switch to new page table
    isb                         // ensure page table switch is complete
1:

    // ── restore incoming integer registers ──────────────────────────────────
    ldp  x19, x20, [x1, #0]
    ldp  x21, x22, [x1, #16]
    ldp  x23, x24, [x1, #32]
    ldp  x25, x26, [x1, #48]
    ldp  x27, x28, [x1, #64]
    ldp  x29, x30, [x1, #80]    // x30 = return addr or entry point
    ldr  x9,  [x1, #96]
    mov  sp,  x9

    // ── restore incoming FP/SIMD state ──────────────────────────────────────
    ldr  x9,  [x1, #624]        // fpcr
    ldr  x10, [x1, #632]        // fpsr
    msr  fpcr, x9
    msr  fpsr, x10

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

    // restore TLS register
    ldr  x10, [x1, #640]
    msr  tpidr_el0, x10

    // Debug: Direct UART write 'R' before ret (which jumps to ret_to_user)
    stp  x0, x1, [sp, #-16]!
    mov  x0, #'R'
    mov  x1, #0x09000000
    str  w0, [x1]
    ldp  x0, x1, [sp], #16

    ret                          // branch to x30
"#);

// ─── x86-64 context switch + user-mode trampoline (Intel syntax) ──────────────

#[cfg(target_arch = "x86_64")]
core::arch::global_asm!(r#"
.intel_syntax noprefix

.global cpu_switch_to
.type   cpu_switch_to, @function
cpu_switch_to:
    // rdi = *mut CpuContext (old)
    // rsi = *const CpuContext (new)

    // ── save FS.base ─────────────────────────────────────────────────────────
    mov   ecx, 0xC0000100
    rdmsr
    shl   rdx, 32
    or    rax, rdx
    mov   [rdi + 272], rax

    // ── save outgoing SSE/FPU state ──────────────────────────────────────────
    movdqu [rdi + 8],   xmm0
    movdqu [rdi + 24],  xmm1
    movdqu [rdi + 40],  xmm2
    movdqu [rdi + 56],  xmm3
    movdqu [rdi + 72],  xmm4
    movdqu [rdi + 88],  xmm5
    movdqu [rdi + 104], xmm6
    movdqu [rdi + 120], xmm7
    movdqu [rdi + 136], xmm8
    movdqu [rdi + 152], xmm9
    movdqu [rdi + 168], xmm10
    movdqu [rdi + 184], xmm11
    movdqu [rdi + 200], xmm12
    movdqu [rdi + 216], xmm13
    movdqu [rdi + 232], xmm14
    movdqu [rdi + 248], xmm15
    stmxcsr [rdi + 264]

    // ── save outgoing integer registers ──────────────────────────────────────
    push rbx
    push rbp
    push r12
    push r13
    push r14
    push r15
    mov  [rdi], rsp         // save rsp into old->rsp

    // ── restore incoming integer registers ───────────────────────────────────
    mov  rsp, [rsi]         // load rsp from new->rsp
    pop  r15
    pop  r14
    pop  r13
    pop  r12
    pop  rbp
    pop  rbx

    // ── restore incoming SSE/FPU state ───────────────────────────────────────
    ldmxcsr [rsi + 264]
    movdqu xmm0,  [rsi + 8]
    movdqu xmm1,  [rsi + 24]
    movdqu xmm2,  [rsi + 40]
    movdqu xmm3,  [rsi + 56]
    movdqu xmm4,  [rsi + 72]
    movdqu xmm5,  [rsi + 88]
    movdqu xmm6,  [rsi + 104]
    movdqu xmm7,  [rsi + 120]
    movdqu xmm8,  [rsi + 136]
    movdqu xmm9,  [rsi + 152]
    movdqu xmm10, [rsi + 168]
    movdqu xmm11, [rsi + 184]
    movdqu xmm12, [rsi + 200]
    movdqu xmm13, [rsi + 216]
    movdqu xmm14, [rsi + 232]
    movdqu xmm15, [rsi + 248]

    // ── restore FS.base ──────────────────────────────────────────────────────
    mov  rax, [rsi + 272]
    mov  rdx, rax
    shr  rdx, 32
    mov  ecx, 0xC0000100
    wrmsr

    ret
.size cpu_switch_to, .-cpu_switch_to

// ─── x86-64 context switch with page table switch ────────────────────────────
.global cpu_switch_to_with_pt
.type   cpu_switch_to_with_pt, @function
cpu_switch_to_with_pt:
    // rdi = *mut CpuContext (old)
    // rsi = *const CpuContext (new)
    // rdx = page_table (physical address, 0 = no change)

    // ── save FS.base ─────────────────────────────────────────────────────────
    mov   ecx, 0xC0000100
    rdmsr
    shl   rdx, 32
    or    rax, rdx
    mov   [rdi + 272], rax

    // ── save outgoing SSE/FPU state ──────────────────────────────────────────
    movdqu [rdi + 8],   xmm0
    movdqu [rdi + 24],  xmm1
    movdqu [rdi + 40],  xmm2
    movdqu [rdi + 56],  xmm3
    movdqu [rdi + 72],  xmm4
    movdqu [rdi + 88],  xmm5
    movdqu [rdi + 104], xmm6
    movdqu [rdi + 120], xmm7
    movdqu [rdi + 136], xmm8
    movdqu [rdi + 152], xmm9
    movdqu [rdi + 168], xmm10
    movdqu [rdi + 184], xmm11
    movdqu [rdi + 200], xmm12
    movdqu [rdi + 216], xmm13
    movdqu [rdi + 232], xmm14
    movdqu [rdi + 248], xmm15
    stmxcsr [rdi + 264]

    // ── switch page table if needed ─────────────────────────────────────────
    // rdx still holds page_table from call
    test rdx, rdx
    jz 1f
    mov cr3, rdx
1:

    // ── restore incoming integer registers ───────────────────────────────────
    mov  rsp, [rsi]
    pop  r15
    pop  r14
    pop  r13
    pop  r12
    pop  rbp
    pop  rbx

    // ── restore incoming SSE/FPU state ───────────────────────────────────────
    ldmxcsr [rsi + 264]
    movdqu xmm0,  [rsi + 8]
    movdqu xmm1,  [rsi + 24]
    movdqu xmm2,  [rsi + 40]
    movdqu xmm3,  [rsi + 56]
    movdqu xmm4,  [rsi + 72]
    movdqu xmm5,  [rsi + 88]
    movdqu xmm6,  [rsi + 104]
    movdqu xmm7,  [rsi + 120]
    movdqu xmm8,  [rsi + 136]
    movdqu xmm9,  [rsi + 152]
    movdqu xmm10, [rsi + 168]
    movdqu xmm11, [rsi + 184]
    movdqu xmm12, [rsi + 200]
    movdqu xmm13, [rsi + 216]
    movdqu xmm14, [rsi + 232]
    movdqu xmm15, [rsi + 248]

    // ── restore FS.base ──────────────────────────────────────────────────────
    mov  rax, [rsi + 272]
    mov  rdx, rax
    shr  rdx, 32
    mov  ecx, 0xC0000100
    wrmsr

    ret
.size cpu_switch_to_with_pt, .-cpu_switch_to_with_pt

// ── iret_to_user — first entry into a user-space task (x86-64) ───────────────
.global iret_to_user
.type   iret_to_user, @function
iret_to_user:
    // Set up segment registers for userspace (DPL 3)
    mov ax, 0x1B
    mov ds, ax
    mov es, ax
    // FS/GS are handled separately via MSRs
    iretq
.size iret_to_user, .-iret_to_user
"#);

