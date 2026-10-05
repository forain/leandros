//! xHCI host controller driver (xHCI 1.2), polled.
//!
//! # Memory-safety model
//!
//! The 2026-07 bring-up attempt corrupted kernel memory because the old
//! `Xhci` struct was 573,568 bytes (256 inline slots, each with an inline
//! 1 KiB device context and 32 rings) and was built and returned **by value**
//! on a 128 KiB kernel stack with no guard page. This driver keeps the
//! controller object small (a compile-time assertion below holds it under
//! 1 KiB), lives behind `Arc<Mutex<_>>` on the heap, and puts per-slot state
//! in `Box`es.
//!
//! Every address the controller can DMA to is a [`DmaBuf`]. A `DmaBuf` that
//! the controller may still reference is never freed:
//! * slot memory is dropped only after a successful Disable Slot (or with
//!   the controller halted);
//! * a transfer that times out is cancelled (Stop Endpoint → Set TR Dequeue)
//!   before returning, and bounce buffers belong to the endpoint, never to
//!   the caller;
//! * if a cancel or a command abort fails, the controller is declared dead
//!   and halted (Run/Stop = 0, then reset);
//! * dropping an `Xhci` halts and resets the controller first, and leaks its
//!   rings rather than freeing them if it will not halt.
//!
//! # Concurrency model
//!
//! One `spin::Mutex` per controller, never held across a wait: a transfer
//! takes the lock to enqueue its TD and ring the doorbell, then repeatedly
//! takes it to drain the event ring (whoever drains files every event under
//! its slot/endpoint or command-TRB pointer) and check for its own
//! completion, releasing it and calling `platform::relax()` (a yield in task
//! context) between polls. Every wait has a deadline on the monotonic clock.

pub mod context;
pub mod regs;
pub mod ring;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::mem::ManuallyDrop;
use spin::Mutex;

use crate::dma::DmaBuf;
use crate::platform::{self, sleep_ms, wait_until, Deadline};
use crate::transport::{BulkTransport, UsbErr};
use crate::{usb_dbg, usb_log};
use context::*;
use regs::*;
use ring::*;

// Completion codes (Table 6-90).
pub const CC_SUCCESS: u8 = 1;
pub const CC_DATA_BUFFER: u8 = 2;
pub const CC_BABBLE: u8 = 3;
pub const CC_TRANSACTION: u8 = 4;
pub const CC_TRB: u8 = 5;
pub const CC_STALL: u8 = 6;
pub const CC_SHORT: u8 = 13;
pub const CC_CONTEXT_STATE: u8 = 19;
pub const CC_EVENT_RING_FULL: u8 = 21;
pub const CC_CMD_RING_STOPPED: u8 = 24;
pub const CC_CMD_ABORTED: u8 = 25;
pub const CC_STOPPED: u8 = 26;

const CMD_TIMEOUT_MS: u32 = 2000;
const ENUM_CTRL_TIMEOUT_MS: u32 = 2000;
const CLASS_CTRL_TIMEOUT_MS: u32 = 5000;
/// Largest data buffer one TRB may describe (and never crossing 64 KiB).
const TRB_MAX_BYTES: usize = 64 * 1024;
/// Largest bulk TD: 16 chained 64 KiB TRBs out of one 1 MiB bounce buffer.
const TD_MAX_BYTES: usize = 16 * TRB_MAX_BYTES;
const TD_MAX_TRBS: usize = TD_MAX_BYTES / TRB_MAX_BYTES + 2;
/// EP0 data-stage buffer: one page. Control transfers above this fail.
const CTRL_BUF_BYTES: usize = 4096;
/// We never enable more slots than this (the DCBAA is one page either way).
const MAX_SLOTS_LIMIT: u8 = 64;

/// One transfer event filed under its endpoint.
#[derive(Clone, Copy)]
struct Ev { ptr: u64, cc: u8, residual: u32 }

struct Endpoint {
    ring: ProducerRing,
    events: Vec<Ev>,
    ep_type: u32,
    mps: u16,
    burst: u8,
    interval: u32,
    avg_trb: u16,
    /// Bounce buffer for bulk data (grown on demand, ≤ TD_MAX_BYTES).
    bounce: Option<DmaBuf>,
}

impl Endpoint {
    fn new(ring: ProducerRing, ep_type: u32, mps: u16, burst: u8, interval: u32, avg_trb: u16) -> Self {
        Endpoint { ring, events: Vec::new(), ep_type, mps, burst, interval, avg_trb, bounce: None }
    }
}

struct Slot {
    port: u8,
    out_ctx: DmaBuf,
    in_ctx: DmaBuf,
    ctrl_buf: DmaBuf,
    /// Indexed by DCI (0 unused, 1 = EP0).
    eps: Vec<Option<Endpoint>>,
}

pub struct Xhci {
    regs: Regs,
    bus: u8,
    csz: usize,
    dma32: bool,
    hci_version: u16,
    max_ports: u8,
    /// Major USB revision of each root port (index port-1): 2, 3, or 0.
    port_rev: Vec<u8>,
    port_slot_type: Vec<u8>,
    dcbaa: ManuallyDrop<DmaBuf>,
    scratch_array: ManuallyDrop<Option<DmaBuf>>,
    scratch: ManuallyDrop<Vec<DmaBuf>>,
    cmd: ManuallyDrop<ProducerRing>,
    evt: ManuallyDrop<EventRing>,
    /// Command completions not yet claimed: (command TRB pointer, cc, slot).
    cmd_done: Vec<(u64, u8, u8)>,
    slots: ManuallyDrop<Vec<Option<Box<Slot>>>>,
    dead: bool,
}

// H1 regression guard: controller and slot objects must stay small, so no
// code path can put a large one on a 128 KiB kernel stack again.
const _: () = assert!(core::mem::size_of::<Xhci>() < 1024);
const _: () = assert!(core::mem::size_of::<Slot>() < 512);

fn map_cc(cc: u8) -> UsbErr {
    match cc {
        CC_STALL => UsbErr::Stall,
        CC_BABBLE => UsbErr::Babble,
        c => UsbErr::Other(c),
    }
}

impl Xhci {
    /// Take ownership of the controller at `base` (mapped, `len` bytes):
    /// BIOS handoff, halt, reset, data structures, run, port power.
    ///
    /// # Safety
    /// `base..base+len` must be the controller's BAR mapped as device memory,
    /// with PCI memory decoding and bus mastering enabled.
    pub unsafe fn init(base: usize, len: usize, bus: u8) -> Result<Xhci, &'static str> {
        let regs = Regs::new(base, len).ok_or("bad capability registers")?;
        let hcs1 = regs.cap(CAP_HCSPARAMS1);
        let hcs2 = regs.cap(CAP_HCSPARAMS2);
        let hcc1 = regs.cap(CAP_HCCPARAMS1);
        let hci_version = (regs.cap(CAP_CAPLENGTH) >> 16) as u16;
        let max_slots_hw = (hcs1 & 0xFF) as u8;
        let max_ports = (hcs1 >> 24) as u8;
        let csz = if hcc1 & HCCPARAMS1_CSZ != 0 { 64 } else { 32 };
        let dma32 = hcc1 & HCCPARAMS1_AC64 == 0;
        if max_slots_hw == 0 || max_ports == 0 {
            return Err("controller reports no slots/ports");
        }
        if regs.op - regs.base + OP_PORTSC + max_ports as usize * 0x10 > len
            || regs.db - regs.base + 4 * (max_slots_hw as usize + 1) > len
        {
            return Err("register space larger than BAR");
        }

