//! x86-64 SMP support — AP bringup via INIT/SIPI and a 16→64-bit trampoline.
//!
//! The BSP calls `smp_init(ncpus)` after its own subsystems are ready.
//! The function:
//!   1. Writes a minimal 32/64-bit GDT + GDTR at physical 0x7100/0x7120.
//!   2. Writes AP boot parameters (CR3, entry, per-AP stacks) at 0x7F00.
//!   3. Copies the trampoline code to physical 0x7000.
//!   4. Sends INIT+SIPI×2 to all APs via the LAPIC broadcast shorthand.
//!
//! Trampoline layout (physical page 0x7000, offsets from base):
//!   +0x000: 16-bit real-mode entry (executes at CS=0x0700, IP=0)
//!   +0x040: 32-bit protected-mode code
//!   +0x0C0: 64-bit long-mode code
//!
//! AP boot parameter block at physical 0x7F00:
//!   +0x00 (u64): CR3  (BSP page table root)
//!   +0x08 (u64): entry point (sched_ap_entry)
//!   +0x10 (u32): AP sequential counter (atomic xadd)
//!   +0x14 (u32): padding
//!   +0x18 (u64 × 8): kernel stack tops for APs 0..7

use super::apic;
use mm::buddy;
use core::sync::atomic::{AtomicUsize, Ordering};

// ── Online CPU accounting ─────────────────────────────────────────────────────

/// CPUs that have completed bringup and entered the scheduler (BSP counts as 1).
static ACTIVE_CPUS: AtomicUsize = AtomicUsize::new(1);

/// Number of online CPUs — used by the scheduler for idle-CPU selection and
/// by the TLB shootdown protocol to size the acknowledgement count.
pub fn active_cpu_count() -> usize {
    ACTIVE_CPUS.load(Ordering::Acquire)
}

#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn arch_active_cpu_count() -> usize {
    active_cpu_count()
}

// ── IPI vectors ───────────────────────────────────────────────────────────────

/// Reschedule IPI — handled in idt.rs (EOI + preempt_check).
pub const RESCHED_VECTOR: u32 = 0x40;
/// TLB shootdown IPI — handled in idt.rs (CR3 reload + ack).
pub const TLB_SHOOTDOWN_VECTOR: u32 = 0xFD;

/// Wait for any in-flight IPI from this CPU's LAPIC to be delivered
/// (ICR Delivery Status, bit 12).
#[cfg(target_arch = "x86_64")]
unsafe fn icr_wait_idle() {
    while apic::read(0x300) & (1 << 12) != 0 {
        core::hint::spin_loop();
    }
}

/// Send a reschedule IPI (vector 0x40) to the CPU with LAPIC ID `cpu`.
///
/// Called by `sched::trigger_preempt` for cross-CPU wake-ups.  Writing the
/// ICR of the *local* APIC is inherently per-CPU, so no lock is needed.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn arch_send_resched_ipi(cpu: usize) {
    icr_wait_idle();
    // ICR high: destination LAPIC ID in bits [31:24].
    apic::write(0x310, (cpu as u32) << 24);
    // ICR low: fixed delivery, physical destination, level=assert.
    apic::write(0x300, (1 << 14) | RESCHED_VECTOR);
}

/// Broadcast the TLB shootdown IPI (vector 0xFD) to every CPU but this one.
#[cfg(target_arch = "x86_64")]
pub unsafe fn send_tlb_shootdown_broadcast() {
    icr_wait_idle();
    apic::write(0x310, 0);
    // Shorthand 0b11 = all-excluding-self, fixed delivery, level=assert.
    apic::write(0x300, (3 << 18) | (1 << 14) | TLB_SHOOTDOWN_VECTOR);
}

// ── SMT topology ──────────────────────────────────────────────────────────────

/// Cached number of low APIC-ID bits that address the SMT (hyperthread)
/// level, from CPUID leaf 0xB.  `usize::MAX` = not yet queried.
static SMT_SHIFT: AtomicUsize = AtomicUsize::new(usize::MAX);

