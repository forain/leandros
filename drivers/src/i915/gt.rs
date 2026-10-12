//! G1/G2: GT bring-up on the blitter engine (bcs0) and in-kernel self-tests.
//!
//! Runs only on request (`i915.selftest=store|blt|fb`, or `i915.start=1`).
//! Sequence, mirroring Linux i915 on gen9 with execlist submission (no GuC):
//!
//!  1. forcewake (all domains) for the whole run;
//!  2. full GT reset (`i915.reset=0` skips it): ready-to-reset handshake on
//!     every engine, GEN6_GDRST = GRDOM_FULL twice, cancel — the display is
//!     not part of the GT and keeps scanning out;
//!  3. private PAT as Linux programs it;
//!  4. kernel objects in system RAM (status page, context image, ring,
//!     scratch), bound into the top 16 MiB of the GGTT, far from the GOP
//!     framebuffer, and mirrored at the same addresses in a 4-level PPGTT
//!     whose unused entries all lead to a scratch page; the framebuffer's
//!     stolen pages are mirrored into the PPGTT too;
//!  5. execlists on bcs0: HWSTAM, RING_MODE run-list enable, MI_MODE
//!     un-stop, HWS_PGA, error registers cleared, CSB pointers reset;
//!  6. one logical ring context (restore-inhibited first image), submitted
//!     through the ELSP; every request ends with the gen8 xcs breadcrumb
//!     (MI_FLUSH_DW post-sync write of a seqno into the status page);
//!  7. a hang detector polls the breadcrumb, ACTHD/HEAD and EIR: a stall or
//!     an error dumps the engine state, resets the engine (RESET_CTL
//!     handshake + GDRST BLT) and stops the self-test, so a bad batch costs
//!     at most `i915.timeout_ms` of boot time.
//!
//! Self-tests (each level includes the previous ones):
//!   store  MI_STORE_DWORD_IMM (GGTT) into the status page, CPU checks it;
//!   blt    XY_COLOR_BLT of a 64x64 square into a 128x128 scratch surface,
//!          CPU checks the square and the untouched border;
//!   fb     XY_COLOR_BLT colour bars into the top band of the visible GOP
//!          framebuffer (stolen memory), read back through the aperture.
//!
//! The kernel objects stay allocated and bound afterwards (about 100 KiB):
//! the engine is left idle with execlists enabled and its status page
//! pointing at them.

use alloc::vec::Vec;
use core::sync::atomic::{fence, Ordering};

use super::hw::{self, cmd, ctx, reg, Cache, EngineClass, HangCheck, HangVerdict, Ppgtt, PtStore};
use super::{delay_us, forcewake_get, forcewake_put, fmt_size, log, now_ns, opt, BootEnv, Device, Gsm, Mmio};
use super::{idump, ilog};

// ── memory ───────────────────────────────────────────────────────────────────

const LOW4G: usize = 1 << 32;

/// Page-table pages from the buddy allocator, accessed through the HHDM.
pub(super) struct KStore;

impl PtStore for KStore {
    fn alloc_page(&mut self) -> Option<u64> {
        let p = mm::buddy::alloc_below(0, LOW4G)?;
        unsafe { core::ptr::write_bytes(mm::phys_to_virt(p) as *mut u8, 0, 4096); }
        Some(p as u64)
    }
    fn free_page(&mut self, phys: u64) { mm::buddy::free(phys as usize, 0); }
    fn read(&self, t: u64, i: usize) -> u64 {
        unsafe { core::ptr::read_volatile((mm::phys_to_virt(t as usize) + i * 8) as *const u64) }
    }
    fn write(&mut self, t: u64, i: usize, v: u64) {
        unsafe { core::ptr::write_volatile((mm::phys_to_virt(t as usize) + i * 8) as *mut u64, v) }
    }
    /// Gen9 big-core parts share the LLC with the CPU: a store fence is all
    /// it takes for the GPU's page walker to see the entries.
    fn flush(&mut self) { fence(Ordering::SeqCst); }
}

/// A physically contiguous, zeroed buffer object bound into the GGTT.
#[derive(Clone, Copy)]
struct Bo { phys: u64, size: u64, ggtt: u64, va: usize }

impl Bo {
    fn rd(&self, dw: usize) -> u32 { unsafe { core::ptr::read_volatile((self.va + dw * 4) as *const u32) } }
    fn wr(&self, dw: usize, v: u32) { unsafe { core::ptr::write_volatile((self.va + dw * 4) as *mut u32, v) } }
}