        // Extended capabilities: BIOS handoff and the port → protocol map.
        let mut port_rev = vec![0u8; max_ports as usize];
        let mut port_slot_type = vec![0u8; max_ports as usize];
        let mut off = ((hcc1 >> 16) as usize) * 4;
        let mut guard = 0;
        while off != 0 && guard < 64 {
            let Some(dw0) = regs.raw(off) else { break };
            match dw0 as u8 {
                XECP_LEGACY => bios_handoff(&regs, off),
                XECP_PROTOCOL => {
                    let major = (dw0 >> 24) as u8;
                    let dw2 = regs.raw(off + 8).unwrap_or(0);
                    let dw3 = regs.raw(off + 12).unwrap_or(0);
                    let first = (dw2 & 0xFF) as usize;
                    let count = ((dw2 >> 8) & 0xFF) as usize;
                    for p in first..first + count {
                        if (1..=max_ports as usize).contains(&p) {
                            port_rev[p - 1] = major;
                            port_slot_type[p - 1] = (dw3 & 0x1F) as u8;
                        }
                    }
                }
                _ => {}
            }
            let next = ((dw0 >> 8) & 0xFF) as usize;
            if next == 0 { break; }
            off += next * 4;
            guard += 1;
        }

        // Stop a controller firmware left running, then reset it. Nothing
        // below is handed to the controller before HCRST completes.
        if regs.op(OP_USBSTS) & STS_HCH == 0 {
            usb_log!("[XHCI] bus {}: controller was left running by firmware, stopping it\n", bus);
            regs.set_op(OP_USBCMD, regs.op(OP_USBCMD) & !(CMD_RUN | CMD_INTE | CMD_HSEE));
            if !wait_until(100, || regs.op(OP_USBSTS) & STS_HCH != 0) {
                return Err("controller did not halt");
            }
        }
        if !wait_until(1000, || regs.op(OP_USBSTS) & STS_CNR == 0) {
            return Err("controller not ready before reset");
        }
        regs.set_op(OP_USBCMD, CMD_HCRST);
        // Some Intel controllers hang if the register is read too soon.
        sleep_ms(1);
        if !wait_until(1000, || regs.op(OP_USBCMD) & CMD_HCRST == 0 && regs.op(OP_USBSTS) & STS_CNR == 0) {
            return Err("controller reset timed out");
        }

        let pagesize_bits = regs.op(OP_PAGESIZE) & 0xFFFF;
        if pagesize_bits == 0 { return Err("PAGESIZE register is zero"); }
        let page = 4096usize << pagesize_bits.trailing_zeros();

        let max_slots = max_slots_hw.min(MAX_SLOTS_LIMIT);
        regs.set_op(OP_CONFIG, (regs.op(OP_CONFIG) & !0xFF) | max_slots as u32);

        let dcbaa = DmaBuf::new(256 * 8, dma32).ok_or("oom: DCBAA")?;

        // Scratchpad buffers (HCSPARAMS2): the controller's private memory,
        // pointed to by DCBAA[0].
        let nscratch = ((((hcs2 >> 21) & 0x1F) << 5) | ((hcs2 >> 27) & 0x1F)) as usize;
        let mut scratch = Vec::new();
        let scratch_array = if nscratch > 0 {
            let arr = DmaBuf::new(nscratch * 8, dma32).ok_or("oom: scratchpad array")?;
            for i in 0..nscratch {
                let p = DmaBuf::new(page, dma32).ok_or("oom: scratchpad page")?;
                arr.write64(i * 8, p.phys());
                scratch.push(p);
            }
            dcbaa.write64(0, arr.phys());
            Some(arr)
        } else {
            None
        };

        let cmd = ProducerRing::new(dma32).ok_or("oom: command ring")?;
        let evt = EventRing::new(dma32).ok_or("oom: event ring")?;
        crate::dma::dma_wmb();
        usb_dbg!("[XHCI] bus {}: DCBAA {:#x}, command ring {:#x}, event ring {:#x}, ERST {:#x}{}\n",
                 bus, dcbaa.phys(), cmd.phys(), evt.seg_phys(), evt.erst_phys(),
                 if crate::platform::force_dma32() { " (forced below 4 GiB)" } else { "" });

        regs.set_op64(OP_DCBAAP, dcbaa.phys());
        regs.set_op64(OP_CRCR, cmd.dequeue_ptr());
        // Interrupter 0: no interrupts (polled), one-segment event ring.
        regs.set_ir(IR_IMAN, IMAN_IP);
        regs.set_ir(IR_IMOD, 0);
        regs.set_ir(IR_ERSTSZ, 1);
        regs.set_ir64(IR_ERDP, evt.erdp());
        regs.set_ir64(IR_ERSTBA, evt.erst_phys());
        regs.set_op(OP_DNCTRL, 0);
        regs.set_op(OP_USBSTS, STS_HSE | STS_EINT | STS_PCD);

        let mut slots = Vec::with_capacity(max_slots as usize + 1);
        slots.resize_with(max_slots as usize + 1, || None);

        let mut x = Xhci {
            regs, bus, csz, dma32, hci_version, max_ports,
            port_rev, port_slot_type,
            dcbaa: ManuallyDrop::new(dcbaa),
            scratch_array: ManuallyDrop::new(scratch_array),
            scratch: ManuallyDrop::new(scratch),
            cmd: ManuallyDrop::new(cmd),
            evt: ManuallyDrop::new(evt),
            cmd_done: Vec::new(),
            slots: ManuallyDrop::new(slots),
            dead: false,
        };

        // Run (INTE stays 0: this driver never takes the controller's IRQ).
        x.regs.set_op(OP_USBCMD, CMD_RUN);
        if !wait_until(100, || regs.op(OP_USBSTS) & STS_HCH == 0) {
            x.die("controller did not start");
            return Err("controller did not start");
        }

        if hcc1 & HCCPARAMS1_PPC != 0 {
            let mut powered = false;
            for p in 1..=max_ports {
                let sc = x.regs.portsc(p);
                if sc & PORTSC_PP == 0 {
                    x.regs.set_portsc(p, (sc & PORTSC_PRESERVE) | PORTSC_PP);
                    powered = true;
                }
            }
            if powered { sleep_ms(20); }
        }

