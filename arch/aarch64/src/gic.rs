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
pub const GICD_BASE_DEFAULT: usize = 0x0800_0000;
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub const GICC_BASE_DEFAULT: usize = 0x0801_0000;

#[cfg(feature = "rpi5")]
pub const GICD_BASE_DEFAULT: usize = 0x107F_FF90_00;
#[cfg(feature = "rpi5")]
pub const GICC_BASE_DEFAULT: usize = 0x107F_FFA0_00;

#[cfg(feature = "raspi4b")]
pub const GICD_BASE_DEFAULT: usize = 0xFF84_1000;
#[cfg(feature = "raspi4b")]
pub const GICC_BASE_DEFAULT: usize = 0xFF84_2000;

/// GICv3 layout on QEMU virt. The distributor is a 64 KiB frame there (GICv2
/// used 4 KiB): `GICD_IROUTER` starts at 0x6100 and the ID registers sit at
/// 0xFFD0+, so the virt build maps the whole frame. Redistributors start at
/// `GICR_BASE`, one `GICR_STRIDE` (RD frame + SGI frame, 64 KiB each) per CPU.
/// `lib.rs` maps `GICR_MAX_FRAMES` of them, matching `smp::MAX_CPUS`.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub const GICD_SIZE: usize = 0x1_0000;
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub const GICR_BASE_DEFAULT: usize = 0x080A_0000;
pub const GICR_STRIDE: usize = 0x2_0000;
pub const GICR_MAX_FRAMES: usize = sched::MAX_CPUS;

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
const GICD_ISPENDR0:    usize = 0x200; // set-pending for IRQs 0-31
const GICD_ICPENDR0:    usize = 0x280; // clear-pending for IRQs 0-31
const GICD_ISACTIVER0:  usize = 0x300; // set-active (read: active state)
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


// ── Runtime platform values ───────────────────────────────────────────────────
//
// The consts above are the QEMU virt / Pi defaults. An ACPI boot (MADT, GTDT)
// overrides them through `set_platform` before `init` maps and touches the GIC.
use core::sync::atomic::{AtomicU32, Ordering as RtOrd};

#[cfg(any(feature = "rpi5", feature = "raspi4b"))]
const GICR_BASE_DEFAULT: usize = 0;
pub const GICR_MAX_RANGES: usize = 8;
static GICD_RT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(GICD_BASE_DEFAULT);
static GICC_RT: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(GICC_BASE_DEFAULT);
static GICR_RT_BASE: [core::sync::atomic::AtomicUsize; GICR_MAX_RANGES] = [const { core::sync::atomic::AtomicUsize::new(0) }; GICR_MAX_RANGES];
static GICR_RT_LEN: [core::sync::atomic::AtomicUsize; GICR_MAX_RANGES] = [const { core::sync::atomic::AtomicUsize::new(0) }; GICR_MAX_RANGES];
static PPI_VIRT_RT: AtomicU32 = AtomicU32::new(27);
static PPI_PHYS_RT: AtomicU32 = AtomicU32::new(30);

#[inline] pub fn gicd_base() -> usize { GICD_RT.load(RtOrd::Relaxed) }
#[inline] pub fn gicc_base() -> usize { GICC_RT.load(RtOrd::Relaxed) }
#[inline] pub fn ppi_virt_timer() -> u32 { PPI_VIRT_RT.load(RtOrd::Relaxed) }
#[inline] pub fn ppi_phys_timer() -> u32 { PPI_PHYS_RT.load(RtOrd::Relaxed) }

/// Discovered GIC / timer values (ACPI MADT + GTDT). Zero fields keep the default.
pub struct PlatformGic<'a> {
    pub gicd: usize,
    pub gicc: usize,
    /// Redistributor discovery ranges (base, length in bytes).
    pub gicr: &'a [(usize, usize)],
    pub ppi_virt: u32,
    pub ppi_phys: u32,
}

pub fn set_platform(p: &PlatformGic) {
    if p.gicd != 0 { GICD_RT.store(p.gicd, RtOrd::Relaxed); }
    if p.gicc != 0 { GICC_RT.store(p.gicc, RtOrd::Relaxed); }
    if p.ppi_virt != 0 { PPI_VIRT_RT.store(p.ppi_virt, RtOrd::Relaxed); }
    if p.ppi_phys != 0 { PPI_PHYS_RT.store(p.ppi_phys, RtOrd::Relaxed); }
    let mut n = 0;
    for &(b, l) in p.gicr.iter().take(GICR_MAX_RANGES) {
        if b == 0 || l == 0 { continue; }
        GICR_RT_BASE[n].store(b, RtOrd::Relaxed);
        GICR_RT_LEN[n].store(l, RtOrd::Relaxed);
        n += 1;
    }
}