fn order_for(size: u64) -> usize {
    let pages = ((size + 4095) / 4096) as usize;
    pages.next_power_of_two().trailing_zeros() as usize
}

fn alloc_bo(size: u64, ggtt: u64, g: &Gsm) -> Option<Bo> {
    let order = order_for(size);
    let phys = mm::buddy::alloc_below(order, LOW4G)? as u64;
    let va = mm::phys_to_virt(phys as usize);
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096 << order); }
    for i in 0..(size + 4095) / 4096 {
        g.write(ggtt / 4096 + i, hw::ggtt_pte(phys + i * 4096));
    }
    Some(Bo { phys, size, ggtt, va })
}

// ── reset ────────────────────────────────────────────────────────────────────

/// `gen8_engine_reset_prepare`: ask the engine to reach a reset-safe point.
fn engine_reset_prepare(m: &Mmio, base: u32) -> Result<(), u32> {
    let r = base + reg::RING_RESET_CTL;
    let ack = m.r(r);
    let (request, mask, want) = if ack & reg::RESET_CTL_CAT_ERROR != 0 {
        (reg::RESET_CTL_CAT_ERROR, reg::RESET_CTL_CAT_ERROR, 0)
    } else if ack & reg::RESET_CTL_READY_TO_RESET == 0 {
        (reg::RESET_CTL_REQUEST_RESET, reg::RESET_CTL_READY_TO_RESET, reg::RESET_CTL_READY_TO_RESET)
    } else {
        return Ok(());
    };
    m.w(r, hw::masked_en(request));
    m.wait(r, mask, want, 700).map(|_| ())
}

fn engine_reset_cancel(m: &Mmio, base: u32) {
    m.w(base + reg::RING_RESET_CTL, hw::masked_dis(reg::RESET_CTL_REQUEST_RESET));
}

/// `gen6_hw_domain_reset`: write the domain mask to GDRST and wait for the
/// hardware to clear it, twice (Linux repeats it on everything before MTL).
fn hw_domain_reset(m: &Mmio, mask: u32) -> Result<u64, u32> {
    let mut total = 0;
    for _ in 0..2 {
        m.w(reg::GEN6_GDRST, mask);
        total += m.wait(reg::GEN6_GDRST, mask, 0, 2000)?;
    }
    Ok(total)
}

fn full_gt_reset(m: &Mmio) {
    let mut unready = Vec::new();
    for e in hw::GEN9_ENGINES.iter() {
        if let Err(v) = engine_reset_prepare(m, e.mmio_base) { unready.push((e.name, v)); }
    }
    let r = hw_domain_reset(m, reg::GRDOM_FULL);
    for e in hw::GEN9_ENGINES.iter() { engine_reset_cancel(m, e.mmio_base); }
    match r {
        Ok(us) => ilog!("GT reset (GRDOM_FULL): done in {} us{}", us,
            if unready.is_empty() { alloc::string::String::new() } else { alloc::format!("; engines not ready beforehand: {:?}", unready) }),
        Err(v) => ilog!("GT reset (GRDOM_FULL): GDRST did not clear (0x{:08x}); continuing", v),
    }
}

/// Reset one engine after a hang.
fn reset_engine(m: &Mmio, e: &hw::EngineDesc) {
    let prep = engine_reset_prepare(m, e.mmio_base);
    let r = hw_domain_reset(m, e.reset_domain);
    engine_reset_cancel(m, e.mmio_base);
    ilog!("{} reset: prepare {:?}, GDRST {:?}", e.name, prep.map_err(|v| alloc::format!("not ready (RESET_CTL=0x{:08x})", v)),
        r.map_err(|v| alloc::format!("stuck (0x{:08x})", v)));
}

// ── the engine ───────────────────────────────────────────────────────────────

const RING_SIZE: u32 = 16 * 1024;
/// Status-page dword the store self-test writes.
const HWS_TEST_INDEX: usize = 0x80;
const STORE_MAGIC: u32 = 0xC0FF_EE01;
/// Status-page dword the TLB-invalidate flush writes into (0x34 * 4 = 0xd0:
/// 8-byte aligned, bit 5 clear as MI_FLUSH_DW requires).
const HWS_FLUSH_SCRATCH: usize = 0x34;
const SCRATCH_W: u32 = 128;
const SCRATCH_H: u32 = 128;
const SENTINEL: u32 = 0x5A5A_5A5A;
const CTX_HW_ID: u32 = 1;

struct Engine<'a> {
    m: &'a Mmio,
    e: &'static hw::EngineDesc,
    hwsp: Bo,
    ctxo: Bo,
    ring: Bo,
    tail: u32,
    seqno: u32,
    csb_head: u32,
    first: bool,
}

