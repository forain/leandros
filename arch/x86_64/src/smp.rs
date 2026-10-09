//! x86-64 SMP support — AP bringup via targeted INIT/SIPI and a 16→64-bit
//! trampoline.
//!
//! The BSP calls `smp_init()` after its own subsystems are ready. It starts
//! the APs `cpus::discover` found in the ACPI MADT **one at a time, each by
//! its APIC ID** — never with the all-excluding-self broadcast, which also
//! wakes processors the kernel has no index or stack for (more than
//! `MAX_CPUS` threads, or CPUs the firmware disabled). Per AP:
//!
//!   1. assign it the next dense CPU index (`cpus::assign`) and a 64 KiB
//!      stack, publish the stack and the AP's APIC ID in the parameter block;
//!   2. SIPI, SIPI (INIT went to every target once, up front, followed by the
//!      single 10 ms wait the MP protocol asks for);
//!   3. the AP computes its own APIC ID from CPUID and claims the parameter
//!      block with `lock cmpxchg` only if the published ID is its own, takes
//!      the stack, and marks the block done;
//!   4. the BSP waits until the AP has counted itself online, then moves on.
//!
//! An AP that does not claim in time is fenced off: the BSP swaps the ID out
//! with the same cmpxchg, so a late riser finds a foreign ID and parks with
//! `cli; hlt` instead of sharing the next AP's stack. Its index is reused.
//!
//! Trampoline layout (physical page 0x7000, offsets from base):
//!   +0x000: 16-bit real-mode entry (executes at CS=0x0700, IP=0)
//!   +0x040: 32-bit protected-mode code
//!   +0x0C0: 64-bit long-mode code
//!   +0xE00: temporary GDT (4 entries), +0xE20: its GDTR
//!
//! AP boot parameter block at physical 0x7F00:
//!   +0x00 (u64): CR3  (BSP page table root)
//!   +0x08 (u64): entry point (sched_ap_entry)
//!   +0x10 (u32): TARGET — APIC ID allowed to claim, or IDLE/CLAIMED/DONE
//!   +0x18 (u64): kernel stack top for the AP being started

use super::{apic, cpus};
use mm::buddy;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

// ── Online CPU accounting ─────────────────────────────────────────────────────

/// CPUs that have completed bringup and entered the scheduler (BSP counts as 1).
/// `smp_init` starts APs strictly one after another, so the online CPUs are
/// always exactly the dense indices `0..ACTIVE_CPUS`.
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

/// Send a reschedule IPI (vector 0x40) to logical CPU `cpu` (dense index).
///
/// Called by `sched::trigger_preempt` for cross-CPU wake-ups.  Writing the
/// ICR of the *local* APIC is inherently per-CPU, so no lock is needed.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn arch_send_resched_ipi(cpu: usize) {
    let dest = cpus::apic_id_of(cpu);
    if dest == cpus::NO_APIC { return; }
    // Fixed delivery, physical destination, level=assert.
    apic::send_ipi(dest, apic::ICR_ASSERT | RESCHED_VECTOR);
}

/// Send an NMI to logical CPU `cpu` (the tick watchdog's stall probe; see
/// `exc_nmi` in idt.rs). Returns false when the CPU has no APIC ID.
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn arch_send_nmi(cpu: usize) -> bool {
    let dest = cpus::apic_id_of(cpu);
    if dest == cpus::NO_APIC { return false; }
    // Delivery mode NMI (100b); the vector field is ignored.
    apic::send_ipi(dest, apic::ICR_ASSERT | (4 << 8));
    true
}

/// Broadcast the TLB shootdown IPI (vector 0xFD) to every CPU but this one.
/// (A fixed-vector IPI, harmless to parked CPUs: they sit with IRQs masked.)
#[cfg(target_arch = "x86_64")]
pub unsafe fn send_tlb_shootdown_broadcast() {
    apic::send_ipi(0, apic::ICR_ALL_BUT_SELF | apic::ICR_ASSERT | TLB_SHOOTDOWN_VECTOR);
}

