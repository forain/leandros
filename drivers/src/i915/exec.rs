//! G3b: user batches on the real engines — execlist submission without GuC.
//!
//! Enabled only by `i915.uapi=1 i915.exec=1` (and never on `i915.fake`).
//! `init` brings the GT up the way Linux does before the first user request
//! and records the golden context; `submit` queues a user batch; a poll
//! thread (1 ms while busy) retires requests and detects hangs.
//!
//! Bring-up (`init`): forcewake held for good, full GT reset, private PAT,
//! GT + GTT workarounds, MOCS (rcs0 + bcs0) and L3CC, render engine
//! workarounds and non-privileged whitelist, execlists on each engine with
//! its own status page (HWSP), then per engine one kernel request in a
//! restore-inhibited kernel context — on rcs0 the context workaround LRIs and
//! the gen9 null render state batch — whose saved image becomes the engine's
//! default state (Linux `__engines_record_defaults`). Any failure leaves
//! execution off and the UAPI answers ENODEV as in G3.
//!
//! Submission model (deliberately the simplest correct one): one request in
//! flight per engine. A request is invalidate + MI_BATCH_BUFFER_START
//! (PPGTT, non-privileged) + breadcrumb (the engine seqno into the HWSP) in
//! the (context, engine) ring; its context is submitted alone through the
//! ELSP once the engine reported active->idle for the previous one, so the
//! CPU only ever edits a context image the hardware has saved, and there is
//! no lite-restore or preemption to get wrong. The fence completes when the
//! breadcrumb lands; the next submission waits for the CSB idle event.
//!
//! Per (context, engine) an `Lrc`: a 1 MiB GGTT slot with the context image
//! (copied from the default state, PPHWSP cleared, registers re-initialised
//! with this ring and the VM's PML4) at +0 and the ring at +512 KiB.
//!
//! Hangs: `HangCheck` over breadcrumb, ACTHD/HEAD and EIR. A hung request is
//! reset (RESET_CTL handshake + GDRST of the engine), the engine state is
//! re-applied (workarounds, whitelist, MOCS, execlists), its context's LRC is
//! discarded (the next submission starts from the default state again) and
//! the request completes as guilty: the UAPI counts the reset, marks the
//! context and bans it if it is not recoverable.

use alloc::collections::{BTreeMap, VecDeque};
use alloc::vec::Vec;
use core::sync::atomic::{fence, AtomicBool, AtomicUsize, Ordering};

use super::gt::{self, alloc_bo, enable_execlists, free_bo, Bo, KStore};
use super::hw::{self, ctx, reg, EngineClass, EngineDesc, HangCheck, HangVerdict, Ppgtt};
use super::wa;
use super::{delay_us, forcewake_get, forcewake_put, ilog, log, now_ns, opt, Device, Gsm, Mmio};

/// Engines driven here, in the UAPI's engine order: rcs0, bcs0.
pub(super) const NENG: usize = 2;
const DESCS: [&EngineDesc; NENG] = [&hw::GEN9_ENGINES[0], &hw::GEN9_ENGINES[1]];

const SLOT_SIZE: u64 = 1 << 20;
const RING_OFF: u64 = 512 << 10;
pub(super) const MAX_RING: u32 = 512 << 10;
const NSLOTS: usize = 1024;
/// Status-page dword the invalidate's post-sync write lands in.
const HWS_SCRATCH: usize = 0x34;
/// The kernel context of each engine (the golden-state recorder).
const KERNEL: Key = (0, u32::MAX, 0);

