//! x86-64 SYSCALL/SYSRET entry point and MSR setup.
//!
//! On SYSCALL (hardware behaviour):
//!   CS  = STAR[47:32] = 0x08 (kernel code)
//!   SS  = STAR[47:32] + 8 = 0x10 (kernel data)
//!   RIP = LSTAR (→ syscall_entry)
//!   RFLAGS &= ~FMASK  (bit 9 cleared → interrupts disabled during syscall)
//!   RCX = user RIP  (restored by SYSRET)
//!   R11 = user RFLAGS (restored by SYSRET)
//!   RSP = unchanged (still user RSP — we switch it manually)
//!
//! Register convention from user space:
//!   RAX = syscall number
//!   RDI = a0, RSI = a1, RDX = a2, R10 = a3, R8 = a4, R9 = a5
//!   (R10 instead of RCX because SYSCALL clobbers RCX)
//!
//! `syscall_dispatch(number, a0, a1, a2)` uses System V C ABI:
//!   RDI = number (from RAX)
//!   RSI = a0     (from RDI)
//!   RDX = a1     (from RSI)
//!   RCX = a2     (from RDX)
//!
//! # Per-CPU stacks and SWAPGS
//!
//! The old implementation used a single global `_syscall_user_rsp` save-slot
//! and a single `_syscall_stack`, both SMP-unsafe.
//!
//! The new implementation uses per-CPU data accessed via the GS segment:
//!   - Each CPU has a `PerCpuSyscall` struct with a private kernel stack top
//!     and a user-RSP save slot.
//!   - `IA32_KERNEL_GS_BASE` MSR is set to point to that struct.
//!   - On SYSCALL entry `swapgs` activates kernel GS (→ per-CPU struct);
//!     on SYSRET `swapgs` restores user GS.
//!
//! `init()` calls `init_per_cpu(0)` for the BSP.
//! `init_ap()` is called by each AP in `smp::sched_ap_entry` after LAPIC init.

const MSR_EFER:         u32 = 0xC000_0080;
const MSR_STAR:         u32 = 0xC000_0081;
const MSR_LSTAR:        u32 = 0xC000_0082;
const MSR_FMASK:        u32 = 0xC000_0084;
const MSR_KERNEL_GSBASE: u32 = 0xC000_0102;

unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    core::arch::asm!(
        "rdmsr",
        in("ecx")  msr,
        out("eax") lo,
        out("edx") hi,
        options(nomem, nostack, preserves_flags)
    );
    (hi as u64) << 32 | lo as u64
}

unsafe fn wrmsr(msr: u32, val: u64) {
    core::arch::asm!(
        "wrmsr",
        in("ecx")  msr,
        in("eax")  val as u32,
        in("edx")  (val >> 32) as u32,
        options(nomem, nostack, preserves_flags)
    );
}

// ── Per-CPU SYSCALL data ──────────────────────────────────────────────────────

/// Number of CPUs supported (must match `sched::MAX_CPUS`).
const MAX_CPUS:   usize = 8;

/// Per-CPU metadata accessed via GS during the SYSCALL path.
///
/// Field offsets are part of the assembly ABI:
///   offset 0  (`gs:0`)  — `kernel_stack_top`: kernel RSP to load on entry.
///   offset 8  (`gs:8`)  — `user_rsp_save`:    slot for the user RSP.
#[repr(C, align(16))]
struct PerCpuSyscall {
    kernel_stack_top: u64,
    user_rsp_save:    u64,
}

/// Per-CPU SYSCALL metadata (KERNEL_GS_BASE points here for each CPU).
static mut PER_CPU: [PerCpuSyscall; MAX_CPUS] =
    [const { PerCpuSyscall { kernel_stack_top: 0, user_rsp_save: 0 } }; MAX_CPUS];

/// Update the kernel stack top for the current CPU.
/// Called by `arch_set_kernel_stack` during context switches.
pub fn set_syscall_stack(rsp: u64) {
    let cpu_id = unsafe { crate::smp::arch_cpu_id() };
    let idx = cpu_id.min(MAX_CPUS - 1);
    unsafe {
        PER_CPU[idx].kernel_stack_top = rsp;
    }
}

/// Initialise the per-CPU SYSCALL state for `cpu_id`.
///
/// Writes the address of `PER_CPU[cpu_id]` into the `IA32_KERNEL_GS_BASE`
/// MSR so that `swapgs` on SYSCALL entry makes GS point directly to the
/// per-CPU struct.
fn init_per_cpu(cpu_id: usize) {
    let idx = cpu_id.min(MAX_CPUS - 1);
    unsafe {
        let per_cpu_ptr = core::ptr::addr_of!(PER_CPU[idx]) as u64;
        wrmsr(MSR_KERNEL_GSBASE, per_cpu_ptr);
    }
}

