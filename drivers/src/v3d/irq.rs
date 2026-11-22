//! V3D interrupt lines: hub (TFU done, MMU faults, GMP violation) and core0 (bin/render done,
//! out of binner memory, CSD done). Linux reference: `drivers/gpu/drm/v3d/v3d_irq.c`.
//!
//! DT `interrupts = <0 0xfa 4  0 0xf9 4>`: hub = SPI 250 = INTID 282, core0 = SPI 249 = INTID 281,
//! both level-high.
//!
//! Bring-up runs a software self-test per line: write one unmasked bit to `INT_SET`, which latches
//! `INT_STS` and should assert the SPI. The outcome separates "the CPU took it" (`ok`) from "the
//! GIC has it pending but the CPU never took it" (`gic-pending`, the #101 uart10 shape) from "the
//! GIC never saw it" (`silent`). If either line is not `ok`, the driver runs in polled mode:
//! [`poll`] services `INT_STS` from a tick instead.

use super::regs::V3dRegs;

// Same offsets in the hub block and in the core block.
pub const INT_STS: u32 = 0x50;
pub const INT_SET: u32 = 0x54;
pub const INT_CLR: u32 = 0x58;
pub const INT_MSK_STS: u32 = 0x5c;
pub const INT_MSK_SET: u32 = 0x60;
pub const INT_MSK_CLR: u32 = 0x64;

// Hub bits.
pub const HUB_INT_TFUF: u32 = 1 << 0;
pub const HUB_INT_TFUC: u32 = 1 << 1;
pub const HUB_INT_MSO: u32 = 1 << 2;
pub const HUB_INT_MMU_CAP: u32 = 1 << 3;
pub const HUB_INT_MMU_PTI: u32 = 1 << 4;
pub const HUB_INT_MMU_WRV: u32 = 1 << 5;
/// 7.1 only (GMP moved to the hub).
pub const HUB_INT_GMPV_71: u32 = 1 << 6;
pub const HUB_INT_MMU_ANY: u32 = HUB_INT_MMU_CAP | HUB_INT_MMU_PTI | HUB_INT_MMU_WRV;

// Core bits.
pub const INT_FRDONE: u32 = 1 << 0;
pub const INT_FLDONE: u32 = 1 << 1;
pub const INT_OUTOMEM: u32 = 1 << 2;
/// 4.x core GMP violation (7.1 reports it on the hub).
pub const INT_GMPV_42: u32 = 1 << 5;

pub const HUB_INTID: u32 = 282;
pub const CORE_INTID: u32 = 281;

/// Self-test wait per line.
pub const SELFTEST_TIMEOUT_US: u64 = 50_000;

pub const fn int_csddone(ver: u32) -> u32 { if ver >= 71 { 1 << 6 } else { 1 << 7 } }
pub const fn int_pctr(ver: u32) -> u32 { if ver >= 71 { 1 << 5 } else { 1 << 6 } }

pub const fn core_mask(ver: u32) -> u32 {
    INT_FRDONE | INT_FLDONE | INT_OUTOMEM | int_csddone(ver) | if ver >= 71 { 0 } else { INT_GMPV_42 }
}