        usb_log!("[XHCI] bus {}: xHCI {:x}.{:02x}, {} ports, {} of {} slots, {}-byte contexts, {}, {} scratchpad pages\n",
                 bus, x.hci_version >> 8, x.hci_version & 0xFF, max_ports, max_slots, max_slots_hw,
                 csz, if dma32 { "32-bit DMA" } else { "64-bit DMA" }, nscratch);
        Ok(x)
    }

    /// Halt the controller (Run/Stop = 0, wait HCH, reset). Returns whether
    /// it is guaranteed to have stopped all DMA.
    fn halt(&mut self) -> bool {
        self.regs.set_op(OP_USBCMD, self.regs.op(OP_USBCMD) & !CMD_RUN);
        let regs = self.regs;
        let halted = wait_until(100, || regs.op(OP_USBSTS) & STS_HCH != 0);
        if halted {
            regs.set_op(OP_USBCMD, CMD_HCRST);
            sleep_ms(1);
            let _ = wait_until(1000, || regs.op(OP_USBCMD) & CMD_HCRST == 0);
        }
        self.dead = true;
        halted
    }

    fn die(&mut self, why: &str) {
        if !self.dead {
            usb_log!("[XHCI] bus {}: controller failure ({}), halting it\n", self.bus, why);
            if !self.halt() {
                usb_log!("[XHCI] bus {}: controller will not halt; its DMA memory is leaked\n", self.bus);
            }
        }
    }

    pub fn max_ports(&self) -> u8 { self.max_ports }

    // ── events ──────────────────────────────────────────────────────────────

    /// Drain the event ring, filing every event, and advance ERDP.
    fn poll_events(&mut self) {
        if self.dead { return; }
        let mut n = 0usize;
        while let Some(ev) = self.evt.pop() {
            n += 1;
            match ev.ty() {
                TRB_EV_CMD_COMPLETION => {
                    if self.cmd_done.len() >= 32 { self.cmd_done.remove(0); }
                    self.cmd_done.push((ev.param & !0xF, ev.cc(), ev.slot()));
                }
                TRB_EV_TRANSFER => {
                    let dci = ((ev.control >> 16) & 0x1F) as usize;
                    let rec = Ev { ptr: ev.param, cc: ev.cc(), residual: ev.status & 0xFF_FFFF };
                    match self.ep_mut(ev.slot(), dci) {
                        Some(ep) if ep.events.len() < 64 => ep.events.push(rec),
                        Some(_) => {}
                        None => usb_dbg!("[XHCI] stray transfer event slot {} dci {} cc {}\n", ev.slot(), dci, ev.cc()),
                    }
                }
                TRB_EV_PORT_STATUS => {
                    usb_dbg!("[XHCI] bus {}: port {} status change\n", self.bus, ev.param >> 24);
                }
                TRB_EV_HOST_CONTROLLER => {
                    usb_log!("[XHCI] bus {}: host controller event cc={}\n", self.bus, ev.cc());
                }
                _ => {}
            }
            if n >= 1024 { break; }
        }
        if n > 0 {
            self.regs.set_ir64(IR_ERDP, self.evt.erdp() | ERDP_EHB as u64);
        }
        let sts = self.regs.op(OP_USBSTS);
        if sts & (STS_HSE | STS_HCE) != 0 {
            self.die(if sts & STS_HCE != 0 { "host controller error" } else { "host system error" });
        }
    }

    fn submit_cmd(&mut self, t: Trb) -> u64 {
        let mut p = [0u64; 1];
        self.cmd.enqueue_td(&[t], &mut p);
        self.regs.doorbell(0, 0);
        p[0]
    }

    fn take_cmd(&mut self, ptr: u64) -> Option<(u8, u8)> {
        let i = self.cmd_done.iter().position(|c| c.0 == ptr)?;
        let (_, cc, slot) = self.cmd_done.remove(i);
        Some((cc, slot))
    }

    fn slot(&self, id: u8) -> Option<&Slot> {
        self.slots.get(id as usize).and_then(|s| s.as_deref())
    }
    fn slot_mut(&mut self, id: u8) -> Option<&mut Slot> {
        self.slots.get_mut(id as usize).and_then(|s| s.as_deref_mut())
    }
    fn ep_mut(&mut self, slot: u8, dci: usize) -> Option<&mut Endpoint> {
        self.slot_mut(slot)?.eps.get_mut(dci)?.as_mut()
    }

    fn ep_state(&self, slot: u8, dci: usize) -> Option<u32> {
        let s = self.slot(slot)?;
        s.out_ctx.sync_for_cpu();
        Some(Ctx::output(&s.out_ctx, self.csz).ep_state(dci))
    }

    fn port_connected(&self, slot: u8) -> bool {
        match self.slot(slot) {
            Some(s) => self.regs.portsc(s.port) & PORTSC_CCS != 0,
            None => false,
        }
    }

    // ── transfers (lock held) ───────────────────────────────────────────────

    fn start_td(&mut self, slot: u8, dci: usize, trbs: &[Trb], control: bool) -> Result<Td, UsbErr> {
        if self.dead { return Err(UsbErr::Other(0xFF)); }
        let regs = self.regs;
        let ep = self.ep_mut(slot, dci).ok_or(UsbErr::Disconnected)?;
        let mut td = Td { ptrs: [0; TD_MAX_TRBS], lens: [0; TD_MAX_TRBS], n: trbs.len(), control };
        for (i, t) in trbs.iter().enumerate() {
            if matches!(t.ty(), TRB_NORMAL | TRB_DATA) {
                td.lens[i] = t.status & 0x1_FFFF;
            }
        }
        ep.events.clear();
        ep.ring.enqueue_td(trbs, &mut td.ptrs);
        regs.doorbell(slot, dci as u32);
        Ok(td)
    }

    /// Has `td` finished? `Some(Ok(bytes))`, `Some(Err)`, or `None` (pending).
    fn check_td(&mut self, slot: u8, dci: usize, td: &Td) -> Option<Result<usize, UsbErr>> {
        let Some(ep) = self.ep_mut(slot, dci) else { return Some(Err(UsbErr::Disconnected)) };
        let last = td.n - 1;
        let mut short: Option<(usize, u32)> = None;
        for ev in ep.events.iter() {
            // Events for TRBs outside this TD are stale (a cancelled TD, or
            // the second event some controllers post after a short packet).
            let Some(k) = td.ptrs[..td.n].iter().position(|&p| p == ev.ptr) else { continue };
            match ev.cc {
                CC_SUCCESS => {
                    if k == last {
                        let s = short.or(if ev.residual != 0 { Some((k, ev.residual)) } else { None });
                        return Some(Ok(td.bytes(s)));
                    }
                }
                CC_SHORT => {
                    short = Some((k, ev.residual));
                    // A short packet ends a bulk TD; a control transfer
                    // still runs its status stage.
                    if !td.control || k == last {
                        return Some(Ok(td.bytes(short)));
                    }
                }
                cc => return Some(Err(map_cc(cc))),
            }
        }
        None
    }

    /// Build the input context for a Configure Endpoint that drops `drop`
    /// and (re-)adds `add` (DCI bitmasks), from each endpoint's stored
    /// parameters and current ring position. Returns its physical address.
    fn build_configure_input(&mut self, slot: u8, drop: u32, add: u32) -> Option<u64> {
        let csz = self.csz;
        let s = self.slot(slot)?;
        let inp = Ctx::input(&s.in_ctx, csz);
        let out = Ctx::output(&s.out_ctx, csz);
        s.out_ctx.sync_for_cpu();
        inp.clear();
        inp.set_drop(drop);
        inp.set_add(add | 1);
        let mut entries = 1u32;
        for (dci, e) in s.eps.iter().enumerate() {
            if e.is_some() && dci > entries as usize { entries = dci as u32; }
        }
        let dw0 = out.slot(0);
        inp.set_slot(0, (dw0 & !(0x1F << 27)) | (entries << 27));
        inp.set_slot(1, out.slot(1));
        inp.set_slot(2, out.slot(2));
        inp.set_slot(3, 0);
        for dci in 2..32usize {
            if add & (1 << dci) == 0 { continue; }
            let e = s.eps[dci].as_ref()?;
            inp.write_ep(dci, e.ep_type, e.mps, e.burst, e.interval, e.avg_trb, 0, e.ring.dequeue_ptr());
        }
        s.in_ctx.sync_for_device();
        Some(s.in_ctx.phys())
    }
}

