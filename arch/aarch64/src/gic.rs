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
//! **QEMU -M raspi4b (BCM2711 GIC-400)** — enabled by the `raspi4b` cargo
//! feature. Testable stepping stone for the sdhci driver (see
//! drivers/src/sdhci.rs's top-of-file comment) — not a hardware target.
//! Verified live via QMP `info mtree`:
//!   GICD (distributor)    0xFF84_1000
//!   GICC (CPU interface)  0xFF84_2000
//!
//! We enable PPI #30 (EL1 physical timer, CNTP) so the generic timer can
//! deliver IRQs to CPU 0.
//!
//! Ref: ARM GIC Architecture Specification v2.0

#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub const GICD_BASE: usize = 0x0800_0000;
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub const GICC_BASE: usize = 0x0801_0000;

#[cfg(feature = "rpi5")]
pub const GICD_BASE: usize = 0x107F_FF90_00;
#[cfg(feature = "rpi5")]
pub const GICC_BASE: usize = 0x107F_FFA0_00;

#[cfg(feature = "raspi4b")]
pub const GICD_BASE: usize = 0xFF84_1000;
#[cfg(feature = "raspi4b")]
pub const GICC_BASE: usize = 0xFF84_2000;

// Distributor register offsets
const GICD_CTLR:       usize = 0x000; // distributor control
const GICD_ISENABLER0: usize = 0x100; // set-enable  for IRQs  0-31 (banked per CPU for SGIs/PPIs)
const GICD_IPRIORITYR: usize = 0x400; // priority    (1 byte / IRQ)
const GICD_ITARGETSR:  usize = 0x800; // target CPUs (1 byte / IRQ)
const GICD_SGIR:       usize = 0xF00; // software-generated interrupt register

/// SGI used as the cross-CPU reschedule IPI.
pub const SGI_RESCHED: u32 = 1;

// CPU interface register offsets
const GICC_CTLR: usize = 0x000; // CPU interface control
const GICC_PMR:  usize = 0x004; // priority mask
const GICC_IAR:  usize = 0x00C; // interrupt acknowledge (read)
const GICC_EOIR: usize = 0x010; // end-of-interrupt (write)

/// Spurious interrupt — IAR returns this value when there is no pending IRQ.
pub const SPURIOUS: u32 = 1023;

// ── Helpers ───────────────────────────────────────────────────────────────

unsafe fn gicd_r32(off: usize) -> u32 {
    let base = mm::phys_to_virt(GICD_BASE);
    ((base + off) as *const u32).read_volatile()
}
unsafe fn gicd_w32(off: usize, v: u32) {
    let base = mm::phys_to_virt(GICD_BASE);
    ((base + off) as *mut u32).write_volatile(v)
}
unsafe fn gicc_r32(off: usize) -> u32 {
    let base = mm::phys_to_virt(GICC_BASE);
    ((base + off) as *const u32).read_volatile()
}
unsafe fn gicc_w32(off: usize, v: u32) {
    let base = mm::phys_to_virt(GICC_BASE);
    ((base + off) as *mut u32).write_volatile(v)
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
        // Enable distributor.
        gicd_w32(GICD_CTLR, 1);
        dsb_st();

        // Enable PPI 27 (Virtual Timer) and SGI 1 (reschedule IPI).
        // This word is banked per CPU for IRQs 0-31 — this enables them for
        // the BSP; each AP does the same in init_cpu_interface().
        gicd_w32(GICD_ISENABLER0, (1 << 27) | (1 << SGI_RESCHED));
        // Enable SPI 1 / IRQ 33 (PL011 UART).
        // IRQ 33 is in ISENABLER1 (IRQ IDs 32-63), bit 1 (33 - 32 = 1).
        gicd_w32(GICD_ISENABLER0 + 4, 1 << 1);
        dsb_st();

        // Priority for IRQ 27 (mid-priority = 0xA0).
        let pri_word_off = GICD_IPRIORITYR + (27 / 4) * 4;
        let pri_shift    = (27 % 4) * 8;
        let pri_v = (gicd_r32(pri_word_off) & !(0xFF << pri_shift)) | (0xA0 << pri_shift);
        gicd_w32(pri_word_off, pri_v);
        
        // Priority for IRQ 33 (mid-priority = 0xA0).
        let pri_word_off_33 = GICD_IPRIORITYR + (33 / 4) * 4;
        let pri_shift_33    = (33 % 4) * 8;
        let pri_v_33 = (gicd_r32(pri_word_off_33) & !(0xFF << pri_shift_33)) | (0xA0 << pri_shift_33);
        gicd_w32(pri_word_off_33, pri_v_33);
        dsb_st();

        // Route IRQ 27 to CPU 0.
        let tgt_word_off = GICD_ITARGETSR + (27 / 4) * 4;
        let tgt_shift    = (27 % 4) * 8;
        let tgt_v = (gicd_r32(tgt_word_off) & !(0xFF << tgt_shift)) | (0x01 << tgt_shift);
        gicd_w32(tgt_word_off, tgt_v);

        // Route IRQ 33 to CPU 0.
        let tgt_word_off_33 = GICD_ITARGETSR + (33 / 4) * 4;
        let tgt_shift_33    = (33 % 4) * 8;
        let tgt_v_33 = (gicd_r32(tgt_word_off_33) & !(0xFF << tgt_shift_33)) | (0x01 << tgt_shift_33);
        gicd_w32(tgt_word_off_33, tgt_v_33);
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
/// enable its own banked registers: the CPU interface (GICC_*) plus the
/// banked GICD_ISENABLER0 word covering SGIs and PPIs — without the latter,
/// this CPU would never receive its virtual-timer PPI 27 or reschedule SGI 1.
pub fn init_cpu_interface() {
    unsafe {
        // Banked per-CPU enables: virtual timer PPI 27 + reschedule SGI 1.
        gicd_w32(GICD_ISENABLER0, (1 << 27) | (1 << SGI_RESCHED));

        // Banked per-CPU priority for PPI 27 (match the BSP's 0xA0).
        let pri_word_off = GICD_IPRIORITYR + (27 / 4) * 4;
        let pri_shift    = (27 % 4) * 8;
        let pri_v = (gicd_r32(pri_word_off) & !(0xFF << pri_shift)) | (0xA0 << pri_shift);
        gicd_w32(pri_word_off, pri_v);

        gicc_w32(GICC_CTLR, 1);    // enable CPU interface
        gicc_w32(GICC_PMR,  0xFF); // accept all priorities
        dsb_st();
    }
}

/// Send Software-Generated Interrupt `sgi_id` to the CPU with GIC interface
/// number `cpu` (0-7 on GICv2).
///
/// GICD_SGIR layout: [25:24] target-list filter (0 = use CPU target list),
/// [23:16] CPU target list bitmask, [3:0] SGI ID.
pub fn send_sgi(cpu: usize, sgi_id: u32) {
    if cpu >= 8 { return; } // GICv2 supports at most 8 CPU interfaces
    unsafe {
        gicd_w32(GICD_SGIR, ((1u32 << cpu) << 16) | (sgi_id & 0xF));
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
