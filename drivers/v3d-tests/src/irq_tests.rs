use crate::mock::*;
use crate::v3d::irq::*;
use crate::v3d::mmu::{MMU_CTL, MMU_CTL_ON, MMU_CTL_PT_INVALID, MMU_VIO_ADDR, MMU_VIO_ID};
use crate::v3d::regs::V3dRegs;
use std::cell::Cell;

#[test]
fn masks_for_71_and_42() {
    assert_eq!(core_mask(71), INT_FRDONE | INT_FLDONE | INT_OUTOMEM | (1 << 6));
    assert_eq!(core_mask(42), INT_FRDONE | INT_FLDONE | INT_OUTOMEM | (1 << 7) | INT_GMPV_42);
    assert_eq!(hub_mask(71), HUB_INT_MMU_WRV | HUB_INT_MMU_PTI | HUB_INT_MMU_CAP | HUB_INT_TFUC | (1 << 6));
    assert_eq!(int_pctr(71), 1 << 5);
    assert_eq!((HUB_INTID, CORE_INTID), (32 + 250, 32 + 249));
}

#[test]
fn enable_order_clr_mskset_mskclr_core_then_hub() {
    let r = MockRegs::new();
    enable(&r, 71);
    let w = r.writes.borrow().clone();
    let (c, h) = (core_mask(71), hub_mask(71));
    assert_eq!(w, vec![
        (Blk::Core, INT_CLR, c), (Blk::Core, INT_MSK_SET, !c), (Blk::Core, INT_MSK_CLR, c),
        (Blk::Hub, INT_CLR, h), (Blk::Hub, INT_MSK_SET, !h), (Blk::Hub, INT_MSK_CLR, h),
    ]);
}

#[test]
fn core_handler_acks_exactly_what_it_read() {
    let r = MockRegs::new();
    assert_eq!(handle_core(&r), 0);
    assert!(r.writes.borrow().is_empty(), "no ack when not ours");
    r.core_w(INT_SET, INT_FRDONE | INT_FLDONE);
    r.clear_log();
    assert_eq!(handle_core(&r), INT_FRDONE | INT_FLDONE);
    assert_eq!(*r.writes.borrow(), vec![(Blk::Core, INT_CLR, INT_FRDONE | INT_FLDONE)]);
    assert_eq!(r.core_r(INT_STS), 0);
}

#[test]
fn hub_handler_tfu_done_is_not_a_fault() {
    let r = MockRegs::new();
    r.hub_w(INT_SET, HUB_INT_TFUC);
    assert_eq!(handle_hub(&r, 32), (HUB_INT_TFUC, None));
    assert_eq!(r.hub_r(INT_STS), 0);
}

#[test]
fn hub_handler_mmu_fault_reads_vio_and_acks_mmu_ctl() {
    let r = MockRegs::new();
    r.hub_w(MMU_CTL, MMU_CTL_ON);
    r.hub.borrow_mut().insert(MMU_CTL, MMU_CTL_ON | MMU_CTL_PT_INVALID);
    r.hub.borrow_mut().insert(MMU_VIO_ID, 0x42);
    r.hub.borrow_mut().insert(MMU_VIO_ADDR, 0x0123_4567);
    r.hub_w(INT_SET, HUB_INT_MMU_PTI);
    r.clear_log();
    let (s, f) = handle_hub(&r, 36);
    assert_eq!(s, HUB_INT_MMU_PTI);
    assert_eq!(f, Some(MmuFault { bits: HUB_INT_MMU_PTI, axi_id: 0x42, addr: 0x0123_4567u64 << 4 }));
    let w = r.hub_writes();
    assert_eq!(w[0], (INT_CLR, HUB_INT_MMU_PTI));
    assert_eq!(w[1], (MMU_CTL, MMU_CTL_ON | MMU_CTL_PT_INVALID));
    assert_eq!(r.hub_r(MMU_CTL) & MMU_CTL_PT_INVALID, 0, "write-back acks the sticky bit");
    assert_eq!(MmuFault { bits: 0, axi_id: 0, addr: 0 }.addr, 0);
}

/// Simulated surroundings: `deliver_after` clock readings after INT_SET the "CPU" takes the
/// interrupt (runs the handler body), or never; `gic` is what the distributor shows meanwhile.
struct Env<'a> {
    regs: &'a MockRegs,
    block: Block,
    deliver_after: Option<u32>,
    polls: Cell<u32>,
    count: Cell<u64>,
    gic: u32,
    masked: bool,
}
impl SelftestEnv for Env<'_> {
    fn delivered(&self) -> u64 {
        let n = self.polls.get() + 1;
        self.polls.set(n);
        if self.deliver_after.map_or(false, |d| n >= d) {
            let bits = match self.block { Block::Core => handle_core(self.regs), Block::Hub => handle_hub(self.regs, 32).0 };
            if bits != 0 { self.count.set(self.count.get() + 1); }
        }
        self.count.get()
    }
    fn gic_state(&self) -> u32 { self.gic }
    fn cpu_irqs_masked(&self) -> bool { self.masked }
}

fn run(block: Block, deliver_after: Option<u32>, gic: u32, masked: bool, latches: bool) -> (LineResult, MockRegs) {
    let r = MockRegs::new();
    r.int_set_latches.set(latches);
    let bit = if block == Block::Hub { HUB_INT_TFUC } else { INT_FRDONE };
    let res = {
        let e = Env { regs: &r, block, deliver_after, polls: Cell::new(0), count: Cell::new(0), gic, masked };
        selftest_line(&r, block, bit, &e, &FakeClock::new(), SELFTEST_TIMEOUT_US)
    };
    (res, r)
}

#[test]
fn selftest_delivered_is_ok_and_status_clear() {
    for b in [Block::Hub, Block::Core] {
        let (res, r) = run(b, Some(3), 1, false, true);
        assert_eq!(res, LineResult::Ok);
        assert_eq!(r.hub_r(INT_STS) | r.core_r(INT_STS), 0);
    }
}

#[test]
fn selftest_pending_at_gic_but_never_taken() {
    let (res, r) = run(Block::Hub, None, 1, false, true);
    assert_eq!(res, LineResult::GicPending);
    assert_eq!(r.hub_r(INT_STS), 0, "the test bit is cleared so the level line drops");
    assert_eq!(run(Block::Core, None, 2, true, true).0, LineResult::CpuMasked);
}

#[test]
fn selftest_line_never_reaches_gic() {
    assert_eq!(run(Block::Core, None, 0, false, true).0, LineResult::Silent);
}

#[test]
fn selftest_int_set_ignored_is_inconclusive() {
    assert_eq!(run(Block::Hub, None, 0, false, false).0, LineResult::NoLatch);
}

#[test]
fn selftest_writes_set_then_clear_on_the_right_block() {
    let (_, r) = run(Block::Core, None, 0, false, true);
    let w = r.writes.borrow().clone();
    assert_eq!(w.first(), Some(&(Blk::Core, INT_SET, INT_FRDONE)));
    assert_eq!(w.last(), Some(&(Blk::Core, INT_CLR, INT_FRDONE)));
    assert!(w.iter().all(|x| x.0 == Blk::Core));
}

#[test]
fn tags_are_greppable() {
    let all = [LineResult::Ok, LineResult::GicPending, LineResult::CpuMasked, LineResult::Silent, LineResult::NoLatch, LineResult::Busy];
    let tags: Vec<_> = all.iter().map(|r| r.tag()).collect();
    assert_eq!(tags, ["ok", "gic-pending", "cpu-masked", "silent", "nolatch", "busy"]);
}
