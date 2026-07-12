//! xHCI Transfer/Command/Event rings — ported from drivers/usb/host/xhci-ring.c
//!
//! A ring is a circular array of 16-byte TRBs (Transfer Request Blocks).
//! The Cycle State Bit (CSB) distinguishes HC-owned from driver-owned TRBs.
//! A LINK TRB at the end of each segment chains segments together.

use super::{TRBS_PER_SEGMENT, TRB_SEGMENT_SIZE};

// ── TRB type codes (xHCI spec Table 6-91) ─────────────────────────────────────

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrbType {
    // Transfer ring TRBs
    Normal       = 1,
    SetupStage   = 2,
    DataStage    = 3,
    StatusStage  = 4,
    Isoch        = 5,
    Link         = 6,
    EventData    = 7,
    TrNoop       = 8,
    // Command TRBs
    EnableSlot   = 9,
    DisableSlot  = 10,
    AddressDevice = 11,
    ConfigureEp  = 12,
    EvalContext  = 13,
    ResetEp      = 14,
    StopRing     = 15,
    SetTrDeq     = 16,
    ResetDevice  = 17,
    ForceEvent   = 18,
    NegBandwidth = 19,
    SetLatTolVal = 20,
    GetPortBw    = 21,
    ForceHeader  = 22,
    CmdNoop      = 23,
    // Event TRBs
    TransferEvent = 32,
    CmdCompletion = 33,
    PortStatusChg = 34,
    BandwidthReq  = 35,
    DoorbellEvent = 36,
    HcEvent       = 37,
    DeviceNotify  = 38,
    MfIndexWrap   = 39,
}

// ── TRB field bit masks ───────────────────────────────────────────────────────

pub const TRB_TYPE_SHIFT:   u32 = 10;
pub const TRB_TYPE_MASK:    u32 = 0x0000_FC00;
pub const TRB_CYCLE:        u32 = 1 << 0;  // Cycle bit
pub const TRB_ENT:          u32 = 1 << 1;  // Evaluate Next TRB
pub const TRB_ISP:          u32 = 1 << 2;  // Interrupt on Short Packet
pub const TRB_NO_SNOOP:     u32 = 1 << 3;  // No Snoop
pub const TRB_CHAIN:        u32 = 1 << 4;  // Chain bit
pub const TRB_IOC:          u32 = 1 << 5;  // Interrupt on Completion
pub const TRB_IDT:          u32 = 1 << 6;  // Immediate Data
pub const TRB_BEI:          u32 = 1 << 9;  // Block Event Interrupt
pub const TRB_DIR_IN:       u32 = 1 << 16; // Data Stage direction IN
pub const TRB_SIA:          u32 = 1 << 31; // Start Isoch ASAP
pub const TRB_BSR:          u32 = 1 << 9;  // Block Set Address Request

/// Build the status DWORD: transfer length (bits 16:0) + TD size (bits 21:17)
/// + interrupt target (bits 31:22).
pub const fn trb_status(len: u32, td_size: u32, intr: u32) -> u32 {
    (len & 0x1_FFFF) | ((td_size & 0x1F) << 17) | ((intr & 0x3FF) << 22)
}

/// Build the control DWORD: cycle bit + flags + TRB type.
pub const fn trb_control(trb_type: u8, flags: u32, cycle: bool) -> u32 {
    ((trb_type as u32) << TRB_TYPE_SHIFT) | flags | (cycle as u32)
}

// ── TRB (16 bytes) ────────────────────────────────────────────────────────────

/// A single Transfer Request Block — 16 bytes, 4 × u32.
#[derive(Clone, Copy, Debug, Default)]
#[repr(C, align(16))]
pub struct Trb {
    pub param_lo: u32, // bits 31:0  of parameter
    pub param_hi: u32, // bits 63:32 of parameter (or status in some TRBs)
    pub status:   u32, // transfer length / TD size / interrupter target
    pub control:  u32, // cycle bit | flags | TRB type
}