/// Configure SYSCALL/SYSRET MSRs and initialise per-CPU state for the BSP (CPU 0).
pub fn init() {
    unsafe {
        // Enable SCE (System Call Extensions) and NXE (No-Execute Enable) in EFER.
        // SCE is bit 0, NXE is bit 11.
        wrmsr(MSR_EFER, rdmsr(MSR_EFER) | 0x801);

        // STAR segments:
        //   bits[47:32] = 0x08  → SYSCALL CS = 0x08, SS = 0x10
        //   bits[63:48] = 0x10  → SYSRET64 CS = 0x10+16 = 0x20|3, SS = 0x10+8 = 0x18|3
        wrmsr(MSR_STAR, (0x0008u64 << 32) | (0x0010u64 << 48));

        // LSTAR: entry point for 64-bit SYSCALL.
        wrmsr(MSR_LSTAR, syscall_entry as *const () as u64);

        // FMASK: clear IF (bit 9) on SYSCALL so we run with interrupts disabled.
        wrmsr(MSR_FMASK, 1 << 9);
    }

    // BSP is CPU 0.
    init_per_cpu(0);
}

/// Initialise per-CPU SYSCALL state for an Application Processor.
///
/// Must be called after `apic::init()` (so `arch_cpu_id()` returns the
/// correct LAPIC-derived CPU index).  Called from `smp::sched_ap_entry`.
pub fn init_ap() {
    let cpu_id = unsafe { crate::smp::arch_cpu_id() };
    init_per_cpu(cpu_id);
}

// ── SYSCALL entry trampoline ──────────────────────────────────────────────────
//
// Uses per-CPU stacks and save slots accessed through the GS segment.
// FMASK clears IF on SYSCALL so no maskable interrupt can fire between
// swapgs and the callq, preventing GS from being in an inconsistent state.
//
// Register convention on SYSCALL entry:
//   rax = syscall number
//   rdi = a0, rsi = a1, rdx = a2, r10 = a3, r8 = a4, r9 = a5
//   rcx = user RIP (written by SYSCALL; NOT a user arg)
//   r11 = user RFLAGS (written by SYSCALL)
//
// syscall_dispatch(number, a0, a1, a2, a3, a4, a5, frame_ptr) uses SysV ABI:
//   rdi=number, rsi=a0, rdx=a1, rcx=a2, r8=a3, r9=a4
//   [rsp+8]=a5, [rsp+16]=frame_ptr  (stack args)
//
// Linux x86-64 syscall ABI: the kernel must preserve all registers except
// rax (return value), rcx (user RIP, trashed by SYSCALL), and r11 (user
// RFLAGS, trashed by SYSCALL).  We save rdi/rsi/rdx/r8/r9/r10 because the
// register-rearrangement below clobbers them before syscall_dispatch, and
// syscall_dispatch may further trash caller-saved regs.
//
// Stack layout (RSP grows downward from kernel_stack_top = 16-byte aligned):
//   top -  8 : r11  (user RFLAGS, for SYSRET)
//   top - 16 : rcx  (user RIP,    for SYSRET)   ← 16-byte aligned
//   top - 24 : r10  (user r10 = a3, to restore)
//   top - 32 : r9   (user r9  = a5, to restore) ← 16-byte aligned
//   top - 40 : r8   (user r8  = a4, to restore)
//   top - 48 : rdx  (user rdx = a2, to restore) ← 16-byte aligned
//   top - 56 : rsi  (user rsi = a1, to restore)
//   top - 64 : rdi  (user rdi = a0, to restore) ← 16-byte aligned
//   top - 72 : 0    (arg8 = frame_ptr)
//   top - 80 : r9   (arg7 = a5, original value)  RSP%16==0 before call ✓

