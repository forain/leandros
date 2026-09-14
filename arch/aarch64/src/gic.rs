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
//! `init` enables PPI #27 (EL1 *virtual* timer, CNTV — the tick this kernel
//! actually programs, in `timer::init`) and SGI #1 (reschedule IPI) on the BSP,
//! plus SPI 33 (PL011). PPI #30, the physical timer, is dispatched too but
//! never enabled by us; firmware can leave it armed.
//!
//! **Both Pi boards are GICv2** (a GIC-400), and on those builds this file is
//! GICv2 only. **QEMU virt is GICv2 or GICv3 depending on `gic-version=`**,
//! and since QEMU 11.1 HVF on Apple Silicon refuses to launch a GICv2 machine
//! at all, so the virt build detects the version at `init` and drives either.
//!
//! The two differ in exactly the places this file touches, and nowhere else:
//!
//! * **Acknowledge / EOI.** GICv2 goes through the memory-mapped CPU
//!   interface (`GICC_IAR` / `GICC_EOIR`); GICv3 replaces that wholesale with
//!   the `ICC_*` system registers (`ICC_IAR1_EL1` / `ICC_EOIR1_EL1`), which
//!   must first be switched on through `ICC_SRE_EL1` — and, if the kernel was
//!   entered at EL2, through `ICC_SRE_EL2` before the drop (see
//!   `entry_aarch64.s` and the AP stub in `smp.rs`).
//! * **Per-CPU interrupts.** GICv2 banks the SGI/PPI words of the distributor
//!   per CPU; GICv3 moves them into a per-CPU *redistributor* frame, found by
//!   matching `GICR_TYPER`'s affinity against this CPU's `MPIDR_EL1`, and a
//!   sleeping redistributor must be woken (`GICR_WAKER`) before it forwards
//!   anything.
//! * **SPI routing.** GICv2's `GICD_ITARGETSR` is an 8-bit CPU mask; GICv3
//!   with affinity routing enabled (`GICD_CTLR.ARE`) ignores it and reads a
//!   64-bit `GICD_IROUTER[n]` holding an affinity value instead.
//! * **SGIs.** `GICD_SGIR` becomes `ICC_SGI1R_EL1`, targeted by affinity.
//!
//! Everything else — enable/disable/pending/priority/config arrays, the
//! `ITLinesNumber` field, the spurious ID 1023 — has the same layout in both,
//! and the dispatch table below is version-agnostic. Only four interrupts are
//! consumed in the whole kernel (PPI 27/30, SGI 1, SPI 33; every virtio device
//! is polled), no MSI is ever allocated, and so no ITS is needed.
//!
//! Ref: ARM GIC Architecture Specification v2.0; ARM IHI 0069 (GICv3/v4).

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

/// GICv3 layout on QEMU virt. The distributor is a 64 KiB frame there (GICv2
/// used 4 KiB): `GICD_IROUTER` starts at 0x6100 and the ID registers sit at
/// 0xFFD0+, so the virt build maps the whole frame. Redistributors start at
/// `GICR_BASE`, one `GICR_STRIDE` (RD frame + SGI frame, 64 KiB each) per CPU.
/// `lib.rs` maps `GICR_MAX_FRAMES` of them, matching `smp::MAX_CPUS`.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub const GICD_SIZE: usize = 0x1_0000;
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub const GICR_BASE: usize = 0x080A_0000;
pub const GICR_STRIDE: usize = 0x2_0000;
pub const GICR_MAX_FRAMES: usize = 8;

// Distributor register offsets.
//
// Every register this driver touches lives inside the first 4 KiB of the
// distributor frame (the highest is GICD_SGIR at 0xF00), which matters:
// `paging.rs:310` maps GICD with a single `map_4k`. Adding a register above
// 0xFFF — GICv2 has none we need — would fault, not silently misbehave.
const GICD_CTLR:        usize = 0x000; // distributor control
const GICD_TYPER:       usize = 0x004; // controller type (ITLinesNumber)
const GICD_ISENABLER0:  usize = 0x100; // set-enable   for IRQs 0-31 (banked per CPU for SGIs/PPIs)
const GICD_ICENABLER0:  usize = 0x180; // clear-enable for IRQs 0-31
const GICD_ICPENDR0:    usize = 0x280; // clear-pending for IRQs 0-31
const GICD_IPRIORITYR:  usize = 0x400; // priority    (1 byte / IRQ)
const GICD_ITARGETSR:   usize = 0x800; // target CPUs (1 byte / IRQ)
const GICD_ICFGR:       usize = 0xC00; // configuration (2 bits / IRQ)
const GICD_SGIR:        usize = 0xF00; // software-generated interrupt register
// GICv3-only distributor registers.
const GICD_IGROUPR:     usize = 0x080; // interrupt group (1 bit / IRQ; 1 = Group 1)
const GICD_IROUTER:     usize = 0x6100; // affinity routing (8 bytes / SPI), ARE=1 only
const GICD_PIDR2_V2:    usize = 0xFE8; // GICv2 location; ArchRev in bits [7:4] (v3 keeps it at 0xFFE8)
const GICD_CTLR_ARE:    u32 = 1 << 4;  // affinity routing enable (ARE / ARE_NS)
const GICD_CTLR_RWP:    u32 = 1 << 31; // register write pending