/// Send the TLB shootdown IPI (vector 0xFD) to logical CPU `cpu`.
#[cfg(target_arch = "x86_64")]
pub unsafe fn send_tlb_shootdown_ipi(cpu: usize) {
    let dest = cpus::apic_id_of(cpu);
    if dest == cpus::NO_APIC { return; }
    apic::send_ipi(dest, apic::ICR_ASSERT | TLB_SHOOTDOWN_VECTOR);
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

/// Physical core a logical CPU (dense index) belongs to: its APIC ID with
/// the SMT bits shifted away. Used by the scheduler to prefer whole-idle
/// cores over the busy sibling of a hyperthread pair. (Shifting the *index*
/// would be wrong as soon as APIC IDs are sparse.)
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub extern "C" fn arch_core_of(cpu: usize) -> usize {
    let id = cpus::apic_id_of(cpu);
    if id == cpus::NO_APIC { return cpu | 1 << 40; } // unique, matches nothing real
    (id as usize) >> smt_shift()
}

// ── Trampoline page address ───────────────────────────────────────────────────
const TRAMPOLINE_BASE: usize = 0x7000;

// ── AP GDT / GDTR location within the page ───────────────────────────────────
const AP_GDT_BASE:  usize = 0x7E00; // 4 × 8-byte GDT entries
const AP_GDTR_ADDR: usize = 0x7E20; // 2-byte limit + 4-byte base

// ── AP boot parameter block ───────────────────────────────────────────────────
const AP_CR3_OFF:    usize = 0x7F00;
const AP_ENTRY_OFF:  usize = 0x7F08;
const AP_TARGET_OFF: usize = 0x7F10;
const AP_STACK_OFF:  usize = 0x7F18;

/// TARGET values that are never an APIC ID. (0xFFFFFFFF is the x2APIC
/// broadcast ID; the two below it are not assigned to processors.)
const TARGET_IDLE:    u32 = 0xFFFF_FFFF;
const TARGET_CLAIMED: u32 = 0xFFFF_FFFE;
const TARGET_DONE:    u32 = 0xFFFF_FFFD;

// ── AP startup trampoline (copied to 0x7000 at runtime) ──────────────────────
//
// The global_asm! blob is placed in section .ap_trampoline; smp_init() copies
// it to physical 0x7000 before sending SIPI.
//
// Absolute physical addresses used by the trampoline:
//   0x7E20 — GDTR (loaded by lgdtl in 16-bit mode)
//   0x7040 — 32-bit code entry (target of first ljmpl)
//   0x70C0 — 64-bit code entry (target of second ljmpl)
//   0x7F00 — CR3
//   0x7F08 — entry function pointer
//   0x7F10 — TARGET (u32, claimed with lock cmpxchg)
//   0x7F18 — stack top
//
// GDT (written by smp_init to 0x7E00):
//   0x00 — null descriptor
//   0x08 — 32-bit code  (P=1, S=1, D/B=1, G=1, Type=0xA)
//   0x10 — data         (P=1, S=1, D/B=1, G=1, Type=0x2)
//   0x18 — 64-bit code  (P=1, S=1, L=1,   G=1, Type=0xA)
//
// EFER.NXE is set together with LME (when CPUID says NX exists): the BSP's
// page tables may carry NX bits, which are *reserved* bits — a #PF on the
// first stack push — until NXE is on.
//
// Labels avoid 0/1-only numbers (they read as binary literals in Intel syntax).
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
    lgdt  [0x7E20]
    mov   eax, cr0
    or    al, 1
    mov   cr0, eax
    ljmp  0x08, 0x7040

// .org (not .balign): if a stage outgrows its slot, .org to an already-passed
// offset is a hard assembler error, whereas .balign silently pads to the NEXT
// boundary and the ljmp below lands mid-instruction.  (That exact failure
// shipped here once: .balign 0x80 placed the 64-bit stage at +0x100 while
// ljmp still jumped to +0x80.)
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
    or    eax, 0x80000000
    mov   cr0, eax
    ljmp  0x18, 0x70C0

.org 0xC0, 0x90
.code64
    xor   eax, eax
    mov   ds, eax
    mov   es, eax
    mov   ss, eax
    // Own APIC ID -> r8d: CPUID leaf 0xB EDX (x2APIC ID) when the leaf
    // exists, else leaf 1 EBX[31:24]. Same rule as cpus::own_apic_id().
    xor   eax, eax
    cpuid
    cmp   eax, 0xB
    jb    2f
    mov   eax, 0xB
    xor   ecx, ecx
    cpuid
    test  ebx, ebx
    jz    2f
    mov   r8d, edx
    jmp   3f
2:
    mov   eax, 1
    cpuid
    shr   ebx, 24
    mov   r8d, ebx
3:
    // Claim the parameter block only if it is addressed to us.
    mov   eax, r8d
    mov   ecx, {claimed}
    lock cmpxchg dword ptr [0x7F10], ecx
    jne   4f
    mov   rsp, qword ptr [0x7F18]
    mov   dword ptr [0x7F10], {done}
    call  qword ptr [0x7F08]
4:
    cli
    hlt
    jmp   4b

.global ap_trampoline_end
ap_trampoline_end:
"#,
    claimed = const TARGET_CLAIMED,
    done = const TARGET_DONE,
);

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