/// The redistributor ranges in force: discovered ones, else the QEMU virt default.
pub fn gicr_ranges(i: usize) -> Option<(usize, usize)> {
    let b = GICR_RT_BASE[i.min(GICR_MAX_RANGES - 1)].load(RtOrd::Relaxed);
    if i == 0 && b == 0 {
        return if GICR_BASE_DEFAULT != 0 { Some((GICR_BASE_DEFAULT, GICR_STRIDE * GICR_MAX_FRAMES)) } else { None };
    }
    if i >= GICR_MAX_RANGES || b == 0 { None } else { Some((b, GICR_RT_LEN[i].load(RtOrd::Relaxed))) }
}

/// Number of redistributor frames the kernel may touch (capped at `GICR_MAX_FRAMES`).
pub fn gicr_frames() -> usize {
    let mut n = 0;
    for r in 0..GICR_MAX_RANGES {
        let Some((_, l)) = gicr_ranges(r) else { break; };
        n += l / GICR_STRIDE;
    }
    n.min(GICR_MAX_FRAMES)
}

/// HHDM virtual base of the `i`-th redistributor frame across all ranges.
unsafe fn gicr_virt(i: usize) -> Option<usize> {
    let mut left = i;
    for r in 0..GICR_MAX_RANGES {
        let (b, l) = gicr_ranges(r)?;
        let frames = l / GICR_STRIDE;
        if left < frames { return Some(mm::phys_to_virt(b + left * GICR_STRIDE)); }
        left -= frames;
    }
    None
}

/// SGI used as the cross-CPU reschedule IPI.
pub const SGI_RESCHED: u32 = 1;

/// SGI that kicks a CPU out of guest mode (KVM_RUN signal/IRQ-line kick). The
/// handler is a no-op: taking the IRQ out of the guest is the point.
pub const SGI_KVM_KICK: u32 = 2;
/// vGIC maintenance interrupt (PPI 9 = INTID 25 on GIC-400 and QEMU virt).
pub const PPI_KVM_MAINT: u32 = 25;
/// The guest's EL1 virtual timer PPI (architectural INTID 27). With KVM the host
/// tick moved to the EL2 timer (`ppi_virt_timer()`), so 27 is free for guests; it
/// is enabled only while a guest runs (`ppi_set_enabled`).
pub const PPI_GUEST_VTIMER: u32 = 27;
#[inline] pub fn ppi_virt_timer_guest() -> u32 { PPI_GUEST_VTIMER }

/// Enable or disable banked PPI/SGI `id` (< 32) on the calling CPU. Used by KVM
/// to arm the guest-timer PPI around a guest run.
#[cfg(feature = "kvm")]
pub fn ppi_set_enabled(id: u32, on: bool) {
    static RD: [core::sync::atomic::AtomicUsize; sched::MAX_CPUS] = [const { core::sync::atomic::AtomicUsize::new(0) }; sched::MAX_CPUS];
    unsafe {
        if is_v3() {
            let cpu = sched::cpu_id().min(sched::MAX_CPUS - 1);
            let mut rd = RD[cpu].load(Ordering::Relaxed);
            if rd == 0 {
                rd = own_redistributor().unwrap_or(0);
                RD[cpu].store(rd, Ordering::Relaxed);
            }
            if rd == 0 { return; }
            gicr_w32(rd + GICR_SGI_BASE, if on { GICR_ISENABLER0 } else { GICR_ICENABLER0 }, 1 << id);
        } else {
            gicd_w32(if on { GICD_ISENABLER0 } else { GICD_ICENABLER0 }, 1 << id);
        }
    }
}

/// Extra banked enables only a KVM build turns on (SGI 2 and the maintenance PPI).
pub const KVM_ENABLE_MASK: u32 = if cfg!(feature = "kvm") { (1 << SGI_KVM_KICK) | (1 << PPI_KVM_MAINT) } else { 0 };

/// PPI carrying the EL1 *virtual* timer (CNTV) — the tick this kernel runs on.
const PPI_VIRT_TIMER_DEFAULT: u32 = 27;
/// PPI carrying the EL1 *physical* timer (CNTP). Not enabled by us, but
/// firmware/QEMU can leave it live, so it is dispatched to the same handler.
const PPI_PHYS_TIMER_DEFAULT: u32 = 30;
/// The console PL011's INTID. QEMU virt: SPI 1 / INTID 33.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub const SPI_PL011: u32 = 33;
/// BCM2711 (QEMU raspi4b / Pi 4) uart0: DTB `interrupts = <0 121 4>`, INTID 153.
#[cfg(feature = "raspi4b")]
pub const SPI_PL011: u32 = 32 + 121;
/// Raspberry Pi 5 uart10 (0x10_7D001000): DTB `interrupts = <0 121 4>`, i.e.
/// SPI 121 / INTID 153, level-high. Until 2026-10-26 the Pi enabled INTID 33,
/// so RX was only polled at tick rate and line-speed input overran the FIFO.
#[cfg(feature = "rpi5")]
pub const SPI_PL011: u32 = 32 + 121;

/// Priority every interrupt this kernel enables runs at.
///
/// One value for everything: GICv2 pre-emption is off (we never lower
/// `GICC_BPR`/re-enable IRQs inside a handler), so distinct priorities would
/// only reorder simultaneously-pending interrupts, and a *lower* number is
/// higher priority — an easy way to accidentally starve the timer. 0xA0 is the
/// mid-range value the timer PPI has always used here.
const IRQ_PRIORITY: u32 = 0xA0;

