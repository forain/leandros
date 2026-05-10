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

#[inline]
pub unsafe fn read(off: usize) -> u32 {
    ((LAPIC_BASE + off) as *const u32).read_volatile()
}

#[inline]
pub unsafe fn write(off: usize, val: u32) {
    ((LAPIC_BASE + off) as *mut u32).write_volatile(val)
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

unsafe fn wrmsr(msr: u32, val: u64) {
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
    wrmsr(IA32_APIC_BASE_MSR, apic_msr | APIC_GLOBAL_ENABLE);

    // Mask 8259 before unmasking LAPIC to prevent spurious legacy IRQs.
    mask_pic();

    // Accept all interrupt priorities (TPR = 0).
    write(LAPIC_TPR, 0);

    // Enable APIC software and set spurious vector to 0xFF.
    write(LAPIC_SVR, SVR_ENABLE | SPURIOUS_VEC);
}

/// Send End-Of-Interrupt to the Local APIC.
///
/// **Must** be called at the end of every LAPIC interrupt handler before
/// `iretq` / `eret`.  Writing any value to the EOI register signals EOI.
#[inline]
pub fn eoi() {
    unsafe { write(LAPIC_EOI, 0); }
}