unsafe fn write64(phys: usize, val: u64) {
    (mm::phys_to_virt(phys) as *mut u64).write_volatile(val);
}

fn target() -> &'static AtomicU32 {
    unsafe { &*(mm::phys_to_virt(AP_TARGET_OFF) as *const AtomicU32) }
}

/// Spin until `cond()` or `ns` nanoseconds pass; returns the final `cond()`.
fn wait_ns(ns: u64, cond: impl Fn() -> bool) -> bool {
    let t0 = super::timer::monotonic_ns();
    loop {
        if cond() { return true; }
        if super::timer::monotonic_ns().wrapping_sub(t0) >= ns { return cond(); }
        core::hint::spin_loop();
    }
}

// ── arch_cpu_id — provides the logical CPU index ──────────────────────────────

/// `IA32_TSC_AUX` — the scratch MSR `RDTSCP` returns in ECX. Holds this CPU's
/// dense index (the same thing Linux keeps there for `getcpu`).
const MSR_TSC_AUX: u32 = 0xC000_0103;

/// Set once the BSP has confirmed `RDTSCP` exists *and* seeded its own
/// `IA32_TSC_AUX`. Every AP seeds its own at the top of `sched_ap_entry`,
/// before it can reach an `arch_cpu_id()` caller, so a `true` here means
/// every CPU that can observe it has a valid value.
static CPU_ID_VIA_TSC_AUX: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

#[cfg(target_arch = "x86_64")]
fn has_rdtscp() -> bool {
    // CPUID.80000001H:EDX[27] = RDTSCP (which implies IA32_TSC_AUX).
    core::arch::x86_64::__cpuid(0x8000_0001).edx & (1 << 27) != 0
}

