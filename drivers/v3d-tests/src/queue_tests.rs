//! The CL queue against a scripted GPU: start sequences and their order, FLDONE -> render,
//! OUTOMEM overflow, cache clean, dependencies, the hang timer and reset, in-order fences.
use crate::v3d::queue::*;
use crate::v3d::regs::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum B { Hub, Core, Sms }

/// Register file + a GPU that is "running" a CT after a write to its QEA. Busy bits
/// (L2TCACTL TMUWCF/L2TFLS) drop after `busy_reads` reads; `stuck` keeps them set forever.
struct Gpu {
    regs: RefCell<HashMap<(u8, u32), u32>>,
    log: RefCell<Vec<(B, u32, u32)>>,
    busy_left: Cell<u32>,
    busy_reads: Cell<u32>,
    stuck: Cell<bool>,
}
impl Gpu {
    fn new() -> Self {
        Gpu { regs: RefCell::new(HashMap::new()), log: RefCell::new(vec![]), busy_left: Cell::new(0), busy_reads: Cell::new(2), stuck: Cell::new(false) }
    }
    fn set(&self, b: u8, off: u32, v: u32) { self.regs.borrow_mut().insert((b, off), v); }
    fn core_writes(&self) -> Vec<(u32, u32)> { self.log.borrow().iter().filter(|w| w.0 == B::Core).map(|w| (w.1, w.2)).collect() }
    fn pos(&self, off: u32) -> Option<usize> { self.log.borrow().iter().position(|w| w.1 == off && w.0 == B::Core) }
    fn wrote(&self, b: B, off: u32) -> bool { self.log.borrow().iter().any(|w| w.0 == b && w.1 == off) }
    fn clear(&self) { self.log.borrow_mut().clear(); }
}
impl V3dRegs for Gpu {
    fn hub_r(&self, off: u32) -> u32 { *self.regs.borrow().get(&(0, off)).unwrap_or(&0) }
    fn hub_w(&self, off: u32, v: u32) { self.log.borrow_mut().push((B::Hub, off, v)); self.set(0, off, v); }
    fn core_r(&self, off: u32) -> u32 {
        let v = *self.regs.borrow().get(&(1, off)).unwrap_or(&0);
        if off == CTL_L2TCACTL {
            if self.stuck.get() { return v; }
            if self.busy_left.get() > 0 { self.busy_left.set(self.busy_left.get() - 1); return v; }
            return 0;
        }
        v
    }
    fn core_w(&self, off: u32, v: u32) {
        self.log.borrow_mut().push((B::Core, off, v));
        if off == CTL_L2TCACTL { self.busy_left.set(self.busy_reads.get()); }
        self.set(1, off, v);
    }
    fn sms_r(&self, off: u32) -> u32 { *self.regs.borrow().get(&(2, off)).unwrap_or(&0) }
    fn sms_w(&self, off: u32, v: u32) { self.log.borrow_mut().push((B::Sms, off, v)); self.set(2, off, v & !0x1f); }
}

struct Env { now: Cell<u64>, done: RefCell<HashSet<u64>>, signalled: RefCell<Vec<u64>>, reinits: Cell<u32> }
impl Env {
    fn new() -> Self { Env { now: Cell::new(1000), done: RefCell::new(HashSet::new()), signalled: RefCell::new(vec![]), reinits: Cell::new(0) } }
    fn adv(&self, ns: u64) { self.now.set(self.now.get() + ns); }
}
impl QueueEnv for Env {
    fn now_ns(&self) -> u64 { self.now.set(self.now.get() + 1); self.now.get() }
    fn fence_done(&self, f: u64) -> bool { self.done.borrow().contains(&f) || self.signalled.borrow().contains(&f) }
    fn signal(&self, f: u64) { self.signalled.borrow_mut().push(f); }
    fn reinit(&self, _r: &dyn V3dRegs) -> bool { self.reinits.set(self.reinits.get() + 1); true }
}

fn q() -> ClQueue { let mut q = ClQueue::new(); q.reserve(); q }
fn job(fence: u64, bin: bool) -> ClJob {
    ClJob { fence, bcl_start: if bin { 0x1000 } else { 0 }, bcl_end: if bin { 0x1100 } else { 0 },
            rcl_start: 0x2000, rcl_end: 0x2200, ..Default::default() }
}