impl<'a> Engine<'a> {
    fn r(&self, off: u32) -> u32 { self.m.r(self.e.mmio_base + off) }
    fn w(&self, off: u32, v: u32) { self.m.w(self.e.mmio_base + off, v) }

    /// `enable_execlists` + `reset_csb_pointers`.
    fn enable(&mut self) -> bool {
        self.w(reg::RING_HWSTAM, 0xFFFF_FFFF);
        self.w(reg::RING_IMR, 0xFFFF_FFFF);
        self.w(reg::RING_MODE_GEN7, hw::masked_en(reg::GFX_RUN_LIST_ENABLE));
        self.w(reg::RING_MI_MODE, hw::masked_dis(reg::STOP_RING));
        self.w(reg::RING_HWS_PGA, self.hwsp.ggtt as u32);
        let _ = self.r(reg::RING_HWS_PGA);
        self.w(reg::RING_EMR, 0xFFFF_FFFF);
        self.w(reg::RING_EIR, 0xFFFF_FFFF);
        let esr = self.r(reg::RING_ESR);
        // CSB: mirror filled with -1 so a stale entry is recognisable, our
        // read pointer and the mirrored write pointer at the last entry.
        for i in 0..2 * hw::CSB_ENTRIES as usize { self.hwsp.wr(hw::HWS_CSB_BUF0_INDEX + i, 0xFFFF_FFFF); }
        self.hwsp.wr(hw::HWS_CSB_WRITE_INDEX, hw::CSB_ENTRIES - 1);
        fence(Ordering::SeqCst);
        self.w(reg::RING_CONTEXT_STATUS_PTR, hw::csb_ptr_reset());
        let _ = self.r(reg::RING_CONTEXT_STATUS_PTR);
        self.csb_head = hw::CSB_ENTRIES - 1;
        let mode = self.r(reg::RING_MODE_GEN7);
        let mi = self.r(reg::RING_MI_MODE);
        ilog!("{}: execlists enabled: MODE=0x{:08x} (run-list={}) MI_MODE=0x{:08x} HWS_PGA=0x{:08x} ESR=0x{:08x} CSB_PTR=0x{:08x}",
            self.e.name, mode, (mode >> 15) & 1, mi, self.r(reg::RING_HWS_PGA), esr, self.r(reg::RING_CONTEXT_STATUS_PTR));
        mode & reg::GFX_RUN_LIST_ENABLE != 0
    }

    fn regs_dw(&self, i: usize) -> usize { ctx::LRC_STATE_PN * 1024 + i }

    /// Copy `cs` into the ring at the tail, point the context image's
    /// RING_TAIL at the new end and submit the context through the ELSP.
    fn submit(&mut self, cs: &[u32]) -> bool {
        let bytes = cs.len() as u32 * 4;
        if self.tail + bytes > RING_SIZE { log("ring full (self-test ring never wraps)"); return false; }
        for (i, &d) in cs.iter().enumerate() { self.ring.wr((self.tail / 4) as usize + i, d); }
        self.tail += bytes;
        self.ctxo.wr(self.regs_dw(ctx::RING_TAIL), self.tail);
        fence(Ordering::SeqCst);
        let desc = hw::ctx_descriptor(self.ctxo.ggtt as u32, CTX_HW_ID + self.e.uapi_class as u32, true, self.first);
        self.first = false;
        for w in hw::elsp_writes(desc, 0) { self.w(reg::RING_ELSP, w); }
        true
    }

    /// One request: `body`, then the breadcrumb and the request tail.
    fn request(&mut self, body: &[u32]) -> (bool, u32) {
        self.seqno += 1;
        // Invalidate the TLBs first (Linux's EMIT_INVALIDATE flush): the
        // post-sync write goes to a spare status-page dword.
        let mut cs: Vec<u32> = Vec::with_capacity(body.len() + 32);
        let scratch = (self.hwsp.ggtt + HWS_FLUSH_SCRATCH as u64 * 4) as u32;
        let seq_addr = (self.hwsp.ggtt + hw::HWS_SEQNO_INDEX as u64 * 4) as u32;
        if self.e.class == EngineClass::Render {
            cmd::rcs_invalidate(&mut cs, scratch);
            cs.extend_from_slice(body);
            cmd::rcs_breadcrumb(&mut cs, seq_addr, self.seqno);
        } else {
            cmd::flush_dw_store_ggtt(&mut cs, scratch, 0, true);
            cs.extend_from_slice(body);
            cmd::flush_dw_store_ggtt(&mut cs, seq_addr, self.seqno, false);
        }
        cmd::fini_tail(&mut cs);
        cmd::pad_qword(&mut cs);
        (self.submit(&cs), self.seqno)
    }

