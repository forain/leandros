//! ACPI S3 (suspend to RAM): the CPU-level half (lane s3core).
//!
//! The platform half (FADT registers, `_PTS`/`_WAK`, device hooks) lives in
//! `kernel/src/pm_s3.rs`. This file owns what only the BSP's CPU state needs:
//!
//!  * the firmware wake stub, copied to physical 0x7400 (offset 0x400 of the
//!    AP trampoline page): real mode -> protected mode -> long mode on a
//!    temporary page table root below 4 GiB, then a jump to the higher half
//!    `pm_s3_resume64`;
//!  * `pm_s3_lowlevel`, a setjmp-like routine: it returns 0 when the platform
//!    sleep call comes back (S3 not entered) and 1 when it is "returned to"
//!    by `pm_s3_resume64` after a real wake;
//!  * the CPU context save (`save_ctx`) and the Rust half of the restore
//!    (`s3_restore_cpu`);
//!  * interrupt controller / clock restore helpers.
//!
//! Layout inside the AP page (0x7000): AP trampoline 0x000-0x13F, S3 wake
//! stub 0x400-0x5FF (own GDT at 0x5C0, GDTR at 0x5E0), S3 parameter block
//! 0xD00, AP GDT 0xE00, AP parameters 0xF00. The 16-bit stack is 0x7CF0.
use core::arch::{asm, global_asm};
use core::ptr::{addr_of, addr_of_mut};
use core::sync::atomic::{AtomicU64, Ordering};

use super::{apic, gdt, idt, ioapic, paging, syscall, timer};

pub const WAKE_PHYS: usize = 0x7400;
const PARAM: usize = 0x7D00;
const SIG: u32 = 0x5733_534C; // "LS3W"
/// Stub flag: sound the PC speaker in real mode right after the wake (breadcrumb).
pub const FLAG_BEEP: u32 = 1;
/// Stub flag: restore `IA32_MISC_ENABLE` from the parameter block first.
pub const FLAG_MISC: u32 = 2;

const MSR_EFER: u32 = 0xC000_0080;
const MSR_FS_BASE: u32 = 0xC000_0100;
const MSR_GS_BASE: u32 = 0xC000_0101;
const MSR_KGS_BASE: u32 = 0xC000_0102;
const MSR_MISC_ENABLE: u32 = 0x1A0;

/// Saved BSP state. Offsets 0..48 are used by the assembly below.
#[repr(C)]
pub struct S3Ctx {
    pub rsp: u64,        // 0
    pub cr0: u64,        // 8
    pub cr3: u64,        // 16
    pub cr4: u64,        // 24
    pub efer: u64,       // 32
    pub xcr0: u64,       // 40
    pub gdtr: [u64; 2],  // 48
    pub idtr: [u64; 2],  // 64
    pub fs_base: u64,    // 80
    pub gs_base: u64,    // 88
    pub kgs_base: u64,   // 96
    pub misc_enable: u64,// 104
}

#[no_mangle]
pub static mut S3_CTX: S3Ctx = S3Ctx {
    rsp: 0, cr0: 0, cr3: 0, cr4: 0, efer: 0, xcr0: 0, gdtr: [0; 2], idtr: [0; 2],
    fs_base: 0, gs_base: 0, kgs_base: 0, misc_enable: 0,
};

