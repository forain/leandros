//! A scripted V3D register file: MMU flush/TLB-clear busy bits that drop after N reads, INT_SET
//! latching INT_STS, INT_CLR acking it, and a log of every write in order.
use crate::v3d::irq::{INT_CLR, INT_SET, INT_STS};
use crate::v3d::mmu::*;
use crate::v3d::regs::V3dRegs;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Blk { Hub, Core, Sms }

pub struct MockRegs {
    pub hub: RefCell<HashMap<u32, u32>>,
    pub core: RefCell<HashMap<u32, u32>>,
    pub writes: RefCell<Vec<(Blk, u32, u32)>>,
    /// Reads of the busy bit before it clears; `u32::MAX` = stuck.
    pub mmuc_busy_reads: Cell<u32>,
    pub tlb_busy_reads: Cell<u32>,
    mmuc_left: Cell<u32>,
    tlb_left: Cell<u32>,
    /// INT_SET latches INT_STS (real hardware behaviour); false models a part that ignores it.
    pub int_set_latches: Cell<bool>,
    /// When set, PTE `idx` of the table at `ptr` is recorded at every MMUC flush write.
    pub pt_watch: Cell<Option<(*const u32, usize)>>,
    pub pte_at_flush: RefCell<Vec<u32>>,
}

impl MockRegs {
    pub fn new() -> Self {
        MockRegs {
            hub: RefCell::new(HashMap::new()),
            core: RefCell::new(HashMap::new()),
            writes: RefCell::new(Vec::new()),
            mmuc_busy_reads: Cell::new(2),
            tlb_busy_reads: Cell::new(3),
            mmuc_left: Cell::new(0),
            tlb_left: Cell::new(0),
            int_set_latches: Cell::new(true),
            pt_watch: Cell::new(None),
            pte_at_flush: RefCell::new(Vec::new()),
        }
    }
    pub fn hub_writes(&self) -> Vec<(u32, u32)> {
        self.writes.borrow().iter().filter(|w| w.0 == Blk::Hub).map(|w| (w.1, w.2)).collect()
    }
    pub fn clear_log(&self) { self.writes.borrow_mut().clear(); }
    fn int_write(map: &RefCell<HashMap<u32, u32>>, off: u32, v: u32, latch: bool) -> bool {
        let mut m = map.borrow_mut();
        match off {
            INT_SET => { if latch { *m.entry(INT_STS).or_insert(0) |= v; } true }
            INT_CLR => { *m.entry(INT_STS).or_insert(0) &= !v; true }
            _ => false,
        }
    }
}

impl V3dRegs for MockRegs {
    fn hub_r(&self, off: u32) -> u32 {
        let v = *self.hub.borrow().get(&off).unwrap_or(&0);
        match off {
            MMUC_CONTROL if self.mmuc_left.get() > 0 => {
                if self.mmuc_left.get() != u32::MAX { self.mmuc_left.set(self.mmuc_left.get() - 1); }
                v | MMUC_CONTROL_FLUSHING
            }
            MMU_CTL if self.tlb_left.get() > 0 => {
                if self.tlb_left.get() != u32::MAX { self.tlb_left.set(self.tlb_left.get() - 1); }
                v | MMU_CTL_TLB_CLEARING
            }
            _ => v,
        }
    }
    fn hub_w(&self, off: u32, v: u32) {
        self.writes.borrow_mut().push((Blk::Hub, off, v));
        if Self::int_write(&self.hub, off, v, self.int_set_latches.get()) { return; }
        match off {
            MMUC_CONTROL => {
                if v & MMUC_CONTROL_FLUSH != 0 {
                    self.mmuc_left.set(self.mmuc_busy_reads.get());
                    if let Some((p, i)) = self.pt_watch.get() {
                        self.pte_at_flush.borrow_mut().push(unsafe { p.add(i).read_volatile() });
                    }
                }
                self.hub.borrow_mut().insert(off, v & !MMUC_CONTROL_FLUSH);
            }
            MMU_CTL => {
                if v & MMU_CTL_TLB_CLEAR != 0 { self.tlb_left.set(self.tlb_busy_reads.get()); }
                // Sticky fault bits are write-1-to-clear.
                let w1c = MMU_CTL_PT_INVALID | MMU_CTL_WRITE_VIOLATION | MMU_CTL_CAP_EXCEEDED;
                let old = *self.hub.borrow().get(&off).unwrap_or(&0);
                let keep = old & w1c & !v;
                self.hub.borrow_mut().insert(off, (v & !(MMU_CTL_TLB_CLEAR | w1c)) | keep);
            }
            _ => { self.hub.borrow_mut().insert(off, v); }
        }
    }
    fn core_r(&self, off: u32) -> u32 { *self.core.borrow().get(&off).unwrap_or(&0) }
    fn core_w(&self, off: u32, v: u32) {
        self.writes.borrow_mut().push((Blk::Core, off, v));
        if Self::int_write(&self.core, off, v, self.int_set_latches.get()) { return; }
        self.core.borrow_mut().insert(off, v);
    }
    fn sms_r(&self, _off: u32) -> u32 { 0 }
    fn sms_w(&self, off: u32, v: u32) { self.writes.borrow_mut().push((Blk::Sms, off, v)); }
}

/// Advances 1 us per reading.
pub struct FakeClock(pub Cell<u64>);
impl FakeClock { pub fn new() -> Self { FakeClock(Cell::new(0)) } }
impl Clock for FakeClock {
    fn now_us(&self) -> u64 { let t = self.0.get(); self.0.set(t + 1); t }
}