    fn wait(&self, seqno: u32, budget_ms: u64) -> (HangVerdict, u64) {
        let t0 = now_ns();
        let mut hc = HangCheck::new(t0, budget_ms * 1_000_000, (budget_ms / 2).max(50) * 1_000_000);
        loop {
            let done = self.hwsp.rd(hw::HWS_SEQNO_INDEX).wrapping_sub(seqno) as i32 >= 0;
            let acthd = (self.r(reg::RING_ACTHD_UDW) as u64) << 32 | self.r(reg::RING_ACTHD) as u64;
            let v = hc.sample(now_ns(), done, acthd, self.r(reg::RING_HEAD), self.r(reg::RING_EIR));
            if v != HangVerdict::Running { return (v, now_ns().wrapping_sub(t0) / 1000); }
            delay_us(10);
        }
    }

    /// Log the CSB events since the last call (HWSP mirror and MMIO copy).
    fn drain_csb(&mut self) -> u32 {
        let ptr = self.r(reg::RING_CONTEXT_STATUS_PTR);
        let wr_mmio = ptr & 7;
        let wr_hwsp = self.hwsp.rd(hw::HWS_CSB_WRITE_INDEX) & 7;
        let mut seen = 0u32;
        for i in hw::csb_pending(self.csb_head, wr_hwsp) {
            let lo = self.hwsp.rd(hw::HWS_CSB_BUF0_INDEX + 2 * i as usize);
            let hi = self.hwsp.rd(hw::HWS_CSB_BUF0_INDEX + 2 * i as usize + 1);
            let mlo = self.r(reg::RING_CONTEXT_STATUS_BUF + 8 * i);
            ilog!("{}: CSB[{}] status=0x{:08x} ({}) ctx=0x{:08x}{}", self.e.name, i, lo, hw::csb_status_str(lo), hi,
                if mlo != lo { alloc::format!(" (mmio copy 0x{:08x})", mlo) } else { alloc::string::String::new() });
            seen |= lo;
        }
        if wr_hwsp != wr_mmio {
            ilog!("{}: CSB write pointer HWSP={} MMIO={} (CSB_PTR=0x{:08x})", self.e.name, wr_hwsp, wr_mmio, ptr);
        }
        self.csb_head = wr_hwsp;
        // Tell the hardware how far we have read (masked READ_PTR field).
        self.w(reg::RING_CONTEXT_STATUS_PTR, (0x700 << 16) | (wr_hwsp << 8));
        seen
    }

    /// Wait (bounded) for the context to be switched out after its last request.
    fn wait_idle(&mut self, ms: u64) {
        let t0 = now_ns();
        let mut seen = 0;
        while now_ns().wrapping_sub(t0) < ms * 1_000_000 {
            seen |= self.drain_csb();
            if seen & hw::CSB_ACTIVE_IDLE != 0 { break; }
            delay_us(200);
        }
        if seen & hw::CSB_ACTIVE_IDLE == 0 {
            ilog!("{}: no active->idle event within {} ms (EXECLIST_STATUS=0x{:08x}_{:08x})", self.e.name, ms,
                self.r(reg::RING_EXECLIST_STATUS_HI), self.r(reg::RING_EXECLIST_STATUS_LO));
        }
    }