// GICv3 redistributor: RD frame, then the SGI frame 64 KiB above it.
const GICR_CTLR:        usize = 0x000;
const GICR_TYPER:       usize = 0x008; // 64-bit; [63:32] affinity, bit 4 Last
const GICR_WAKER:       usize = 0x014; // bit 1 ProcessorSleep, bit 2 ChildrenAsleep
const GICR_CTLR_RWP:    u32 = 1 << 3;
const GICR_SGI_BASE:    usize = 0x1_0000;
const GICR_IGROUPR0:    usize = 0x080;
const GICR_ISENABLER0:  usize = 0x100;
const GICR_ICENABLER0:  usize = 0x180;
const GICR_IPRIORITYR:  usize = 0x400;

/// SGI used as the cross-CPU reschedule IPI.
pub const SGI_RESCHED: u32 = 1;

/// PPI carrying the EL1 *virtual* timer (CNTV) — the tick this kernel runs on.
pub const PPI_VIRT_TIMER: u32 = 27;
/// PPI carrying the EL1 *physical* timer (CNTP). Not enabled by us, but
/// firmware/QEMU can leave it live, so it is dispatched to the same handler.
pub const PPI_PHYS_TIMER: u32 = 30;
/// SPI 1 / INTID 33: the PL011 UART, on every board this kernel targets.
pub const SPI_PL011: u32 = 33;

/// Priority every interrupt this kernel enables runs at.
///
/// One value for everything: GICv2 pre-emption is off (we never lower
/// `GICC_BPR`/re-enable IRQs inside a handler), so distinct priorities would
/// only reorder simultaneously-pending interrupts, and a *lower* number is
/// higher priority — an easy way to accidentally starve the timer. 0xA0 is the
/// mid-range value the timer PPI has always used here.
const IRQ_PRIORITY: u32 = 0xA0;

/// CPU-interface mask used for every SPI: CPU 0 only.
///
/// All device polling and timekeeping is already BSP-only (see
/// `timer::on_tick`, which gates everything but the local tick reload on
/// `arch_cpu_id() == 0`), so spreading SPIs across CPUs would create
/// concurrent consumers of single-consumer queues, not parallelism.
const SPI_TARGET_CPU_MASK: u32 = 0x01;

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
unsafe fn gicd_w64(off: usize, v: u64) {
    let base = mm::phys_to_virt(GICD_BASE);
    ((base + off) as *mut u64).write_volatile(v)
}
/// `rd` is a redistributor frame's *virtual* base (see `own_redistributor`).
unsafe fn gicr_r32(rd: usize, off: usize) -> u32 { ((rd + off) as *const u32).read_volatile() }
unsafe fn gicr_w32(rd: usize, off: usize, v: u32) { ((rd + off) as *mut u32).write_volatile(v) }
unsafe fn gicr_r64(rd: usize, off: usize) -> u64 { ((rd + off) as *const u64).read_volatile() }

// ── GICv3 mode ────────────────────────────────────────────────────────────
//
// Decided once by the BSP in `init`, read on every interrupt by `ack`/`eoi`.
// A relaxed load: the APs are started by CPU_ON strictly after `init` returns,
// and the BSP's own first interrupt cannot arrive before `init` unmasks it.

use core::sync::atomic::AtomicBool;

static V3: AtomicBool = AtomicBool::new(false);

/// Packed affinity of the BSP in `GICD_IROUTER` layout (Aff3 at [39:32],
/// Aff2/1/0 at [23:0]). Every SPI is routed here, matching the GICv2 path's
/// `SPI_TARGET_CPU_MASK = 0x01`.
static BSP_ROUTE: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