/// Publish this CPU's dense index into `IA32_TSC_AUX` so `arch_cpu_id()` can
/// read it without touching the APIC. Returns the index, or `None` for a CPU
/// `cpus` has no slot for. Idempotent.
///
/// Called by the BSP from `apic::init()` and by every AP first thing in
/// `sched_ap_entry`.
///
/// # Why a register and not the LAPIC
/// `arch_cpu_id()` used to read the LAPIC ID register on every call, and
/// `sched::current_pid()` calls it every time. The APIC page is uncacheable, so
/// that read costs a trip to the memory controller (~100 ns) where the atomic
/// load it guards costs ~1 ns. Profiling a live COSMIC session on x86_64/KVM
/// put **12.5% of all running guest CPU time** on the instruction immediately
/// after that load — classic long-latency-load skid. aarch64 never showed it
/// because `mrs mpidr_el1` is a register read.
#[cfg(target_arch = "x86_64")]
pub unsafe fn seed_cpu_id_fastpath() -> Option<usize> {
    // The BSP is index 0 even before `cpus::discover` ran.
    let idx = if cpus::assigned() <= 1 { Some(0) } else { cpus::index_of(cpus::own_apic_id()) }?;
    // CPUID.80000001H:EDX[27] = RDTSCP. Present on every CPU we care about and
    // on QEMU TCG's `-cpu max`, but the APIC path stays as the fallback rather
    // than assuming it: getting this wrong hands out the wrong per-CPU slot,
    // which corrupts scheduler state silently instead of faulting.
    if has_rdtscp() {
        apic::wrmsr(MSR_TSC_AUX, idx as u64);
        if idx == 0 { CPU_ID_VIA_TSC_AUX.store(true, Ordering::Release); }
    }
    Some(idx)
}

/// Return the dense logical CPU index (0 = BSP, 0..active_cpu_count()) of
/// the calling CPU — never the APIC ID, which may be sparse or large.
///
/// Used by `sched` to index the per-CPU state arrays, so it sits on the hot
/// path of `current_pid()`/`current_tgid()` and must stay cheap — see
/// [`seed_cpu_id_fastpath`].
#[cfg(target_arch = "x86_64")]
#[no_mangle]
pub unsafe extern "C" fn arch_cpu_id() -> usize {
    if CPU_ID_VIA_TSC_AUX.load(Ordering::Acquire) {
        let mut aux: u32 = 0;
        // RDTSCP returns IA32_TSC_AUX in ECX; the timestamp itself is ignored.
        let _ = core::arch::x86_64::__rdtscp(&mut aux as *mut u32);
        return aux as usize;
    }
    // No RDTSCP (or not seeded yet): only the BSP runs before the first AP is
    // assigned; afterwards translate the APIC ID (slow, but only on CPUs
    // without RDTSCP).
    if cpus::assigned() <= 1 { return 0; }
    cpus::index_of(cpus::own_apic_id()).unwrap_or(0)
}

// ── AP entry called from the long-mode trampoline ─────────────────────────────

/// Rust-side AP entry.
///
/// Called by the 64-bit trampoline after the stack is set up.  Brings this
/// CPU to full parity with the BSP, then hands off to the shared scheduler
/// loop (which parks until the BSP opens the scheduler):
///
///   1. CPU index — `IA32_TSC_AUX`, before anything can ask `arch_cpu_id()`.
///   2. LAPIC — enable (in the BSP's xAPIC/x2APIC mode), so IPIs and the
///      local timer can be delivered.
///   3. SSE — CR0/CR4 are per-CPU; user code uses XMM registers.
///   4. GDT + per-CPU TSS — ring-0 stack for interrupts from user mode.
///   5. IDT — `lidt` is a per-CPU register.
///   6. SYSCALL MSRs — EFER.SCE/STAR/LSTAR/FMASK are all per-CPU.
///   7. Local APIC timer — 100 Hz preemption ticks from BSP calibration.
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
    if seed_cpu_id_fastpath().is_none() {
        // Not a CPU smp_init started (cannot happen with the claim protocol);
        // stay out of every per-CPU table.
        loop { core::arch::asm!("cli", "hlt"); }
    }
    apic::init();
    super::enable_sse();
    super::gdt::init_cpu(arch_cpu_id());
    super::idt::load();
    super::syscall::init_ap();
    super::timer::init_local_timer();

    ACTIVE_CPUS.fetch_add(1, Ordering::AcqRel);

    sched::ap_entry()
}

// ── smp_init — bring up the enumerated application processors ────────────────