    fn dump_state(&self, why: &str) {
        ilog!("{}: {}: HEAD=0x{:08x} TAIL=0x{:08x} START=0x{:08x} CTL=0x{:08x} ACTHD=0x{:08x}_{:08x} IPEHR=0x{:08x} INSTDONE=0x{:08x} EIR=0x{:08x} ESR=0x{:08x} EXECLIST_STATUS=0x{:08x}_{:08x} CSB_PTR=0x{:08x} BBADDR=0x{:08x} seqno(hwsp)=0x{:08x}",
            self.e.name, why, self.r(reg::RING_HEAD), self.r(reg::RING_TAIL), self.r(reg::RING_START), self.r(reg::RING_CTL),
            self.r(reg::RING_ACTHD_UDW), self.r(reg::RING_ACTHD), self.r(reg::RING_IPEHR), self.r(reg::RING_INSTDONE),
            self.r(reg::RING_EIR), self.r(reg::RING_ESR), self.r(reg::RING_EXECLIST_STATUS_HI), self.r(reg::RING_EXECLIST_STATUS_LO),
            self.r(reg::RING_CONTEXT_STATUS_PTR), self.r(reg::RING_BBADDR), self.hwsp.rd(hw::HWS_SEQNO_INDEX));
        let m = self.m;
        ilog!("faults: RING_FAULT=0x{:08x} FAULT_TLB_DATA=0x{:08x}/0x{:08x} ERROR_GEN6=0x{:08x} GDRST=0x{:08x}",
            m.r(reg::GEN8_RING_FAULT_REG), m.r(reg::GEN8_FAULT_TLB_DATA0), m.r(reg::GEN8_FAULT_TLB_DATA1),
            m.r(reg::ERROR_GEN6), m.r(reg::GEN6_GDRST));
        let mut s = alloc::string::String::new();
        for i in 0..(self.tail / 4).min(64) as usize { s.push_str(&alloc::format!(" {:08x}", self.ring.rd(i))); }
        idump!("{}: ring[0..]:{}", self.e.name, s);
        let mut c = alloc::string::String::new();
        for i in 0..0x60usize { c.push_str(&alloc::format!(" {:08x}", self.ctxo.rd(self.regs_dw(i)))); }
        idump!("{}: context regs[0..0x60]:{}", self.e.name, c);
    }

    /// Run one request and judge it. Resets the engine on a hang.
    fn run(&mut self, name: &str, body: &[u32], budget_ms: u64) -> bool {
        let (ok, seq) = self.request(body);
        if !ok { return false; }
        let (v, us) = self.wait(seq, budget_ms);
        match v {
            HangVerdict::Done => {
                ilog!("self-test {}: request {} completed in {} us", name, seq, us);
                self.wait_idle(50);
                true
            }
            HangVerdict::Hung | HangVerdict::Timeout => {
                ilog!("self-test {}: request {} {} after {} us", name, seq,
                    if v == HangVerdict::Hung { "HUNG (no progress or engine error)" } else { "TIMED OUT (still moving)" }, us);
                self.dump_state("hang");
                let _ = self.drain_csb();
                reset_engine(self.m, self.e);
                false
            }
            HangVerdict::Running => unreachable!(),
        }
    }
}

// ── entry ────────────────────────────────────────────────────────────────────

/// Which self-tests to run (`i915.selftest=` list).
#[derive(Clone, Copy, Default)]
pub(super) struct Tests { pub store: bool, pub blt: bool, pub fb: bool, pub rcs: bool }

pub(super) fn bringup_and_selftest(d: &Device, env: &BootEnv, t: Tests) {
    let t_start = now_ns();
    ilog!("=== i915 bring-up: self-tests store={} blt={} fb={} rcs={} ===", t.store as u8, t.blt as u8, t.fb as u8, t.rcs as u8);
    if d.platform.family.is_lp() || !d.platform.has_llc() {
        log("Gen9 LP (no LLC) parts are not supported by the bring-up; stopping");
        return;
    }
    let g = match d.gsm { Some(g) => g, None => { log("no GGTT mapping; stopping"); return; } };
    if d.gmch.ggtt_size < 512 << 20 {
        ilog!("GGTT is only {}; stopping", fmt_size(d.gmch.ggtt_size));
        return;
    }
    let m = &d.mmio;
    let c = if d.fake { crate::pci::CMD_BUS_MASTER } else { d.func.command() };
    if c & crate::pci::CMD_BUS_MASTER == 0 {
        log("PCI bus mastering was off; enabling it");
        d.func.set_bus_master(true);
    }
    match forcewake_get(m) {
        Ok(l) => ilog!("forcewake held (acks in {}/{}/{} us)", l[0], l[1], l[2]),
        Err(dom) => { ilog!("forcewake domain '{}' did not ack; stopping", dom); return; }
    }
    if opt("i915.reset") != Some(0) { full_gt_reset(m); }
    let pat = hw::gen9_private_ppat();
    m.w(reg::GEN8_PRIVATE_PAT_LO, pat as u32);
    m.w(reg::GEN8_PRIVATE_PAT_HI, (pat >> 32) as u32);
    ilog!("private PAT programmed: 0x{:08x}{:08x}", m.r(reg::GEN8_PRIVATE_PAT_HI), m.r(reg::GEN8_PRIVATE_PAT_LO));

    let ok = run_selftests(d, env, &g, t);
    forcewake_put(m);
    ilog!("=== i915 bring-up {} in {} ms (forcewake released; engines idle, execlists on) ===",
        if ok { "PASSED" } else { "FAILED" }, now_ns().wrapping_sub(t_start) / 1_000_000);
}

