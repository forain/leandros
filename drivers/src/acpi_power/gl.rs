//! FACS global lock (ACPI 5.2.10.1) ownership tracking: cmpxchg on FACS+16;
//! bit0 pending, bit1 owned. Kept free of kernel dependencies so the host
//! tests in `drivers/acpi-aml-tests` can include it by path.
//!
//! Callers serialise on the interpreter lock, so `depth`/`owned` are not
//! contended; they are atomics only to live in a `static`.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

pub struct Gl {
    depth: AtomicU32,
    owned: AtomicBool,
    pending: AtomicBool,
    pub fails: AtomicU32,
}

impl Gl {
    pub const fn new() -> Self {
        Gl { depth: AtomicU32::new(0), owned: AtomicBool::new(false), pending: AtomicBool::new(false), fails: AtomicU32::new(0) }
    }
    pub fn owned(&self) -> bool { self.owned.load(Ordering::Relaxed) }

    /// Take the lock (nestable). `false` = timed out after `tries` attempts: the
    /// lock is NOT owned, our pending bit is withdrawn, and the caller must not
    /// touch the protected field nor call [`release`](Self::release).
    pub fn acquire(&self, word: &AtomicU32, tries: u32, delay: &mut dyn FnMut()) -> bool {
        if self.depth.load(Ordering::Relaxed) > 0 { self.depth.fetch_add(1, Ordering::Relaxed); return true; }
        for _ in 0..tries {
            let old = word.load(Ordering::Acquire);
            let new = (old & !3) | 2 | if old & 2 != 0 { 1 } else { 0 };
            if word.compare_exchange(old, new, Ordering::AcqRel, Ordering::Acquire).is_ok() {
                if new & 1 == 0 {
                    self.pending.store(false, Ordering::Relaxed);
                    self.owned.store(true, Ordering::Relaxed);
                    self.depth.store(1, Ordering::Relaxed);
                    return true;
                }
                // Owned by firmware: pending set, wait for its release.
                self.pending.store(true, Ordering::Relaxed);
            }
            delay();
        }
        // Timed out: withdraw only the pending bit we set, never the firmware's owned bit.
        if self.pending.swap(false, Ordering::Relaxed) {
            let mut old = word.load(Ordering::Acquire);
            while old & 1 != 0 {
                match word.compare_exchange(old, old & !1, Ordering::AcqRel, Ordering::Acquire) { Ok(_) => break, Err(o) => old = o }
            }
        }
        self.fails.fetch_add(1, Ordering::Relaxed);
        false
    }

    /// Release one level. Returns `true` when the outermost level was released and
    /// firmware had the pending bit set (the caller must then set PM1 GBL_RLS).
    /// Touches FACS only when we actually own the lock.
    pub fn release(&self, word: &AtomicU32) -> bool {
        if !self.owned.load(Ordering::Relaxed) { return false; }
        if self.depth.fetch_sub(1, Ordering::Relaxed) > 1 { return false; }
        self.owned.store(false, Ordering::Relaxed);
        word.fetch_and(!3, Ordering::AcqRel) & 1 != 0
    }
}