impl Trb {
    /// Build a NORMAL transfer TRB for a contiguous buffer.
    pub fn normal(buf_phys: u64, len: u32, ioc: bool, chain: bool, cycle: bool) -> Self {
        let flags = if ioc { TRB_IOC } else { 0 }
                  | if chain { TRB_CHAIN } else { 0 };
        Self {
            param_lo: buf_phys as u32,
            param_hi: (buf_phys >> 32) as u32,
            status:   trb_status(len, 0, 0),
            control:  trb_control(TrbType::Normal as u8, flags, cycle),
        }
    }

    /// Build a SETUP STAGE TRB (control transfer).
    pub fn setup(request: [u8; 8], transfer_type: u8, cycle: bool) -> Self {
        let lo = u32::from_le_bytes(request[0..4].try_into().unwrap());
        let hi = u32::from_le_bytes(request[4..8].try_into().unwrap());
        let flags = TRB_IDT | ((transfer_type as u32) << 16);
        Self {
            param_lo: lo,
            param_hi: hi,
            status:   trb_status(8, 0, 0),
            control:  trb_control(TrbType::SetupStage as u8, flags, cycle),
        }
    }

    /// Build a DATA STAGE TRB.
    pub fn data(buf_phys: u64, len: u32, dir_in: bool, cycle: bool) -> Self {
        let flags = if dir_in { TRB_DIR_IN } else { 0 } | TRB_IOC;
        Self {
            param_lo: buf_phys as u32,
            param_hi: (buf_phys >> 32) as u32,
            status:   trb_status(len, 0, 0),
            control:  trb_control(TrbType::DataStage as u8, flags, cycle),
        }
    }

    /// Build a STATUS STAGE TRB (direction opposite to data stage).
    pub fn status(dir_in: bool, cycle: bool) -> Self {
        let flags = if dir_in { 0 } else { TRB_DIR_IN } | TRB_IOC;
        Self {
            param_lo: 0, param_hi: 0,
            status:   0,
            control:  trb_control(TrbType::StatusStage as u8, flags, cycle),
        }
    }

    /// Build a LINK TRB (end of ring segment, wraps back to `next_phys`).
    pub fn link(next_phys: u64, toggle_cycle: bool, cycle: bool) -> Self {
        let flags = if toggle_cycle { 1 << 1 } else { 0 }; // TC bit
        Self {
            param_lo: next_phys as u32,
            param_hi: (next_phys >> 32) as u32,
            status:   0,
            control:  trb_control(TrbType::Link as u8, flags, cycle),
        }
    }

    /// Build a generic command TRB.
    pub fn command(trb_type: TrbType, a: u32, b: u32, flags: u32) -> Self {
        Self {
            param_lo: a,
            param_hi: b,
            status:   0,
            control:  trb_control(trb_type as u8, flags, false),
        }
    }

    pub fn trb_type(&self) -> u8 {
        ((self.control & TRB_TYPE_MASK) >> TRB_TYPE_SHIFT) as u8
    }

    pub fn cycle_bit(&self) -> bool { self.control & TRB_CYCLE != 0 }
}

// ── Ring ──────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingType { Transfer, Command, Event }

/// A single-segment xHCI ring.
///
/// In the real driver (xhci-ring.c) rings can be multi-segment; here we use
/// a single statically-allocated segment for simplicity.
///
/// The TRB array lives in a `buddy`-allocated page, not as plain struct
/// storage: the xHC's DMA engine reads/writes TRBs directly, so it needs a
/// real physical address (written into DCBAAP/CRCR/ERSTBA/ERDP/LINK-TRB
/// fields) — a kernel virtual address (what `&self.trbs` would have given
/// under the HHDM) would make the controller fault or hang on real
/// hardware. The CPU side accesses the same page through its HHDM alias
/// (`mm::phys_to_virt`) with volatile reads/writes, since the hardware can
/// modify this memory concurrently (event ring cycle-bit polling).
pub struct Ring {
    pub kind: RingType,
    /// Physical base address of the one-page (4096 B = TRBS_PER_SEGMENT * 16)
    /// TRB array backing this ring.
    phys: usize,
    /// Index of the next TRB to produce (enqueue pointer).
    enq: usize,
    /// Index of the next TRB to consume (dequeue pointer, event rings only).
    deq: usize,
    /// Producer Cycle State.
    pcs: bool,
    /// Consumer Cycle State (event ring).
    ccs: bool,
}

