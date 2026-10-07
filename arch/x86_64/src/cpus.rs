//! Logical CPU numbering: a dense CPU index (0..N-1) ↔ local APIC ID.
//!
//! Every per-CPU array in the kernel is indexed by `arch_cpu_id()`, which
//! must therefore be dense and below `sched::MAX_CPUS`. The APIC ID is
//! neither: a 4-core part without SMT reports 0,2,4,6; AMD parts with several
//! CCXs, hybrid Intel parts and two-socket QEMU topologies leave gaps; x2APIC
//! IDs can exceed 255. So the IDs are only an *address* (for IPIs, INIT/SIPI,
//! IOAPIC/MSI destinations) and the index is assigned here:
//!
//!   * index 0 is always the BSP;
//!   * the APs come from the ACPI MADT (enabled Local APIC / Local x2APIC
//!     entries, in table order, deduplicated by ID) and get the next index
//!     only when they actually come up (`smp::smp_init` assigns, and takes
//!     the slot back from an AP that never answers), so the online CPUs are
//!     always exactly the indices `0..active_cpu_count()`;
//!   * enabled processors beyond `MAX_CPUS` are left in wait-for-SIPI and
//!     logged; disabled/"online capable" ones (hot-add) are never started.
//!
//! `arch_cpu_id()` reads the index from `IA32_TSC_AUX` (seeded per CPU by
//! `smp::seed_cpu_id_fastpath`), never from this table, so the hot path is
//! unchanged.

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

/// Capacity of every per-CPU array (`sched::MAX_CPUS`).
pub const MAX_CPUS: usize = sched::MAX_CPUS;

/// `APIC_ID[i]` for an unassigned index.
pub const NO_APIC: u32 = u32::MAX;

/// Dense index → APIC ID. Slots `0..ASSIGNED` are valid.
static APIC_ID: [AtomicU32; MAX_CPUS] = [const { AtomicU32::new(NO_APIC) }; MAX_CPUS];
/// Number of valid leading slots of `APIC_ID` (BSP + APs being or already
/// started).
static ASSIGNED: AtomicUsize = AtomicUsize::new(0);

/// APs to start, in MADT order (APIC IDs, BSP excluded), at most
/// `MAX_CPUS - 1` of them.
static mut CANDIDATES: [u32; MAX_CPUS - 1] = [0; MAX_CPUS - 1];
static CANDIDATE_N: AtomicUsize = AtomicUsize::new(0);

/// Enabled processors the MADT lists beyond `MAX_CPUS` (left parked).
static OVERFLOW: AtomicUsize = AtomicUsize::new(0);
/// Some CPU we intend to run has an APIC ID that xAPIC cannot address.
static NEED_X2APIC: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn arch_serial_putc(c: u8);
}

pub(crate) fn log(s: &str) {
    for &b in s.as_bytes() { unsafe { arch_serial_putc(b) } }
}

pub(crate) fn log_dec(mut n: u64) {
    let mut buf = [0u8; 20];
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 { break; }
    }
    for &b in &buf[i..] { unsafe { arch_serial_putc(b) } }
}

/// The calling CPU's initial APIC ID, from CPUID — independent of the APIC
/// mode and of whether the LAPIC page is mapped yet.
///
/// Leaf 0xB (extended topology) EDX is the full 32-bit x2APIC ID when the
/// leaf exists (EBX != 0 at subleaf 0); otherwise leaf 1 EBX[31:24] is the
/// 8-bit initial APIC ID. The AP trampoline in `smp.rs` computes the same
/// value the same way, so both sides agree on who is who.
#[cfg(target_arch = "x86_64")]
pub fn own_apic_id() -> u32 {
    use core::arch::x86_64::{__cpuid, __cpuid_count};
    let max_leaf = __cpuid(0).eax;
    if max_leaf >= 0xB {
        let r = __cpuid_count(0xB, 0);
        if r.ebx != 0 { return r.edx; }
    }
    __cpuid(1).ebx >> 24
}

/// CPUID.1:ECX[21] — the CPU can run its local APIC in x2APIC mode.
#[cfg(target_arch = "x86_64")]
pub fn x2apic_supported() -> bool {
    core::arch::x86_64::__cpuid(1).ecx & (1 << 21) != 0
}