/// Bits of the APIC ID that select the thread within a core.
///
/// CPUID leaf 0xB (extended topology), subleaf 0, reports the SMT level:
/// EAX[4:0] is the number of APIC-ID bits to shift away to get the core ID
/// (1 with 2 threads/core, 0 without SMT).  QEMU models this with
/// `-smp N,cores=C,threads=T`.
#[cfg(target_arch = "x86_64")]
fn smt_shift() -> usize {
    let cached = SMT_SHIFT.load(Ordering::Relaxed);
    if cached != usize::MAX { return cached; }

    let mut shift = 0usize;
    let max_leaf = unsafe { core::arch::x86_64::__cpuid(0) }.eax;
    if max_leaf >= 0xB {
        let topo = unsafe { core::arch::x86_64::__cpuid_count(0xB, 0) };
        // ECX[15:8] = level type; 1 = SMT.  EBX = logical CPUs at level
        // (0 ⇒ leaf unsupported on this part).
        if (topo.ecx >> 8) & 0xFF == 1 && topo.ebx != 0 {
            shift = (topo.eax & 0x1F) as usize;
        }
    }
    SMT_SHIFT.store(shift, Ordering::Relaxed);
    shift
}

/// Physical core a logical CPU (LAPIC ID) belongs to.  Used by the scheduler
/// to prefer whole-idle cores over the busy sibling of a hyperthread pair.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn arch_core_of(cpu: usize) -> usize {
    cpu >> smt_shift()
}

// ── Trampoline page address ───────────────────────────────────────────────────
const TRAMPOLINE_BASE: usize = 0x7000;

// ── AP GDT / GDTR location within the page ───────────────────────────────────
const AP_GDT_BASE:  usize = 0x7100; // 4 × 8-byte GDT entries
const AP_GDTR_ADDR: usize = 0x7120; // 2-byte limit + 4-byte base

// ── AP boot parameter block ───────────────────────────────────────────────────
const AP_CR3_OFF:    usize = 0x7F00;
const AP_ENTRY_OFF:  usize = 0x7F08;
const AP_CTR_OFF:    usize = 0x7F10;
const AP_STACKS_OFF: usize = 0x7F18;

// ── AP startup trampoline (copied to 0x7000 at runtime) ──────────────────────
//
// The global_asm! blob is placed in section .ap_trampoline; smp_init() copies
// it to physical 0x7000 before sending SIPI.
//
// Absolute physical addresses used by the trampoline:
//   0x7120 — GDTR (loaded by lgdtl in 16-bit mode)
//   0x7040 — 32-bit code entry (target of first ljmpl)
//   0x70C0 — 64-bit code entry (target of second ljmpl)
//   0x7F00 — CR3
//   0x7F08 — entry function pointer
//   0x7F10 — atomic AP counter (u32)
//   0x7F18 — AP stack array (u64 × 8)
//
// GDT (written by smp_init to 0x7100):
//   0x00 — null descriptor
//   0x08 — 32-bit code  (P=1, S=1, D/B=1, G=1, Type=0xA)
//   0x10 — data         (P=1, S=1, D/B=1, G=1, Type=0x2)
//   0x18 — 64-bit code  (P=1, S=1, L=1,   G=1, Type=0xA)
core::arch::global_asm!(r#"
.section .ap_trampoline, "ax", @progbits
.code16
.global ap_trampoline_start
ap_trampoline_start:
    cli
    cld
    xor   ax, ax
    mov   ds, ax
    mov   es, ax
    mov   ss, ax
    lgdt  [0x7120]
    mov   eax, cr0
    or    al, 1
    mov   cr0, eax
    ljmp  0x08, 0x7040

// .org (not .balign): if a stage outgrows its slot, .org to an already-passed
// offset is a hard assembler error, whereas .balign silently pads to the NEXT
// boundary and the ljmp below lands mid-instruction.  (The 32-bit stage is
// 67 bytes — that exact failure shipped here once: .balign 0x80 placed the
// 64-bit stage at +0x100 while ljmp still jumped to +0x80.)
.org 0x40, 0x90
.code32
    mov   ax, 0x10
    mov   ds, ax
    mov   es, ax
    mov   ss, ax
    xor   ax, ax
    mov   fs, ax
    mov   gs, ax
    mov   eax, cr4
    or    eax, (1 << 5) | (1 << 16)
    mov   cr4, eax
    mov   eax, [0x7F00]
    mov   cr3, eax
    mov   ecx, 0xC0000080
    rdmsr
    or    eax, (1 << 8)
    wrmsr
    mov   eax, cr0
    or    eax, 0x80000000
    mov   cr0, eax
    ljmp  0x18, 0x70C0

.org 0xC0, 0x90
.code64
    xor   eax, eax
    mov   ds, eax
    mov   es, eax
    mov   ss, eax
    mov   rbx, 0x7F10
    mov   eax, 1
    lock xadd [rbx], eax
    mov   rbx, 0x7F18
    mov   rsp, [rbx + rax*8]
    call  qword ptr [0x7F08]
0:  hlt
    jmp   0b

.global ap_trampoline_end
ap_trampoline_end:
"#);

extern "C" {
    static ap_trampoline_start: u8;
    static ap_trampoline_end:   u8;
}

// ── Helpers ───────────────────────────────────────────────────────────────────
//
// BSP-side writes to the trampoline page go through the HHDM: with Limine
// base revision 1+ the kernel has no lower-half identity mapping, so raw
// physical pointers would fault.  (The APs themselves *execute* at the
// physical address — smp_init identity-maps the page for that.)

unsafe fn write32(phys: usize, val: u32) {
    (mm::phys_to_virt(phys) as *mut u32).write_volatile(val);
}

unsafe fn write64(phys: usize, val: u64) {
    (mm::phys_to_virt(phys) as *mut u64).write_volatile(val);
}

// ── arch_cpu_id — provides the logical CPU index ──────────────────────────────

/// Return the LAPIC ID of the calling CPU.
///
/// For xAPIC the ID lives in bits [31:24] of the APIC ID register.
/// On QEMU/typical hardware: BSP = 0, APs = 1, 2, 3 …
///
/// Used by `sched` to index the per-CPU state arrays.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn arch_cpu_id() -> usize {
    ((apic::read(apic::LAPIC_ID) >> 24) & 0xFF) as usize
}