/// Bring up the APs `cpus::discover` enumerated, up to `MAX_CPUS - 1`.
///
/// Must be called after the identity-mapped page table is active, the
/// physical allocator is initialised and the timer is calibrated (the
/// INIT/SIPI delays come from `timer::monotonic_ns`).
///
/// # Safety
/// Must be called from the BSP after `apic::init()`.  Physical addresses
/// 0x7000–0x7FFF must be free for the trampoline.
#[cfg(target_arch = "x86_64")]
pub unsafe fn smp_init() {
    let cands = cpus::candidates();
    if cands.is_empty() {
        cpus::log("[SMP] 1 CPU online (no APs enumerated)\n");
        return;
    }

    // ── Identity-map the trampoline page ─────────────────────────────────────
    // The APs execute at physical 0x7000 and keep fetching from that address
    // in the first instructions *after* enabling paging with the BSP's CR3
    // (they also read the temporary GDT and the parameter block there).
    // Limine base revision 1+ provides no lower-half identity map, so add one
    // page. A failure (e.g. already mapped by a direct-boot identity map) is fine.
    let cr3: u64;
    core::arch::asm!("mov {}, cr3", out(reg) cr3, options(nomem, nostack));
    {
        let root = (cr3 & !0xFFF) as usize;
        let _ = super::paging::map_4k(
            root,
            TRAMPOLINE_BASE,
            TRAMPOLINE_BASE,
            super::paging::PageTableFlags::PRESENT | super::paging::PageTableFlags::WRITABLE,
        );
    }

    // ── Copy trampoline to physical 0x7000 (via HHDM) ────────────────────────
    let src = &ap_trampoline_start as *const u8;
    let end = &ap_trampoline_end   as *const u8;
    let len = end as usize - src as usize;
    if len > AP_GDT_BASE - TRAMPOLINE_BASE {
        cpus::log("[SMP] AP trampoline too large; APs not started\n");
        return;
    }
    core::ptr::copy_nonoverlapping(src, mm::phys_to_virt(TRAMPOLINE_BASE) as *mut u8, len);

    // ── GDT + GDTR ───────────────────────────────────────────────────────────
    write64(AP_GDT_BASE + 0x00, 0x0000_0000_0000_0000); // null
    write64(AP_GDT_BASE + 0x08, 0x00CF_9A00_0000_FFFF); // 32-bit code
    write64(AP_GDT_BASE + 0x10, 0x00CF_9200_0000_FFFF); // data
    write64(AP_GDT_BASE + 0x18, 0x00AF_9A00_0000_FFFF); // 64-bit code (L=1)
    (mm::phys_to_virt(AP_GDTR_ADDR) as *mut u16).write_volatile(0x001F);
    (mm::phys_to_virt(AP_GDTR_ADDR + 2) as *mut u32).write_volatile(AP_GDT_BASE as u32);

    // ── Parameter block ──────────────────────────────────────────────────────
    // CR3 — share the BSP's page table (assumed < 4 GiB for the 32-bit load).
    write64(AP_CR3_OFF, cr3);
    write64(AP_ENTRY_OFF, sched_ap_entry as *const () as u64);
    target().store(TARGET_IDLE, Ordering::SeqCst);
    write64(AP_STACK_OFF, 0);

    let x2 = apic::is_x2apic();

    // ── INIT every target, then the one 10 ms wait ───────────────────────────
    // INIT (level assert) leaves each AP in wait-for-SIPI, whatever loop the
    // firmware parked it in. Doing them all first pays the MP-spec delay
    // once instead of once per AP.
    for &id in cands {
        if id > 254 && !x2 { continue; }
        apic::send_ipi(id, apic::ICR_INIT | apic::ICR_ASSERT | apic::ICR_LEVEL_TRIG);
    }
    wait_ns(10_000_000, || false);

    // ── Start them one at a time ─────────────────────────────────────────────
    const SIPI_VECTOR: u32 = (TRAMPOLINE_BASE >> 12) as u32; // 0x07 → 0x7000
    let mut next = 1usize;
    let mut failed = 0usize;
    for &id in cands {
        if next >= cpus::MAX_CPUS { break; }
        if id > 254 && !x2 { failed += 1; continue; }

        // Per-AP kernel stack (64 KiB), stored as an HHDM *virtual* address:
        // the AP loads RSP only after paging is on with the kernel CR3, where
        // the HHDM is guaranteed but a physical identity map is not.
        let Some(stack) = buddy::alloc(4) else {
            cpus::log("[SMP] out of memory for AP stacks\n");
            break;
        };
        let stack_virt = mm::phys_to_virt(stack);
        (stack_virt as *mut u8).write_bytes(0, buddy::PAGE_SIZE * 16);
        write64(AP_STACK_OFF, (stack_virt + buddy::PAGE_SIZE * 16) as u64);

        cpus::assign(next, id);
        let before = ACTIVE_CPUS.load(Ordering::Acquire);
        target().store(id, Ordering::SeqCst);

        // SIPI, ~200 µs, SIPI (a SIPI reaching an AP that is already running
        // the trampoline is ignored), then allow a generous window for slow
        // emulators before declaring the AP absent.
        apic::send_ipi(id, apic::ICR_STARTUP | SIPI_VECTOR);
        let tgt = target();
        if !wait_ns(200_000, || tgt.load(Ordering::Acquire) != id) {
            apic::send_ipi(id, apic::ICR_STARTUP | SIPI_VECTOR);
            wait_ns(1_000_000_000, || tgt.load(Ordering::Acquire) != id);
        }
        if tgt.compare_exchange(id, TARGET_IDLE, Ordering::SeqCst, Ordering::SeqCst).is_ok() {
            // Never claimed, and now it cannot: give its index and stack back.
            cpus::log("[SMP] APIC ID ");
            cpus::log_dec(id as u64);
            cpus::log(" did not start; skipped\n");
            cpus::unassign(next);
            buddy::free(stack, 4);
            failed += 1;
            continue;
        }
        // Claimed: it holds the stack once it marks DONE, then counts itself.
        wait_ns(1_000_000_000, || tgt.load(Ordering::Acquire) == TARGET_DONE);
        let online = wait_ns(5_000_000_000, || ACTIVE_CPUS.load(Ordering::Acquire) > before);
        next += 1;
        tgt.store(TARGET_IDLE, Ordering::SeqCst);
        if !online {
            // It owns index `next - 1` and may still arrive; starting more
            // would let a later index come online before it.
            cpus::log("[SMP] APIC ID ");
            cpus::log_dec(id as u64);
            cpus::log(" claimed its stack but never came online; stopping AP bring-up\n");
            break;
        }
    }

    cpus::log("[SMP] ");
    cpus::log_dec(active_cpu_count() as u64);
    cpus::log(" CPUs online (");
    cpus::log(if x2 { "x2APIC" } else { "xAPIC" });
    cpus::log(") cpu:apic");
    for i in 0..cpus::assigned() {
        cpus::log(" ");
        cpus::log_dec(i as u64);
        cpus::log(":");
        cpus::log_dec(cpus::apic_id_of(i) as u64);
    }
    if failed != 0 { cpus::log("; "); cpus::log_dec(failed as u64); cpus::log(" failed"); }
    // Enumerated but never sent a SIPI: beyond MAX_CPUS, or after an early stop.
    let parked = cpus::overflow() + (cands.len() + 1).saturating_sub(cpus::assigned() + failed);
    if parked != 0 {
        cpus::log("; ");
        cpus::log_dec(parked as u64);
        cpus::log(" left parked (MAX_CPUS=");
        cpus::log_dec(cpus::MAX_CPUS as u64);
        cpus::log(")");
    }
    cpus::log("\n");
}