/// (`uapi::Open::gen` — unique per open for the whole boot, never the
/// recycled VFS open_id; UAPI context id; engine-map slot, 0 without a map).
///
/// The slot is part of the identity because Linux gives every entry of a
/// context's engine map its own logical context (`intel_context`, its own
/// LRC image), even when two entries name the same physical engine. iris on
/// gen9 creates exactly that: one context with the map [rcs0, rcs0], its
/// render batches on slot 0 and its compute batches on slot 1, and relies
/// on the two keeping separate hardware state. Keyed without the slot, both
/// ran in ONE image: the compute batch's PIPELINE_SELECT(GPGPU) and its own
/// STATE_BASE_ADDRESS replaced the render state iris had emitted once, and
/// every later render batch drew nothing and blitted nothing (boot 16,
/// i915gltest comp-*: the first batch rendered, everything after the first
/// glFinish — which flushes the compute batch — read back zeros).
pub(super) type Key = (u32, u32, u32);

/// A request that left the engine: `guilty` is set when it hung.
pub(super) struct Retired { pub eng: usize, pub seq: u64, pub guilty: Option<Key> }

struct Lrc { slot: usize, ctxo: Bo, ring: Bo, ring_size: u32, tail: u32, fresh: bool, pml4: u64 }

struct Req { key: Key, seq: u64, pml4: u64, ring_size: u32, pre: Vec<u32>, batch: Option<(u64, bool)> }

struct Flight { req: Req, hc: HangCheck, done_ns: Option<u64>, idle: bool, warned: bool }

struct Eng {
    d: &'static EngineDesc,
    hwsp: Bo,
    csb_head: u32,
    queue: VecDeque<Req>,
    flight: Option<Flight>,
    lrcs: BTreeMap<Key, Lrc>,
    /// Keys whose LRC is freed once nothing of theirs is queued or in flight.
    dead: Vec<Key>,
    /// The golden context image (dwords), once recorded.
    default: Option<Vec<u32>>,
    /// Requests that never reached the engine, to report as retired.
    failed: Vec<Retired>,
}

struct Core {
    m: Mmio,
    g: Gsm,
    slot_base: u64,
    slots: Vec<bool>,
    eng: Vec<Eng>,
    budget_ns: u64,
    stall_ns: u64,
    resets: u32,
    /// GGTT address | cachelines of the rcs indirect context workaround
    /// batch every render context image points at (0: none, `i915.ctxfix=0`).
    indirect_ctx: u32,
}

static CORE: spin::Mutex<Option<Core>> = spin::Mutex::new(None);
static READY: AtomicBool = AtomicBool::new(false);
/// Kernel VA of each engine's status-page seqno dword (0: not ready), for
/// the lock-free completion check `hw_done`.
static HW_SEQ_VA: [AtomicUsize; NENG] = [const { AtomicUsize::new(0) }; NENG];

/// Lock-free: has engine `e`'s breadcrumb for `seq` landed? The breadcrumb
/// is written after the request's flushes (RT/depth/DC flush + CS stall on
/// rcs, MI_FLUSH_DW on bcs), so the request's writes are visible once it
/// shows. Lets a fence waiter see completion without waiting for the poll
/// thread to retire it (the poll thread still does the bookkeeping: CSB,
/// next request, hang detection).
pub(super) fn hw_done(e: usize, seq: u64) -> bool {
    let Some(a) = HW_SEQ_VA.get(e) else { return false };
    let va = a.load(Ordering::Acquire);
    if va == 0 { return false; }
    let hw = unsafe { core::ptr::read_volatile(va as *const u32) };
    hw.wrapping_sub(seq as u32) as i32 >= 0
}

pub(super) fn ready() -> bool { READY.load(Ordering::Acquire) }

impl Core {
    fn r(&self, e: usize, off: u32) -> u32 { self.m.r(self.eng[e].d.mmio_base + off) }

    fn alloc_slot(&mut self) -> Option<usize> {
        let i = self.slots.iter().position(|&u| !u)?;
        self.slots[i] = true;
        Some(i)
    }

