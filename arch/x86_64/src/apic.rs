//! x86-64 Local APIC (LAPIC) driver.
//!
//! On UEFI systems the legacy 8259 PIC is typically masked by firmware.
//! We mask it explicitly, read the APIC base address from the
//! IA32_APIC_BASE MSR, enable the LAPIC via the Spurious Vector Register,
//! and expose `eoi()` for use by interrupt handlers.
//!
//! The APIC timer is initialised separately by `timer::init()`.
//!
//! Ref: Intel SDM Vol 3A §10 (Local APIC); AMD64 APM Vol 2 §16

// ── MSR ───────────────────────────────────────────────────────────────────────
const IA32_APIC_BASE_MSR: u32 = 0x1B;
/// Bit 11: global APIC enable in IA32_APIC_BASE.
const APIC_GLOBAL_ENABLE: u64 = 1 << 11;
/// Mask for APIC MMIO base address (bits 51:12).
const APIC_BASE_MASK: u64 = 0x0000_FFFF_FFFF_F000;

// ── LAPIC register offsets (from LAPIC MMIO base) ────────────────────────────
pub const LAPIC_ID:         usize = 0x020;
pub const LAPIC_VER:        usize = 0x030;
pub const LAPIC_TPR:        usize = 0x080; // Task Priority
pub const LAPIC_EOI:        usize = 0x0B0; // End-of-Interrupt (write 0)
pub const LAPIC_SVR:        usize = 0x0F0; // Spurious Interrupt Vector
pub const LAPIC_LVT_TIMER:  usize = 0x320; // LVT: Timer
pub const LAPIC_LVT_LINT0:  usize = 0x350; // LVT: LINT0
pub const LAPIC_LVT_LINT1:  usize = 0x360; // LVT: LINT1
pub const LAPIC_TIMER_INIT: usize = 0x380; // Timer Initial Count
pub const LAPIC_TIMER_CURR: usize = 0x390; // Timer Current Count (RO)
pub const LAPIC_TIMER_DIV:  usize = 0x3E0; // Timer Divide Configuration

// Spurious Vector Register: bit 8 = software enable.
const SVR_ENABLE:   u32 = 1 << 8;
/// Spurious interrupt vector — must be 0xFx on Intel; 0xFF is conventional.
const SPURIOUS_VEC: u32 = 0xFF;

// ── Cached LAPIC MMIO base (default 0xFEE0_0000) ─────────────────────────────
static mut LAPIC_BASE: usize = 0xFEE0_0000;
static mut HHDM_OFFSET: u64 = 0;

// ── Low-level helpers ─────────────────────────────────────────────────────────

// ── x2APIC ────────────────────────────────────────────────────────────────────
//
// In x2APIC mode the register file moves from MMIO to MSRs 0x800 + (off >> 4)
// and the MMIO window stops responding. Firmware hands over in x2APIC mode on
// many machines (any with APIC IDs above 254, and commonly with interrupt
// remapping), and the mode cannot be left without disabling the APIC, so the
// kernel follows: `init` on the BSP picks the mode once — x2APIC if the
// firmware already enabled it or `cpus::need_x2apic()` — and every AP matches.

/// IA32_APIC_BASE bit 10: x2APIC mode (together with bit 11).
const APIC_EXTD: u64 = 1 << 10;
/// First x2APIC MSR; register `off` lives at `X2APIC_MSR_BASE + (off >> 4)`.
const X2APIC_MSR_BASE: u32 = 0x800;
/// x2APIC ICR: one 64-bit MSR, destination in bits [63:32].
const X2APIC_ICR: u32 = 0x830;

static X2APIC: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// The local APICs run in x2APIC mode.
#[inline]
pub fn is_x2apic() -> bool { X2APIC.load(core::sync::atomic::Ordering::Relaxed) }

#[inline]
pub unsafe fn read(off: usize) -> u32 {
    if is_x2apic() { return rdmsr(X2APIC_MSR_BASE + (off >> 4) as u32) as u32; }
    ((LAPIC_BASE + off) as *const u32).read_volatile()
}