// Every SPI targets the BSP only (GICv2: its CPU-interface mask, recorded in
// `V2_IFACE[0]`; GICv3: `BSP_ROUTE`). All device polling and timekeeping is
// already BSP-only (see `timer::on_tick`, which gates everything but the
// local tick reload on `arch_cpu_id() == 0`), so spreading SPIs across CPUs
// would create concurrent consumers of single-consumer queues, not
// parallelism.

// CPU interface register offsets
const GICC_CTLR: usize = 0x000; // CPU interface control
const GICC_PMR:  usize = 0x004; // priority mask
const GICC_IAR:  usize = 0x00C; // interrupt acknowledge (read)
const GICC_EOIR: usize = 0x010; // end-of-interrupt (write)
const GICC_RPR:   usize = 0x014; // running priority (read)
const GICC_HPPIR: usize = 0x018; // highest-priority pending interrupt (read, no ack)

/// Spurious interrupt — IAR returns this value when there is no pending IRQ.
pub const SPURIOUS: u32 = 1023;

// ── Helpers ───────────────────────────────────────────────────────────────

unsafe fn gicd_r32(off: usize) -> u32 {
    let base = mm::phys_to_virt(gicd_base());
    ((base + off) as *const u32).mmio_read()
}
unsafe fn gicd_w32(off: usize, v: u32) {
    let base = mm::phys_to_virt(gicd_base());
    ((base + off) as *mut u32).mmio_write(v)
}
unsafe fn gicc_r32(off: usize) -> u32 {
    let base = mm::phys_to_virt(gicc_base());
    ((base + off) as *const u32).mmio_read()
}
unsafe fn gicc_w32(off: usize, v: u32) {
    let base = mm::phys_to_virt(gicc_base());
    ((base + off) as *mut u32).mmio_write(v)
}
unsafe fn gicd_w64(off: usize, v: u64) {
    let base = mm::phys_to_virt(gicd_base());
    ((base + off) as *mut u64).mmio_write(v)
}
/// `rd` is a redistributor frame's *virtual* base (see `own_redistributor`).
pub(crate) unsafe fn gicr_r32(rd: usize, off: usize) -> u32 { ((rd + off) as *const u32).mmio_read() }
pub(crate) unsafe fn gicr_w32(rd: usize, off: usize, v: u32) { ((rd + off) as *mut u32).mmio_write(v) }
pub(crate) unsafe fn gicr_r64(rd: usize, off: usize) -> u64 { ((rd + off) as *const u64).mmio_read() }
pub(crate) unsafe fn gicr_w64(rd: usize, off: usize, v: u64) { ((rd + off) as *mut u64).mmio_write(v) }

// ── GICv3 mode ────────────────────────────────────────────────────────────
//
// Decided once by the BSP in `init`, read on every interrupt by `ack`/`eoi`.
// A relaxed load: the APs are started by CPU_ON strictly after `init` returns,
// and the BSP's own first interrupt cannot arrive before `init` unmasks it.

#[allow(unused_imports)]
use leandros_lib::mmio::{MmioRd, MmioWr};
use core::sync::atomic::AtomicBool;

static V3: AtomicBool = AtomicBool::new(false);

/// Packed affinity of the BSP in `GICD_IROUTER` layout (Aff3 at [39:32],
/// Aff2/1/0 at [23:0]). Every SPI is routed here, matching the GICv2 path's
/// BSP interface mask.
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

pub(crate) unsafe fn gicr_wait_rwp(rd: usize) {
    for _ in 0..1_000_000 {
        if gicr_r32(rd, GICR_CTLR) & GICR_CTLR_RWP == 0 { return; }
    }
    crate::uart::serial_print_str("[GIC] GICR_CTLR.RWP never cleared\n");
}

/// Virtual base of this CPU's redistributor frame, found by walking the
/// region and matching `GICR_TYPER`'s affinity to our `MPIDR_EL1`. `None` if
/// the walk hits the Last frame without a match — a CPU the GIC cannot see.
#[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
pub(crate) unsafe fn own_redistributor() -> Option<usize> {
    let want = mpidr_to_typer_affinity(read_mpidr());
    for i in 0..gicr_frames() {
        let Some(rd) = gicr_virt(i) else { break; };
        let typer = gicr_r64(rd, GICR_TYPER);
        if (typer >> 32) as u32 == want { return Some(rd); }
        if typer & (1 << 4) != 0 { break; } // Last
    }
    None
}
#[cfg(any(feature = "rpi5", feature = "raspi4b"))]
pub(crate) unsafe fn own_redistributor() -> Option<usize> { None }

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
    for id in [ppi_virt_timer(), SGI_RESCHED, SGI_KVM_KICK, PPI_KVM_MAINT, PPI_GUEST_VTIMER] {
        let off   = GICR_IPRIORITYR + (id as usize / 4) * 4;
        let shift = (id % 4) * 8;
        let v = (gicr_r32(sgi, off) & !(0xFF << shift)) | (IRQ_PRIORITY << shift);
        gicr_w32(sgi, off, v);
    }
    gicr_w32(sgi, GICR_ISENABLER0, (1 << ppi_virt_timer()) | (1 << SGI_RESCHED) | KVM_ENABLE_MASK);
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
            gicd_set_byte_field(GICD_ITARGETSR,  id, v2_iface_mask(0));
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