    /// Engine registers Linux re-applies after any reset of the engine.
    fn engine_regs(&self, e: usize) {
        let m = &self.m;
        let d = self.eng[e].d;
        if d.class == EngineClass::Render {
            for w in wa::gen9_rcs_engine_was().0 {
                let old = if w.needs_read() { m.r(w.reg) } else { 0 };
                m.w(w.reg, w.value(old));
            }
            for (r, v) in wa::whitelist_writes(d.mmio_base, &wa::KBL_RCS_WHITELIST) { m.w(r, v); }
            for (r, v) in wa::mocs_writes(wa::r::GEN9_RCS0_MOCS0) { m.w(r, v); }
            for (r, v) in wa::l3cc_writes() { m.w(r, v); }
        } else {
            for (r, v) in wa::mocs_writes(wa::r::GEN9_BCS0_MOCS0) { m.w(r, v); }
        }
    }

    /// The LRC of `key` on engine `e`, created on first use.
    fn lrc(&mut self, e: usize, key: Key, ring_size: u32, pml4: u64) -> Result<(), &'static str> {
        // A released context's LRC is still used here by the requests it
        // queued BEFORE it was released (a process exiting or killed with
        // batches queued): `reap` keeps the LRC until they drained. That is
        // not key reuse; real reuse is checked at queue time (`submit`).
        if self.eng[e].lrcs.contains_key(&key) { return Ok(()); }
        let slot = self.alloc_slot().ok_or("out of GGTT context slots")?;
        let base = self.slot_base + slot as u64 * SLOT_SIZE;
        let class = self.eng[e].d.class;
        let csize = hw::context_size(class) as u64;
        let ctxo = match alloc_bo(csize, base, &self.g) { Some(b) => b, None => { self.slots[slot] = false; return Err("out of memory (context image)"); } };
        let ring = match alloc_bo(ring_size as u64, base + RING_OFF, &self.g) {
            Some(b) => b,
            None => { free_bo(&ctxo, &self.g); self.slots[slot] = false; return Err("out of memory (ring)"); }
        };
        let inhibit = match &self.eng[e].default {
            Some(img) => {
                for (i, &v) in img.iter().enumerate() { ctxo.wr(i, v); }
                // Linux clears the per-process HWSP page of the copy.
                for i in 0..1024 { ctxo.wr(i, 0); }
                false
            }
            None => true,
        };
        let mut regs = alloc::vec![0u32; 1024];
        if !inhibit { for (i, r) in regs.iter_mut().enumerate() { *r = ctxo.rd(ctx::LRC_STATE_PN * 1024 + i); } }
        hw::lrc_init_regs(&mut regs, &hw::LrcInit {
            class, mmio_base: self.eng[e].d.mmio_base, ring_ggtt: ring.ggtt as u32, ring_size,
            head: 0, tail: 0, pml4, inhibit, rpcs: 0, indirect_ctx: self.indirect_ctx,
        });
        for (i, &v) in regs.iter().enumerate() { ctxo.wr(ctx::LRC_STATE_PN * 1024 + i, v); }
        fence(Ordering::SeqCst);
        self.m.w(reg::GFX_FLSH_CNTL_GEN6, 1);
        self.eng[e].lrcs.insert(key, Lrc { slot, ctxo, ring, ring_size, tail: 0, fresh: true, pml4 });
        Ok(())
    }

    fn free_lrc(&mut self, e: usize, key: Key) {
        if let Some(l) = self.eng[e].lrcs.remove(&key) {
            free_bo(&l.ctxo, &self.g);
            free_bo(&l.ring, &self.g);
            self.slots[l.slot] = false;
            self.m.w(reg::GFX_FLSH_CNTL_GEN6, 1);
        }
    }

    /// Free the LRCs of released contexts that are no longer in use.
    fn reap(&mut self, e: usize) {
        let busy = |eng: &Eng, k: Key| eng.flight.as_ref().map_or(false, |f| f.req.key == k) || eng.queue.iter().any(|r| r.key == k);
        let mut i = 0;
        while i < self.eng[e].dead.len() {
            let k = self.eng[e].dead[i];
            if busy(&self.eng[e], k) { i += 1; continue; }
            self.eng[e].dead.swap_remove(i);
            self.free_lrc(e, k);
        }
    }

    /// Start the next queued request if the engine is free. A request whose
    /// context cannot be set up completes at once as guilty (`failed`).
    fn pump(&mut self, e: usize) {
        while self.eng[e].flight.is_none() {
            let Some(req) = self.eng[e].queue.pop_front() else { return };
            if let Err(why) = self.lrc(e, req.key, req.ring_size, req.pml4) {
                ilog!("{}: request {} of context {:?} dropped: {}", self.eng[e].d.name, req.seq, req.key, why);
                if req.key != KERNEL { self.eng[e].failed.push(Retired { eng: e, seq: req.seq, guilty: Some(req.key) }); }
                continue;
            }
            self.start(e, req);
        }
    }

    fn start(&mut self, e: usize, req: Req) {
        let d = self.eng[e].d;
        let hwsp = self.eng[e].hwsp;
        let scratch = (hwsp.ggtt + HWS_SCRATCH as u64 * 4) as u32;
        let seq_addr = (hwsp.ggtt + hw::HWS_SEQNO_INDEX as u64 * 4) as u32;
        let cs = hw::build_request(d.class, scratch, seq_addr, req.seq as u32, &req.pre, req.batch);
        let l = self.eng[e].lrcs.get_mut(&req.key).expect("lrc");
        let bytes = cs.len() as u32 * 4;
        let (noops, start) = hw::ring_place(l.tail, bytes, l.ring_size).expect("request larger than its ring");
        for i in 0..noops { l.ring.wr((l.tail / 4 + i) as usize, hw::cmd::MI_NOOP); }
        for (i, &v) in cs.iter().enumerate() { l.ring.wr((start / 4) as usize + i, v); }
        l.tail = (start + bytes) % l.ring_size;
        let rd = |i: usize| ctx::LRC_STATE_PN * 1024 + i;
        l.ctxo.wr(rd(ctx::RING_TAIL), l.tail);
        if l.pml4 != req.pml4 {
            l.ctxo.wr(rd(ctx::PDP0_UDW), (req.pml4 >> 32) as u32);
            l.ctxo.wr(rd(ctx::PDP0_LDW), req.pml4 as u32);
            l.pml4 = req.pml4;
        }
        let desc = hw::ctx_descriptor(l.ctxo.ggtt as u32, 0x100 + l.slot as u32, true, l.fresh);
        l.fresh = false;
        fence(Ordering::SeqCst);
        for w in hw::elsp_writes(desc, 0) { self.m.w(d.mmio_base + reg::RING_ELSP, w); }
        let now = now_ns();
        self.eng[e].flight = Some(Flight { req, hc: HangCheck::new(now, self.budget_ns, self.stall_ns), done_ns: None, idle: false, warned: false });
    }

    /// Read the CSB events since the last call; true if one is active->idle.
    fn drain_csb(&mut self, e: usize) -> bool {
        let eng = &mut self.eng[e];
        let wr = eng.hwsp.rd(hw::HWS_CSB_WRITE_INDEX) & 7;
        if wr >= hw::CSB_ENTRIES { return false; }
        let mut idle = false;
        for i in hw::csb_pending(eng.csb_head, wr) {
            let st = eng.hwsp.rd(hw::HWS_CSB_BUF0_INDEX + 2 * i as usize);
            if st & hw::CSB_ACTIVE_IDLE != 0 { idle = true; }
        }
        if wr != eng.csb_head {
            eng.csb_head = wr;
            self.m.w(eng.d.mmio_base + reg::RING_CONTEXT_STATUS_PTR, (0x700 << 16) | (wr << 8));
        }
        idle
    }

    /// Advance engine `e`: retire, detect hangs, start the next request.
    fn poll(&mut self, e: usize, out: &mut Vec<Retired>) {
        out.append(&mut self.eng[e].failed);
        if self.eng[e].flight.is_none() { self.reap(e); self.pump(e); return; }
        let idle = self.drain_csb(e);
        let now = now_ns();
        let hw_seq = self.eng[e].hwsp.rd(hw::HWS_SEQNO_INDEX);
        let (acthd, head, eir) = (
            (self.r(e, reg::RING_ACTHD_UDW) as u64) << 32 | self.r(e, reg::RING_ACTHD) as u64,
            self.r(e, reg::RING_HEAD), self.r(e, reg::RING_EIR));
        let name = self.eng[e].d.name;
        let f = self.eng[e].flight.as_mut().expect("flight");
        f.idle |= idle;
        if f.done_ns.is_none() {
            let done = hw_seq.wrapping_sub(f.req.seq as u32) as i32 >= 0;
            match f.hc.sample(now, done, acthd, head, eir) {
                HangVerdict::Done => {
                    f.done_ns = Some(now);
                    if f.req.key != KERNEL { out.push(Retired { eng: e, seq: f.req.seq, guilty: None }); }
                }
                HangVerdict::Running => {}
                v => {
                    ilog!("{}: request {} of context {:?} {} (HEAD=0x{:x} ACTHD=0x{:x} EIR=0x{:x} seqno(hwsp)={})", name, f.req.seq,
                        f.req.key, if v == HangVerdict::Hung { "HUNG" } else { "TIMED OUT" }, head, acthd, eir, hw_seq);
                    let key = f.req.key;
                    let seq = f.req.seq;
                    self.fail_flight(e, true);
                    if key != KERNEL { out.push(Retired { eng: e, seq, guilty: Some(key) }); }
                    self.reap(e);
                    self.pump(e);
                    return;
                }
            }
        }
        let (st_hi, st_lo, csb_ptr) = (self.r(e, reg::RING_EXECLIST_STATUS_HI), self.r(e, reg::RING_EXECLIST_STATUS_LO),
                                       self.r(e, reg::RING_CONTEXT_STATUS_PTR));
        let f = self.eng[e].flight.as_mut().expect("flight");
        let Some(t) = f.done_ns else { return };
        if !f.idle && now.wrapping_sub(t) > 100_000_000 {
            // The breadcrumb landed 100 ms ago but no active->idle event
            // arrived: log once and go on (the request is complete).
            if !f.warned {
                ilog!("{}: no active->idle CSB event 100 ms after request {} (EXECLIST_STATUS=0x{:08x}_{:08x} CSB_PTR=0x{:08x}); continuing",
                    name, f.req.seq, st_hi, st_lo, csb_ptr);
            }
            f.warned = true;
            f.idle = true;
        }
        if f.idle {
            self.eng[e].flight = None;
            self.reap(e);
            self.pump(e);
        }
    }

    /// Take the in-flight request off the engine: reset it if it is
    /// running, and discard its context's LRC.
    fn fail_flight(&mut self, e: usize, reset: bool) {
        let Some(f) = self.eng[e].flight.take() else { return };
        if reset {
            gt::reset_engine(&self.m, self.eng[e].d);
            self.resets += 1;
            self.engine_regs(e);
            let hwsp = self.eng[e].hwsp;
            let (_, head) = enable_execlists(&self.m, self.eng[e].d, &hwsp);
            self.eng[e].csb_head = head;
            // The hung breadcrumb never landed; later ones carry larger seqnos.
        }
        if f.req.key != KERNEL { self.free_lrc(e, f.req.key); }
    }
}

