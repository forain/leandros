//! Per-generation gates (design B, decision D1). One `Gate` per load, keyed by a `KmodId` that is
//! never reused. Every kernel-to-module call goes through `Gate::call`; a closed gate refuses it,
//! so a registration left over from generation N can never reach the code of generation N+1
//! even though both live at the same virtual address.

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering::SeqCst};
use kmod_abi::KmodId;
use spin::Mutex;

const CLOSED: u32 = 1 << 31;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Class { Upper, Lower }

pub struct Gate {
    pub id: KmodId,
    upper: AtomicU32,
    lower: AtomicU32,
    pub stopping: AtomicBool,
}

// In a hard IRQ, GEN belongs to the interrupted task unless a gate was entered inside this very
// IRQ (sched::kmodgen::entered_depth() == irq_depth()).
static IRQ_FALLBACK_LOGGED: AtomicBool = AtomicBool::new(false);

static GATES: Mutex<Vec<Arc<Gate>>> = Mutex::new(Vec::new());

impl Gate {
    fn ctr(&self, c: Class) -> &AtomicU32 { if c == Class::Upper { &self.upper } else { &self.lower } }

    /// Run `f` as an activation of this generation, or None when the class is closed.
    /// The current thread's `kmod_gen` is this generation for the duration (restored after).
    pub fn call<R>(&self, c: Class, f: impl FnOnce() -> R) -> Option<R> {
        let a = self.ctr(c);
        if a.fetch_add(1, SeqCst) & CLOSED != 0 { a.fetch_sub(1, SeqCst); return None; }
        let prev = sched::kmodgen::get();
        let pe = sched::kmodgen::entered_depth();
        sched::kmodgen::set_entered_depth(sched::irqctx::irq_depth());
        sched::kmodgen::set(self.id.0);
        let r = f();
        sched::kmodgen::set(prev);
        sched::kmodgen::set_entered_depth(pe);
        a.fetch_sub(1, SeqCst);
        Some(r)
    }

    /// Close the class (no new activation) and wait for the running ones. False on timeout
    /// (the class stays closed).
    pub fn close_and_drain(&self, c: Class, timeout_ms: u64) -> bool {
        let a = self.ctr(c);
        a.fetch_or(CLOSED, SeqCst);
        let end = sched::monotonic_ns().saturating_add(timeout_ms.saturating_mul(1_000_000));
        while a.load(SeqCst) != CLOSED {
            if sched::monotonic_ns() >= end { return false; }
            sched::kthread::msleep(1);
        }
        true
    }

    /// Undo a `close_and_drain` (an unload that found the module busy).
    pub fn reopen(&self, c: Class) { self.ctr(c).fetch_and(!CLOSED, SeqCst); }
    pub fn upper_closed(&self) -> bool { self.upper.load(SeqCst) & CLOSED != 0 }

    pub fn inflight(&self) -> (u32, u32) { (self.upper.load(SeqCst) & !CLOSED, self.lower.load(SeqCst) & !CLOSED) }
    pub fn is_stopping(&self) -> bool { self.stopping.load(SeqCst) }
}

/// A module code pointer (or ops table): reachable only through its generation's gate.
pub struct ModFn<T: Copy> { gate: Arc<Gate>, class: Class, v: T }

impl<T: Copy> ModFn<T> {
    pub fn new(gate: Arc<Gate>, class: Class, v: T) -> Self { ModFn { gate, class, v } }
    pub fn with<R>(&self, f: impl FnOnce(T) -> R) -> Option<R> { self.gate.call(self.class, || f(self.v)) }
    pub fn gate(&self) -> &Arc<Gate> { &self.gate }
}

/// Create the gate for a new load.
pub fn create(id: KmodId) -> Arc<Gate> {
    let g = Arc::new(Gate { id, upper: AtomicU32::new(0), lower: AtomicU32::new(0), stopping: AtomicBool::new(false) });
    GATES.lock().push(g.clone());
    g
}

pub fn get(id: KmodId) -> Option<Arc<Gate>> { GATES.lock().iter().find(|g| g.id == id).cloned() }

/// Forget the table entry (leaked `ModFn`s keep the Arc alive; their gate stays closed).
pub fn remove(id: KmodId) { GATES.lock().retain(|g| g.id != id); }

/// The generation whose code is running on this thread.
pub fn current() -> Option<Arc<Gate>> {
    match sched::kmodgen::get() { 0 => None, g => get(KmodId(g)) }
}

/// Owner for a registration: the running generation (D2), not the module-supplied id.
pub fn owner(claimed: KmodId) -> KmodId {
    let mut gen = sched::kmodgen::get();
    if gen != 0 && sched::irqctx::in_irq() && sched::kmodgen::entered_depth() != sched::irqctx::irq_depth() {
        if !IRQ_FALLBACK_LOGGED.swap(true, SeqCst) {
            super::log("[kmod] owner(): hard-IRQ without a gate entered here, using module-claimed id (logged once)\n");
        }
        gen = 0;
    }
    match gen {
        0 => claimed,
        g => {
            if g != claimed.0 { super::log(&alloc::format!("[kmod] owner mismatch: claimed {} running {}\n", claimed.0, g)); }
            KmodId(g)
        }
    }
}