/// Read-only distributor state of SPI `id`: bit0 pending, bit1 active, bit2 enabled.
/// Used by device self-tests that raise their own interrupt in software, to tell
/// "the line never reached the GIC" from "the GIC has it but the CPU never took it".
pub fn spi_state(id: u32) -> u32 {
    if id < 32 || (id as usize) >= NUM_IRQS { return 0; }
    let word = (id as usize / 32) * 4;
    let bit = 1u32 << (id % 32);
    unsafe {
        let p = (gicd_r32(GICD_ISPENDR0 + word) & bit != 0) as u32;
        let a = (gicd_r32(GICD_ISACTIVER0 + word) & bit != 0) as u32;
        let e = (gicd_r32(GICD_ISENABLER0 + word) & bit != 0) as u32;
        p | (a << 1) | (e << 2)
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
    // KVM builds at EL2: the host tick is CNTHP (INTID 26), not CNTV (PPI 27).
    #[cfg(feature = "kvm")]
    PPI_VIRT_RT.store(super::hyp::host_timer_intid(ppi_virt_timer()), RtOrd::Relaxed);
    // Claim the built-in interrupts before anything can be delivered. IRQs are
    // still masked at EL1 here (`timer::init` does the `daifclr` afterwards),
    // but doing this first also means the distributor is never enabled with an
    // empty table.
    super::exception::register_builtin_handlers();

    if detect_v3() {
        V3.store(true, Ordering::Release);
        BSP_ROUTE.store(mpidr_to_irouter(read_mpidr()), Ordering::Release);
        unsafe { init_dist_v3(); init_cpu_v3(); }
        enable_spi(uart_intid());
        UART_IRQ_LIVE.store(true, Ordering::Release);
        print_uart_irq_diag("init", &uart_irq_diag());
        return;
    }

    unsafe {
        record_v2_iface();
        // Enable distributor.
        gicd_w32(GICD_CTLR, 1);
        dsb_st();

        // Enable PPI 27 (Virtual Timer) and SGI 1 (reschedule IPI).
        // This word is banked per CPU for IRQs 0-31 — this enables them for
        // the BSP; each AP does the same in init_cpu_interface().
        gicd_w32(GICD_ISENABLER0, (1 << ppi_virt_timer()) | (1 << SGI_RESCHED) | KVM_ENABLE_MASK);
        gicd_set_byte_field(GICD_IPRIORITYR, ppi_virt_timer(), IRQ_PRIORITY);
        gicd_set_byte_field(GICD_ITARGETSR,  ppi_virt_timer(), v2_iface_mask(0));
        dsb_st();

        // Enable CPU interface.
        gicc_w32(GICC_CTLR, 1);
        // Accept any priority (mask = 0xFF = accept all).
        gicc_w32(GICC_PMR, 0xFF);
        dsb_st();
    }

    // SPI 1 / INTID 33: the PL011 UART. Now just the first customer of the
    // generic path rather than a hand-unrolled special case.
    enable_spi(uart_intid());
    #[cfg(any(feature = "rpi5", feature = "raspi4b"))]
    probe_uart_line();
    UART_IRQ_LIVE.store(true, Ordering::Release);
    print_uart_irq_diag("init", &uart_irq_diag());
}

/// INTID the console UART actually raises. `SPI_PL011` unless the rpi5 line
/// probe found it elsewhere (BCM2712 D0 moved uart10 from SPI 121 to SPI 120:
/// raspberrypi/linux bcm2712d0-rpi-5-b.dts `&uart10 { interrupts = <GIC_SPI
/// 120 ...> }`, applied by the firmware as overlays/bcm2712d0.dtbo on a D0 board).
static UART_INTID: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(SPI_PL011);

/// Console UART INTID from ACPI SPCR (GSI == INTID on ARM).
pub fn set_uart_intid(id: u32) { if id != 0 { UART_INTID.store(id, Ordering::Release); } }
pub fn uart_intid() -> u32 { UART_INTID.load(Ordering::Relaxed) }

/// Bounded busy wait on the generic counter (no timer interrupt needed).
#[cfg(any(feature = "rpi5", feature = "raspi4b"))]
fn spin_us(us: u64) {
    let (f, t0): (u64, u64);
    unsafe {
        core::arch::asm!("mrs {}, cntfrq_el0", out(reg) f, options(nomem, nostack));
        core::arch::asm!("isb; mrs {}, cntvct_el0", out(reg) t0, options(nomem, nostack));
    }
    let ticks = (f.max(1_000_000) / 1_000_000) * us;
    loop {
        let t: u64;
        unsafe { core::arch::asm!("isb; mrs {}, cntvct_el0", out(reg) t, options(nomem, nostack)); }
        if t.wrapping_sub(t0) >= ticks { return; }
    }
}

/// Raspberry Pi 5: find which GIC input the uart10 interrupt line drives.
///
/// Runs once in `init` with IRQs still masked at EL1. Candidate INTIDs 152
/// (SPI 120, BCM2712 D0) and 153 (SPI 121, C1). For a level SPI the GIC's
/// pending bit follows the input line whether or not it is enabled, so the
/// probe toggles the PL011 output via `UARTIMSC` and watches `GICD_ISPENDR4`:
/// mask all (line low) -> unmask the bits `UARTRIS` already latches (line
/// high) -> restore. The INTID whose bit reads 0/1/0 is the UART. Prints one
/// `[uartirq] probe` line; on no match it keeps `SPI_PL011`. Inert: touches
/// only IMSC (restored) and the enable/pending state of 152/153.
/// Also built for QEMU raspi4b (BCM2711 uart0 is INTID 153 too), where it
/// exercises the method against QEMU's GIC-400 model and must print `-> 99`.
#[cfg(any(feature = "rpi5", feature = "raspi4b"))]
fn probe_uart_line() {
    use crate::uart::{serial_print_str as s, print_hex as h};
    const CANDS: [u32; 2] = [32 + 120, 32 + 121];
    let word = || unsafe { gicd_r32(GICD_ISPENDR0 + (CANDS[0] as usize / 32) * 4) };
    let bits = |w: u32| -> [u32; 2] { [(w >> (CANDS[0] % 32)) & 1, (w >> (CANDS[1] % 32)) & 1] };
    let (imsc0, ris, _, _, _) = super::uart::irq_regs();
    let assert_mask = ris & 0x7FF;
    unsafe { super::uart::write_imsc(0); }
    spin_us(50);
    let lo = bits(word());
    unsafe { super::uart::write_imsc(assert_mask); }
    spin_us(50);
    let hi = bits(word());
    unsafe { super::uart::write_imsc(0); }
    spin_us(50);
    let lo2 = bits(word());
    unsafe { super::uart::write_imsc(imsc0); }
    let mut found = None;
    for i in 0..2 {
        if lo[i] == 0 && hi[i] == 1 && lo2[i] == 0 { found = Some(CANDS[i]); break; }
    }
    s("[uartirq] probe ris "); h(ris as usize);
    for i in 0..2 {
        s(" i"); h(CANDS[i] as usize); s(" ");
        h(lo[i] as usize); s("/"); h(hi[i] as usize); s("/"); h(lo2[i] as usize);
    }
    // No line match (RIS had nothing latched to raise the line): fall back on
    // the stepping. D0 moved uart10 to SPI 120. The chip id decides; the DT
    // only when the chip id does not decode (a card without
    // overlays/bcm2712d0.dtbo boots a D0 chip with the C1 tree).
    #[cfg(feature = "rpi5")]
    if found.is_none() {
        use drivers::bcm2712_id::{Source, Stepping};
        let st = drivers::bcm2712_id::stepping();
        if st.stepping == Stepping::D0 {
            found = Some(CANDS[0]);
            s(if st.source == Source::Hw { " hw=d0" } else { " dt=d0" });
        }
    }
    s(" -> ");
    match found {
        Some(id) => h(id as usize),
        None => s("none"),
    }
    s("\n");
    if let Some(id) = found {
        if id != SPI_PL011 {
            register_handler(id, super::exception::uart_irq);
            UART_INTID.store(id, Ordering::Release);
            enable_spi(id);
            disable_spi(SPI_PL011);
        }
    }
}

/// Set once the console UART's SPI is enabled; `pend_uart_irq` is a no-op
/// before (early prints run before the distributor is mapped/programmed).
static UART_IRQ_LIVE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Software-pend the console UART interrupt. Level-sensitive SPIs keep a
/// software-set pending state until acknowledged, so `uart_irq` runs (and
/// drains `uart::RX_RING`) as soon as the target CPU opens IRQs.
pub fn pend_uart_irq() {
    if !UART_IRQ_LIVE.load(Ordering::Acquire) { return; }
    let id = uart_intid() as usize;
    unsafe { gicd_w32(GICD_ISPENDR0 + (id / 32) * 4, 1 << (id % 32)); }
}

// ── Console-UART interrupt diagnostic (openbugs #101, Pi 5 RX IRQ never fires) ──

/// One snapshot of everything that decides whether `SPI_PL011` reaches a CPU.
#[derive(Clone, Copy, Default)]
pub struct UartIrqDiag {
    pub el: u64,
    pub gicd_ctlr: u32, pub gicd_typer: u32,
    pub enabled: u32, pub pending: u32, pub active: u32, pub group: u32,
    pub target: u32, pub prio: u32, pub cfg: u32,
    pub gicc_ctlr: u32, pub gicc_pmr: u32, pub gicc_rpr: u32, pub gicc_hppir: u32,
    pub imsc: u32, pub ris: u32, pub mis: u32, pub fr: u32, pub cr: u32,
    /// `GICD_ISPENDR0..15` (word 0 is the reading CPU's banked SGI/PPI word).
    pub spend: [u32; 16],
}

/// Read the distributor/CPU-interface/PL011 state of the console UART SPI.
/// Read-only: no IAR read, so nothing is acknowledged. GICv2 only (on v3 the
/// GICC fields stay 0).
pub fn uart_irq_diag() -> UartIrqDiag {
    let id = uart_intid() as usize;
    let bit = |base: usize| unsafe { (gicd_r32(base + (id / 32) * 4) >> (id % 32)) & 1 };
    let byte = |base: usize| unsafe { (gicd_r32(base + (id / 4) * 4) >> ((id % 4) * 8)) & 0xFF };
    let el: u64;
    unsafe { core::arch::asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)); }
    let (imsc, ris, mis, fr, cr) = super::uart::irq_regs();
    let mut d = UartIrqDiag { el: (el >> 2) & 3, imsc, ris, mis, fr, cr, ..Default::default() };
    unsafe {
        d.gicd_ctlr = gicd_r32(GICD_CTLR);
        d.gicd_typer = gicd_r32(GICD_TYPER);
        d.enabled = bit(GICD_ISENABLER0);
        d.pending = bit(GICD_ISPENDR0);
        d.active = bit(GICD_ISACTIVER0);
        d.group = bit(GICD_IGROUPR);
        d.target = byte(GICD_ITARGETSR);
        d.prio = byte(GICD_IPRIORITYR);
        d.cfg = (gicd_r32(GICD_ICFGR + (id / 16) * 4) >> ((id % 16) * 2)) & 3;
        let words = (((d.gicd_typer & 0x1F) + 1) as usize).min(d.spend.len());
        for w in 0..words { d.spend[w] = gicd_r32(GICD_ISPENDR0 + w * 4); }
        if !is_v3() {
            d.gicc_ctlr = gicc_r32(GICC_CTLR);
            d.gicc_pmr = gicc_r32(GICC_PMR);
            d.gicc_rpr = gicc_r32(GICC_RPR);
            d.gicc_hppir = gicc_r32(GICC_HPPIR);
        }
    }
    d
}