impl Drop for Xhci {
    fn drop(&mut self) {
        let stopped = (self.dead && self.regs.op(OP_USBSTS) & STS_HCH != 0) || self.halt();
        if stopped {
            unsafe {
                ManuallyDrop::drop(&mut self.slots);
                ManuallyDrop::drop(&mut self.evt);
                ManuallyDrop::drop(&mut self.cmd);
                ManuallyDrop::drop(&mut self.scratch);
                ManuallyDrop::drop(&mut self.scratch_array);
                ManuallyDrop::drop(&mut self.dcbaa);
            }
        }
        // else: the controller may still DMA into these — leak them.
    }
}

/// USB Legacy Support capability: take ownership from the BIOS and disable
/// its SMIs.
fn bios_handoff(regs: &Regs, off: usize) {
    let Some(v) = regs.raw(off) else { return };
    if v & USBLEGSUP_BIOS_OWNED != 0 {
        regs.set_raw(off, v | USBLEGSUP_OS_OWNED);
        let released = wait_until(1000, || regs.raw(off).unwrap_or(0) & USBLEGSUP_BIOS_OWNED == 0);
        if released {
            usb_dbg!("[XHCI] BIOS handoff complete\n");
        } else {
            usb_log!("[XHCI] BIOS did not release the controller in 1 s, taking it\n");
            let v = regs.raw(off).unwrap_or(0);
            regs.set_raw(off, (v & !USBLEGSUP_BIOS_OWNED) | USBLEGSUP_OS_OWNED);
        }
    }
    if let Some(c) = regs.raw(off + 4) {
        regs.set_raw(off + 4, (c & USBLEGCTLSTS_KEEP) | USBLEGCTLSTS_SMI_EVENTS);
    }
}

/// A submitted TD: the physical address and data length of each TRB.
struct Td {
    ptrs: [u64; TD_MAX_TRBS],
    lens: [u32; TD_MAX_TRBS],
    n: usize,
    control: bool,
}

impl Td {
    fn bytes(&self, short: Option<(usize, u32)>) -> usize {
        match short {
            None => self.lens[..self.n].iter().map(|&l| l as usize).sum(),
            Some((k, residual)) => {
                let before: usize = self.lens[..k].iter().map(|&l| l as usize).sum();
                before + (self.lens[k].saturating_sub(residual)) as usize
            }
        }
    }
}

// ── waiting (lock taken and released per poll) ──────────────────────────────

type Hc = Mutex<Xhci>;

/// Run one command; returns (completion code, slot id).
fn run_cmd(hc: &Hc, t: Trb) -> Result<(u8, u8), UsbErr> {
    let ty = t.ty();
    let ptr = {
        let mut x = hc.lock();
        if x.dead { return Err(UsbErr::Other(0xFF)); }
        x.submit_cmd(t)
    };
    let d = Deadline::after_ms(CMD_TIMEOUT_MS);
    loop {
        {
            let mut x = hc.lock();
            x.poll_events();
            if let Some(r) = x.take_cmd(ptr) {
                usb_dbg!("[XHCI] cmd {} -> cc {} slot {}\n", ty, r.0, r.1);
                return Ok(r);
            }
            if x.dead { return Err(UsbErr::Other(0xFF)); }
            if d.expired() {
                usb_log!("[XHCI] bus {}: command type {} timed out, aborting the command ring\n", x.bus, ty);
                abort_command(&mut x, ptr);
                return Err(UsbErr::Timeout);
            }
        }
        platform::relax();
    }
}

/// Abort the command ring after a command timed out, and turn the stuck
/// command into a no-op so it is not executed when the ring restarts.
fn abort_command(x: &mut Xhci, ptr: u64) {
    x.regs.set_op(OP_CRCR, CRCR_CA);
    x.regs.set_op(OP_CRCR + 4, 0);
    let regs = x.regs;
    if !wait_until(5000, || regs.op(OP_CRCR) & CRCR_CRR == 0) {
        x.die("command ring abort failed");
        return;
    }
    x.poll_events();
    if x.take_cmd(ptr).is_none() {
        x.cmd.neutralise(ptr, TRB_NOOP_CMD);
    }
}

/// Wait for `td` on (slot, dci). On timeout or disconnect the TD is
/// cancelled before returning.
fn wait_td(hc: &Hc, slot: u8, dci: usize, td: &Td, timeout_ms: u32) -> Result<usize, UsbErr> {
    let d = Deadline::after_ms(timeout_ms);
    loop {
        let gone = {
            let mut x = hc.lock();
            x.poll_events();
            if let Some(r) = x.check_td(slot, dci, td) {
                let connected = x.port_connected(slot);
                return r.map_err(|e| match e {
                    UsbErr::Other(_) if !connected => UsbErr::Disconnected,
                    e => e,
                });
            }
            if x.dead { return Err(UsbErr::Other(0xFF)); }
            !x.port_connected(slot)
        };
        if gone || d.expired() {
            let _ = recover_ep(hc, slot, dci);
            return Err(if gone { UsbErr::Disconnected } else { UsbErr::Timeout });
        }
        platform::relax();
    }
}

fn ep_cmd(ty: u32, slot: u8, dci: usize) -> Trb {
    Trb::new(ty, 0, 0, (slot as u32) << 24 | (dci as u32) << 16)
}

/// Bring a halted, stopped or running endpoint back to an idle Stopped
/// state with its dequeue pointer at our enqueue pointer, discarding any
/// TD still on the ring. If this fails the controller is halted, so the
/// endpoint's buffers can never be written behind our back.
fn recover_ep(hc: &Hc, slot: u8, dci: usize) -> Result<(), UsbErr> {
    let r = recover_ep_inner(hc, slot, dci);
    if r.is_err() {
        let mut x = hc.lock();
        if !x.dead && x.slot(slot).is_some() {
            x.die("endpoint recovery failed");
        }
    }
    r
}

fn recover_ep_inner(hc: &Hc, slot: u8, dci: usize) -> Result<(), UsbErr> {
    let state = hc.lock().ep_state(slot, dci).ok_or(UsbErr::Disconnected)?;
    if state != EP_STATE_HALTED {
        let (cc, _) = run_cmd(hc, ep_cmd(TRB_STOP_EP, slot, dci))?;
        if cc != CC_SUCCESS && cc != CC_CONTEXT_STATE {
            return Err(UsbErr::Other(cc));
        }
    }
    if hc.lock().ep_state(slot, dci) == Some(EP_STATE_HALTED) {
        // TSP = 0: also resets the host-side data toggle / sequence number.
        let (cc, _) = run_cmd(hc, ep_cmd(TRB_RESET_EP, slot, dci))?;
        if cc != CC_SUCCESS { return Err(UsbErr::Other(cc)); }
    }
    set_tr_deq(hc, slot, dci)
}