/// Queue a user batch on engine `eng` for context `key` whose VM's PML4 is
/// `pml4`; `seq` is the engine seqno the UAPI assigned (strictly increasing
/// per engine, submitted in order).
pub(super) fn submit(eng: usize, key: Key, seq: u64, batch: u64, pml4: u64, ring_size: u32) -> Result<(), i32> {
    let mut g = CORE.lock();
    let c = g.as_mut().ok_or(19)?;
    let ring_size = ring_size.clamp(16 << 10, MAX_RING).next_power_of_two();
    // A live context naming a released one's LRC would run on the dead
    // context's state and lose its own at the next reap. Keys are (Open::gen,
    // ctx id), neither ever reused, so this must never fire. Checked when the
    // request is QUEUED: at dequeue time (`lrc`, where boot 14's three
    // warnings came from) a released context's own earlier requests
    // legitimately still run on its LRC.
    if c.eng[eng].dead.contains(&key) {
        ilog!("{}: context {:?} submits on a RELEASED context's LRC (key reuse)", c.eng[eng].d.name, key);
    }
    c.eng[eng].queue.push_back(Req { key, seq, pml4, ring_size, pre: Vec::new(), batch: Some((batch, true)) });
    c.pump(eng);
    Ok(())
}

/// A context is gone: free its LRCs once their requests are done.
pub(super) fn release(pred: impl Fn(Key) -> bool) {
    let mut g = CORE.lock();
    let Some(c) = g.as_mut() else { return };
    for e in 0..NENG {
        let keys: Vec<Key> = c.eng[e].lrcs.keys().copied().filter(|&k| k != KERNEL && pred(k)).collect();
        for k in keys { if !c.eng[e].dead.contains(&k) { c.eng[e].dead.push(k); } }
        c.reap(e);
    }
}