#[inline]
pub fn is_v3() -> bool { V3.load(Ordering::Relaxed) }

/// The `ICC_*` system registers by encoding rather than name, so the kernel's
/// own target features (a softfloat build, see `project_aarch64_kernel_fpsimd`)
/// never decide whether the assembler accepts them.
mod icc {
    macro_rules! sysreg {
        ($name:ident, $enc:literal) => {
            #[allow(dead_code)]
            pub mod $name {
                #[inline(always)]
                pub unsafe fn read() -> u64 {
                    let v: u64;
                    core::arch::asm!(concat!("mrs {}, ", $enc), out(reg) v, options(nomem, nostack));
                    v
                }
                #[inline(always)]
                pub unsafe fn write(v: u64) {
                    core::arch::asm!(concat!("msr ", $enc, ", {}"), in(reg) v, options(nomem, nostack));
                }
            }
        };
    }
    sysreg!(sre_el1,    "S3_0_C12_C12_5"); // ICC_SRE_EL1
    sysreg!(pmr_el1,    "S3_0_C4_C6_0");   // ICC_PMR_EL1
    sysreg!(igrpen1_el1,"S3_0_C12_C12_7"); // ICC_IGRPEN1_EL1
    sysreg!(iar1_el1,   "S3_0_C12_C12_0"); // ICC_IAR1_EL1
    sysreg!(eoir1_el1,  "S3_0_C12_C12_1"); // ICC_EOIR1_EL1
    sysreg!(sgi1r_el1,  "S3_0_C12_C11_5"); // ICC_SGI1R_EL1
}

#[inline(always)]
unsafe fn isb() { core::arch::asm!("isb", options(nomem, nostack)); }

fn read_mpidr() -> u64 {
    let v: u64;
    unsafe { core::arch::asm!("mrs {}, mpidr_el1", out(reg) v, options(nomem, nostack)); }
    v
}

/// `MPIDR_EL1` → the 32-bit affinity value `GICR_TYPER[63:32]` reports
/// (Aff3:Aff2:Aff1:Aff0, one byte each).
fn mpidr_to_typer_affinity(mpidr: u64) -> u32 {
    ((mpidr & 0x00FF_FFFF) | ((mpidr >> 32 & 0xFF) << 24)) as u32
}

/// `MPIDR_EL1` → `GICD_IROUTER` layout (Aff3 at [39:32], Aff2:Aff1:Aff0 at [23:0]).
fn mpidr_to_irouter(mpidr: u64) -> u64 {
    (mpidr & 0x00FF_FFFF) | (mpidr & 0xFF_0000_0000)
}

/// Is this a GICv3? Virt build only; both Pi boards are GIC-400 by construction.
///
/// A functional probe rather than an ID register: write `GICD_CTLR.ARE` (with
/// both group enables clear, the only state in which the spec allows ARE to
/// change) and read it back. On every GICv2 that bit is reserved, RAZ/WI —
/// QEMU's `arm_gic` keeps only the two enable bits, and the GIC-400 TRM lists
/// it reserved — so it reads back 0. On a GICv3 it sticks, and it is the very
/// bit `init_dist_v3` needs set anyway, so the probe costs nothing.
///
/// The obvious alternatives are both wrong here. `ID_AA64PFR0_EL1.GIC` is 0
/// under HVF with `-cpu host` (QEMU masks it, since the Apple in-kernel GIC
/// bypasses the CPU-interface model that would set it) — measured, and the
/// reason this is not the ID check. `GICD_PIDR2` sits at 0xFFE8 on v3 but
/// 0xFE8 on v2, and reading the v3 offset on a QEMU GICv2 board lands in an
/// unassigned hole above its 4 KiB frame, which TCG turns into an external
/// abort. The v2-offset value is still logged for the record.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
fn detect_v3() -> bool {
    let pfr0: u64;
    unsafe { core::arch::asm!("mrs {}, id_aa64pfr0_el1", out(reg) pfr0, options(nomem, nostack)); }
    let (pidr2_v2, ctlr) = unsafe {
        let pidr2_v2 = gicd_r32(GICD_PIDR2_V2);
        gicd_w32(GICD_CTLR, GICD_CTLR_ARE);
        gicd_wait_rwp();
        (pidr2_v2, gicd_r32(GICD_CTLR))
    };
    let v3 = ctlr & GICD_CTLR_ARE != 0;
    crate::uart::serial_print_str("[GIC] ID_AA64PFR0.GIC=");
    crate::uart::print_hex(((pfr0 >> 24) & 0xF) as usize);
    crate::uart::serial_print_str(" PIDR2@0xFE8=");
    crate::uart::print_hex(pidr2_v2 as usize);
    crate::uart::serial_print_str(" GICD_CTLR after ARE write=");
    crate::uart::print_hex(ctlr as usize);
    crate::uart::serial_print_str(if v3 { " -> GICv3\n" } else { " -> GICv2\n" });
    v3
}
#[cfg(any(feature = "rpi5", feature = "raspi4b"))]
fn detect_v3() -> bool { false }