// ── AP entry called from the long-mode trampoline ─────────────────────────────

/// Rust-side AP entry.
///
/// Called by the 64-bit trampoline after the stack is set up.  Brings this
/// CPU to full parity with the BSP, then hands off to the shared scheduler
/// loop (which parks until the BSP opens the scheduler):
///
///   1. LAPIC — enable, so `arch_cpu_id()` reads the right ID and IPIs and
///      the local timer can be delivered.
///   2. SSE — CR0/CR4 are per-CPU; user code uses XMM registers.
///   3. GDT + per-CPU TSS — ring-0 stack for interrupts from user mode.
///   4. IDT — `lidt` is a per-CPU register.
///   5. SYSCALL MSRs — EFER.SCE/STAR/LSTAR/FMASK are all per-CPU.
///   6. Local APIC timer — 100 Hz preemption ticks from BSP calibration.
///
/// IA32_PAT is step 0, before any of them. It is per-logical-processor like the
/// SYSCALL MSRs, but unlike them a divergence is silent: an AP that keeps its
/// reset PAT gives a different memory type to the same physical page than the
/// BSP does, which the SDM leaves undefined and which shows up as occasional
/// corruption rather than a fault. Doing it first also keeps the claim in
/// `paging::init_pat_ap` true — that this AP has touched nothing but write-back
/// memory when the value changes.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn sched_ap_entry() -> ! {
    super::paging::init_pat_ap();
    apic::init();
    super::enable_sse();
    super::gdt::init_cpu(arch_cpu_id());
    super::idt::load();
    super::syscall::init_ap();
    super::timer::init_local_timer();

    ACTIVE_CPUS.fetch_add(1, Ordering::AcqRel);

    sched::ap_entry()
}

// ── smp_init — bring up ncpus application processors ─────────────────────────

