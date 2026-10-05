//! xHCI rings (xHCI 1.2 §4.9).
//!
//! * [`ProducerRing`] — command and transfer rings: one 4 KiB segment of 256
//!   TRBs whose last entry is a Link TRB (Toggle Cycle) back to the start.
//!   Software produces; the controller consumes.
//! * [`EventRing`] — one 256-TRB segment described by a one-entry ERST. The
//!   controller produces into **all 256** entries (an event ring has no Link
//!   TRB); software consumes and toggles its cycle state on wrap.
//!
//! Both are backed by `DmaBuf`, so dropping a ring frees its memory — only
//! ever done once the controller can no longer reach it.

use crate::dma::{dma_rmb, dma_wmb, DmaBuf};

pub const RING_TRBS: usize = 256;
const TRB_SIZE: usize = 16;

// TRB types (Table 6-91).
pub const TRB_NORMAL: u32 = 1;
pub const TRB_SETUP: u32 = 2;
pub const TRB_DATA: u32 = 3;
pub const TRB_STATUS: u32 = 4;
pub const TRB_LINK: u32 = 6;
pub const TRB_ENABLE_SLOT: u32 = 9;
pub const TRB_DISABLE_SLOT: u32 = 10;
pub const TRB_ADDRESS_DEVICE: u32 = 11;
pub const TRB_CONFIGURE_EP: u32 = 12;
pub const TRB_EVALUATE_CTX: u32 = 13;
pub const TRB_RESET_EP: u32 = 14;
pub const TRB_STOP_EP: u32 = 15;
pub const TRB_SET_TR_DEQ: u32 = 16;
pub const TRB_NOOP_CMD: u32 = 23;
pub const TRB_EV_TRANSFER: u32 = 32;
pub const TRB_EV_CMD_COMPLETION: u32 = 33;
pub const TRB_EV_PORT_STATUS: u32 = 34;
pub const TRB_EV_HOST_CONTROLLER: u32 = 37;

// Control-dword flags.
pub const TRB_CYCLE: u32 = 1 << 0;
pub const TRB_TC: u32 = 1 << 1;   // Link: toggle cycle
pub const TRB_ISP: u32 = 1 << 2;
pub const TRB_CH: u32 = 1 << 4;
pub const TRB_IOC: u32 = 1 << 5;
pub const TRB_IDT: u32 = 1 << 6;
pub const TRB_BSR: u32 = 1 << 9;  // Address Device: block SET_ADDRESS
pub const TRB_DIR_IN: u32 = 1 << 16;

/// One TRB as four dwords: parameter lo/hi, status, control.
#[derive(Clone, Copy, Default, Debug)]
pub struct Trb {
    pub param: u64,
    pub status: u32,
    pub control: u32,
}

impl Trb {
    pub fn new(ty: u32, param: u64, status: u32, flags: u32) -> Self {
        Trb { param, status, control: (ty << 10) | (flags & !TRB_CYCLE) }
    }
    #[inline] pub fn ty(&self) -> u32 { (self.control >> 10) & 0x3F }
    /// Completion code (event TRBs).
    #[inline] pub fn cc(&self) -> u8 { (self.status >> 24) as u8 }
    /// Slot ID (event TRBs).
    #[inline] pub fn slot(&self) -> u8 { (self.control >> 24) as u8 }
}

pub struct ProducerRing {
    buf: DmaBuf,
    enq: usize,
    pcs: bool,
}

impl ProducerRing {
    pub fn new(dma32: bool) -> Option<Self> {
        let buf = DmaBuf::new(RING_TRBS * TRB_SIZE, dma32)?;
        let r = ProducerRing { buf, enq: 0, pcs: true };
        // Link TRB, cycle bit clear (= not yet valid for PCS=1). It is made
        // valid by `advance` when the producer actually reaches it.
        r.write_raw(RING_TRBS - 1, &Trb::new(TRB_LINK, r.buf.phys(), 0, TRB_TC), false);
        Some(r)
    }

    #[inline] pub fn phys(&self) -> u64 { self.buf.phys() }
    #[inline] fn trb_phys(&self, idx: usize) -> u64 { self.buf.phys() + (idx * TRB_SIZE) as u64 }

    /// The current enqueue pointer with the DCS/RCS bit, for CRCR, an
    /// endpoint context, or a Set TR Dequeue Pointer command.
    pub fn dequeue_ptr(&self) -> u64 { self.trb_phys(self.enq) | self.pcs as u64 }