fn set_tr_deq(hc: &Hc, slot: u8, dci: usize) -> Result<(), UsbErr> {
    let deq = {
        let mut x = hc.lock();
        x.ep_mut(slot, dci).ok_or(UsbErr::Disconnected)?.ring.dequeue_ptr()
    };
    let t = Trb::new(TRB_SET_TR_DEQ, deq, 0, (slot as u32) << 24 | (dci as u32) << 16);
    let (cc, _) = run_cmd(hc, t)?;
    let mut x = hc.lock();
    if let Some(ep) = x.ep_mut(slot, dci) { ep.events.clear(); }
    if cc != CC_SUCCESS { return Err(UsbErr::Other(cc)); }
    Ok(())
}

/// Reset the host-side sequence state of a bulk endpoint so it matches a
/// device that is about to receive CLEAR_FEATURE(ENDPOINT_HALT).
fn reset_ep_toggle(hc: &Hc, slot: u8, dci: usize) -> Result<(), UsbErr> {
    let state = hc.lock().ep_state(slot, dci).ok_or(UsbErr::Disconnected)?;
    if state == EP_STATE_HALTED {
        let (cc, _) = run_cmd(hc, ep_cmd(TRB_RESET_EP, slot, dci))?;
        if cc != CC_SUCCESS { return Err(UsbErr::Other(cc)); }
        return set_tr_deq(hc, slot, dci);
    }
    // Not halted: Reset Endpoint would be a context-state error, so drop
    // and re-add the endpoint, which reinitialises its sequence state.
    if state == EP_STATE_RUNNING {
        let (cc, _) = run_cmd(hc, ep_cmd(TRB_STOP_EP, slot, dci))?;
        if cc != CC_SUCCESS && cc != CC_CONTEXT_STATE { return Err(UsbErr::Other(cc)); }
    }
    let bit = 1u32 << dci;
    let inp = hc.lock().build_configure_input(slot, bit, bit).ok_or(UsbErr::Disconnected)?;
    let (cc, _) = run_cmd(hc, Trb::new(TRB_CONFIGURE_EP, inp, 0, (slot as u32) << 24))?;
    let mut x = hc.lock();
    if let Some(ep) = x.ep_mut(slot, dci) { ep.events.clear(); }
    if cc != CC_SUCCESS { return Err(UsbErr::Other(cc)); }
    Ok(())
}

pub(crate) enum CtrlData<'a> {
    None,
    In(&'a mut [u8]),
    Out(&'a [u8]),
}

/// A control transfer on EP0. A STALL (or any other error) is recovered
/// (Reset Endpoint + Set TR Dequeue) before returning, so EP0 is always
/// usable for the next request.
#[allow(clippy::too_many_arguments)]
fn control(hc: &Hc, slot: u8, req_type: u8, req: u8, value: u16, index: u16,
           data: CtrlData<'_>, timeout_ms: u32) -> Result<usize, UsbErr> {
    let (len, dir_in) = match &data {
        CtrlData::None => (0usize, false),
        CtrlData::In(b) => (b.len(), true),
        CtrlData::Out(b) => (b.len(), false),
    };
    if len > CTRL_BUF_BYTES { return Err(UsbErr::Other(0xFF)); }
    let td = {
        let mut x = hc.lock();
        let s = x.slot(slot).ok_or(UsbErr::Disconnected)?;
        let buf_phys = s.ctrl_buf.phys();
        if let CtrlData::Out(b) = &data {
            s.ctrl_buf.copy_in(0, b);
            s.ctrl_buf.sync_for_device();
        }
        let setup = req_type as u64 | (req as u64) << 8 | (value as u64) << 16
            | (index as u64) << 32 | (len as u64) << 48;
        let trt = if len == 0 { 0 } else if dir_in { 3 } else { 2 };
        let mut trbs = [Trb::default(); 3];
        let mut n = 0;
        trbs[n] = Trb::new(TRB_SETUP, setup, 8, TRB_IDT | trt << 16);
        n += 1;
        if len > 0 {
            trbs[n] = Trb::new(TRB_DATA, buf_phys, len as u32, TRB_ISP | if dir_in { TRB_DIR_IN } else { 0 });
            n += 1;
        }
        let status_in = len == 0 || !dir_in;
        trbs[n] = Trb::new(TRB_STATUS, 0, 0, TRB_IOC | if status_in { TRB_DIR_IN } else { 0 });
        n += 1;
        x.start_td(slot, 1, &trbs[..n], true)?
    };
    match wait_td(hc, slot, 1, &td, timeout_ms) {
        Ok(got) => {
            let got = got.min(len);
            if let CtrlData::In(b) = data {
                let x = hc.lock();
                let s = x.slot(slot).ok_or(UsbErr::Disconnected)?;
                s.ctrl_buf.sync_for_cpu();
                s.ctrl_buf.copy_out(0, &mut b[..got]);
            }
            Ok(got)
        }
        Err(e) => {
            if !matches!(e, UsbErr::Timeout | UsbErr::Disconnected) {
                let _ = recover_ep(hc, slot, 1);
            }
            usb_dbg!("[XHCI] control {:02x}/{:02x} v={:04x} i={:04x} len={} failed: {:?}\n",
                     req_type, req, value, index, len, e);
            Err(e)
        }
    }
}

/// A bulk transfer through the endpoint's bounce buffer: chunks of up to
/// 1 MiB, each one TD of chained ≤ 64 KiB TRBs. A short IN chunk ends the
/// transfer. On STALL the endpoint is left halted (the class driver
/// recovers with clear_halt_*); on any other error it is recovered.
fn bulk(hc: &Hc, slot: u8, dci: usize, out: Option<&[u8]>, mut inb: Option<&mut [u8]>,
        timeout_ms: u32) -> Result<usize, UsbErr> {
    let total = match (&out, &inb) {
        (Some(b), _) => b.len(),
        (_, Some(b)) => b.len(),
        _ => 0,
    };
    let mut done = 0usize;
    loop {
        let chunk = (total - done).min(TD_MAX_BYTES);
        let td = {
            let mut x = hc.lock();
            if x.dead { return Err(UsbErr::Other(0xFF)); }
            let dma32 = x.dma32;
            let ep = x.ep_mut(slot, dci).ok_or(UsbErr::Disconnected)?;
            let want = chunk.max(1).next_power_of_two().max(TRB_MAX_BYTES);
            if ep.bounce.as_ref().map_or(true, |b| b.len() < want) {
                // Safe to free the old buffer: no TD is outstanding on this
                // endpoint (transfers are synchronous and a failed TD is
                // always cancelled before its call returns).
                ep.bounce = None;
                let b = DmaBuf::new(want, dma32).ok_or(UsbErr::Other(0xFF))?;
                usb_dbg!("[XHCI] slot {} dci {}: bounce buffer {} KiB at {:#x}\n", slot, dci, want / 1024, b.phys());
                ep.bounce = Some(b);
            }
            let bounce = ep.bounce.as_ref().unwrap();
            if let Some(src) = out {
                bounce.copy_in(0, &src[done..done + chunk]);
            }
            bounce.sync_for_device();
            let base = bounce.phys();
            let mps = ep.mps.max(1) as usize;
            let ntrbs = chunk.div_ceil(TRB_MAX_BYTES).max(1);
            let mut trbs = [Trb::default(); TD_MAX_TRBS];
            for (i, t) in trbs.iter_mut().enumerate().take(ntrbs) {
                let off = i * TRB_MAX_BYTES;
                let l = (chunk - off).min(TRB_MAX_BYTES);
                let last = i + 1 == ntrbs;
                // TD Size: packets still to come after this TRB (max 31).
                let td_size = if last { 0 } else { (chunk - off - l).div_ceil(mps).min(31) as u32 };
                let flags = TRB_ISP | if last { TRB_IOC } else { TRB_CH };
                *t = Trb::new(TRB_NORMAL, base + off as u64, l as u32 | td_size << 17, flags);
            }
            x.start_td(slot, dci, &trbs[..ntrbs], false)?
        };
        let got = match wait_td(hc, slot, dci, &td, timeout_ms) {
            Ok(g) => g.min(chunk),
            Err(e) => {
                if !matches!(e, UsbErr::Timeout | UsbErr::Disconnected | UsbErr::Stall) {
                    let _ = recover_ep(hc, slot, dci);
                }
                usb_dbg!("[XHCI] bulk slot {} dci {} len {} failed: {:?}\n", slot, dci, chunk, e);
                return Err(e);
            }
        };
        if let Some(dst) = inb.as_mut() {
            let mut x = hc.lock();
            let ep = x.ep_mut(slot, dci).ok_or(UsbErr::Disconnected)?;
            let bounce = ep.bounce.as_ref().ok_or(UsbErr::Other(0xFF))?;
            bounce.sync_for_cpu();
            bounce.copy_out(0, &mut dst[done..done + got]);
        }
        done += got;
        if got < chunk || done >= total { return Ok(done); }
    }
}

// ── enumeration ─────────────────────────────────────────────────────────────

/// Reset/enable a root port; returns the port speed ID.
fn reset_port(regs: &Regs, port: u8, rev: u8) -> Result<u8, &'static str> {
    let sc = regs.portsc(port);
    if sc & PORTSC_CCS == 0 { return Err("no device"); }
    let ss_speed = ((sc >> PORTSC_SPEED_SHIFT) & 0xF) as u8 >= SPEED_SS;
    if rev == 3 || (rev == 0 && ss_speed) {
        // USB3: the link trains by itself and the port enables in U0.
        if !wait_until(300, || regs.portsc(port) & PORTSC_PED != 0) {
            let sc = regs.portsc(port);
            regs.set_portsc(port, (sc & PORTSC_PRESERVE) | PORTSC_WPR);
            if !wait_until(500, || {
                let s = regs.portsc(port);
                s & (PORTSC_PRC | PORTSC_WRC) != 0 && s & PORTSC_PR == 0
            }) {
                return Err("USB3 warm reset timed out");
            }
        }
    } else {
        regs.set_portsc(port, (sc & PORTSC_PRESERVE) | PORTSC_PR);
        if !wait_until(500, || {
            let s = regs.portsc(port);
            s & PORTSC_PRC != 0 && s & PORTSC_PR == 0
        }) {
            return Err("port reset timed out");
        }
        sleep_ms(10); // TRSTRCY
    }
    let s = regs.portsc(port);
    regs.set_portsc(port, (s & PORTSC_PRESERVE) | (s & PORTSC_CHANGE));
    let s = regs.portsc(port);
    if s & PORTSC_CCS == 0 { return Err("device disconnected during reset"); }
    if s & PORTSC_PED == 0 { return Err("port not enabled after reset"); }
    Ok(((s >> PORTSC_SPEED_SHIFT) & 0xF) as u8)
}