/// Bring up `ncpus` Application Processors.
///
/// Must be called after the identity-mapped page table is active and the
/// physical allocator is initialised.
///
/// # Safety
/// Must be called from the BSP after `apic::init()`.  Physical addresses
/// 0x7000–0x7FFF must be identity-mapped and writeable.
#[cfg(target_arch = "x86_64")]
pub unsafe fn smp_init(ncpus: usize) {
    if ncpus == 0 { return; }
    let ncpus = ncpus.min(sched::MAX_CPUS - 1);

    // ── Identity-map the trampoline page ─────────────────────────────────────
    // The APs execute at physical 0x7000 and keep fetching from that address
    // in the first instructions *after* enabling paging with the BSP's CR3
    // (they also read the temporary GDT at 0x7100 there).  Limine base
    // revision 1+ provides no lower-half identity map, so add one page.
    // A failure (e.g. already mapped by a direct-boot identity map) is fine.
    {
        let cr3: u64;
        core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
        let root = (cr3 & !0xFFF) as usize;
        let _ = super::paging::map_4k(
            root,
            TRAMPOLINE_BASE,
            TRAMPOLINE_BASE,
            super::paging::PageTableFlags::PRESENT | super::paging::PageTableFlags::WRITABLE,
        );
    }

    // ── GDT at 0x7100 ─────────────────────────────────────────────────────────
    write64(AP_GDT_BASE + 0x00, 0x0000_0000_0000_0000); // null
    write64(AP_GDT_BASE + 0x08, 0x00CF_9A00_0000_FFFF); // 32-bit code
    write64(AP_GDT_BASE + 0x10, 0x00CF_9200_0000_FFFF); // data
    write64(AP_GDT_BASE + 0x18, 0x00AF_9A00_0000_FFFF); // 64-bit code (L=1)

    // ── GDTR at 0x7120 (limit = 0x1F, base = 0x7100) ─────────────────────────
    (mm::phys_to_virt(AP_GDTR_ADDR) as *mut u16).write_volatile(0x001F);
    (mm::phys_to_virt(AP_GDTR_ADDR + 2) as *mut u32).write_volatile(AP_GDT_BASE as u32);

    // ── AP boot parameters at 0x7F00 ─────────────────────────────────────────
    // CR3 — share the BSP's page table (assumed < 4 GiB for this write)
    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    write64(AP_CR3_OFF, cr3);

    // Entry function pointer
    write64(AP_ENTRY_OFF, sched_ap_entry as *const () as u64);

    // AP sequential counter (starts at 0)
    write32(AP_CTR_OFF,     0);
    write32(AP_CTR_OFF + 4, 0); // padding

    // Per-AP kernel stacks (64 KiB each).
    //
    // The SIPI below is a broadcast shorthand: ALL non-BSP APs start and use
    // an atomic counter to grab a sequential index, then load
    // AP_STACKS_OFF[index] as their stack.  If any index's entry is 0 (because
    // its alloc failed) the AP starts with SP=0 and immediately faults.
    //
    // Guard: only proceed to the SIPI if EVERY requested stack was allocated.
    // If OOM prevents any allocation, skip the SIPI entirely — some APs won't
    // start, but no AP will crash with a null stack.
    // Stack tops are stored as HHDM *virtual* addresses: the AP loads RSP
    // only after paging is enabled with the kernel CR3, where the HHDM is
    // guaranteed but a physical identity map is not.
    let mut stacks_ok = true;
    for i in 0..ncpus {
        match buddy::alloc(4) {
            Some(stack) => {
                let stack_virt = mm::phys_to_virt(stack);
                (stack_virt as *mut u8).write_bytes(0, buddy::PAGE_SIZE * 16);
                write64(AP_STACKS_OFF + i * 8, (stack_virt + buddy::PAGE_SIZE * 16) as u64);
            }
            None => { stacks_ok = false; break; }
        }
    }
    if !stacks_ok { return; }

    // ── Copy trampoline to physical 0x7000 (via HHDM) ────────────────────────
    let src = &ap_trampoline_start as *const u8;
    let end = &ap_trampoline_end   as *const u8;
    let len = end as usize - src as usize;
    core::ptr::copy_nonoverlapping(src, mm::phys_to_virt(TRAMPOLINE_BASE) as *mut u8, len);
    // Ensure trampoline is visible to APs (write-back flush via sfence).
    core::arch::asm!("sfence", options(nostack, nomem));

    // ── INIT / SIPI × 2 via LAPIC broadcast ──────────────────────────────────
    // ICR high = 0 (destination ignored when using shorthand)
    apic::write(0x310, 0);

    // INIT assert (delivery=INIT, level=assert, trigger=level, shorthand=all-excl-self)
    // 0xCC500 = (11<<18) | (1<<15) | (1<<14) | (5<<8)
    apic::write(0x300, 0x000C_C500);

    // ~10 ms spin (no calibrated delay yet)
    for _ in 0..10_000_000usize { core::hint::spin_loop(); }

    // SIPI #1 (vector 0x07 → startup at 0x7000)
    // 0xC0607 = (11<<18) | (6<<8) | 0x07
    apic::write(0x300, 0x000C_0607);

    // ~200 µs spin
    for _ in 0..200_000usize { core::hint::spin_loop(); }

    // SIPI #2 (Intel MP spec recommends two SIPIs)
    apic::write(0x300, 0x000C_0607);
}