/// One console line assembled in memory and handed to the kernel's line-atomic
/// console (`serial_print`: staged per CPU, queued whole behind the console
/// lock) in ONE call. The `uart::serial_print_str` byte-at-a-time path these
/// diagnostics used writes straight to the PL011 from any CPU, so a line printed
/// while a user `write(2)` held the console was spliced into it byte by byte
/// (lane conloss: `[uartirq] rx-pending` damaged 2-5 lines of a 20000-line
/// burst on aarch64 TCG; no byte was lost on the way, QEMU logged the mix).
struct DiagLine { buf: [u8; 640], len: usize }
impl DiagLine {
    fn new() -> Self { DiagLine { buf: [0; 640], len: 0 } }
    fn s(&mut self, t: &str) {
        for &b in t.as_bytes() {
            if self.len < self.buf.len() { self.buf[self.len] = b; self.len += 1; }
        }
    }
    /// 16 zero-padded hex digits, like `uart::print_hex`.
    fn h(&mut self, n: usize) {
        for i in (0..16).rev() {
            let d = ((n as u64) >> (i * 4)) & 0xF;
            let c = b"0123456789abcdef"[d as usize];
            if self.len < self.buf.len() { self.buf[self.len] = c; self.len += 1; }
        }
    }
    fn flush(&self) {
        extern "C" { fn serial_print(ptr: *const u8, len: usize); }
        unsafe { serial_print(self.buf.as_ptr(), self.len) }
    }
}

