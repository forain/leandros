//! Lower edge bookkeeping (lane U2): IRQ handles kept and freed, hrtimer owner and free,
//! PCI functions a load touched (quiesced before exit), DMA pages a load still owns.
//! Callbacks reach module code only through `ModFn` (gated); the context boxes are leaked on
//! purpose so a stale registration can still run its trampoline, which finds a closed gate.

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use kmod_abi::{KIrqHandler, KmodId};
use spin::Mutex;
use super::gate::{Class, Gate, ModFn};

// ── IRQ ──────────────────────────────────────────────────────────────────────

struct IrqCtx { f: ModFn<KIrqHandler>, ctx: usize }

extern "C" fn irq_tramp(c: usize) -> drivers::irq::IrqReturn {
    let c = unsafe { &*(c as *const IrqCtx) };
    // The gate entered here is the registration's own generation, never the interrupted code's.
    match c.f.with(|h| h(c.ctx)) {
        Some(1) => drivers::irq::IrqReturn::Handled,
        _ => drivers::irq::IrqReturn::None,
    }
}

/// Wrap a module handler: (trampoline, trampoline ctx). None when the generation has no gate.
pub fn wrap_irq(owner: KmodId, h: KIrqHandler, ctx: usize) -> Option<(drivers::irq::IrqHandler, usize)> {
    let g: Arc<Gate> = super::gate::get(super::gate::owner(owner))?;
    let c = Box::into_raw(Box::new(IrqCtx { f: ModFn::new(g, Class::Lower, h), ctx })) as usize;
    Some((irq_tramp, c))
}

static IRQS: Mutex<Vec<(KmodId, u64, drivers::irq::IrqHandle)>> = Mutex::new(Vec::new());

pub fn hold_irq(owner: KmodId, key: u64, h: drivers::irq::IrqHandle) {
    IRQS.lock().push((super::gate::owner(owner), key, h));
}

/// Is any MSI-X handle on the table at `table_va` still held?
pub fn has_msix_handle(table_va: usize) -> bool { IRQS.lock().iter().any(|x| x.2.msix_table() == Some(table_va)) }

/// Free one held handle. Not synchronous on the `kapi_irq_free` path: it does not wait for a handler in flight
/// (no synchronize-IRQ). Safety comes from the gate: the module's handler runs through the lower gate, so a late
/// delivery after the free finds the gate closed (on unload the gate was drained first).
pub fn release_irq(owner: KmodId, key: u64) -> bool {
    let h = {
        let mut v = IRQS.lock();
        match v.iter().position(|x| x.0 == owner && x.1 == key) { Some(i) => v.remove(i).2, None => return false }
    };
    drivers::irq::free_irq(h);
    true
}

// ── hrtimer ──────────────────────────────────────────────────────────────────

struct HrCtx { f: ModFn<extern "C" fn(usize)>, ctx: usize }

extern "C" fn hr_tramp(c: usize) {
    let c = unsafe { &*(c as *const HrCtx) };
    let _ = c.f.with(|f| f(c.ctx));
}

/// Create a timer owned by the running generation. Returns the handle (0 = no callback).
pub fn hrtimer_new(cb: extern "C" fn(usize), ctx: usize, irq_mode: bool) -> usize {
    let mode = if irq_mode { sched::hrtimer::TimerMode::Irq } else { sched::hrtimer::TimerMode::Worker };
    let id = super::gate::running_owner();
    let Some(g) = super::gate::get(id).filter(|_| id.0 != 0) else {
        super::log("[kmod] hrtimer_new refused: no gate open\n");
        return 0;
    };
    let c = Box::into_raw(Box::new(HrCtx { f: ModFn::new(g, Class::Lower, cb), ctx })) as usize;
    let h = Box::into_raw(sched::hrtimer::HrTimer::new(hr_tramp, c, mode)) as usize;
    super::registry::note_registration(id, kmod_abi::RegKind::HRTIMER, h as u64);
    h
}

pub fn release_hrtimer(handle: u64) -> bool {
    let t = unsafe { Box::from_raw(handle as *mut sched::hrtimer::HrTimer) };
    t.cancel_sync();
    drop(t);
    true
}

// ── PCI functions touched by a load ──────────────────────────────────────────

static PCI_TOUCHED: Mutex<Vec<(KmodId, u32)>> = Mutex::new(Vec::new());

pub fn note_pci(bdf: u32) {
    let g = super::gate::running_owner().0;
    if g == 0 { return; }
    let mut v = PCI_TOUCHED.lock();
    if !v.iter().any(|x| x.0 .0 == g && x.1 == bdf) { v.push((KmodId(g), bdf)); }
}

/// D3 step 3: INTx disabled, MSI/MSI-X off, bus mastering off on every function the load touched.
pub fn quiesce_pci(owner: KmodId) {
    let fns: Vec<u32> = {
        let mut v = PCI_TOUCHED.lock();
        let r = v.iter().filter(|x| x.0 == owner).map(|x| x.1).collect();
        v.retain(|x| x.0 != owner);
        r
    };
    for bdf in fns {
        let f = super::exports::func_of_pub(bdf);
        if f.read16(0) == 0xffff { continue; }
        // Never bridges (header type 1, class 0x06): they carry other devices' traffic.
        if f.read16(0xe) & 0x7f == 1 || f.read16(0xa) >> 8 == 0x06 { continue; }
        super::exports::msix_forget(bdf);
        if let Some(c) = f.find_capability(0x05) { let ctl = f.read16(c + 2); f.write16(c + 2, ctl & !1); }
        if let Some(c) = f.find_capability(0x11) { let ctl = f.read16(c + 2); f.write16(c + 2, ctl & !0x8000); }
        let cmd = f.read16(4);
        f.write16(4, (cmd & !0x4) | 0x400);
    }
}

// ── DMA pages ────────────────────────────────────────────────────────────────

static DMA: sched::ksync::KSpinLock<Vec<(KmodId, u64, u32)>> = sched::ksync::KSpinLock::new(Vec::new());

pub fn note_page_alloc(phys: u64, order: u32) {
    let g = super::gate::running_owner().0;
    if g != 0 && phys != 0 { DMA.lock().push((KmodId(g), phys, order)); }
}
pub fn note_page_free(phys: u64) {
    let mut v = DMA.lock();
    if let Some(i) = v.iter().position(|x| x.1 == phys) { v.remove(i); }
}
/// Pages the generation still owns after exit: logged, never freed (no IOMMU).
pub fn dma_leftover(owner: KmodId) -> usize {
    let mut v = DMA.lock();
    let n: usize = v.iter().filter(|x| x.0 == owner).map(|x| 1usize << x.2).sum();
    v.retain(|x| x.0 != owner);
    n
}