core::arch::global_asm!(r#"
.section .text, "ax", @progbits
.global syscall_entry
.type   syscall_entry, @function
syscall_entry:
    // 1. Activate kernel GS (IA32_KERNEL_GS_BASE → GS; user GS stashed).
    swapgs

    // 2. Save user RSP in scratch slot, then switch to kernel stack.
    mov   gs:[8], rsp     // user RSP → scratch
    mov   rsp, gs:[0]     // RSP = kernel stack top (16-byte aligned)

    // 3. Build an IRETQ frame on the kernel stack.
    //    Layout: SS, RSP, RFLAGS, CS, RIP
    push  0x1B            // user data selector (DPL 3)
    push  gs:[8]          // user RSP
    push  r11             // user RFLAGS (from SYSCALL instruction)
    push  0x23            // user code selector (DPL 3)
    push  rcx             // user RIP (from SYSCALL instruction)

    // 4. Save user registers to complete the UserFrame.
    push  r11             // r11 (user RFLAGS)
    push  rcx             // rcx (user RIP)
    push  rax             // rax
    push  rdi             // rdi
    push  rsi             // rsi
    push  rdx             // rdx
    push  r8              // r8
    push  r9              // r9
    push  r10             // r10
    push  rbx             // rbx
    push  rbp             // rbp
    push  r12             // r12
    push  r13             // r13
    push  r14             // r14
    push  r15             // r15

    // 5. Prepare arguments for syscall_dispatch.
    // Our dispatch: number, a0, a1, a2, a3, a4, a5, frame_ptr

    mov   r11, rsp        // frame_ptr = current RSP (pointing to r15)

    // Stack args: [rsp+8]=a5, [rsp+16]=frame_ptr
    // Initial RSP was 16-byte aligned.
    // We pushed 21 words (gs[0] + 5-word iret + 15-regs) = 168 bytes.
    // 168 is not 16-aligned. We need one padding word to make it 176,
    // then the call will push 8 bytes to make it 184 (not aligned) or...
    // Actually, System V says RSP+8 must be 16-aligned on entry.
    // So RSP must be 16-aligned BEFORE the call.
    // 168 + 24 (3 pushes) = 192 (16-aligned).
    push  0               // padding (ensure 16-byte alignment for call)
    push  r11             // arg8 = frame_ptr
    push  r9              // arg7 = a5 (original value)

    // 6. Rearrange regs for System V 6-register calling convention.
    // number(rax), a0(rdi), a1(rsi), a2(rdx), a3(r10), a4(r8)
    mov   r9,  r8         // a4 (r8)  → r9  (arg6)
    mov   r8,  r10        // a3 (r10) → r8  (arg5)
    mov   rcx, rdx        // a2 (rdx) → rcx (arg4)
    mov   rdx, rsi        // a1 (rsi) → rdx (arg3)
    mov   rsi, rdi        // a0 (rdi) → rsi (arg2)
    mov   rdi, rax        // number (rax) → rdi (arg1)

    call  syscall_dispatch
    // rax = return value

    // 7. Clean up stack args.
    add   rsp, 24

    // 8. Restore user registers.
    pop   r15
    pop   r14
    pop   r13
    pop   r12
    pop   rbp
    pop   rbx
    pop   r10
    pop   r9
    pop   r8
    pop   rdx
    pop   rsi
    pop   rdi
    add   rsp, 8          // skip rax (it's the return value)
    pop   rcx             // restore user RIP
    pop   r11             // restore user RFLAGS

    // 9. Return to user space via IRETQ.
    swapgs
    iretq

// ── fork_ret_to_user — first entry into a child process after fork ───────
//
// Called from cpu_switch_to when a newly-forked task is first scheduled.
// RSP points to the GP register part of a UserFrame.
.global fork_ret_to_user
.type   fork_ret_to_user, @function
fork_ret_to_user:
    // UserFrame layout: r15, r14, r13, r12, rbp, rbx, r10, r9, r8, rdx, rsi, rdi, rax, rcx, r11,
    // followed by [rip, cs, rflags, rsp, ss].
    pop   r15
    pop   r14
    pop   r13
    pop   r12
    pop   rbp
    pop   rbx
    pop   r10
    pop   r9
    pop   r8
    pop   rdx
    pop   rsi
    pop   rdi
    pop   rax             // Restore rax (contains 0 in child)
    pop   rcx             // restore user RIP
    pop   r11             // restore user RFLAGS
    // Restore user GS (kernel GS was activated on SYSCALL entry).
    swapgs
    iretq

// ── arch_execve_return — drop into user space at a new entry / stack ──────
//
// Called from sched::replace_address_space after the new address space is
// installed.  Constructs an IRETQ frame on the kernel stack and iretq's.
// Never returns.
//
// System V AMD64 calling convention: rdi = entry, rsi = user_sp.
//
// GDT layout (from arch/x86_64/src/gdt.rs):
//   0x18 = user data  (DPL 3)  → SS  = 0x18|3 = 0x1B
//   0x20 = user code  (DPL 3)  → CS  = 0x20|3 = 0x23
//   RFLAGS = 0x202 (IF=1, reserved bit 1 set)
.global arch_execve_return
.type   arch_execve_return, @function
arch_execve_return:
    // Build 5-word IRET frame: [RIP, CS, RFLAGS, RSP, SS]
    // Pushed in reverse order (stack grows down).
    push  0x1B            // SS  = user data selector
    push  rsi             // RSP = user stack pointer
    push  0x202           // RFLAGS = IF=1
    push  0x23            // CS  = user code selector
    push  rdi             // RIP = entry point
    // Zero all general-purpose registers so the new process starts clean.
    xor   rax, rax
    xor   rbx, rbx
    xor   rcx, rcx
    xor   rdx, rdx
    xor   rsi, rsi
    xor   rdi, rdi
    xor   rbp, rbp
    xor   r8,  r8
    xor   r9,  r9
    xor   r10, r10
    xor   r11, r11
    xor   r12, r12
    xor   r13, r13
    xor   r14, r14
    xor   r15, r15
    // Restore user GS (kernel GS was activated on SYSCALL entry).
    swapgs
    iretq
"#);

extern "C" {
    fn syscall_entry();
}