/// Print a snapshot as one `[uartirq]` line, tagged with `when`.
pub fn print_uart_irq_diag(when: &str, d: &UartIrqDiag) {
    let mut l = DiagLine::new();
    l.s("[uartirq] "); l.s(when); l.s(" intid "); l.h(uart_intid() as usize);
    l.s(" EL "); l.h(d.el as usize);
    l.s(" D.ctlr "); l.h(d.gicd_ctlr as usize); l.s(" D.typer "); l.h(d.gicd_typer as usize);
    l.s(" en "); l.h(d.enabled as usize); l.s(" pend "); l.h(d.pending as usize);
    l.s(" act "); l.h(d.active as usize); l.s(" grp "); l.h(d.group as usize);
    l.s(" tgt "); l.h(d.target as usize); l.s(" prio "); l.h(d.prio as usize);
    l.s(" cfg "); l.h(d.cfg as usize);
    l.s(" C.ctlr "); l.h(d.gicc_ctlr as usize); l.s(" C.pmr "); l.h(d.gicc_pmr as usize);
    l.s(" C.rpr "); l.h(d.gicc_rpr as usize); l.s(" C.hppir "); l.h(d.gicc_hppir as usize);
    l.s(" imsc "); l.h(d.imsc as usize); l.s(" ris "); l.h(d.ris as usize);
    l.s(" mis "); l.h(d.mis as usize); l.s(" fr "); l.h(d.fr as usize); l.s(" cr "); l.h(d.cr as usize);
    // Every non-zero ISPENDR word, `wN=bits` (INTID = 32*N + bit).
    l.s(" spend");
    for (w, &v) in d.spend.iter().enumerate() {
        if v != 0 { l.s(" w"); l.h(w); l.s("="); l.h(v as usize); }
    }
    l.s("\n");
    l.flush();
}