/// Allocate, bind (GGTT + PPGTT mirror) and initialise one engine's status
/// page, ring and context image in the GGTT slot at `slot`:
/// hwsp +0, ring +0x4000, context +0x10000.
fn setup_engine<'a>(m: &'a Mmio, e: &'static hw::EngineDesc, slot: u64, g: &Gsm,
                    pp: &mut Ppgtt, st: &mut KStore) -> Option<Engine<'a>> {
    let ctx_size = hw::context_size(e.class) as u64;
    let mut bos = [Bo { phys: 0, size: 0, ggtt: 0, va: 0 }; 3];
    for (k, (off, size)) in [(0u64, 4096u64), (0x4000, RING_SIZE as u64), (0x10000, ctx_size)].iter().enumerate() {
        bos[k] = match alloc_bo(*size, slot + off, g) {
            Some(b) => b,
            None => { log("out of memory for kernel objects"); return None; }
        };
        if let Err(err) = pp.map_range(st, bos[k].ggtt, bos[k].phys, (size + 4095) & !4095, Cache::Cached) {
            ilog!("PPGTT map: {:?}", err);
            return None;
        }
    }
    let (hwsp, ring, ctxo) = (bos[0], bos[1], bos[2]);
    for (n, b) in ["hwsp", "ring", "context"].iter().zip(bos.iter()) {
        ilog!("{} {}: GGTT 0x{:08x} -> phys 0x{:x} ({}); PTE readback 0x{:016x}", e.name, n, b.ggtt, b.phys,
            fmt_size(b.size), g.read(b.ggtt / 4096));
    }
    // Context image: PPHWSP zero, register state restore-inhibited.
    let mut regs = alloc::vec![0u32; 1024];
    hw::lrc_init_regs(&mut regs, &hw::LrcInit {
        class: e.class, mmio_base: e.mmio_base, ring_ggtt: ring.ggtt as u32, ring_size: RING_SIZE,
        head: 0, tail: 0, pml4: pp.pml4, inhibit: true, rpcs: 0,
    });
    for (i, &v) in regs.iter().enumerate() { ctxo.wr(ctx::LRC_STATE_PN * 1024 + i, v); }
    fence(Ordering::SeqCst);
    Some(Engine { m, e, hwsp, ctxo, ring, tail: 0, seqno: 0, csb_head: hw::CSB_ENTRIES - 1, first: true })
}

/// Bring an engine up and run the store test on it.
fn store_test(eng: &mut Engine, budget: u64) -> bool {
    let idle0 = eng.r(reg::RING_MI_MODE);
    if idle0 & reg::MODE_IDLE == 0 {
        ilog!("{}: MI_MODE=0x{:08x} reports not idle before enabling execlists (continuing)", eng.e.name, idle0);
    }
    if !eng.enable() {
        ilog!("{}: RING_MODE did not take the run-list enable; stopping", eng.e.name);
        return false;
    }
    let mut cs = Vec::new();
    cmd::store_dword_ggtt(&mut cs, eng.hwsp.ggtt + HWS_TEST_INDEX as u64 * 4, STORE_MAGIC);
    let name = alloc::format!("store/{}", eng.e.name);
    if !eng.run(&name, &cs, budget) { return false; }
    let got = eng.hwsp.rd(HWS_TEST_INDEX);
    ilog!("self-test {}: status page dword 0x{:x} = 0x{:08x} (want 0x{:08x}) {}", name, HWS_TEST_INDEX, got, STORE_MAGIC,
        if got == STORE_MAGIC { "PASS" } else { "FAIL" });
    let rd = |i: usize| eng.ctxo.rd(ctx::LRC_STATE_PN * 1024 + i);
    ilog!("{}: context image saved by the GPU: RING_HEAD=0x{:x} RING_TAIL=0x{:x} CTX_TIMESTAMP=0x{:x} CONTEXT_CONTROL=0x{:08x}",
        eng.e.name, rd(ctx::RING_HEAD), rd(ctx::RING_TAIL), rd(ctx::TIMESTAMP), rd(ctx::CONTEXT_CONTROL));
    got == STORE_MAGIC
}