/// What enumeration learned about a device.
pub struct Enumerated {
    pub info: crate::UsbDeviceInfo,
    pub bulk: Option<XhciBulk>,
}

#[derive(Clone, Copy, Default)]
struct EpDesc { addr: u8, mps: u16, burst: u8 }

#[derive(Clone, Copy, Default)]
struct BotIface { iface: u8, alt: u8, subclass: u8, ep_in: EpDesc, ep_out: EpDesc }

struct ConfigInfo {
    value: u8,
    first_class: u8,
    bot: Option<BotIface>,
    uas_seen: bool,
}

fn finish_iface(cur: &Option<BotIface>, is_bot: bool, ci: &mut ConfigInfo) {
    if let (Some(c), true) = (cur, is_bot) {
        if c.ep_in.addr != 0 && c.ep_out.addr != 0 {
            let better = match &ci.bot {
                None => true,
                Some(b) => b.subclass != 0x06 && c.subclass == 0x06,
            };
            if better { ci.bot = Some(*c); }
        }
    }
}

/// Walk a full configuration descriptor looking for a bulk-only
/// mass-storage interface (class 08, protocol 0x50), in any alternate
/// setting — UAS devices (protocol 0x62) expose BOT as another alt.
fn parse_config(d: &[u8]) -> ConfigInfo {
    let mut ci = ConfigInfo { value: d.get(5).copied().unwrap_or(1), first_class: 0, bot: None, uas_seen: false };
    let mut cur: Option<BotIface> = None;
    let mut cur_is_bot = false;
    let mut last_ep_in = true;
    let mut seen_iface = false;
    let mut off = 0usize;
    while off + 2 <= d.len() {
        let len = d[off] as usize;
        let ty = d[off + 1];
        if len < 2 || off + len > d.len() { break; }
        let b = &d[off..off + len];
        match ty {
            crate::descriptor::DT_INTERFACE if len >= 9 => {
                finish_iface(&cur, cur_is_bot, &mut ci);
                let (class, sub, proto) = (b[5], b[6], b[7]);
                if !seen_iface { ci.first_class = class; seen_iface = true; }
                if class == 0x08 && proto == 0x62 { ci.uas_seen = true; }
                cur_is_bot = class == 0x08 && proto == 0x50;
                cur = Some(BotIface { iface: b[2], alt: b[3], subclass: sub, ..Default::default() });
            }
            crate::descriptor::DT_ENDPOINT if len >= 7 => {
                if let Some(c) = cur.as_mut() {
                    let addr = b[2];
                    let mps = u16::from_le_bytes([b[4], b[5]]) & 0x7FF;
                    if b[3] & 0x3 == 2 {
                        let e = EpDesc { addr, mps, burst: 0 };
                        last_ep_in = addr & 0x80 != 0;
                        if last_ep_in {
                            if c.ep_in.addr == 0 { c.ep_in = e; }
                        } else if c.ep_out.addr == 0 {
                            c.ep_out = e;
                        }
                    }
                }
            }
            crate::descriptor::DT_SS_ENDPOINT if len >= 6 => {
                if let Some(c) = cur.as_mut() {
                    let e = if last_ep_in { &mut c.ep_in } else { &mut c.ep_out };
                    if e.addr != 0 { e.burst = b[2].min(15); }
                }
            }
            _ => {}
        }
        off += len;
    }
    finish_iface(&cur, cur_is_bot, &mut ci);
    ci
}

fn get_string(hc: &Hc, slot: u8, idx: u8, langid: u16) -> String {
    let mut s = String::new();
    if idx == 0 { return s; }
    let mut buf = [0u8; 255];
    let Ok(n) = control(hc, slot, 0x80, 6, 0x0300 | idx as u16, langid, CtrlData::In(&mut buf), ENUM_CTRL_TIMEOUT_MS)
    else { return s };
    let n = n.min(buf[0] as usize);
    let mut i = 2;
    while i + 1 < n {
        let c = u16::from_le_bytes([buf[i], buf[i + 1]]);
        s.push(if (0x20..0x7F).contains(&c) { c as u8 as char } else { '?' });
        i += 2;
    }
    String::from(s.trim())
}