/// Snapshot taken by the cpu0 tick the first time it finds RX bytes the
/// interrupt has not taken yet (i.e. while SPI_PL011 should be pending).
/// Written once from IRQ context, printed later by task context.
pub static mut UART_IRQ_PENDING_DIAG: UartIrqDiag = UartIrqDiag {
    el: 0, gicd_ctlr: 0, gicd_typer: 0, enabled: 0, pending: 0, active: 0, group: 0,
    target: 0, prio: 0, cfg: 0, gicc_ctlr: 0, gicc_pmr: 0, gicc_rpr: 0, gicc_hppir: 0,
    imsc: 0, ris: 0, mis: 0, fr: 0, cr: 0, spend: [0; 16],
};
/// 0 = not taken, 1 = taken (readable), 2 = printed.
pub static UART_IRQ_PENDING_STATE: core::sync::atomic::AtomicU8 = core::sync::atomic::AtomicU8::new(0);

/// Tick hook: capture the snapshot once, before the fallback drains the FIFO.
pub fn uart_irq_diag_tick() {
    if UART_IRQ_PENDING_STATE.load(Ordering::Relaxed) != 0 || !super::uart::rx_pending() { return; }
    let d = uart_irq_diag();
    unsafe { *core::ptr::addr_of_mut!(UART_IRQ_PENDING_DIAG) = d; }
    UART_IRQ_PENDING_STATE.store(1, Ordering::Release);
}

/// Task-context hook: print the tick snapshot once it exists.
pub fn uart_irq_diag_report() {
    uart_irq_swpend_test();
    if UART_IRQ_PENDING_STATE.compare_exchange(1, 2, Ordering::Acquire, Ordering::Relaxed).is_ok() {
        let d = unsafe { *core::ptr::addr_of!(UART_IRQ_PENDING_DIAG) };
        print_uart_irq_diag("rx-pending", &d);
    }
}

/// Two-step one-shot: the first report call sets the console UART INTID
/// pending in `GICD_ISPENDR`; the next call (one kiostat window later, so the
/// caller's own IRQ-masked context cannot hide the delivery) checks whether
/// `exception::uart_irq` counted it. `taken 1` proves distributor -> CPU
/// interface -> CPU -> handler works for that INTID, so a dead RX interrupt is
/// the device line, not the GIC path. Prints
/// `[uartirq] swpend intid N irqs A->B taken T pend P`.
fn uart_irq_swpend_test() {
    use core::sync::atomic::{AtomicU64, AtomicU8};
    static STATE: AtomicU8 = AtomicU8::new(0);
    static BEFORE: AtomicU64 = AtomicU64::new(0);
    if !cfg!(any(feature = "rpi5", feature = "raspi4b")) || !UART_IRQ_LIVE.load(Ordering::Acquire) { return; }
    let id = uart_intid() as usize;
    let irqs = &super::uart::UART_RX_IRQS;
    match STATE.load(Ordering::Acquire) {
        0 => {
            BEFORE.store(irqs.load(Ordering::Relaxed), Ordering::Relaxed);
            STATE.store(1, Ordering::Release);
            pend_uart_irq();
        }
        1 => {
            STATE.store(2, Ordering::Release);
            let before = BEFORE.load(Ordering::Relaxed);
            let after = irqs.load(Ordering::Relaxed);
            let pend = unsafe { (gicd_r32(GICD_ISPENDR0 + (id / 32) * 4) >> (id % 32)) & 1 };
            let mut l = DiagLine::new();
            l.s("[uartirq] swpend intid "); l.h(id); l.s(" irqs "); l.h(before as usize); l.s("->"); l.h(after as usize);
            l.s(" taken "); l.h((after != before) as usize); l.s(" pend "); l.h(pend as usize); l.s("\n");
            l.flush();
        }
        _ => {}
    }
}

/// GICv2 CPU-interface mask of each dense CPU index (0 = not recorded).
static V2_IFACE: [core::sync::atomic::AtomicU8; sched::MAX_CPUS] =
    [const { core::sync::atomic::AtomicU8::new(0) }; sched::MAX_CPUS];

/// Record the calling CPU's GICv2 interface mask: `GICD_ITARGETSR0..7` are
/// banked and read back the reading CPU's own one-hot mask in every byte.
unsafe fn record_v2_iface() {
    let cpu = super::smp::arch_cpu_id();
    if cpu >= sched::MAX_CPUS { return; }
    let m = (gicd_r32(GICD_ITARGETSR) & 0xFF) as u8;
    V2_IFACE[cpu].store(m, Ordering::Relaxed);
}