/// Spin until the distributor has absorbed a `GICD_CTLR` write. Bounded so a
/// misdetected controller degrades to a slow boot, not a silent hang.
unsafe fn gicd_wait_rwp() {
    for _ in 0..1_000_000 {
        if gicd_r32(GICD_CTLR) & GICD_CTLR_RWP == 0 { return; }
    }
    crate::uart::serial_print_str("[GIC] GICD_CTLR.RWP never cleared\n");
}

unsafe fn gicr_wait_rwp(rd: usize) {
    for _ in 0..1_000_000 {
        if gicr_r32(rd, GICR_CTLR) & GICR_CTLR_RWP == 0 { return; }
    }
    crate::uart::serial_print_str("[GIC] GICR_CTLR.RWP never cleared\n");
}

/// Virtual base of this CPU's redistributor frame, found by walking the
/// region and matching `GICR_TYPER`'s affinity to our `MPIDR_EL1`. `None` if
/// the walk hits the Last frame without a match — a CPU the GIC cannot see.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
unsafe fn own_redistributor() -> Option<usize> {
    let want = mpidr_to_typer_affinity(read_mpidr());
    for i in 0..GICR_MAX_FRAMES {
        let rd = mm::phys_to_virt(GICR_BASE + i * GICR_STRIDE);
        let typer = gicr_r64(rd, GICR_TYPER);
        if (typer >> 32) as u32 == want { return Some(rd); }
        if typer & (1 << 4) != 0 { break; } // Last
    }
    None
}
#[cfg(any(feature = "rpi5", feature = "raspi4b"))]
unsafe fn own_redistributor() -> Option<usize> { None }

