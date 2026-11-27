//! EL2 / VHE host state (KVM lane `kvm-vhe`).
//!
//! Without the `kvm` cargo feature (Kconfig `KVM`, kernel feature `drv-kvm`)
//! every function here is a `const` "no": the kernel is dropped to EL1 by the
//! entry stubs exactly as before and nothing in this module changes behaviour.
//!
//! With `kvm`, entry (kernel/src/entry_aarch64.s and the AP stub in smp.rs)
//! keeps the CPU at EL2 with HCR_EL2.{E2H,TGE}=1 when the CPU has VHE
//! (ID_AA64MMFR1_EL1.VH), and falls back to the EL1 drop otherwise. Under E2H
//! the kernel's `*_EL1` register accesses are redirected to the EL2 registers.
//! The host tick moves from CNTV (PPI 27) to CNTHP (INTID 26); EL2 physical
//! timer accesses (`cntp_*_el0`) are redirected to CNTHP under E2H.

/// HCR_EL2 bits the host runs with: RW | TGE | E2H (+ API|APK when PAuth).
pub const HCR_RW: u64 = 1 << 31;
pub const HCR_TGE: u64 = 1 << 27;
pub const HCR_E2H: u64 = 1 << 34;
pub const HCR_APK: u64 = 1 << 40;
pub const HCR_API: u64 = 1 << 41;

/// Default CNTHP INTID (PPI 10) on QEMU virt and BCM2712.
pub const HYP_TIMER_INTID_DEFAULT: u32 = 26;

#[cfg(feature = "kvm")]
static GTDT_EL2: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// True when this CPU executes at EL2 (always false without the `kvm` feature).
#[cfg(feature = "kvm")]
#[inline]
pub fn at_el2() -> bool {
    let el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)); }
    (el >> 2) & 3 == 2
}
#[cfg(not(feature = "kvm"))]
#[inline(always)]
pub const fn at_el2() -> bool { false }

/// True when running at EL2 with E2H set (VHE host).
#[cfg(feature = "kvm")]
#[inline]
pub fn vhe() -> bool {
    if !at_el2() { return false; }
    let h: u64;
    unsafe { core::arch::asm!("mrs {}, hcr_el2", out(reg) h, options(nomem, nostack)); }
    h & HCR_E2H != 0
}
#[cfg(not(feature = "kvm"))]
#[inline(always)]
pub const fn vhe() -> bool { false }

/// HCR_EL2 value of the running host (0 when not at EL2).
#[cfg(feature = "kvm")]
pub fn host_hcr() -> u64 {
    if !at_el2() { return 0; }
    let h: u64;
    unsafe { core::arch::asm!("mrs {}, hcr_el2", out(reg) h, options(nomem, nostack)); }
    h
}
#[cfg(not(feature = "kvm"))]
pub const fn host_hcr() -> u64 { 0 }

/// ACPI GTDT "EL2 timer GSIV" (0 = unknown, use the default).
#[cfg(feature = "kvm")]
pub fn note_gtdt_el2(gsiv: u32) {
    GTDT_EL2.store(gsiv, core::sync::atomic::Ordering::Relaxed);
}
#[cfg(not(feature = "kvm"))]
pub fn note_gtdt_el2(_gsiv: u32) {}

/// INTID of the host tick: CNTHP at EL2, else `fallback` (the CNTV PPI).
#[cfg(feature = "kvm")]
pub fn host_timer_intid(fallback: u32) -> u32 {
    if !at_el2() { return fallback; }
    match GTDT_EL2.load(core::sync::atomic::Ordering::Relaxed) {
        0 => HYP_TIMER_INTID_DEFAULT,
        g => g,
    }
}
#[cfg(not(feature = "kvm"))]
#[inline(always)]
pub const fn host_timer_intid(fallback: u32) -> u32 { fallback }

/// `CurrentEL` as 0..3 (the boot line and the ktest read this).
pub fn current_el() -> u64 {
    let el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)); }
    (el >> 2) & 3
}

/// Print the boot line the runtests suite greps for (kvm builds only, so
/// the default boot log is unchanged).
#[cfg(not(feature = "kvm"))]
pub fn report() {}
#[cfg(feature = "kvm")]
pub fn report() {
    let el = current_el();
    if vhe() {
        super::uart::serial_print_str("[el] running at EL2 (VHE)\n");
    } else if at_el2() {
        super::uart::serial_print_str("[el] running at EL2 (nVHE)\n");
    } else if el == 1 {
        super::uart::serial_print_str("[el] running at EL1\n");
    } else {
        super::uart::serial_print_str("[el] running at EL?\n");
    }
}