#[test]
fn bin_then_render_order_and_fence() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    let mut j = job(7, true);
    j.qma = 0x5000; j.qms = 0x100; j.qts = 0x6000;
    q.push(j).unwrap();
    q.service(&g, &e, 0);
    assert_eq!(q.phase(), Some(Phase::Bin));
    let w = g.core_writes();
    assert_eq!(w[0], (CTL_L2TCACTL, L2TCACTL_L2TFLS), "L2T flush first");
    assert_eq!(w[1], (CTL_SLCACTL, 0x0F0F_0F0F));
    assert_eq!(w[2], (PTB_BPOS, 0), "previous overflow dropped before the start");
    assert!(w.contains(&(CLE_CT0QMA, 0x5000)) && w.contains(&(CLE_CT0QMS, 0x100)));
    assert!(w.contains(&(CLE_CT0QTS, 0x6000 | CLE_CT0QTS_ENABLE)));
    assert!(g.pos(CLE_CT0QBA).unwrap() < g.pos(CLE_CT0QEA).unwrap());
    assert_eq!(*w.last().unwrap(), (CLE_CT0QEA, 0x1100), "QEA write starts the bin job, last");
    assert!(!g.wrote(B::Core, CLE_CT1QEA));
    g.clear();
    q.service(&g, &e, INT_FLDONE);
    assert_eq!(q.phase(), Some(Phase::Render));
    let w = g.core_writes();
    assert_eq!(w[0].0, CTL_L2TCACTL);
    assert_eq!(&w[w.len() - 2..], &[(CLE_CT1QBA, 0x2000), (CLE_CT1QEA, 0x2200)]);
    assert!(e.signalled.borrow().is_empty());
    q.service(&g, &e, INT_FRDONE);
    assert_eq!(*e.signalled.borrow(), vec![7]);
    assert!(q.idle());
    let r = q.take_retired().unwrap();
    assert!(r.ok && r.fence == 7);
}

#[test]
fn render_only_skips_bin() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    q.push(job(1, false)).unwrap();
    q.service(&g, &e, 0);
    assert_eq!(q.phase(), Some(Phase::Render));
    assert!(!g.wrote(B::Core, CLE_CT0QEA) && !g.wrote(B::Core, PTB_BPOS));
    q.service(&g, &e, INT_FRDONE);
    assert_eq!(*e.signalled.borrow(), vec![1]);
}

#[test]
fn stray_done_bits_do_not_complete() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    q.push(job(1, true)).unwrap();
    q.service(&g, &e, INT_FRDONE); // render not started yet
    assert_eq!(q.phase(), Some(Phase::Bin));
    assert!(e.signalled.borrow().is_empty());
    assert_eq!(q.stats.stray_irq, 1);
}

#[test]
fn jobs_run_one_at_a_time_in_order() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    for f in 1..=3 { q.push(job(f, f != 2)).unwrap(); }
    q.service(&g, &e, 0);
    q.service(&g, &e, INT_FLDONE);
    q.service(&g, &e, INT_FRDONE); // job 1 done; job 2 (render-only) starts
    assert_eq!(q.phase(), Some(Phase::Render));
    q.service(&g, &e, INT_FRDONE); // job 2 done; job 3 bins
    assert_eq!(q.phase(), Some(Phase::Bin));
    q.service(&g, &e, INT_FLDONE);
    q.service(&g, &e, INT_FRDONE);
    assert_eq!(*e.signalled.borrow(), vec![1, 2, 3]);
}

#[test]
fn outomem_hands_out_pool_chunks_and_returns_them() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    assert!(q.add_chunk(0x10_0000) && q.add_chunk(0x14_0000));
    q.push(job(1, true)).unwrap();
    q.service(&g, &e, 0);
    g.clear();
    q.service(&g, &e, INT_OUTOMEM);
    assert_eq!(g.core_writes(), vec![(PTB_BPOA, 0x14_0000), (PTB_BPOS, OVERFLOW_BYTES)]);
    q.service(&g, &e, INT_OUTOMEM);
    assert_eq!(q.free_chunks(), 0);
    g.clear();
    q.service(&g, &e, INT_OUTOMEM); // pool empty: nothing written, counted
    assert!(g.core_writes().is_empty());
    assert_eq!((q.stats.outomem, q.stats.overflow_fail), (3, 1));
    q.service(&g, &e, INT_FLDONE);
    assert_eq!(q.free_chunks(), 0, "chunks stay with the job until render is done");
    q.service(&g, &e, INT_FRDONE);
    assert_eq!(q.free_chunks(), 2);
    assert!(q.take_retired().unwrap().overflow.is_empty());
}

#[test]
fn outomem_outside_bin_is_ignored() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    q.add_chunk(0x10_0000);
    q.push(job(1, false)).unwrap();
    q.service(&g, &e, 0);
    q.service(&g, &e, INT_OUTOMEM);
    assert_eq!(q.free_chunks(), 1);
}

