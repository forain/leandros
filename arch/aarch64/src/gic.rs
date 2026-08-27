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
//! **All three targets are GICv2.** `scripts/run-qemu.sh:161` pins the virt
//! board to `-machine virt,gic-version=2`, and both Pi boards carry a GIC-400,
//! which *is* a GICv2 implementation. That is not a convenience assumption we
//! could relax later in one place: acknowledge and EOI here go through the
//! memory-mapped CPU interface (`GICC_IAR`/`GICC_EOIR`), which GICv3 replaces
//! wholesale with the `ICC_*` system registers. A GICv3 port would therefore
//! rewrite `ack`/`eoi`/`init` as well as the affinity routing — so `enable_spi`
//! deliberately programs only the GICv2 `GICD_ITARGETSR` 8-bit CPU mask rather
//! than pretending to abstract over a `GICD_IROUTER` path that nothing else in
//! this file could survive.
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
        gicd_set_byte_field(GICD_ITARGETSR,  id, SPI_TARGET_CPU_MASK);
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
pub fn init() {
    // Claim the built-in interrupts before anything can be delivered. IRQs are
    // still masked at EL1 here (`timer::init` does the `daifclr` afterwards),
    // but doing this first also means the distributor is never enabled with an
    // empty table.
    super::exception::register_builtin_handlers();

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