/// GICv3 per-CPU bring-up: wake this CPU's redistributor, enable its PPI 27 and
/// SGI 1 there, then switch on the system-register CPU interface. Run by the
/// BSP from `init` and by every AP from `init_cpu_interface`.
unsafe fn init_cpu_v3() {
    let Some(rd) = own_redistributor() else {
        crate::uart::serial_print_str("[GIC] no redistributor matches MPIDR ");
        crate::uart::print_hex(read_mpidr() as usize);
        crate::uart::serial_print_str("\n");
        return;
    };

    // Wake: clear ProcessorSleep, wait for ChildrenAsleep to drop.
    gicr_w32(rd, GICR_WAKER, gicr_r32(rd, GICR_WAKER) & !(1 << 1));
    for _ in 0..1_000_000 {
        if gicr_r32(rd, GICR_WAKER) & (1 << 2) == 0 { break; }
    }

    let sgi = rd + GICR_SGI_BASE;
    // Everything per-CPU is Group 1 (the group ICC_IGRPEN1_EL1 enables and
    // IAR1/EOIR1 serve); mask all first so the enables below are the only
    // thing that opens the gate.
    gicr_w32(sgi, GICR_ICENABLER0, 0xFFFF_FFFF);
    gicr_wait_rwp(rd);
    gicr_w32(sgi, GICR_IGROUPR0, 0xFFFF_FFFF);
    for id in [PPI_VIRT_TIMER, SGI_RESCHED] {
        let off   = GICR_IPRIORITYR + (id as usize / 4) * 4;
        let shift = (id % 4) * 8;
        let v = (gicr_r32(sgi, off) & !(0xFF << shift)) | (IRQ_PRIORITY << shift);
        gicr_w32(sgi, off, v);
    }
    gicr_w32(sgi, GICR_ISENABLER0, (1 << PPI_VIRT_TIMER) | (1 << SGI_RESCHED));
    dsb_st();

    // CPU interface: system registers on (SRE), accept every priority, Group 1
    // enabled. The isb after SRE is what makes the rest of the ICC_* space
    // accessible at all.
    icc::sre_el1::write(icc::sre_el1::read() | 1);
    isb();
    icc::pmr_el1::write(0xFF);
    icc::igrpen1_el1::write(1);
    isb();
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

/// Read-modify-write one byte-wide field of a GICD register array
/// (`GICD_IPRIORITYR`, `GICD_ITARGETSR`: 4 interrupts per 32-bit word).
unsafe fn gicd_set_byte_field(array_base: usize, id: u32, value: u32) {
    let off   = array_base + (id as usize / 4) * 4;
    let shift = (id % 4) * 8;
    let v = (gicd_r32(off) & !(0xFF << shift)) | ((value & 0xFF) << shift);
    gicd_w32(off, v);
}

/// Set one interrupt's trigger configuration in `GICD_ICFGR` (2 bits per
/// interrupt, 16 per word; bit[1] selects edge). Must only be called while the
/// interrupt is disabled — GICv2 leaves the effect of changing ICFGR on an
/// enabled or pending interrupt UNPREDICTABLE.
///
/// Ignored for SGIs (always edge) and, on some implementations, PPIs
/// (read-only) — writing them is harmless but pointless, so we skip.
unsafe fn gicd_set_cfg(id: u32, edge: bool) {
    if id < 32 { return; }
    let off   = GICD_ICFGR + (id as usize / 16) * 4;
    let shift = (id % 16) * 2;
    let mut v = gicd_r32(off) & !(0b11 << shift);
    if edge { v |= 0b10 << shift; }
    gicd_w32(off, v);
}

// ── Interrupt dispatch table ──────────────────────────────────────────────
//
// The table replaces the `if id == 27 … else if id == 33 …` chain that used to
// live in `exception.rs::handle_irq`, so a driver can claim an interrupt
// without editing the exception path.
//
// **IRQ-context safety (the whole reason this is an atomic array).** Commit
// `82d0cc3` records the failure mode this design exists to make impossible: a
// handler that takes a lock ordered against the scheduler's `RUN_QUEUE`, or
// that touches user memory and demand-faults, re-enters the scheduler lock and
// freezes every vCPU with interrupts masked and *no panic at all* — nothing
// prints, so there is nothing to debug from. A second incident
// (`virtio_keyboard::init` holding a lock across probe while the timer IRQ
// polled the same structure) is the same shape from the registration side.
//
// So the table is not a `Mutex<[…]>` and not a `try_lock`: it is one
// `AtomicUsize` per interrupt ID holding a raw `fn()` pointer, `0` meaning
// unclaimed. Dispatch is a single relaxed-cost `Acquire` load; registration is
// a single `Release` store. There is no lock to hold, so there is no lock a
// handler can be waiting on, and no ordering to get wrong — a driver may
// register from its init path while another CPU is dispatching a different
// (or the same) interrupt, with no window in which the slot is torn or
// half-written. The `Release`/`Acquire` pair additionally publishes whatever
// state the driver initialised *before* registering, so the first interrupt
// cannot observe a half-built device.
//
// The constraints the table cannot enforce, and that every registered handler
// must therefore honour, are stated on `register_handler`.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Number of interrupt IDs the dispatch table covers: SGIs 0-15, PPIs 16-31,
/// SPIs 32-511.
///
/// Sized from what the three targets can actually raise, not from a round
/// number:
///
/// * **GIC-400 (both Pi boards)** implements up to 480 SPIs, i.e. INTIDs up to
///   511 — this is the binding constraint. BCM2712's SPI numbering is sparse
///   and reaches well into the 200s, so the 64 entries an earlier sketch
///   suggested would have covered *none* of the Pi 5 peripherals, V3D
///   included, and would have failed as a silent "unhandled IRQ" rather than
///   as a build error.
/// * **QEMU virt, gic-version=2** tops out at INTID 175: the board's GICv2m
///   frame allocates 64 MSI SPIs from SPI 80, i.e. INTIDs 112-175, above the
///   32 virtio-mmio transports at INTIDs 48-79.
///
/// 512 covers both with room for the whole architectural GIC-400 range. The
/// cost is 512 × 8 B = 4 KiB, and because every entry initialises to zero it
/// lands in `.bss` — it adds nothing to the kernel image, which matters here
/// (`project_box_x86_boot_failure.md`: this kernel has been bitten by `.data`
/// growth before).
pub const NUM_IRQS: usize = 512;

/// One slot per interrupt ID. `0` = unclaimed; otherwise a `fn()` pointer.
///
/// A `fn()` is never null, so `0` is an unambiguous sentinel.
static HANDLERS: [AtomicUsize; NUM_IRQS] = [const { AtomicUsize::new(0) }; NUM_IRQS];

/// Claim interrupt `id`; `f` is then called from IRQ context on every delivery.
///
/// This only wires up dispatch — it does **not** enable the interrupt at the
/// distributor. Call `enable_spi` afterwards, or use `request_irq`, which does
/// both in the correct order.
///
/// # Contract every handler must satisfy
///
/// `f` runs on the interrupt stack with IRQs masked, in whatever task context
/// happened to be running. It must therefore:
///
/// * **never touch user memory** — a demand-paging fault from IRQ context
///   re-enters the fault path underneath the scheduler and hangs the machine
///   with no output (`82d0cc3`). Hand bytes to a server queue instead;
///   `evdev_server::push_event`, which the UART handler uses, is the model.
/// * **never take a lock that is ordered against `RUN_QUEUE`**, and never
///   block. Anything that could sleep belongs in a bottom half woken from
///   here, not here.
/// * **quiesce its device before returning.** EOI is written by the common
///   dispatcher immediately afterwards; a level-triggered SPI whose device
///   condition is still asserted re-fires immediately and livelocks the CPU.
pub fn register_handler(id: u32, f: fn()) {
    if (id as usize) >= NUM_IRQS {
        crate::uart::serial_print_str("[GIC] register_handler: IRQ id out of range: ");
        crate::uart::print_hex(id as usize);
        crate::uart::serial_print_str("\n");
        return;
    }
    HANDLERS[id as usize].store(f as usize, Ordering::Release);
}

/// Release interrupt `id`. Subsequent deliveries take the unhandled path.
pub fn unregister_handler(id: u32) {
    if (id as usize) >= NUM_IRQS { return; }
    HANDLERS[id as usize].store(0, Ordering::Release);
}

/// Register a handler for an SPI and enable it, in the only safe order.
///
/// Registering *first* is not stylistic. `enable_spi` can be answered by a
/// device that already has a condition latched, so an enable-then-register
/// sequence has a real window in which the interrupt arrives at an empty slot;
/// the unhandled path would then mask the SPI off again (see
/// `exception::handle_irq`) and the driver would never hear from its device.
/// This is the `virtio_keyboard::init` race in miniature, and the one-line fix
/// for it is to make the ordering the API rather than a comment.
pub fn request_irq(id: u32, f: fn()) {
    register_handler(id, f);
    enable_spi(id);
}

/// Invoke the handler registered for `id`, if any. Returns `false` when the
/// interrupt is unclaimed.
#[inline]
pub fn dispatch(id: u32) -> bool {
    if (id as usize) >= NUM_IRQS { return false; }
    let p = HANDLERS[id as usize].load(Ordering::Acquire);
    if p == 0 { return false; }
    // SAFETY: the slot is only ever written by `register_handler` from a valid
    // `fn()`, which has the same size and ABI as a `usize` on AArch64, and `0`
    // (the only other value it can hold) was excluded above.
    let f: fn() = unsafe { core::mem::transmute::<usize, fn()>(p) };
    f();
    true
}

// ── SPI enable / disable ──────────────────────────────────────────────────

/// Number of interrupt IDs this distributor implements, decoded from
/// `GICD_TYPER.ITLinesNumber` as `32 × (ITLinesNumber + 1)`.
///
/// Diagnostic only — `enable_spi` bounds against `NUM_IRQS`, which is a
/// compile-time property of the dispatch table. Useful when bringing up a new
/// board to check that an SPI from its device tree is one this GIC can even
/// raise. QEMU virt reports 224 here; a GIC-400 reports up to 512.
pub fn num_irqs_implemented() -> u32 {
    let typer = unsafe { gicd_r32(GICD_TYPER) };
    32 * ((typer & 0x1F) + 1)
}

/// Enable a Shared Peripheral Interrupt (`id` >= 32): route it to CPU 0 at the
/// common priority, configure it level-sensitive, then set its enable bit.
///
/// The order is deliberate — priority, target and trigger are all programmed
/// *before* the enable bit, so the interrupt cannot be delivered while its
/// target mask is still 0 (delivered to no CPU, i.e. lost) and cannot be
/// enabled while `GICD_ICFGR` is being changed, which GICv2 leaves
/// UNPREDICTABLE.
///
/// Level-sensitive is the right default for everything this kernel drives:
/// PL011, the SDHCI controller, virtio-mmio transports and V3D all assert a
/// level until the driver clears the condition in the device. Use
/// `enable_spi_edge` for a genuinely edge-triggered source.
pub fn enable_spi(id: u32) {
    enable_spi_configured(id, false);
}

/// As `enable_spi`, but configures the SPI edge-triggered.
pub fn enable_spi_edge(id: u32) {
    enable_spi_configured(id, true);
}

fn enable_spi_configured(id: u32, edge: bool) {
    // SGIs and PPIs are banked per CPU and are set up by `init` /
    // `init_cpu_interface`; routing them through here would program the
    // calling CPU's bank only, which is a silent half-enable on SMP.
    if id < 32 || (id as usize) >= NUM_IRQS {
        crate::uart::serial_print_str("[GIC] enable_spi: not an SPI id: ");
        crate::uart::print_hex(id as usize);
        crate::uart::serial_print_str("\n");
        return;
    }
    unsafe {
        gicd_set_cfg(id, edge);
        gicd_set_byte_field(GICD_IPRIORITYR, id, IRQ_PRIORITY);
        if is_v3() {
            // ARE=1: ITARGETSR is ignored; IROUTER carries the target affinity.
            gicd_w64(GICD_IROUTER + id as usize * 8, BSP_ROUTE.load(Ordering::Relaxed));
        } else {
            gicd_set_byte_field(GICD_ITARGETSR,  id, SPI_TARGET_CPU_MASK);
        }
        dsb_st();
        gicd_w32(GICD_ISENABLER0 + (id as usize / 32) * 4, 1 << (id % 32));
        dsb_st();
    }
}

/// Disable a Shared Peripheral Interrupt and drop any pending state for it.
///
/// Clearing pending as well as enable matters for the unhandled-IRQ path: a
/// level-triggered SPI nobody claimed is still asserted by its device, and
/// leaving it latched pending would have it delivered again the moment
/// anything re-enables it.
pub fn disable_spi(id: u32) {
    if id < 32 || (id as usize) >= NUM_IRQS { return; }
    unsafe {
        gicd_w32(GICD_ICENABLER0 + (id as usize / 32) * 4, 1 << (id % 32));
        gicd_w32(GICD_ICPENDR0   + (id as usize / 32) * 4, 1 << (id % 32));
        dsb_st();
    }
}

/// Initialise GICv2 and enable PPI #27 (EL1 virtual timer).
/// GICv3 distributor bring-up, BSP only. Affinity routing on, every SPI in
/// Group 1, then enable.
///
/// `GICD_CTLR`'s bit assignments depend on `DS` (Disable Security): with DS=1
/// (QEMU virt, `secure=off`) bit 0 is EnableGrp0 and bit 1 EnableGrp1NS; from
/// the Non-secure view of a DS=0 controller bit 0 is EnableGrp1 and bit 1
/// EnableGrp1A. Setting both low bits plus ARE is correct under either reading
/// — an enabled Group 0 with nothing configured in it delivers nothing.
unsafe fn init_dist_v3() {
    gicd_w32(GICD_CTLR, GICD_CTLR_ARE);
    gicd_wait_rwp();
    let words = (num_irqs_implemented() / 32) as usize;
    for w in 1..words { // word 0 is SGIs/PPIs, owned by the redistributors
        gicd_w32(GICD_IGROUPR + w * 4, 0xFFFF_FFFF);
    }
    gicd_w32(GICD_CTLR, GICD_CTLR_ARE | 0b11);
    gicd_wait_rwp();
    dsb_st();
}

pub fn init() {
    // Claim the built-in interrupts before anything can be delivered. IRQs are
    // still masked at EL1 here (`timer::init` does the `daifclr` afterwards),
    // but doing this first also means the distributor is never enabled with an
    // empty table.
    super::exception::register_builtin_handlers();

    if detect_v3() {
        V3.store(true, Ordering::Release);
        BSP_ROUTE.store(mpidr_to_irouter(read_mpidr()), Ordering::Release);
        unsafe { init_dist_v3(); init_cpu_v3(); }
        enable_spi(SPI_PL011);
        return;
    }

    unsafe {
        // Enable distributor.
        gicd_w32(GICD_CTLR, 1);
        dsb_st();

        // Enable PPI 27 (Virtual Timer) and SGI 1 (reschedule IPI).
        // This word is banked per CPU for IRQs 0-31 — this enables them for
        // the BSP; each AP does the same in init_cpu_interface().
        gicd_w32(GICD_ISENABLER0, (1 << PPI_VIRT_TIMER) | (1 << SGI_RESCHED));
        gicd_set_byte_field(GICD_IPRIORITYR, PPI_VIRT_TIMER, IRQ_PRIORITY);
        gicd_set_byte_field(GICD_ITARGETSR,  PPI_VIRT_TIMER, SPI_TARGET_CPU_MASK);
        dsb_st();

        // Enable CPU interface.
        gicc_w32(GICC_CTLR, 1);
        // Accept any priority (mask = 0xFF = accept all).
        gicc_w32(GICC_PMR, 0xFF);
        dsb_st();
    }

    // SPI 1 / INTID 33: the PL011 UART. Now just the first customer of the
    // generic path rather than a hand-unrolled special case.
    enable_spi(SPI_PL011);
}

/// Initialise only the CPU interface for a secondary CPU (AP).
///
/// The distributor was already configured by the BSP; each AP must separately
/// enable its own banked registers: the CPU interface (GICC_*) plus the
/// banked GICD_ISENABLER0 word covering SGIs and PPIs — without the latter,
/// this CPU would never receive its virtual-timer PPI 27 or reschedule SGI 1.
pub fn init_cpu_interface() {
    // Idempotent: re-publishing the same `fn()` pointers is a plain atomic
    // store per slot, so an AP doing this concurrently with a dispatch on the
    // BSP cannot observe a torn slot. Repeated here rather than assumed from
    // `init` so that any future AP-only or AP-first bring-up path is still
    // covered — the cost is a handful of stores, once per CPU.
    super::exception::register_builtin_handlers();

    if is_v3() {
        unsafe { init_cpu_v3(); }
        return;
    }

    unsafe {
        // Banked per-CPU enables: virtual timer PPI 27 + reschedule SGI 1.
        gicd_w32(GICD_ISENABLER0, (1 << PPI_VIRT_TIMER) | (1 << SGI_RESCHED));

        // Banked per-CPU priority for PPI 27 (match the BSP's).
        gicd_set_byte_field(GICD_IPRIORITYR, PPI_VIRT_TIMER, IRQ_PRIORITY);

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
    if is_v3() {
        // ICC_SGI1R_EL1: Aff3 [55:48], Aff2 [39:32], INTID [27:24],
        // Aff1 [23:16], TargetList [15:0] — one bit per Aff0 within the
        // (Aff3,Aff2,Aff1) cluster. The target's MPIDR comes from what it
        // recorded at entry; a CPU that has not recorded one yet is addressed
        // as Aff0 = index in cluster 0, which is what QEMU virt reports.
        let mpidr = super::smp::mpidr_of(cpu).unwrap_or(cpu as u64);
        let aff0 = mpidr & 0xFF;
        if aff0 >= 16 { return; }
        let v = ((mpidr >> 32) & 0xFF) << 48
              | ((mpidr >> 16) & 0xFF) << 32
              | ((sgi_id as u64) & 0xF) << 24
              | ((mpidr >> 8) & 0xFF) << 16
              | 1u64 << aff0;
        unsafe {
            dsb_st();
            icc::sgi1r_el1::write(v);
            isb();
        }
        return;
    }
    if cpu >= 8 { return; } // GICv2 supports at most 8 CPU interfaces
    unsafe {
        gicd_w32(GICD_SGIR, ((1u32 << cpu) << 16) | (sgi_id & 0xF));
        dsb_st();
    }
}

/// Acknowledge the current interrupt; returns the raw IAR value.
#[inline]
pub fn ack() -> u32 {
    if is_v3() {
        // INTID is 24 bits on GICv3 (bits above 511 are LPIs, which this
        // kernel never allocates); 1023 is spurious in both.
        unsafe { icc::iar1_el1::read() as u32 }
    } else {
        unsafe { gicc_r32(GICC_IAR) }
    }
}

/// Signal end-of-interrupt.
#[inline]
pub fn eoi(iar: u32) {
    if is_v3() {
        unsafe { icc::eoir1_el1::write(iar as u64); isb(); }
    } else {
        unsafe { gicc_w32(GICC_EOIR, iar); }
    }
}

/// Extract the interrupt ID from a raw IAR value: bits [9:0] on GICv2 (the
/// rest is the source CPU of an SGI), [23:0] on GICv3.
#[inline]
pub fn irq_id(iar: u32) -> u32 {
    if is_v3() { iar & 0xFF_FFFF } else { iar & 0x3FF }
}