/// GICv2 target-list mask for dense CPU index `cpu`. Falls back to
/// `1 << cpu` when nothing was recorded (a uniprocessor GIC may read 0).
fn v2_iface_mask(cpu: usize) -> u32 {
    if cpu >= sched::MAX_CPUS { return 0; }
    match V2_IFACE[cpu].load(Ordering::Relaxed) {
        0 if cpu < 8 => 1 << cpu,
        0 => 0,
        m => m as u32,
    }
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
        record_v2_iface();
        // Banked per-CPU enables: virtual timer PPI 27 + reschedule SGI 1.
        gicd_w32(GICD_ISENABLER0, (1 << ppi_virt_timer()) | (1 << SGI_RESCHED) | KVM_ENABLE_MASK);

        // Banked per-CPU priority for PPI 27 (match the BSP's).
        gicd_set_byte_field(GICD_IPRIORITYR, ppi_virt_timer(), IRQ_PRIORITY);

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
    // GICv2 addresses CPU *interfaces* (at most 8), whose numbering need not
    // match our dense CPU index: use the mask each CPU read for itself.
    let mask = v2_iface_mask(cpu);
    if mask == 0 { return; }
    unsafe {
        gicd_w32(GICD_SGIR, (mask << 16) | (sgi_id & 0xF));
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

// ── kexec ─────────────────────────────────────────────────────────────────

const GICD_ICACTIVER0: usize = 0x380;
const GICR_ICPENDR0:   usize = 0x280;
const GICR_ICACTIVER0: usize = 0x380;

/// Leave the GIC as a fresh boot would find it (kexec, last CPU standing):
/// every interrupt disabled, nothing pending or active in the distributor
/// and in every redistributor, the distributor's groups off, and this CPU's
/// interface off. The next kernel's `init` re-enables what it uses; it does
/// not clear stale active state itself.
pub fn kexec_quiesce() {
    super::its::quiesce();
    unsafe {
        let words = (num_irqs_implemented() / 32) as usize;
        if is_v3() {
            #[cfg(not(any(feature = "rpi5", feature = "raspi4b")))]
            for i in 0..gicr_frames() {
                let Some(rd) = gicr_virt(i) else { break; };
                let sgi = rd + GICR_SGI_BASE;
                gicr_w32(sgi, GICR_ICENABLER0, 0xFFFF_FFFF);
                gicr_w32(sgi, GICR_ICPENDR0, 0xFFFF_FFFF);
                gicr_w32(sgi, GICR_ICACTIVER0, 0xFFFF_FFFF);
                gicr_wait_rwp(rd);
                if gicr_r64(rd, GICR_TYPER) & (1 << 4) != 0 { break; } // Last
            }
            for w in 1..words {
                gicd_w32(GICD_ICENABLER0 + w * 4, 0xFFFF_FFFF);
                gicd_w32(GICD_ICPENDR0 + w * 4, 0xFFFF_FFFF);
                gicd_w32(GICD_ICACTIVER0 + w * 4, 0xFFFF_FFFF);
            }
            gicd_w32(GICD_CTLR, GICD_CTLR_ARE); // groups off, routing mode kept
            gicd_wait_rwp();
            icc::igrpen1_el1::write(0);
            isb();
        } else {
            for w in 0..words { // word 0 is this CPU's bank
                gicd_w32(GICD_ICENABLER0 + w * 4, 0xFFFF_FFFF);
                gicd_w32(GICD_ICPENDR0 + w * 4, 0xFFFF_FFFF);
                gicd_w32(GICD_ICACTIVER0 + w * 4, 0xFFFF_FFFF);
            }
            gicv2_clear_sgis();
            gicd_w32(GICD_CTLR, 0);
            gicc_w32(GICC_CTLR, 0);
        }
        dsb_st();
    }
}

/// GICv2 (GIC-400, Pi 5 / raspi4b): a pending SGI is NOT cleared by
/// GICD_ICPENDR0 (its SGI bits are write-ignored); GICD_CPENDSGIRn (banked:
/// this CPU's pending SGIs, one byte per SGI = source-CPU mask) is the way.
/// The GIC-400 distributor stays powered across PSCI CPU_OFF/ON, so whatever
/// is left pending here is delivered to the next kernel's fresh vector table.
const GICD_CPENDSGIR0: usize = 0xF10;
unsafe fn gicv2_clear_sgis() {
    for i in 0..4 { gicd_w32(GICD_CPENDSGIR0 + i * 4, 0xFFFF_FFFF); }
}

/// This CPU's interface off (an AP on its way to CPU_OFF).
pub fn kexec_cpu_off() {
    unsafe {
        if is_v3() { icc::igrpen1_el1::write(0); isb(); } else {
            // This CPU's banked SGI/PPI state lives in the always-on
            // distributor: disable, unpend and deactivate it before CPU_OFF.
            gicd_w32(GICD_ICENABLER0, 0xFFFF_FFFF);
            gicd_w32(GICD_ICPENDR0, 0xFFFF_FFFF);
            gicd_w32(GICD_ICACTIVER0, 0xFFFF_FFFF);
            gicv2_clear_sgis();
            gicc_w32(GICC_CTLR, 0);
            dsb_st();
        }
    }
}
