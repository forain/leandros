//! ARM GICv2 / GIC-400 generic interrupt controller driver.
//!
//! **QEMU -machine virt** (default):
//!   GICD (distributor)    0x0800_0000
//!   GICC (CPU interface)  0x0801_0000
//!
//! **Raspberry Pi 5 (BCM2712 GIC-400)** — enabled by the `rpi5` cargo feature:
//!   GICD (distributor)    0x107F_FF90_00
//!   GICC (CPU interface)  0x107F_FFA0_00
//!
//! We enable PPI #30 (EL1 physical timer, CNTP) so the generic timer can
//! deliver IRQs to CPU 0.
//!
//! Ref: ARM GIC Architecture Specification v2.0

#[cfg(not(feature = "rpi5"))]
const GICD_PHYS: usize = 0x0800_0000;
#[cfg(not(feature = "rpi5"))]
const GICC_PHYS: usize = 0x0801_0000;

#[cfg(feature = "rpi5")]
const GICD_PHYS: usize = 0x107F_FF90_00;
#[cfg(feature = "rpi5")]
const GICC_PHYS: usize = 0x107F_FFA0_00;

pub static mut GICD_BASE: usize = GICD_PHYS;
pub static mut GICC_BASE: usize = GICC_PHYS;

// Distributor register offsets
const GICD_CTLR:       usize = 0x000; // distributor control
const GICD_ISENABLER0: usize = 0x100; // set-enable  for IRQs  0-31
const GICD_IPRIORITYR: usize = 0x400; // priority    (1 byte / IRQ)
const GICD_ITARGETSR:  usize = 0x800; // target CPUs (1 byte / IRQ)

// CPU interface register offsets
const GICC_CTLR: usize = 0x000; // CPU interface control
const GICC_PMR:  usize = 0x004; // priority mask
const GICC_IAR:  usize = 0x00C; // interrupt acknowledge (read)
const GICC_EOIR: usize = 0x010; // end-of-interrupt (write)

/// Spurious interrupt — IAR returns this value when there is no pending IRQ.
pub const SPURIOUS: u32 = 1023;

// ── Helpers ───────────────────────────────────────────────────────────────

unsafe fn gicd_r32(off: usize) -> u32 {
    ((GICD_BASE + off) as *const u32).read_volatile()
}
unsafe fn gicd_w32(off: usize, v: u32) {
    ((GICD_BASE + off) as *mut u32).write_volatile(v)
}
unsafe fn gicc_r32(off: usize) -> u32 {
    ((GICC_BASE + off) as *const u32).read_volatile()
}
unsafe fn gicc_w32(off: usize, v: u32) {
    ((GICC_BASE + off) as *mut u32).write_volatile(v)
}

// ── Public API ────────────────────────────────────────────────────────────

/// Issue a data synchronization barrier for device (store) ordering.
///
/// Required after writes to GIC MMIO registers to ensure the write has
/// propagated to the peripheral before the caller continues.
#[inline]
unsafe fn dsb_st() {
    core::arch::asm!("dsb st", options(nomem, nostack));
}

/// Initialise GICv2 and enable PPI #27 (EL1 virtual timer).
pub fn init() {
    unsafe {
        // Update bases to use HHDM virtual addresses
        GICD_BASE = mm::phys_to_virt(GICD_PHYS);
        GICC_BASE = mm::phys_to_virt(GICC_PHYS);

        // Enable distributor.
        gicd_w32(GICD_CTLR, 1);
        dsb_st();

        // Enable PPI 27.  GICD_ISENABLER[0] is a write-set register covering
        // IRQ IDs 0-31; writing a 1 to bit N enables IRQ N.
        gicd_w32(GICD_ISENABLER0, 1 << 27);
        dsb_st();

        // Priority for IRQ 27 (mid-priority = 0xA0).
        // IPRIORITYR is byte-addressed: IRQ 27 lives at byte 27 = word 6, byte 3.
        let pri_word_off = GICD_IPRIORITYR + (27 / 4) * 4;
        let pri_shift    = (27 % 4) * 8;
        let pri_v = (gicd_r32(pri_word_off) & !(0xFF << pri_shift)) | (0xA0 << pri_shift);
        gicd_w32(pri_word_off, pri_v);
        dsb_st();

        // Route IRQ 27 to CPU 0.
        // ITARGETSR is byte-addressed similarly; bit 0 of the byte = CPU 0.
        let tgt_word_off = GICD_ITARGETSR + (27 / 4) * 4;
        let tgt_shift    = (27 % 4) * 8;
        let tgt_v = (gicd_r32(tgt_word_off) & !(0xFF << tgt_shift)) | (0x01 << tgt_shift);
        gicd_w32(tgt_word_off, tgt_v);
        dsb_st();

        // Enable CPU interface.
        gicc_w32(GICC_CTLR, 1);
        // Accept any priority (mask = 0xFF = accept all).
        gicc_w32(GICC_PMR, 0xFF);
        dsb_st();
    }
}

/// Initialise only the CPU interface for a secondary CPU (AP).
///
/// The distributor was already configured by the BSP; each AP must separately
/// enable its own banked CPU interface registers.
pub fn init_cpu_interface() {
    unsafe {
        gicc_w32(GICC_CTLR, 1);    // enable CPU interface
        gicc_w32(GICC_PMR,  0xFF); // accept all priorities
        dsb_st();
    }
}

/// Acknowledge the current interrupt; returns the raw IAR value.
#[inline]
pub fn ack() -> u32 {
    unsafe { gicc_r32(GICC_IAR) }
}

/// Signal end-of-interrupt.
#[inline]
pub fn eoi(iar: u32) {
    unsafe { gicc_w32(GICC_EOIR, iar); }
}

/// Extract the interrupt ID from a raw IAR value (bits [9:0]).
#[inline]
pub fn irq_id(iar: u32) -> u32 {
    iar & 0x3FF
}