fn run_selftests(d: &Device, env: &BootEnv, g: &Gsm, t: Tests) -> bool {
    let m = &d.mmio;
    let budget = opt("i915.timeout_ms").unwrap_or(500).clamp(10, 10_000);

    // Kernel objects at the top 16 MiB of the GGTT:
    //   +0x000000 bcs0 slot, +0x080000 blt scratch, +0x100000 rcs0 slot.
    let base = opt("i915.ggttbase").unwrap_or(d.gmch.ggtt_size - (16 << 20)) & !0xFFFF;
    if base < d.gmadr_size || base + (16 << 20) > d.gmch.ggtt_size {
        ilog!("GGTT window 0x{:x} overlaps the aperture or runs past the GGTT; stopping", base);
        return false;
    }
    let busy = (0..4096u64).filter(|i| g.read(base / 4096 + i) & 1 != 0).count();
    ilog!("GGTT window for kernel objects: [0x{:x}, +16 MiB) ({} of 4096 PTEs were present before)", base, busy);

    let mut st = KStore;
    let mut pp = match Ppgtt::new(&mut st) {
        Ok(p) => p,
        Err(e) => { ilog!("PPGTT: {:?}; stopping", e); return false; }
    };
    let mut bcs = match setup_engine(m, &hw::GEN9_ENGINES[1], base, g, &mut pp, &mut st) { Some(e) => e, None => return false };
    let scratch = match alloc_bo((SCRATCH_W * SCRATCH_H * 4) as u64, base + 0x80000, g) {
        Some(b) => b,
        None => { log("out of memory for the scratch surface"); return false; }
    };
    if pp.map_range(&mut st, scratch.ggtt, scratch.phys, scratch.size, Cache::Cached).is_err() { return false; }
    let mut rcs = if t.rcs {
        match setup_engine(m, &hw::GEN9_ENGINES[0], base + 0x100000, g, &mut pp, &mut st) { Some(e) => Some(e), None => return false }
    } else { None };
    // The framebuffer's stolen pages, mirrored at their GGTT addresses.
    let fb_pages = ((env.fb_pitch as u64 * env.fb_height as u64) + 4095) / 4096;
    let mut fb_mirrored = false;
    if let Some(off) = d.fb_ggtt {
        let mut ok = true;
        for i in 0..fb_pages {
            match hw::ggtt_pte_phys(g.read(off / 4096 + i)) {
                // Uncached (PPAT 3), as Linux maps scanout on parts without
                // eLLC: the display engine does not snoop the LLC.
                Some(ph) => if pp.map(&mut st, off + i * 4096, ph, Cache::Uncached, false).is_err() { ok = false; break; },
                None => { ok = false; break; }
            }
        }
        st.flush();
        fb_mirrored = ok;
    }
    let _ = g.read(base / 4096);
    m.w(reg::GFX_FLSH_CNTL_GEN6, 1);
    ilog!("PPGTT: PML4 at 0x{:x}, {} table pages, scratch page 0x{:x}; framebuffer mirrored={}", pp.pml4,
        pp.table_pages() + 5, pp.scratch[0], fb_mirrored as u8);

    let mut ok = true;
    // bcs0 tests stop at the first failure; rcs0 still runs afterwards (a
    // separate engine and context).
    if t.store || t.blt || t.fb {
        ok = store_test(&mut bcs, budget)
            && (!(t.blt || t.fb) || blt_test(&mut bcs, &scratch, budget))
            && (!t.fb || fb_test(d, env, &mut bcs, fb_mirrored, budget));
    }
    if let Some(r) = rcs.as_mut() {
        ok &= store_test(r, budget);
    }
    ok
}

/// XY_COLOR_BLT a 64x64 square at (16,16) of the 128x128 scratch surface.
fn blt_test(eng: &mut Engine, scratch: &Bo, budget: u64) -> bool {
    for i in 0..(SCRATCH_W * SCRATCH_H) as usize { scratch.wr(i, SENTINEL); }
    fence(Ordering::SeqCst);
    let color = 0x00C0_FFEE;
    let mut cs = Vec::new();
    cmd::xy_color_blt(&mut cs, scratch.ggtt, SCRATCH_W * 4, false, 16, 16, 80, 80, color);
    if !eng.run("blt", &cs, budget) { return false; }
    let (mut inside_bad, mut outside_bad) = (0u32, 0u32);
    for y in 0..SCRATCH_H {
        for x in 0..SCRATCH_W {
            let v = scratch.rd((y * SCRATCH_W + x) as usize);
            let inside = (16..80).contains(&x) && (16..80).contains(&y);
            if inside && v != color { inside_bad += 1; }
            if !inside && v != SENTINEL { outside_bad += 1; }
        }
    }
    ilog!("self-test blt: square {} of 4096 pixels wrong, border {} of {} pixels touched (pixel(16,16)=0x{:08x}, pixel(0,0)=0x{:08x}) {}",
        inside_bad, outside_bad, SCRATCH_W * SCRATCH_H - 4096, scratch.rd((16 * SCRATCH_W + 16) as usize), scratch.rd(0),
        if inside_bad == 0 && outside_bad == 0 { "PASS" } else { "FAIL" });
    inside_bad == 0 && outside_bad == 0
}