impl Ring {
    pub fn new(kind: RingType) -> Self {
        let phys = mm::buddy::alloc(0).expect("xhci: out of memory allocating ring");
        unsafe {
            core::ptr::write_bytes(mm::phys_to_virt(phys) as *mut u8, 0, TRB_SEGMENT_SIZE);
        }
        let r = Self { kind, phys, enq: 0, deq: 0, pcs: true, ccs: true };
        // Install LINK TRB at the last slot pointing back to slot 0.
        let link = Trb::link(r.phys as u64, true, true);
        unsafe { r.trb_ptr(TRBS_PER_SEGMENT - 1).write_volatile(link); }
        r
    }

    /// Virtual pointer to TRB slot `idx` through the HHDM alias of this
    /// ring's physical page.
    fn trb_ptr(&self, idx: usize) -> *mut Trb {
        (mm::phys_to_virt(self.phys) as *mut Trb).wrapping_add(idx)
    }

    /// Physical base address of TRB[0].
    pub fn phys_base(&self) -> u64 {
        self.phys as u64
    }

    /// Physical address of the current enqueue pointer.
    pub fn enq_phys(&self) -> u64 {
        self.phys as u64 + (self.enq * core::mem::size_of::<Trb>()) as u64
    }

    /// Physical address of the current dequeue pointer (used to update ERDP).
    pub fn deq_phys(&self) -> u64 {
        self.phys as u64 + (self.deq * core::mem::size_of::<Trb>()) as u64
    }

    /// Enqueue a TRB, advancing the enqueue pointer and toggling PCS at LINK.
    pub fn enqueue(&mut self, mut trb: Trb) {
        // Set cycle bit to current PCS.
        trb.control = (trb.control & !TRB_CYCLE) | (self.pcs as u32);
        unsafe { self.trb_ptr(self.enq).write_volatile(trb); }

        self.enq += 1;
        // Skip the LINK TRB slot; if we hit it, toggle PCS and wrap.
        if self.enq == TRBS_PER_SEGMENT - 1 {
            // Update LINK TRB cycle bit.
            unsafe {
                let link_ptr = self.trb_ptr(TRBS_PER_SEGMENT - 1);
                let mut link = link_ptr.read_volatile();
                link.control = (link.control & !TRB_CYCLE) | (self.pcs as u32);
                link_ptr.write_volatile(link);
            }
            self.pcs = !self.pcs;
            self.enq = 0;
        }
    }

    /// Dequeue the next event TRB whose cycle bit matches CCS.
    ///
    /// LINK TRBs are skipped automatically — they are bookkeeping entries that
    /// should never be presented as events to the caller.  Returns `None` when
    /// no new events are available.
    pub fn dequeue_event(&mut self) -> Option<Trb> {
        loop {
            let trb = unsafe { self.trb_ptr(self.deq).read_volatile() };
            if trb.cycle_bit() != self.ccs { return None; }

            // Advance the dequeue pointer, wrapping at the LINK TRB slot.
            self.deq += 1;
            if self.deq == TRBS_PER_SEGMENT - 1 {
                self.deq = 0;
                self.ccs = !self.ccs;
            }

            // Skip LINK TRBs — the HC may produce them during ring management.
            if trb.trb_type() == TrbType::Link as u8 { continue; }

            return Some(trb);
        }
    }

    pub fn is_empty(&self) -> bool { self.enq == self.deq }
}