/// Enumerate the CPUs. Runs once, on the BSP, before `apic::init()` (which
/// needs [`need_x2apic`]) and before anything calls `arch_cpu_id()` on an AP.
///
/// Without an RSDP or MADT the machine is treated as a uniprocessor: the old
/// broadcast INIT/SIPI is gone for good, since it starts processors the
/// kernel has no slot or stack for.
///
/// # Safety
/// ACPI tables must be readable through the HHDM at `hhdm_offset`.
#[cfg(target_arch = "x86_64")]
pub unsafe fn discover(rsdp_phys: u64, hhdm_offset: u64) {
    let bsp = own_apic_id();
    APIC_ID[0].store(bsp, Ordering::Relaxed);
    ASSIGNED.store(1, Ordering::Release);
    if bsp > 254 { NEED_X2APIC.store(true, Ordering::Relaxed); }

    let can_x2 = x2apic_supported();
    let cands = &mut *core::ptr::addr_of_mut!(CANDIDATES);
    let mut n = 0usize;
    let mut overflow = 0usize;
    let mut not_addressable = 0usize;
    let mut hotplug = 0usize;
    let mut seen_bsp = false;
    let have_madt = boot::acpi::for_each_madt_cpu(rsdp_phys, hhdm_offset, &mut |c| {
        if c.kind != boot::acpi::MADT_LAPIC && c.kind != boot::acpi::MADT_X2APIC { return; }
        let id = c.id as u32;
        if !c.enabled {
            if c.online_capable { hotplug += 1; }
            return;
        }
        // 0xFF in a Local APIC entry is the broadcast ID, never a CPU; 0xFFFFFFFF
        // likewise for x2APIC.
        if (c.kind == boot::acpi::MADT_LAPIC && id == 0xFF) || id == u32::MAX { return; }
        if id == bsp { seen_bsp = true; return; }
        if cands[..n].contains(&id) { return; }
        if id > 254 && !can_x2 { not_addressable += 1; return; }
        if n < cands.len() {
            cands[n] = id;
            n += 1;
            if id > 254 { NEED_X2APIC.store(true, Ordering::Relaxed); }
        } else {
            overflow += 1;
        }
    });
    CANDIDATE_N.store(n, Ordering::Release);
    OVERFLOW.store(overflow, Ordering::Relaxed);

    log("[CPUS] BSP apic=");
    log_dec(bsp as u64);
    if !have_madt {
        log(": no ACPI MADT, running uniprocessor\n");
        return;
    }
    log(", MADT lists ");
    log_dec((n + overflow + not_addressable + 1) as u64);
    log(" enabled CPU(s)");
    if !seen_bsp { log(" (BSP missing from MADT)"); }
    if hotplug != 0 { log(", "); log_dec(hotplug as u64); log(" hot-pluggable left offline"); }
    if overflow != 0 {
        log(", ");
        log_dec(overflow as u64);
        log(" beyond MAX_CPUS=");
        log_dec(MAX_CPUS as u64);
        log(" left parked");
    }
    if not_addressable != 0 {
        log(", ");
        log_dec(not_addressable as u64);
        log(" with APIC ID > 254 left parked (no x2APIC)");
    }
    log("\n");
}

/// APIC IDs of the APs `smp_init` should start, in order.
pub fn candidates() -> &'static [u32] {
    let n = CANDIDATE_N.load(Ordering::Acquire);
    // Written once by `discover` on the BSP before any reader exists.
    let all: &'static [u32; MAX_CPUS - 1] = unsafe { &*core::ptr::addr_of!(CANDIDATES) };
    &all[..n]
}

/// Number of enabled processors left parked because the tables are full.
pub fn overflow() -> usize { OVERFLOW.load(Ordering::Relaxed) }

/// Some CPU that will run has an APIC ID above 254, so the LAPICs must run in
/// x2APIC mode.
pub fn need_x2apic() -> bool { NEED_X2APIC.load(Ordering::Relaxed) }

/// APIC ID of dense CPU index `idx`, or [`NO_APIC`].
#[inline]
pub fn apic_id_of(idx: usize) -> u32 {
    if idx >= MAX_CPUS { return NO_APIC; }
    APIC_ID[idx].load(Ordering::Relaxed)
}

/// APIC ID of the BSP (index 0).
#[inline]
pub fn bsp_apic_id() -> u32 {
    let id = APIC_ID[0].load(Ordering::Relaxed);
    if id == NO_APIC { 0 } else { id }
}

/// Dense index of the CPU with APIC ID `apic`, if it has one.
pub fn index_of(apic: u32) -> Option<usize> {
    let n = ASSIGNED.load(Ordering::Acquire).min(MAX_CPUS);
    (0..n).find(|&i| APIC_ID[i].load(Ordering::Relaxed) == apic)
}

/// Number of assigned indices (BSP + APs started or being started).
pub fn assigned() -> usize { ASSIGNED.load(Ordering::Acquire) }

/// Give index `idx` (== `assigned()`) to the AP with APIC ID `apic`, before
/// it is sent its SIPI.
pub fn assign(idx: usize, apic: u32) {
    debug_assert!(idx == ASSIGNED.load(Ordering::Relaxed) && idx < MAX_CPUS);
    APIC_ID[idx].store(apic, Ordering::Relaxed);
    ASSIGNED.store(idx + 1, Ordering::Release);
}

/// Take back the most recent [`assign`] (the AP never answered and was
/// fenced off, so it can no longer reach the lookup).
pub fn unassign(idx: usize) {
    ASSIGNED.store(idx, Ordering::Release);
    APIC_ID[idx].store(NO_APIC, Ordering::Relaxed);
}