/// Tear a slot down after a failed enumeration. Memory is released only if
/// the controller confirms the slot is disabled (or is halted).
fn disable_slot(hc: &Hc, slot: u8) {
    let ok = matches!(run_cmd(hc, Trb::new(TRB_DISABLE_SLOT, 0, 0, (slot as u32) << 24)), Ok((CC_SUCCESS, _)));
    let mut x = hc.lock();
    let halted = x.dead && x.regs.op(OP_USBSTS) & STS_HCH != 0;
    if ok || halted {
        x.dcbaa.write64(slot as usize * 8, 0);
        if let Some(s) = x.slots.get_mut(slot as usize) { *s = None; }
    } else {
        usb_log!("[XHCI] bus {}: slot {} would not disable; keeping its memory\n", x.bus, slot);
    }
}

/// Enumerate the device on root `port`.
fn enumerate_port(hc: &Arc<Hc>, port: u8) -> Result<Enumerated, &'static str> {
    let (regs, rev, slot_type, bus) = {
        let x = hc.lock();
        (x.regs, x.port_rev[port as usize - 1], x.port_slot_type[port as usize - 1], x.bus)
    };
    let speed = reset_port(&regs, port, rev)?;
    usb_dbg!("[XHCI] bus {} port {}: USB{} port, {} device\n", bus, port, if rev == 0 { 2 } else { rev }, speed_name(speed));

    let (cc, slot) = run_cmd(hc, Trb::new(TRB_ENABLE_SLOT, 0, 0, (slot_type as u32) << 16))
        .map_err(|_| "Enable Slot timed out")?;
    if cc != CC_SUCCESS || slot == 0 { return Err("Enable Slot failed"); }

    match enumerate_slot(hc, port, slot, speed) {
        Ok(e) => Ok(e),
        Err(e) => {
            disable_slot(hc, slot);
            Err(e)
        }
    }
}

fn enumerate_slot(hc: &Arc<Hc>, port: u8, slot: u8, speed: u8) -> Result<Enumerated, &'static str> {
    let (csz, dma32, bus) = {
        let x = hc.lock();
        (x.csz, x.dma32, x.bus)
    };
    let mps0 = default_mps0(speed);
    // Slot state: contexts, EP0 ring, and the DCBAA entry.
    let in_phys = {
        let mut x = hc.lock();
        if slot as usize >= x.slots.len() { return Err("slot id out of range"); }
        let out_ctx = DmaBuf::new(4096, dma32).ok_or("oom: output context")?;
        let in_ctx = DmaBuf::new(4096, dma32).ok_or("oom: input context")?;
        let ctrl_buf = DmaBuf::new(CTRL_BUF_BYTES, dma32).ok_or("oom: control buffer")?;
        let ring = ProducerRing::new(dma32).ok_or("oom: EP0 ring")?;
        let mut eps = Vec::with_capacity(32);
        eps.resize_with(32, || None);
        let deq = ring.dequeue_ptr();
        eps[1] = Some(Endpoint::new(ring, EP_TYPE_CTRL, mps0, 0, 0, 8));

        let inp = Ctx::input(&in_ctx, csz);
        inp.clear();
        inp.set_add(0b11);
        inp.set_slot(0, slot_dw0(0, speed, 1));
        inp.set_slot(1, slot_dw1(port));
        inp.write_ep(1, EP_TYPE_CTRL, mps0, 0, 0, 8, 0, deq);
        in_ctx.sync_for_device();
        let in_phys = in_ctx.phys();
        x.dcbaa.write64(slot as usize * 8, out_ctx.phys());
        x.slots[slot as usize] = Some(Box::new(Slot { port, out_ctx, in_ctx, ctrl_buf, eps }));
        in_phys
    };

    let (cc, _) = run_cmd(hc, Trb::new(TRB_ADDRESS_DEVICE, in_phys, 0, (slot as u32) << 24))
        .map_err(|_| "Address Device timed out")?;
    if cc != CC_SUCCESS {
        usb_dbg!("[XHCI] Address Device cc={}\n", cc);
        return Err("Address Device failed");
    }
    sleep_ms(2); // SET_ADDRESS recovery interval

    // First 8 bytes of the device descriptor → the real bMaxPacketSize0.
    let mut dd = [0u8; 18];
    control(hc, slot, 0x80, 6, 0x0100, 0, CtrlData::In(&mut dd[..8]), ENUM_CTRL_TIMEOUT_MS)
        .map_err(|_| "GET_DESCRIPTOR(device, 8) failed")?;
    let real_mps0 = if speed >= SPEED_SS { 1u16 << dd[7].min(15) } else { dd[7] as u16 };
    if real_mps0 != mps0 && real_mps0 >= 8 {
        let in_phys = {
            let mut x = hc.lock();
            let s = x.slot_mut(slot).ok_or("slot vanished")?;
            let ep0 = s.eps[1].as_mut().ok_or("no EP0")?;
            ep0.mps = real_mps0;
            let deq = ep0.ring.dequeue_ptr();
            let inp = Ctx::input(&s.in_ctx, csz);
            inp.clear();
            inp.set_add(0b10);
            inp.write_ep(1, EP_TYPE_CTRL, real_mps0, 0, 0, 8, 0, deq);
            s.in_ctx.sync_for_device();
            s.in_ctx.phys()
        };
        let (cc, _) = run_cmd(hc, Trb::new(TRB_EVALUATE_CTX, in_phys, 0, (slot as u32) << 24))
            .map_err(|_| "Evaluate Context timed out")?;
        if cc != CC_SUCCESS { return Err("Evaluate Context failed"); }
        usb_dbg!("[XHCI] slot {}: EP0 max packet {} -> {}\n", slot, mps0, real_mps0);
    }

    let n = control(hc, slot, 0x80, 6, 0x0100, 0, CtrlData::In(&mut dd), ENUM_CTRL_TIMEOUT_MS)
        .map_err(|_| "GET_DESCRIPTOR(device) failed")?;
    if n < 18 || dd[1] != crate::descriptor::DT_DEVICE { return Err("bad device descriptor"); }
    let vendor = u16::from_le_bytes([dd[8], dd[9]]);
    let product = u16::from_le_bytes([dd[10], dd[11]]);
    let dev_class = dd[4];

    let mut hdr = [0u8; 9];
    control(hc, slot, 0x80, 6, 0x0200, 0, CtrlData::In(&mut hdr), ENUM_CTRL_TIMEOUT_MS)
        .map_err(|_| "GET_DESCRIPTOR(config, 9) failed")?;
    let total = (u16::from_le_bytes([hdr[2], hdr[3]]) as usize).clamp(9, CTRL_BUF_BYTES);
    let mut cfg = vec![0u8; total];
    let got = control(hc, slot, 0x80, 6, 0x0200, 0, CtrlData::In(&mut cfg), ENUM_CTRL_TIMEOUT_MS)
        .map_err(|_| "GET_DESCRIPTOR(config) failed")?;
    cfg.truncate(got);
    let ci = parse_config(&cfg);

    // Strings are best-effort (many devices have none).
    let mut langs = [0u8; 4];
    let lang = match control(hc, slot, 0x80, 6, 0x0300, 0, CtrlData::In(&mut langs), ENUM_CTRL_TIMEOUT_MS) {
        Ok(n) if n >= 4 => u16::from_le_bytes([langs[2], langs[3]]),
        _ => 0x0409,
    };
    let manufacturer = get_string(hc, slot, dd[14], lang);
    let product_name = get_string(hc, slot, dd[15], lang);

    let address = {
        let x = hc.lock();
        let s = x.slot(slot).ok_or("slot vanished")?;
        s.out_ctx.sync_for_cpu();
        Ctx::output(&s.out_ctx, csz).usb_address()
    };

    let mut info = crate::UsbDeviceInfo {
        bus, port, slot, address, vendor, product, speed,
        class: if dev_class != 0 { dev_class } else { ci.first_class },
        manufacturer, product_name, mass_storage: false, interface: 0,
    };

    let Some(bot) = ci.bot else {
        if ci.uas_seen {
            usb_log!("[XHCI] slot {}: UAS-only storage without a bulk-only alt setting, unsupported\n", slot);
        }
        return Ok(Enumerated { info, bulk: None });
    };

    // Configure the two bulk endpoints, then select the configuration (and
    // alt setting) on the device — Linux's order (bandwidth first).
    let dci_in = dci_of(bot.ep_in.addr);
    let dci_out = dci_of(bot.ep_out.addr);
    let in_phys = {
        let mut x = hc.lock();
        let s = x.slot_mut(slot).ok_or("slot vanished")?;
        for (dci, e, ty) in [(dci_in, bot.ep_in, EP_TYPE_BULK_IN), (dci_out, bot.ep_out, EP_TYPE_BULK_OUT)] {
            let ring = ProducerRing::new(dma32).ok_or("oom: bulk ring")?;
            // Bulk: Interval 0, average TRB length 3 KiB (xHCI §4.14.1.1).
            s.eps[dci] = Some(Endpoint::new(ring, ty, e.mps, e.burst, 0, 3072));
        }
        x.build_configure_input(slot, 0, (1 << dci_in) | (1 << dci_out)).ok_or("slot vanished")?
    };
    let (cc, _) = run_cmd(hc, Trb::new(TRB_CONFIGURE_EP, in_phys, 0, (slot as u32) << 24))
        .map_err(|_| "Configure Endpoint timed out")?;
    if cc != CC_SUCCESS {
        usb_dbg!("[XHCI] Configure Endpoint cc={}\n", cc);
        return Err("Configure Endpoint failed");
    }
    control(hc, slot, 0x00, 9, ci.value as u16, 0, CtrlData::None, ENUM_CTRL_TIMEOUT_MS)
        .map_err(|_| "SET_CONFIGURATION failed")?;
    if bot.alt != 0 {
        control(hc, slot, 0x01, 11, bot.alt as u16, bot.iface as u16, CtrlData::None, ENUM_CTRL_TIMEOUT_MS)
            .map_err(|_| "SET_INTERFACE failed")?;
    }
    info.mass_storage = true;
    info.interface = bot.iface;
    usb_dbg!("[XHCI] slot {}: BOT interface {} alt {} subclass {:02x}, IN {:02x} (mps {}, burst {}), OUT {:02x} (mps {}, burst {})\n",
             slot, bot.iface, bot.alt, bot.subclass, bot.ep_in.addr, bot.ep_in.mps, bot.ep_in.burst,
             bot.ep_out.addr, bot.ep_out.mps, bot.ep_out.burst);

    Ok(Enumerated {
        info,
        bulk: Some(XhciBulk {
            hc: hc.clone(), slot, iface: bot.iface,
            ep_in: bot.ep_in.addr, ep_out: bot.ep_out.addr, dci_in, dci_out,
        }),
    })
}