// Real-mode wake stub. Position dependent (absolute physical addresses), like
// the AP trampoline; `.org` (not `.balign`) so an overgrown stage is a hard
// assembler error.
global_asm!(r#"
.section .s3_wake, "ax", @progbits
.code16
.global s3_wake_start
s3_wake_start:
    cli
    cld
    xor   ax, ax
    mov   ds, ax
    mov   es, ax
    mov   ss, ax
    mov   sp, 0x7CF0
    cmp   dword ptr [0x7D00], 0x5733534C
    jne   9f
    test  dword ptr [0x7D04], 1
    jz    3f
    mov   al, 0xB6
    out   0x43, al
    mov   al, 0x4C
    out   0x42, al
    mov   al, 0x05
    out   0x42, al
    in    al, 0x61
    or    al, 3
    out   0x61, al
    mov   ecx, 0x30000000
2:
    dec   ecx
    jnz   2b
    in    al, 0x61
    and   al, 0xFC
    out   0x61, al
3:
    test  dword ptr [0x7D04], 2
    jz    4f
    mov   ecx, 0x1A0
    mov   eax, dword ptr [0x7D18]
    mov   edx, dword ptr [0x7D1C]
    wrmsr
4:
    lgdt  [0x75E0]
    mov   eax, cr0
    or    al, 1
    mov   cr0, eax
    ljmp  0x08, 0x74A0
9:
    hlt
    jmp   9b

.org 0xA0, 0x90
.code32
    mov   ax, 0x10
    mov   ds, ax
    mov   es, ax
    mov   ss, ax
    xor   ax, ax
    mov   fs, ax
    mov   gs, ax
    mov   eax, cr4
    or    eax, (1 << 5)
    mov   cr4, eax
    mov   eax, dword ptr [0x7D08]
    mov   cr3, eax
    mov   eax, 0x80000001
    cpuid
    mov   esi, edx
    mov   ecx, 0xC0000080
    rdmsr
    or    eax, (1 << 8)
    bt    esi, 20
    jnc   5f
    or    eax, (1 << 11)
5:
    wrmsr
    mov   eax, cr0
    or    eax, 0x80010001
    mov   cr0, eax
    ljmp  0x18, 0x7520

.org 0x120, 0x90
.code64
    xor   eax, eax
    mov   ds, eax
    mov   es, eax
    mov   ss, eax
    mov   rax, qword ptr [0x7D10]
    jmp   rax

.org 0x1C0, 0x90
    .quad 0
    .quad 0x00CF9A000000FFFF
    .quad 0x00CF92000000FFFF
    .quad 0x00AF9A000000FFFF
.org 0x1E0, 0x90
    .word 31
    .long 0x75C0
.global s3_wake_end
s3_wake_end:
"#);

extern "C" {
    static s3_wake_start: u8;
    static s3_wake_end: u8;
    /// Returns 0 if `enter` returned (S3 not entered), 1 after a real wake.
    pub fn pm_s3_lowlevel(ctx: *mut S3Ctx, enter: extern "C" fn()) -> u64;
    fn pm_s3_resume64();
}

global_asm!(r#"
.section .text.pm_s3, "ax", @progbits
.global pm_s3_lowlevel
pm_s3_lowlevel:
    push  rbx
    push  rbp
    push  r12
    push  r13
    push  r14
    push  r15
    pushfq
    mov   qword ptr [rdi], rsp
    call  rsi
    popfq
    pop   r15
    pop   r14
    pop   r13
    pop   r12
    pop   rbp
    pop   rbx
    xor   eax, eax
    ret

// Entered by the wake stub: long mode, temporary CR3, no valid stack, IF=0.
.global pm_s3_resume64
pm_s3_resume64:
    lea   rbx, [rip + S3_CTX]
    mov   rax, qword ptr [rbx + 24]
    mov   cr4, rax
    mov   ecx, 0xC0000080
    mov   eax, dword ptr [rbx + 32]
    mov   edx, dword ptr [rbx + 36]
    wrmsr
    mov   eax, dword ptr [rbx + 40]
    mov   edx, dword ptr [rbx + 44]
    test  eax, eax
    jz    2f
    xor   ecx, ecx
    xsetbv
2:
    mov   rax, qword ptr [rbx + 8]
    mov   cr0, rax
    mov   rax, qword ptr [rbx + 16]
    mov   cr3, rax
    mov   rsp, qword ptr [rbx]
    call  s3_restore_cpu
    popfq
    pop   r15
    pop   r14
    pop   r13
    pop   r12
    pop   rbp
    pop   rbx
    mov   eax, 1
    ret
"#);

unsafe fn rdmsr(msr: u32) -> u64 {
    let (lo, hi): (u32, u32);
    asm!("rdmsr", in("ecx") msr, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
    ((hi as u64) << 32) | lo as u64
}
unsafe fn wrmsr(msr: u32, v: u64) {
    asm!("wrmsr", in("ecx") msr, in("eax") v as u32, in("edx") (v >> 32) as u32, options(nomem, nostack, preserves_flags));
}

fn is_intel() -> bool {
    core::arch::x86_64::__cpuid(0).ebx == 0x756e_6547
}

// Temporary page table pages (PML4, PDPT, PD, PT), all below 4 GiB; allocated once.
static TEMP_PAGES: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

unsafe fn zero_page(phys: usize) {
    core::ptr::write_bytes(mm::phys_to_virt(phys) as *mut u8, 0, 4096);
}

/// Build the temporary root: kernel half copied from the live root, and a
/// 4 KiB identity map of the 0x7000 page. Returns its physical address.
unsafe fn build_temp_root() -> Result<u64, &'static str> {
    if TEMP_PAGES[0].load(Ordering::Relaxed) == 0 {
        for p in TEMP_PAGES.iter() {
            let a = mm::buddy::alloc_below(0, 1usize << 32).ok_or("no page below 4 GiB for the wake page table")?;
            zero_page(a);
            p.store(a as u64, Ordering::Relaxed);
        }
        let t = |i: usize| mm::phys_to_virt(TEMP_PAGES[i].load(Ordering::Relaxed) as usize) as *mut u64;
        t(0).write_volatile(TEMP_PAGES[1].load(Ordering::Relaxed) | 3);
        t(1).write_volatile(TEMP_PAGES[2].load(Ordering::Relaxed) | 3);
        t(2).write_volatile(TEMP_PAGES[3].load(Ordering::Relaxed) | 3);
        t(3).add(0x7000 >> 12).write_volatile(0x7000 | 3);
    }
    let live: u64;
    asm!("mov {}, cr3", out(reg) live, options(nomem, nostack));
    let src = mm::phys_to_virt((live & !0xFFF) as usize) as *const u64;
    let dst = mm::phys_to_virt(TEMP_PAGES[0].load(Ordering::Relaxed) as usize) as *mut u64;
    for i in 256..512 { dst.add(i).write_volatile(src.add(i).read_volatile()); }
    Ok(TEMP_PAGES[0].load(Ordering::Relaxed))
}

unsafe fn page_w32(phys: usize, v: u32) { (mm::phys_to_virt(phys) as *mut u32).write_volatile(v); }

/// Copy the wake stub to 0x7400 and fill the parameter block. Call with the
/// final flags just before the sleep; may fail (before `_PTS`).
pub unsafe fn prepare_wake(beep: bool) -> Result<(), &'static str> {
    let root = build_temp_root()?;
    let len = addr_of!(s3_wake_end) as usize - addr_of!(s3_wake_start) as usize;
    if len > 0x200 { return Err("wake stub larger than 512 bytes"); }
    core::ptr::copy_nonoverlapping(addr_of!(s3_wake_start), mm::phys_to_virt(WAKE_PHYS) as *mut u8, len);
    save_ctx();
    let c = &*addr_of!(S3_CTX);
    let mut flags = if beep { FLAG_BEEP } else { 0 };
    if is_intel() { flags |= FLAG_MISC; }
    page_w32(PARAM, SIG);
    page_w32(PARAM + 4, flags);
    page_w32(PARAM + 8, root as u32);
    page_w32(PARAM + 0xC, (root >> 32) as u32);
    let r = pm_s3_resume64 as usize as u64;
    page_w32(PARAM + 0x10, r as u32);
    page_w32(PARAM + 0x14, (r >> 32) as u32);
    page_w32(PARAM + 0x18, c.misc_enable as u32);
    page_w32(PARAM + 0x1C, (c.misc_enable >> 32) as u32);
    Ok(())
}

/// Snapshot the BSP control state (IF must be off).
pub unsafe fn save_ctx() {
    let c = &mut *addr_of_mut!(S3_CTX);
    asm!("mov {}, cr0", out(reg) c.cr0, options(nomem, nostack));
    asm!("mov {}, cr3", out(reg) c.cr3, options(nomem, nostack));
    asm!("mov {}, cr4", out(reg) c.cr4, options(nomem, nostack));
    c.efer = rdmsr(MSR_EFER);
    c.xcr0 = 0;
    if c.cr4 & (1 << 18) != 0 {
        let (lo, hi): (u32, u32);
        asm!("xgetbv", in("ecx") 0u32, out("eax") lo, out("edx") hi, options(nomem, nostack, preserves_flags));
        c.xcr0 = ((hi as u64) << 32) | lo as u64;
    }
    asm!("sgdt [{}]", in(reg) c.gdtr.as_mut_ptr(), options(nostack));
    asm!("sidt [{}]", in(reg) c.idtr.as_mut_ptr(), options(nostack));
    c.fs_base = rdmsr(MSR_FS_BASE);
    c.gs_base = rdmsr(MSR_GS_BASE);
    c.kgs_base = rdmsr(MSR_KGS_BASE);
    c.misc_enable = if is_intel() { rdmsr(MSR_MISC_ENABLE) } else { 0 };
}

/// Rust half of the CPU restore: CR0/CR3/CR4/EFER/XCR0 and the stack are
/// already back (assembly above); this redoes everything the per-CPU init does.
#[no_mangle]
pub unsafe extern "C" fn s3_restore_cpu() {
    super::enable_sse(false);
    paging::init_pat_ap();
    gdt::init_cpu(0); // rewrites the TSS descriptor as available: ltr cannot #GP on BUSY
    idt::load();
    syscall::init_ap();
    let c = &*addr_of!(S3_CTX);
    wrmsr(MSR_FS_BASE, c.fs_base);
    wrmsr(MSR_GS_BASE, c.gs_base);
    wrmsr(MSR_KGS_BASE, c.kgs_base);
    super::smp::seed_cpu_id_fastpath();
}

/// The exact same restore, called without a firmware round trip (`pm_test=core`).
pub unsafe fn restore_cpu_in_place() { s3_restore_cpu(); }

// ---- interrupt controllers and clocks ------------------------------------

/// Resume the interrupt hardware: PIC masked, LAPIC re-enabled, IOAPIC redirection table back.
pub unsafe fn resume_irq_hw(io: &ioapic::PmState) {
    apic::init();
    ioapic::pm_restore(io);
}

/// Re-seed the monotonic clock (TSC epoch and every raw-TSC variable) so that
/// CLOCK_MONOTONIC continues from `mono_ns`; returns the TSC delta applied.
pub unsafe fn rebase_clock(mono_ns: u64) -> u64 { timer::pm_rebase(mono_ns) }

/// Then restart this CPU's local timer (after the rebase, IRQs still off).
pub unsafe fn restart_timer() { timer::init_local_timer(); apic::eoi(); }