#[inline]
pub unsafe fn write(off: usize, val: u32) {
    if is_x2apic() { return wrmsr(X2APIC_MSR_BASE + (off >> 4) as u32, val as u64); }
    ((LAPIC_BASE + off) as *mut u32).write_volatile(val)
}

/// This CPU's local APIC ID as the APIC reports it (all 32 bits in x2APIC
/// mode, bits [31:24] of the ID register in xAPIC mode).
pub unsafe fn id() -> u32 {
    if is_x2apic() { read(LAPIC_ID) } else { (read(LAPIC_ID) >> 24) & 0xFF }
}

/// ICR low-word fields for [`send_ipi`].
pub const ICR_INIT:         u32 = 5 << 8;
pub const ICR_STARTUP:      u32 = 6 << 8;
pub const ICR_ASSERT:       u32 = 1 << 14;
pub const ICR_LEVEL_TRIG:   u32 = 1 << 15;
/// Destination shorthand "all excluding self".
pub const ICR_ALL_BUT_SELF: u32 = 3 << 18;

/// Send an IPI. `low` is the ICR low word (vector, delivery mode, level,
/// trigger, shorthand); `dest` is the target's APIC ID (ignored with a
/// shorthand). Callable with IRQs on: the xAPIC two-register sequence runs
/// with IRQs masked so an IRQ handler sending its own IPI in between cannot
/// retarget this one.
pub unsafe fn send_ipi(dest: u32, low: u32) {
    if is_x2apic() {
        // x2APIC ICR writes are not serializing: order every earlier store
        // (a TLB-flush request flag, a run-queue update) before the IPI, as
        // Linux's weak_wrmsr_fence does.
        core::arch::asm!("mfence", "lfence", options(nostack, preserves_flags));
        wrmsr(X2APIC_ICR, ((dest as u64) << 32) | low as u64);
        return;
    }
    let rflags: u64;
    core::arch::asm!("pushfq", "pop {}", "cli", out(reg) rflags);
    // Wait for any in-flight IPI from this LAPIC (ICR Delivery Status, bit 12).
    while read(0x300) & (1 << 12) != 0 { core::hint::spin_loop(); }
    write(0x310, (dest & 0xFF) << 24);
    write(0x300, low);
    if rflags & (1 << 9) != 0 { core::arch::asm!("sti"); }
}

pub unsafe fn rdmsr(msr: u32) -> u64 {
    let lo: u32;
    let hi: u32;
    core::arch::asm!(
        "rdmsr",
        in("ecx")  msr,
        out("eax") lo,
        out("edx") hi,
        options(nomem, nostack, preserves_flags)
    );
    ((hi as u64) << 32) | (lo as u64)
}

pub(crate) unsafe fn wrmsr(msr: u32, val: u64) {
    core::arch::asm!(
        "wrmsr",
        in("ecx")  msr,
        in("eax")  val as u32,
        in("edx")  (val >> 32) as u32,
        options(nomem, nostack, preserves_flags)
    );
}