fn poll_all(out: &mut Vec<Retired>) -> bool {
    let mut g = CORE.lock();
    let Some(c) = g.as_mut() else { return false };
    for e in 0..NENG { c.poll(e, out); }
    c.eng.iter().any(|e| e.flight.is_some() || !e.queue.is_empty())
}

/// The poll period while work is queued or in flight (`i915.poll_us`).
static POLL_BUSY_NS: AtomicUsize = AtomicUsize::new(200_000);

fn poll_thread(retire: fn(&[Retired])) {
    POLL_BUSY_NS.store(opt("i915.poll_us").unwrap_or(200).clamp(20, 5_000) as usize * 1000, Ordering::Relaxed);
    let mut out = Vec::new();
    loop {
        out.clear();
        let busy = poll_all(&mut out);
        if !out.is_empty() { retire(&out); }
        // Busy: 200 us (one-shot timer), not msleep(1), which can sleep a
        // whole tick and left a ~5 ms bubble between requests on one engine.
        if busy { sched::kthread::sleep_ns(POLL_BUSY_NS.load(Ordering::Relaxed) as u64) } else { sched::kthread::msleep(5) }
    }
}

// ── bring-up ─────────────────────────────────────────────────────────────────

/// Run one kernel request synchronously (bring-up only).
fn run_kernel(c: &mut Core, e: usize, pre: Vec<u32>, batch: Option<(u64, bool)>, pml4: u64, ms: u64) -> bool {
    let hwsp = c.eng[e].hwsp;
    hwsp.wr(hw::HWS_SEQNO_INDEX, 0);
    c.eng[e].queue.push_back(Req { key: KERNEL, seq: 1, pml4, ring_size: 16 << 10, pre, batch });
    c.pump(e);
    let t0 = now_ns();
    let mut sink = Vec::new();
    while c.eng[e].flight.is_some() || !c.eng[e].queue.is_empty() {
        c.poll(e, &mut sink);
        if now_ns().wrapping_sub(t0) > ms * 1_000_000 * 2 { break; }
        delay_us(100);
    }
    let ok = c.eng[e].flight.is_none() && hwsp.rd(hw::HWS_SEQNO_INDEX) == 1;
    if c.eng[e].flight.is_some() { c.fail_flight(e, true); }
    hwsp.wr(hw::HWS_SEQNO_INDEX, 0);
    ok
}