    fn write_raw(&self, idx: usize, t: &Trb, cycle: bool) {
        let off = idx * TRB_SIZE;
        self.buf.write32(off, t.param as u32);
        self.buf.write32(off + 4, (t.param >> 32) as u32);
        self.buf.write32(off + 8, t.status);
        dma_wmb();
        self.buf.write32(off + 12, (t.control & !TRB_CYCLE) | cycle as u32);
    }

    /// Step past the TRB just written, following (and validating) the Link
    /// TRB at the end of the segment. `chain` = the TD continues past it.
    fn advance(&mut self, chain: bool) {
        self.enq += 1;
        if self.enq == RING_TRBS - 1 {
            let flags = TRB_TC | if chain { TRB_CH } else { 0 };
            let link = Trb::new(TRB_LINK, self.buf.phys(), 0, flags);
            self.write_raw(RING_TRBS - 1, &link, self.pcs);
            self.pcs = !self.pcs;
            self.enq = 0;
        }
    }

    /// Enqueue a whole TD and return the physical address of each TRB in
    /// `ptrs`. The first TRB is published (its cycle bit flipped to valid)
    /// only after every other TRB of the TD is in place, so a controller
    /// that is already running this ring never sees a partial TD.
    pub fn enqueue_td(&mut self, trbs: &[Trb], ptrs: &mut [u64]) {
        assert!(!trbs.is_empty() && trbs.len() < RING_TRBS - 8 && ptrs.len() >= trbs.len());
        let first_idx = self.enq;
        let first_cycle = self.pcs;
        for (i, t) in trbs.iter().enumerate() {
            ptrs[i] = self.trb_phys(self.enq);
            let cycle = if i == 0 { !self.pcs } else { self.pcs };
            self.write_raw(self.enq, t, cycle);
            self.advance(t.control & TRB_CH != 0);
        }
        dma_wmb();
        let off = first_idx * TRB_SIZE + 12;
        self.buf.write32(off, (trbs[0].control & !TRB_CYCLE) | first_cycle as u32);
        dma_wmb();
    }

    /// Replace the (still-owned-by-controller) TRB at physical address `ptr`
    /// with a no-op of the same cycle state — used to neutralise a command
    /// that was aborted, so the controller does not execute it again when
    /// the command ring restarts.
    pub fn neutralise(&self, ptr: u64, noop_type: u32) {
        let base = self.buf.phys();
        if ptr < base || ptr >= base + (RING_TRBS * TRB_SIZE) as u64 { return; }
        let off = (ptr - base) as usize;
        let cycle = self.buf.read32(off + 12) & TRB_CYCLE;
        self.buf.write32(off, 0);
        self.buf.write32(off + 4, 0);
        self.buf.write32(off + 8, 0);
        dma_wmb();
        self.buf.write32(off + 12, (noop_type << 10) | cycle);
    }
}

pub struct EventRing {
    seg: DmaBuf,
    erst: DmaBuf,
    deq: usize,
    ccs: bool,
}

impl EventRing {
    pub fn new(dma32: bool) -> Option<Self> {
        let seg = DmaBuf::new(RING_TRBS * TRB_SIZE, dma32)?;
        let erst = DmaBuf::new(64, dma32)?;
        // ERST entry 0: segment base (64-byte aligned), size in TRBs.
        erst.write64(0, seg.phys());
        erst.write32(8, RING_TRBS as u32);
        erst.write32(12, 0);
        dma_wmb();
        Some(EventRing { seg, erst, deq: 0, ccs: true })
    }

    pub fn erst_phys(&self) -> u64 { self.erst.phys() }
    pub fn seg_phys(&self) -> u64 { self.seg.phys() }
    /// Current dequeue pointer for ERDP.
    pub fn erdp(&self) -> u64 { self.seg.phys() + (self.deq * TRB_SIZE) as u64 }

    /// Next event, if the controller has produced one.
    pub fn pop(&mut self) -> Option<Trb> {
        let off = self.deq * TRB_SIZE;
        let control = self.seg.read32(off + 12);
        if (control & TRB_CYCLE != 0) != self.ccs { return None; }
        dma_rmb();
        let t = Trb {
            param: self.seg.read64(off),
            status: self.seg.read32(off + 8),
            control,
        };
        self.deq += 1;
        if self.deq == RING_TRBS {
            self.deq = 0;
            self.ccs = !self.ccs;
        }
        Some(t)
    }
}