/// Mask all 8259 PIC IRQs so ghost interrupts do not reach the CPU.
///
/// On UEFI the firmware may have already done this, but we do it explicitly
/// before unmasking LAPIC interrupts.
unsafe fn mask_pic() {
    core::arch::asm!(
        "out 0x21, al",   // OCW1: mask all master PIC IRQs
        in("al") 0xFFu8,
        options(nomem, nostack)
    );
    core::arch::asm!(
        "out 0xA1, al",   // OCW1: mask all slave PIC IRQs
        in("al") 0xFFu8,
        options(nomem, nostack)
    );
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Initialise the Local APIC.
///
/// • Reads the APIC base address from IA32_APIC_BASE MSR.
/// • Ensures the global APIC enable bit is set.
/// • Masks the legacy 8259 PIC.
/// • Enables the LAPIC via the Spurious Vector Register.
/// • Sets Task Priority Register to 0 (accept all priorities).
///
/// Must be called after the IDT is loaded (so LAPIC interrupts have handlers)
/// but before `timer::init()` programs the APIC timer.
pub unsafe fn set_hhdm_offset(offset: u64) {
    HHDM_OFFSET = offset;
}

/// Initialise the Local APIC.
///
/// • Reads the APIC base address from IA32_APIC_BASE MSR.
/// • Ensures the global APIC enable bit is set.
/// • Masks the legacy 8259 PIC.
/// • Enables the LAPIC via the Spurious Vector Register.
/// • Sets Task Priority Register to 0 (accept all priorities).
///
/// Must be called after the IDT is loaded (so LAPIC interrupts have handlers)
/// but before `timer::init()` programs the APIC timer.
pub unsafe fn init() {
    // Read current APIC base; extract MMIO address and re-enable if needed.
    let apic_msr = rdmsr(IA32_APIC_BASE_MSR);
    // Use HHDM offset if provided, otherwise fallback to the standard high-half mapping
    // for Limine/UEFI kernels linked at -2GB.
    let phys_base = apic_msr & APIC_BASE_MASK;
    let offset = if HHDM_OFFSET != 0 { HHDM_OFFSET } else { 0xffff800000000000 };
    LAPIC_BASE   = (phys_base + offset) as usize;

    // Mode: decided once by the BSP (the first caller), followed by every AP.
    // x2APIC when the firmware already switched to it (there is no way back
    // short of disabling the APIC) or when a CPU we will run has an ID xAPIC
    // cannot address. Otherwise xAPIC, as always.
    use core::sync::atomic::{AtomicBool, Ordering};
    static MODE_DECIDED: AtomicBool = AtomicBool::new(false);
    if !MODE_DECIDED.swap(true, Ordering::AcqRel) {
        let x2 = apic_msr & APIC_EXTD != 0
            || (super::cpus::need_x2apic() && super::cpus::x2apic_supported());
        X2APIC.store(x2, Ordering::Relaxed);
        if x2 {
            super::cpus::log(if apic_msr & APIC_EXTD != 0 {
                "[CPUS] x2APIC mode (enabled by firmware)\n"
            } else {
                "[CPUS] x2APIC mode (APIC IDs above 254)\n"
            });
        }
    }
    if is_x2apic() {
        // Enable first, then EXTD: xAPIC -> x2APIC is legal from the enabled
        // state, and a no-op when the firmware already did it.
        wrmsr(IA32_APIC_BASE_MSR, apic_msr | APIC_GLOBAL_ENABLE);
        wrmsr(IA32_APIC_BASE_MSR, apic_msr | APIC_GLOBAL_ENABLE | APIC_EXTD);
    } else {
        if apic_msr & APIC_EXTD != 0 {
            // This CPU was left in x2APIC mode while the BSP is in xAPIC mode
            // (firmware may switch only some CPUs). x2APIC -> xAPIC is only
            // legal through the disabled state; the MMIO path below would
            // otherwise program nothing.
            super::cpus::log("[CPUS] AP was in x2APIC mode; switching it to xAPIC\n");
            wrmsr(IA32_APIC_BASE_MSR, apic_msr & !(APIC_GLOBAL_ENABLE | APIC_EXTD));
        }
        wrmsr(IA32_APIC_BASE_MSR, (apic_msr & !APIC_EXTD) | APIC_GLOBAL_ENABLE);
    }

    // Mask 8259 before unmasking LAPIC to prevent spurious legacy IRQs.
    mask_pic();

    // Accept all interrupt priorities (TPR = 0).
    write(LAPIC_TPR, 0);

    // Enable APIC software and set spurious vector to 0xFF.
    write(LAPIC_SVR, SVR_ENABLE | SPURIOUS_VEC);

    // This CPU's ID register is now live and stable — publish it somewhere
    // `arch_cpu_id()` can read without touching MMIO. Must happen here, in the
    // one init both the BSP and every AP run, and *after* `init_pat_ap`: the
    // first `arch_cpu_id()` caller on an AP is `gdt::init_cpu()`, two steps
    // further down `sched_ap_entry`, so there is no window in which the fast
    // path could read an unseeded value and hand out CPU 0's index.
    super::smp::seed_cpu_id_fastpath();
}

/// Send End-Of-Interrupt to the Local APIC.
///
/// **Must** be called at the end of every LAPIC interrupt handler before
/// `iretq` / `eret`.  Writing any value to the EOI register signals EOI.
#[inline]
pub fn eoi() {
    unsafe { write(LAPIC_EOI, 0); }
}