pub const fn hub_mask(ver: u32) -> u32 {
    HUB_INT_MMU_WRV | HUB_INT_MMU_PTI | HUB_INT_MMU_CAP | HUB_INT_TFUC | if ver >= 71 { HUB_INT_GMPV_71 } else { 0 }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Block { Hub, Core }

fn rd(regs: &dyn V3dRegs, b: Block, off: u32) -> u32 {
    match b { Block::Hub => regs.hub_r(off), Block::Core => regs.core_r(off) }
}
fn wr(regs: &dyn V3dRegs, b: Block, off: u32, v: u32) {
    match b { Block::Hub => regs.hub_w(off, v), Block::Core => regs.core_w(off, v) }
}

/// `v3d_irq_enable`: ack stale status, mask everything we don't handle, unmask what we do.
pub fn enable(regs: &dyn V3dRegs, ver: u32) {
    for (b, m) in [(Block::Core, core_mask(ver)), (Block::Hub, hub_mask(ver))] {
        wr(regs, b, INT_CLR, m);
        wr(regs, b, INT_MSK_SET, !m);
        wr(regs, b, INT_MSK_CLR, m);
    }
}

/// `v3d_irq_disable`: mask and ack everything.
pub fn disable(regs: &dyn V3dRegs) {
    for b in [Block::Core, Block::Hub] {
        wr(regs, b, INT_MSK_SET, !0);
        wr(regs, b, INT_CLR, !0);
    }
}

/// Core handler body: read and ack `CTL_INT_STS`. Returns the bits (0 = not ours).
pub fn handle_core(regs: &dyn V3dRegs) -> u32 {
    let s = regs.core_r(INT_STS);
    if s != 0 {
        regs.core_w(INT_CLR, s);
    }
    s
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmuFault {
    /// `HUB_INT_MMU_*` bits that fired.
    pub bits: u32,
    pub axi_id: u32,
    /// Faulting GPU VA (`MMU_VIO_ADDR << (va_width - 32)`).
    pub addr: u64,
}

/// Hub handler body: read and ack `HUB_INT_STS`; on an MMU fault read the violation registers
/// and write `MMU_CTL` back to itself, which acks its sticky fault bits.
pub fn handle_hub(regs: &dyn V3dRegs, va_width: u32) -> (u32, Option<MmuFault>) {
    use super::mmu::{MMU_CTL, MMU_VIO_ADDR, MMU_VIO_ID};
    let s = regs.hub_r(INT_STS);
    if s == 0 {
        return (0, None);
    }
    regs.hub_w(INT_CLR, s);
    let mut fault = None;
    if s & HUB_INT_MMU_ANY != 0 {
        let axi_id = regs.hub_r(MMU_VIO_ID);
        let addr = (regs.hub_r(MMU_VIO_ADDR) as u64) << va_width.saturating_sub(32).min(31);
        regs.hub_w(MMU_CTL, regs.hub_r(MMU_CTL));
        fault = Some(MmuFault { bits: s & HUB_INT_MMU_ANY, axi_id, addr });
    }
    (s, fault)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineResult {
    /// The handler ran: SPI delivered end to end.
    Ok,
    /// INT_STS latched and the GIC shows the SPI pending/active, but no handler ran.
    GicPending,
    /// Same, but CPU interrupts were masked during the test, so it proves only the GIC half.
    CpuMasked,
    /// INT_STS latched, the GIC never saw the line (wrong INTID, or not wired).
    Silent,
    /// Writing INT_SET did not latch INT_STS: inconclusive.
    NoLatch,
    /// The line could not be registered.
    Busy,
}

impl LineResult {
    pub fn tag(self) -> &'static str {
        match self {
            LineResult::Ok => "ok",
            LineResult::GicPending => "gic-pending",
            LineResult::CpuMasked => "cpu-masked",
            LineResult::Silent => "silent",
            LineResult::NoLatch => "nolatch",
            LineResult::Busy => "busy",
        }
    }
}

/// What the self-test needs from its surroundings (the GIC and the handler's counter).
pub trait SelftestEnv {
    /// Times this line's handler claimed an interrupt.
    fn delivered(&self) -> u64;
    /// GIC distributor state of the line: bit0 pending, bit1 active.
    fn gic_state(&self) -> u32;
    fn cpu_irqs_masked(&self) -> bool;
}

/// Raise `bit` on `block` in software and see how far it gets. Always leaves the bit cleared.
pub fn selftest_line(
    regs: &dyn V3dRegs,
    block: Block,
    bit: u32,
    env: &dyn SelftestEnv,
    clock: &dyn super::mmu::Clock,
    timeout_us: u64,
) -> LineResult {
    let before = env.delivered();
    wr(regs, block, INT_SET, bit);
    super::mmu::barrier();
    let start = clock.now_us();
    let mut gic = 0;
    loop {
        if env.delivered() > before {
            wr(regs, block, INT_CLR, bit);
            return LineResult::Ok;
        }
        gic |= env.gic_state() & 3;
        if clock.now_us().wrapping_sub(start) >= timeout_us {
            break;
        }
        core::hint::spin_loop();
    }
    let latched = rd(regs, block, INT_STS) & bit != 0;
    gic |= env.gic_state() & 3;
    wr(regs, block, INT_CLR, bit);
    if env.delivered() > before {
        return LineResult::Ok;
    }
    if !latched {
        LineResult::NoLatch
    } else if gic != 0 {
        if env.cpu_irqs_masked() { LineResult::CpuMasked } else { LineResult::GicPending }
    } else {
        LineResult::Silent
    }
}

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
pub use hw::*;

#[cfg(all(target_arch = "aarch64", feature = "rpi5"))]
mod hw {
    use super::*;
    use crate::kenv::v3d::{IrqReturn, IrqSource};
    use core::cell::UnsafeCell;
    use core::fmt::Write;
    use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

    struct RegsCell(UnsafeCell<Option<&'static (dyn V3dRegs + Sync)>>);
    unsafe impl Sync for RegsCell {}
    /// Written once in `bring_up`, before `READY` is released and before any line is requested.
    static REGS: RegsCell = RegsCell(UnsafeCell::new(None));
    static READY: AtomicBool = AtomicBool::new(false);
    static VA_WIDTH: AtomicU32 = AtomicU32::new(32);
    static POLLED: AtomicBool = AtomicBool::new(true);

    pub static HUB_DELIVERED: AtomicU64 = AtomicU64::new(0);
    pub static CORE_DELIVERED: AtomicU64 = AtomicU64::new(0);
    pub static MMU_FAULTS: AtomicU64 = AtomicU64::new(0);
    static CORE_HOOK: AtomicUsize = AtomicUsize::new(0);
    static HUB_HOOK: AtomicUsize = AtomicUsize::new(0);

    fn regs() -> Option<&'static (dyn V3dRegs + Sync)> {
        if !READY.load(Ordering::Acquire) { return None; }
        unsafe { *REGS.0.get() }
    }

    /// Phase C: called with the core `INT_STS` bits from the handler (or from [`poll`]).
    pub fn set_core_hook(f: fn(u32)) { CORE_HOOK.store(f as usize, Ordering::Release); }
    /// Phase C/D: called with the hub `INT_STS` bits (TFUC etc.).
    pub fn set_hub_hook(f: fn(u32)) { HUB_HOOK.store(f as usize, Ordering::Release); }

    fn call_hook(h: &AtomicUsize, bits: u32) {
        let p = h.load(Ordering::Acquire);
        if p != 0 {
            let f: fn(u32) = unsafe { core::mem::transmute::<usize, fn(u32)>(p) };
            f(bits);
        }
    }

    fn service_core(r: &dyn V3dRegs) -> bool {
        let s = handle_core(r);
        if s == 0 { return false; }
        CORE_DELIVERED.fetch_add(1, Ordering::Relaxed);
        call_hook(&CORE_HOOK, s);
        true
    }

    fn service_hub(r: &dyn V3dRegs) -> bool {
        let (s, fault) = handle_hub(r, VA_WIDTH.load(Ordering::Relaxed));
        if s == 0 { return false; }
        HUB_DELIVERED.fetch_add(1, Ordering::Relaxed);
        if let Some(f) = fault {
            if MMU_FAULTS.fetch_add(1, Ordering::Relaxed) < 8 {
                let mut l = super::super::mmu::LineBuf::new();
                let _ = write!(l, "[v3d] mmu fault bits={:#x} axi={:#x} va={:#x}\n", f.bits, f.axi_id, f.addr);
                l.emit();
            }
        }
        call_hook(&HUB_HOOK, s);
        true
    }

    extern "C" fn hub_isr(_ctx: usize) -> IrqReturn {
        match regs() {
            Some(r) if service_hub(r) => IrqReturn::Handled,
            _ => IrqReturn::None,
        }
    }

    extern "C" fn core_isr(_ctx: usize) -> IrqReturn {
        match regs() {
            Some(r) if service_core(r) => IrqReturn::Handled,
            _ => IrqReturn::None,
        }
    }

    /// True when the self-test did not prove both lines; then [`poll`] must be driven by a tick.
    pub fn polled() -> bool { POLLED.load(Ordering::Relaxed) }

    /// Polled fallback: service both blocks' `INT_STS` once. Harmless in IRQ mode.
    pub fn poll() {
        if let Some(r) = regs() {
            service_core(r);
            service_hub(r);
        }
    }

    struct Env { id: u32, counter: &'static AtomicU64 }
    impl SelftestEnv for Env {
        fn delivered(&self) -> u64 { self.counter.load(Ordering::Acquire) }
        fn gic_state(&self) -> u32 { crate::kenv::v3d::irq_line_state(self.id) }
        fn cpu_irqs_masked(&self) -> bool {
            let daif: u64;
            unsafe { core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack)); }
            daif & (1 << 7) != 0
        }
    }

    /// Wire both lines and self-test them; appends `irq hub=282 <r> core=281 <r> mode=<m>`.
    pub fn bring_up(regs: &'static (dyn V3dRegs + Sync), ver: u32, va_width: u32, out: &mut dyn Write) {
        unsafe { *REGS.0.get() = Some(regs); }
        VA_WIDTH.store(va_width, Ordering::Relaxed);
        READY.store(true, Ordering::Release);
        enable(regs, ver);
        let clock = super::super::mmu::CntClock;
        let hub = match crate::kenv::v3d::request_irq(IrqSource::Line(HUB_INTID), hub_isr, 0) {
            Ok(h) => { core::mem::forget(h); selftest_line(regs, Block::Hub, HUB_INT_TFUC,
                &Env { id: HUB_INTID, counter: &HUB_DELIVERED }, &clock, SELFTEST_TIMEOUT_US) }
            Err(_) => LineResult::Busy,
        };
        let core = match crate::kenv::v3d::request_irq(IrqSource::Line(CORE_INTID), core_isr, 0) {
            Ok(h) => { core::mem::forget(h); selftest_line(regs, Block::Core, INT_FRDONE,
                &Env { id: CORE_INTID, counter: &CORE_DELIVERED }, &clock, SELFTEST_TIMEOUT_US) }
            Err(_) => LineResult::Busy,
        };
        let polled = !(hub == LineResult::Ok && core == LineResult::Ok);
        POLLED.store(polled, Ordering::Relaxed);
        let _ = write!(out, "irq hub={} {} core={} {} mode={}", HUB_INTID, hub.tag(), CORE_INTID, core.tag(),
            if polled { "poll" } else { "irq" });
    }
}