/// Colour bars into the top band of the visible framebuffer.
fn fb_test(d: &Device, env: &BootEnv, eng: &mut Engine, fb_mirrored: bool, budget: u64) -> bool {
    let m = &d.mmio;
    // ── fb: colour bars into the visible framebuffer ──
    let off = match d.fb_ggtt {
        Some(o) if fb_mirrored => o,
        _ => { log("self-test fb: framebuffer not in the aperture or not mirrored; skipped"); return true; }
    };
    // The plane that scans out the boot framebuffer (primary plane of any pipe).
    let plane = (0..3u32).map(|p| (p, m.r(reg::plane(reg::PLANE_CTL_1_A, p, 0)), m.r(reg::plane(reg::PLANE_SURF_1_A, p, 0)),
                                   m.r(reg::plane(reg::PLANE_STRIDE_1_A, p, 0))))
        .find(|&(_, ctl, surf, _)| ctl >> 31 != 0 && (surf & 0xFFFF_F000) as u64 == off);
    let (pipe, plane_ctl, stride_reg) = match plane {
        Some((p, ctl, _, st)) => (p, ctl, st),
        None => { log("self-test fb: no enabled primary plane scans out the boot framebuffer; skipped"); return true; }
    };
    let tiling = hw::plane_tiling(plane_ctl);
    let x_tiled = match tiling {
        hw::Tiling::Linear => false,
        hw::Tiling::X => true,
        t => { ilog!("self-test fb: plane 1{} tiling {:?} not handled; skipped", (b'A' + pipe as u8) as char, t); return true; }
    };
    let pitch = hw::plane_stride_bytes(stride_reg, tiling);
    if pitch != env.fb_pitch {
        ilog!("self-test fb: plane stride {} B differs from the boot framebuffer pitch {} B; skipped", pitch, env.fb_pitch);
        return true;
    }
    let band = (env.fb_height / 12).clamp(8, 96) as u16;
    let colors = [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF, 0x00FF_FF00, 0x0000_FFFF, 0x00FF_00FF, 0x00FF_FFFF, 0x0080_8080];
    let w = env.fb_width;
    let mut cs = Vec::new();
    for (i, &col) in colors.iter().enumerate() {
        let x1 = (w * i as u32 / 8) as u16;
        let x2 = (w * (i as u32 + 1) / 8) as u16;
        cmd::xy_color_blt(&mut cs, off, env.fb_pitch, x_tiled, x1, 0, x2, band, col);
    }
    if !eng.run("fb", &cs, budget.max(1000)) { return false; }
    // Read back through the CPU aperture (GMADR -> GGTT -> stolen): the
    // same path the console draws through. Linear only (X-tiled pixel
    // addresses are not linear in the aperture without a fence).
    if !x_tiled {
        let len = (env.fb_pitch as usize) * band as usize;
        match mm::kva::ioremap(d.gmadr_base + off, (len + 4095) & !4095, mm::kva::CacheMode::Uncached) {
            Some(va) => {
                let mut bad = 0;
                let mut s = alloc::string::String::new();
                for (i, &col) in colors.iter().enumerate() {
                    let x = (w * i as u32 / 8 + w / 16) as usize;
                    let y = (band / 2) as usize;
                    let p = unsafe { core::ptr::read_volatile((va + y * env.fb_pitch as usize + x * 4) as *const u32) } & 0x00FF_FFFF;
                    if p != col { bad += 1; }
                    s.push_str(&alloc::format!(" {:06x}", p));
                }
                mm::kva::iounmap(va);
                ilog!("self-test fb: bar centres read back through the aperture:{} -> {} ({} mismatches; the console may have redrawn)",
                    s, if bad == 0 { "PASS" } else { "MISMATCH" }, bad);
            }
            None => log("self-test fb: cannot map the aperture for read-back"),
        }
    } else {
        log("self-test fb: X-tiled framebuffer, read-back skipped (look at the screen)");
    }
    let hold = opt("i915.selftest_hold_ms").unwrap_or(1500).min(10_000);
    ilog!("self-test fb: colour bars drawn in the top {} rows; holding {} ms so they can be seen", band, hold);
    delay_us(hold * 1000);
    true
}