/// Bring the GT up for user execution and record the golden contexts.
/// `retire` is called from the poll thread with completed requests.
pub(super) fn init(d: &Device, sseu: &hw::Sseu, retire: fn(&[Retired])) -> bool {
    let t0 = now_ns();
    let g = match d.gsm { Some(g) => g, None => { log("exec: no GGTT mapping"); return false; } };
    if d.gmch.ggtt_size < 2 << 30 { log("exec: GGTT smaller than 2 GiB; not supported"); return false; }
    let m = d.mmio;
    let rev = if d.fake { 7 } else { d.func.read8(0x08) };
    let step = wa::kbl_step(rev);
    ilog!("=== G3b exec bring-up: KBL rev {:02x} (step ordinal {}) ===", rev, step);
    if !d.fake && d.func.command() & crate::pci::CMD_BUS_MASTER == 0 { d.func.set_bus_master(true); }
    if let Err(dom) = forcewake_get(&m) { ilog!("exec: forcewake domain '{}' did not ack", dom); return false; }
    if opt("i915.reset") != Some(0) { gt::full_gt_reset(&m); }
    let pat = hw::gen9_private_ppat();
    m.w(reg::GEN8_PRIVATE_PAT_LO, pat as u32);
    m.w(reg::GEN8_PRIVATE_PAT_HI, (pat >> 32) as u32);
    // GT workarounds (steer MCR to the first enabled slice/subslice).
    let (sl, ss) = (sseu.slice_mask.trailing_zeros(), sseu.subslice_mask.trailing_zeros());
    let gtw = wa::kbl_gt_was(step, sl, ss);
    let mut lost = 0;
    for w in &gtw.0 {
        let old = if w.needs_read() { m.r(w.reg) } else { 0 };
        m.w(w.reg, w.value(old));
        if !w.holds(m.r(w.reg)) { lost += 1; }
    }
    for &(r, v) in wa::GTT_WAS.iter() { m.w(r, v); }
    ilog!("PAT 0x{:016x}, {} GT workarounds ({} did not read back), MOCS/L3CC + rcs0 workarounds/whitelist next", pat, gtw.0.len(), lost);

    let slot_base = (d.gmch.ggtt_size - (16 << 20) - NSLOTS as u64 * SLOT_SIZE) & !(SLOT_SIZE - 1);
    let mut c = Core {
        m, g, slot_base, slots: alloc::vec![false; NSLOTS], eng: Vec::new(),
        budget_ns: opt("i915.batch_ms").unwrap_or(20_000).clamp(100, 600_000) * 1_000_000,
        stall_ns: opt("i915.hang_ms").unwrap_or(3_000).clamp(50, 60_000) * 1_000_000,
        resets: 0, indirect_ctx: 0,
    };
    // Slots 0..NENG hold the engines' status pages (one page each).
    for (i, desc) in DESCS.iter().enumerate() {
        c.slots[i] = true;
        let hwsp = match alloc_bo(4096, slot_base + i as u64 * SLOT_SIZE, &g) {
            Some(b) => b,
            None => { log("exec: out of memory"); forcewake_put(&m); return false; }
        };
        c.eng.push(Eng { d: desc, hwsp, csb_head: hw::CSB_ENTRIES - 1, queue: VecDeque::new(), flight: None,
                         lrcs: BTreeMap::new(), dead: Vec::new(), default: None, failed: Vec::new() });
    }
    m.w(reg::GFX_FLSH_CNTL_GEN6, 1);
    for e in 0..NENG {
        c.engine_regs(e);
        let hwsp = c.eng[e].hwsp;
        let (ok, head) = enable_execlists(&m, DESCS[e], &hwsp);
        c.eng[e].csb_head = head;
        if !ok { ilog!("exec: {}: run-list enable did not stick", DESCS[e].name); forcewake_put(&m); return false; }
    }
    // An empty VM (every address leads to the scratch page) for the kernel
    // contexts: their batches run from the GGTT.
    let kpt = match Ppgtt::new(&mut KStore) { Ok(p) => p, Err(_) => { log("exec: out of memory"); forcewake_put(&m); return false; } };
    // The null render state batch, in its own slot.
    let rs_slot = c.alloc_slot().expect("slot");
    let rs_ggtt = slot_base + rs_slot as u64 * SLOT_SIZE;
    let (rs, rs_len) = wa::renderstate_batch(rs_ggtt).expect("renderstate relocations");
    let rs_bo = match alloc_bo(rs.len() as u64 * 4, rs_ggtt, &g) { Some(b) => b, None => { forcewake_put(&m); return false; } };
    for (i, &v) in rs.iter().enumerate() { rs_bo.wr(i, v); }
    // The gen9 indirect context workaround batch (Linux `lrc_init_wa_ctx`),
    // 64 KiB into the render state slot, its scratch one page further.
    // `i915.ctxfix=0`: no batch, images as before lane gpuctx.
    if opt("i915.ctxfix") != Some(0) {
        let bb_ggtt = rs_ggtt + 0x10000;
        let wa_bb = wa::gen9_indirect_ctx_bb((bb_ggtt + 4096) as u32);
        match alloc_bo(8192, bb_ggtt, &g) {
            Some(b) => {
                for (i, &v) in wa_bb.iter().enumerate() { b.wr(i, v); }
                c.indirect_ctx = bb_ggtt as u32 | (wa_bb.len() as u32 * 4 / 64);
                ilog!("rcs0: indirect context workaround batch, {} dwords at GGTT 0x{:x} (i915.ctxfix=0 disables)", wa_bb.len(), bb_ggtt);
                core::mem::forget(b);
            }
            None => log("exec: no memory for the indirect context workaround batch; continuing without"),
        }
    }
    fence(Ordering::SeqCst);
    m.w(reg::GFX_FLSH_CNTL_GEN6, 1);
    let ctx_was = wa::kbl_ctx_was(step);
    let budget = opt("i915.timeout_ms").unwrap_or(500).clamp(10, 10_000);
    for e in 0..NENG {
        let (pre, batch) = if DESCS[e].class == EngineClass::Render {
            let mut pre = Vec::new();
            let scratch = (c.eng[e].hwsp.ggtt + HWS_SCRATCH as u64 * 4) as u32;
            hw::cmd::rcs_barrier(&mut pre, scratch);
            pre.extend(wa::ctx_wa_lri(&ctx_was));
            (pre, Some((rs_ggtt, false)))
        } else {
            (Vec::new(), None)
        };
        let t = now_ns();
        if !run_kernel(&mut c, e, pre, batch, kpt.pml4, budget) {
            ilog!("exec: {}: the golden-context request did not complete; hardware execution stays off", DESCS[e].name);
            forcewake_put(&m);
            return false;
        }
        let l = c.eng[e].lrcs.get(&KERNEL).expect("kernel lrc");
        let n = hw::context_size(DESCS[e].class) / 4;
        let img: Vec<u32> = (0..n).map(|i| l.ctxo.rd(i)).collect();
        let rd = |i: usize| img[ctx::LRC_STATE_PN * 1024 + i];
        ilog!("{}: golden context recorded in {} us ({}{} dwords; saved RING_HEAD=0x{:x} TAIL=0x{:x} CTX_CONTROL=0x{:08x})",
            DESCS[e].name, now_ns().wrapping_sub(t) / 1000,
            if batch.is_some() { alloc::format!("{} ctx workarounds + null state {} bytes, ", ctx_was.0.len(), rs_len) } else { alloc::string::String::new() },
            n, rd(ctx::RING_HEAD), rd(ctx::RING_TAIL), rd(ctx::CONTEXT_CONTROL));
        c.eng[e].default = Some(img);
    }
    for e in 0..NENG { HW_SEQ_VA[e].store(c.eng[e].hwsp.va + hw::HWS_SEQNO_INDEX * 4, Ordering::Release); }
    *CORE.lock() = Some(c);
    // Kernel VM, render state and kernel contexts stay allocated (about 200 KiB).
    core::mem::forget(kpt);
    READY.store(true, Ordering::Release);
    if sched::kthread::spawn("i915-exec", move || poll_thread(retire)).is_none() {
        READY.store(false, Ordering::Release);
        log("exec: could not start the poll thread; hardware execution stays off");
        return false;
    }
    ilog!("=== G3b exec ready in {} ms: rcs0 + bcs0, execlists, golden contexts recorded, forcewake held ===",
        now_ns().wrapping_sub(t0) / 1_000_000);
    true
}