/// Bring up one controller and enumerate its root ports (hubs are not
/// supported). Returns the controller handle (keep it alive for as long as
/// its devices are in use) and what was found.
///
/// # Safety
/// As [`Xhci::init`].
pub unsafe fn probe(base: usize, len: usize, bus: u8) -> Result<(Arc<Hc>, Vec<Enumerated>), &'static str> {
    let x = Xhci::init(base, len, bus)?;
    let max_ports = x.max_ports;
    let regs = x.regs;
    let hc = Arc::new(Mutex::new(x));
    let mut found = Vec::new();
    let mut seen = vec![false; max_ports as usize + 1];
    // Debounce (USB 2.0 §7.1.7.3: 100 ms from connect to reset), then a
    // second pass for devices whose link came up late.
    for pass in 0..2 {
        sleep_ms(if pass == 0 { 100 } else { 200 });
        for port in 1..=max_ports {
            if seen[port as usize] || regs.portsc(port) & PORTSC_CCS == 0 { continue; }
            seen[port as usize] = true;
            match enumerate_port(&hc, port) {
                Ok(e) => found.push(e),
                Err(why) => usb_log!("[XHCI] bus {} port {}: enumeration failed: {}\n", bus, port, why),
            }
            if hc.lock().dead { return Ok((hc, found)); }
        }
    }
    Ok((hc, found))
}

// ── BulkTransport ───────────────────────────────────────────────────────────

/// A bulk-only mass-storage interface on an xHCI-attached device.
pub struct XhciBulk {
    hc: Arc<Hc>,
    slot: u8,
    iface: u8,
    ep_in: u8,
    ep_out: u8,
    dci_in: usize,
    dci_out: usize,
}

impl XhciBulk {
    pub fn slot(&self) -> u8 { self.slot }
    /// (bulk IN, bulk OUT) endpoint addresses.
    pub fn endpoints(&self) -> (u8, u8) { (self.ep_in, self.ep_out) }

    fn clear_halt(&mut self, dci: usize, ep_addr: u8) -> Result<(), UsbErr> {
        reset_ep_toggle(&self.hc, self.slot, dci)?;
        // CLEAR_FEATURE(ENDPOINT_HALT) to the endpoint.
        control(&self.hc, self.slot, 0x02, 1, 0, ep_addr as u16, CtrlData::None, CLASS_CTRL_TIMEOUT_MS)?;
        Ok(())
    }
}

impl BulkTransport for XhciBulk {
    fn bulk_out(&mut self, data: &[u8], timeout_ms: u32) -> Result<usize, UsbErr> {
        bulk(&self.hc, self.slot, self.dci_out, Some(data), None, timeout_ms)
    }
    fn bulk_in(&mut self, buf: &mut [u8], timeout_ms: u32) -> Result<usize, UsbErr> {
        bulk(&self.hc, self.slot, self.dci_in, None, Some(buf), timeout_ms)
    }
    fn class_control_out(&mut self, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), UsbErr> {
        let d = if data.is_empty() { CtrlData::None } else { CtrlData::Out(data) };
        control(&self.hc, self.slot, 0x21, request, value, index, d, CLASS_CTRL_TIMEOUT_MS).map(|_| ())
    }
    fn class_control_in(&mut self, request: u8, value: u16, index: u16, buf: &mut [u8]) -> Result<usize, UsbErr> {
        control(&self.hc, self.slot, 0xA1, request, value, index, CtrlData::In(buf), CLASS_CTRL_TIMEOUT_MS)
    }
    fn clear_halt_in(&mut self) -> Result<(), UsbErr> { self.clear_halt(self.dci_in, self.ep_in) }
    fn clear_halt_out(&mut self) -> Result<(), UsbErr> { self.clear_halt(self.dci_out, self.ep_out) }
    fn interface_number(&self) -> u8 { self.iface }
}
