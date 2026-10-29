//! kapi exports: PCI enable, IRQs, wait queues
use super::*;

// ── e1000e-class exports (L6a): PCI enable/caps, IRQs, wait queue, netdev, pm hooks ──────

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use core::sync::atomic::AtomicU64;
use kmod_abi::{KIrqHandler, KNetifOps, KPmFn, KNET_WIFI};

pub(super) fn func_of(bdf: u32) -> drivers::pci::PciFunction {
    let (b, d, f) = unbdf(bdf);
    drivers::pci::PciFunction::new(b, d, f)
}

/// Memory decode on, bus mastering on when asked. 0 ok, -19 no such function.
#[no_mangle]
pub extern "C" fn kapi_pci_enable(bdf: u32, bus_master: bool) -> i32 {
    let f = func_of(bdf);
    if f.read16(0) == 0xffff { return -19; }
    if bus_master { f.enable_mem_and_bus_master(); } else { f.enable_memory(); }
    0
}

/// Config-space offset of capability `cap_id` (PM = 1, PCIe = 0x10, ...), 0 when absent.
#[no_mangle]
pub extern "C" fn kapi_pci_find_cap(bdf: u32, cap_id: u8) -> u16 {
    func_of(bdf).find_capability(cap_id).unwrap_or(0)
}

pub(super) fn irq_errno(e: drivers::irq::IrqError) -> i64 {
    use drivers::irq::IrqError::*;
    match e { NoSpace | NoMemory => -12, NotFound => -19, Unsupported => -95, Busy => -16 }
}

/// The handler must return 0 (not ours) or 1 (handled): that is `IrqReturn`'s repr.
pub(super) fn irq_handler(h: KIrqHandler) -> drivers::irq::IrqHandler {
    unsafe { core::mem::transmute::<KIrqHandler, drivers::irq::IrqHandler>(h) }
}

/// Single-vector MSI (x86_64). Returns the vector, or -errno (-95 where MSI is unsupported).
#[no_mangle]
pub extern "C" fn kapi_irq_request_msi(owner: KmodId, bdf: u32, handler: KIrqHandler, ctx: usize) -> i64 {
    match drivers::irq::request_msi(func_of(bdf), irq_handler(handler), ctx) {
        Ok(h) => {
            let v = h.vector().unwrap_or(0) as u64;
            core::mem::forget(h); // modules are permanent
            crate::kmod::registry::note_registration(owner, RegKind::IRQ, v);
            v as i64
        }
        Err(e) => irq_errno(e),
    }
}

/// The function's INTx line (aarch64 `virt`). Returns the line, or -errno. The caller clears
/// the command register's INTx-disable bit when it wants the line live.
#[no_mangle]
pub extern "C" fn kapi_irq_request_line(owner: KmodId, bdf: u32, handler: KIrqHandler, ctx: usize) -> i64 {
    let Some(line) = func_of(bdf).intx_line() else { return -19 };
    match drivers::irq::request_irq(drivers::irq::IrqSource::Line(line), irq_handler(handler), ctx) {
        Ok(h) => {
            core::mem::forget(h);
            crate::kmod::registry::note_registration(owner, RegKind::IRQ, line as u64);
            line as i64
        }
        Err(e) => irq_errno(e),
    }
}

/// A wait queue with an event counter: the module's `LinkMon`.
pub(super) struct KWait {
    wq: sched::ksync::KWaitQueue,
    events: AtomicU64,
    seen: AtomicU64,
}

#[no_mangle]
pub extern "C" fn kapi_wait_new() -> usize {
    Box::into_raw(Box::new(KWait {
        wq: sched::ksync::KWaitQueue::new(), events: AtomicU64::new(0), seen: AtomicU64::new(0),
    })) as usize
}

/// Any context (IRQ included). Handles are permanent.
#[no_mangle]
pub extern "C" fn kapi_wait_notify(h: usize) {
    if h == 0 { return; }
    let w = unsafe { &*(h as *const KWait) };
    w.events.fetch_add(1, Ordering::AcqRel);
    w.wq.wake_all();
}

/// Sleep up to `ns` or until an event newer than the last wait's arrives (one waiter per handle).
/// True when woken by an event.
#[no_mangle]
pub extern "C" fn kapi_wait_timeout(h: usize, ns: u64) -> bool {
    if h == 0 { return false; }
    let w = unsafe { &*(h as *const KWait) };
    let s = w.seen.load(Ordering::Acquire);
    let hit = w.wq.wait_event_timeout(|| w.events.load(Ordering::Acquire) != s, ns);
    w.seen.store(w.events.load(Ordering::Acquire), Ordering::Release);
    hit
}