#[test]
fn flush_cache_cleans_before_signal() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    let mut j = job(4, false);
    j.flush_cache = true;
    q.push(j).unwrap();
    q.service(&g, &e, 0);
    g.clear();
    q.service(&g, &e, INT_FRDONE);
    let w = g.core_writes();
    assert_eq!(w, vec![(CTL_L2TCACTL, L2TCACTL_TMUWCF), (CTL_L2TCACTL, L2TCACTL_L2TFLS | (2 << 1))]);
    assert_eq!(*e.signalled.borrow(), vec![4]);
    assert!(q.take_retired().unwrap().ok);
}

#[test]
fn clean_timeout_is_bounded_and_marks_job() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    let mut j = job(4, false);
    j.flush_cache = true;
    q.push(j).unwrap();
    q.service(&g, &e, 0);
    g.stuck.set(true);
    q.service(&g, &e, INT_FRDONE);
    assert_eq!(*e.signalled.borrow(), vec![4], "fence still signals");
    assert_eq!(q.stats.clean_fail, 1);
    assert!(!q.take_retired().unwrap().ok);
}

#[test]
fn dependencies_gate_bin_and_render() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    let mut j = job(10, true);
    j.bin_deps = vec![3];
    j.render_deps = vec![4];
    q.push(j).unwrap();
    q.service(&g, &e, 0);
    assert_eq!(q.phase(), None, "bin waits for fence 3");
    assert!(!g.wrote(B::Core, CLE_CT0QEA));
    e.done.borrow_mut().insert(3);
    q.tick(&g, &e);
    assert_eq!(q.phase(), Some(Phase::Bin));
    q.service(&g, &e, INT_FLDONE);
    assert_eq!(q.phase(), Some(Phase::WaitRender), "render waits for fence 4");
    e.adv(2 * HANG_TIMEOUT_NS);
    q.tick(&g, &e);
    assert_eq!(q.stats.resets, 0, "no hang timer while waiting on a dependency");
    e.done.borrow_mut().insert(4);
    q.tick(&g, &e);
    assert_eq!(q.phase(), Some(Phase::Render));
}

#[test]
fn render_only_job_honours_bin_deps() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    let mut j = job(10, false);
    j.bin_deps = vec![3];
    q.push(j).unwrap();
    q.service(&g, &e, 0);
    assert!(!g.wrote(B::Core, CLE_CT1QEA));
    e.done.borrow_mut().insert(3);
    q.service(&g, &e, 0);
    assert!(g.wrote(B::Core, CLE_CT1QEA));
}

#[test]
fn hang_without_progress_resets_and_fails_the_job() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    q.push(job(1, true)).unwrap();
    q.push(job(2, false)).unwrap();
    q.service(&g, &e, 0);
    g.set(1, CLE_CT0CA, 0x1040);
    e.adv(HANG_TIMEOUT_NS);
    q.tick(&g, &e); // CA moved since start: progress, timer extended
    assert_eq!(q.stats.resets, 0);
    e.adv(HANG_TIMEOUT_NS - 10);
    q.tick(&g, &e);
    assert_eq!(q.stats.resets, 0, "not yet due");
    g.clear();
    e.adv(20);
    q.tick(&g, &e); // no progress for 500 ms
    assert_eq!((q.stats.timeouts, q.stats.resets, q.stats.reset_fail), (1, 1, 0));
    let hub = g.log.borrow().iter().position(|w| w.0 == B::Hub && w.1 == GMP_CFG && w.2 == GMP_CFG_STOP_REQ).unwrap();
    let sms = g.log.borrow().iter().position(|w| w.0 == B::Sms && w.1 == SMS_REE_CS && w.2 == SMS_STATE_RESET_REQ).unwrap();
    assert!(hub < sms, "GMP stop before the SMS reset");
    assert!(g.wrote(B::Core, CTL_L2TFLEND));
    assert_eq!(e.reinits.get(), 1);
    assert_eq!(*e.signalled.borrow(), vec![1], "the hung job's fence still signals");
    assert!(!q.take_retired().unwrap().ok);
    assert_eq!(q.phase(), Some(Phase::Render), "the next job runs after the reset");
}

#[test]
fn reset_timeouts_are_bounded() {
    let (g, e, mut q) = (Gpu::new(), Env::new(), q());
    g.set(0, GMP_STATUS, 0x0100_0000); // writes never drain
    q.push(job(1, false)).unwrap();
    q.service(&g, &e, 0);
    e.adv(HANG_TIMEOUT_NS + 1);
    q.tick(&g, &e);
    assert_eq!((q.stats.resets, q.stats.reset_fail), (1, 1));
    assert_eq!(*e.signalled.borrow(), vec![1]);
}

#[test]
fn queue_capacity_is_enforced() {
    let mut q = q();
    for f in 0..MAX_JOBS as u64 { q.push(job(f + 1, false)).unwrap(); }
    assert_eq!(q.push(job(99, false)).unwrap_err(), PushError::Full);
}
