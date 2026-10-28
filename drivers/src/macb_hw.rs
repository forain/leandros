//! Hardware-independent pieces of the Cadence GEM ("macb") Ethernet driver
//! (`drivers/src/macb.rs`): register map, 64-bit DMA descriptor rings, the
//! MDIO / clause-22 PHY code for the Raspberry Pi 5's BCM54213PE (RGMII
//! delays included), link resolution into NCFGR, and the DTB lookup of the
//! controller's `local-mac-address` / `phy-mode` / PHY address.
//!
//! Everything here talks to the chip through [`Regs`] and to descriptor
//! memory through raw pointers, so `drivers/macb-tests` includes this file
//! verbatim and runs it against a mock register file and a simulated PHY.
//!
//! Register and bit names follow Linux `drivers/net/ethernet/cadence/macb.h`.

#![allow(dead_code)]

use core::sync::atomic::{fence, Ordering};

// ── registers (byte offsets) ─────────────────────────────────────────────────

pub const NCR: usize = 0x0000;
pub const NCFGR: usize = 0x0004;
pub const NSR: usize = 0x0008;
/// GEM user I/O: bit 0 selects RGMII.
pub const USRIO: usize = 0x000c;
pub const DMACFG: usize = 0x0010;
pub const TSR: usize = 0x0014;
pub const RBQP: usize = 0x0018;
pub const TBQP: usize = 0x001c;
pub const RSR: usize = 0x0020;
pub const ISR: usize = 0x0024;
pub const IER: usize = 0x0028;
pub const IDR: usize = 0x002c;
pub const IMR: usize = 0x0030;
pub const MAN: usize = 0x0034;
/// AXI max pipeline (Raspberry Pi kernel's GEM_AMP; `cdns,aw2w-max-pipe`,
/// `cdns,ar2r-max-pipe`, `cdns,use-aw2b-fill`).
pub const AMP: usize = 0x0054;
pub const HRB: usize = 0x0080;
pub const HRT: usize = 0x0084;
pub const SA1B: usize = 0x0088;
pub const SA1T: usize = 0x008c;
pub const MID: usize = 0x00fc;
pub const DCFG1: usize = 0x0280;
pub const DCFG6: usize = 0x0294;
/// Priority-queue descriptor bases (queue q >= 1 at +4*(q-1)).
pub const TBQPQ1: usize = 0x0440;
pub const RBQPQ1: usize = 0x0480;
/// Upper 32 bits of every TX / RX queue base (shared by all queues).
pub const TBQPH: usize = 0x04c8;
pub const RBQPH: usize = 0x04d4;
/// Per-queue interrupt disable (queue q >= 1 at +4*(q-1)).
pub const IDRQ1: usize = 0x0620;

// NCR
pub const NCR_LLB: u32 = 1 << 1;
pub const NCR_RE: u32 = 1 << 2;
pub const NCR_TE: u32 = 1 << 3;
pub const NCR_MPE: u32 = 1 << 4;
pub const NCR_CLRSTAT: u32 = 1 << 5;
pub const NCR_TSTART: u32 = 1 << 9;
pub const NCR_THALT: u32 = 1 << 10;

// NCFGR
pub const NCFGR_SPD: u32 = 1 << 0;
pub const NCFGR_FD: u32 = 1 << 1;
pub const NCFGR_CAF: u32 = 1 << 4;
pub const NCFGR_NBC: u32 = 1 << 5;
pub const NCFGR_MTI: u32 = 1 << 6;
pub const NCFGR_UNI: u32 = 1 << 7;
pub const NCFGR_BIG: u32 = 1 << 8;
pub const NCFGR_GBE: u32 = 1 << 10;
pub const NCFGR_PAE: u32 = 1 << 13;
pub const NCFGR_RBOF_SHIFT: u32 = 14;
pub const NCFGR_DRFCS: u32 = 1 << 17;
pub const NCFGR_CLK_SHIFT: u32 = 18;
pub const NCFGR_CLK_MASK: u32 = 7 << 18;
pub const NCFGR_DBW_SHIFT: u32 = 21;
pub const NCFGR_DBW_MASK: u32 = 3 << 21;
/// MDC = pclk / 96 (pclk 160..240 MHz; RP1's is 200 MHz -> 2.08 MHz).
pub const CLK_DIV96: u32 = 5;

// NSR
pub const NSR_MDIO_IDLE: u32 = 1 << 2;

// DMACFG
pub const DMACFG_FBLDO_MASK: u32 = 0x1f;
pub const DMACFG_ENDIA_DESC: u32 = 1 << 6;
pub const DMACFG_ENDIA_PKT: u32 = 1 << 7;
pub const DMACFG_RXBMS_MASK: u32 = 3 << 8;
pub const DMACFG_TXPBMS: u32 = 1 << 10;
pub const DMACFG_TXCOEN: u32 = 1 << 11;
pub const DMACFG_RXBS_SHIFT: u32 = 16;
pub const DMACFG_RXBS_MASK: u32 = 0xff << 16;
pub const DMACFG_DDRP: u32 = 1 << 24;
pub const DMACFG_RXEXT: u32 = 1 << 28;
pub const DMACFG_TXEXT: u32 = 1 << 29;
pub const DMACFG_ADDR64: u32 = 1 << 30;

// DCFG6: 64-bit DMA addressing supported; priority-queue mask in bits 1..15.
pub const DCFG6_DAW64: u32 = 1 << 23;

// ISR / IER / IDR / IMR
pub const INT_MFD: u32 = 1 << 0;
pub const INT_RCOMP: u32 = 1 << 1;
pub const INT_RXUBR: u32 = 1 << 2;
pub const INT_TXUBR: u32 = 1 << 3;
pub const INT_TUND: u32 = 1 << 4;
pub const INT_RLE: u32 = 1 << 5;
pub const INT_TXERR: u32 = 1 << 6;
pub const INT_TCOMP: u32 = 1 << 7;
pub const INT_ROVR: u32 = 1 << 10;
pub const INT_HRESP: u32 = 1 << 11;
/// What the driver enables: RX completion and RX trouble, TX errors, bus errors.
pub const INT_RX_MASK: u32 = INT_RCOMP | INT_RXUBR | INT_ROVR;
pub const INT_ERR_MASK: u32 = INT_TUND | INT_RLE | INT_TXERR | INT_HRESP;
pub const INT_ALL: u32 = 0xffff_ffff;

// MAN (clause 22): SOF=01, RW 01 write / 10 read, CODE=10.
pub const MAN_SOF_C22: u32 = 1 << 30;
pub const MAN_RW_WRITE: u32 = 1 << 28;
pub const MAN_RW_READ: u32 = 2 << 28;
pub const MAN_CODE: u32 = 2 << 16;

// ── register access ─────────────────────────────────────────────────────────

/// The chip's register file (MMIO in the kernel, a mock in host tests).
pub trait Regs {
    fn r32(&self, off: usize) -> u32;
    fn w32(&mut self, off: usize, v: u32);
    fn delay_us(&mut self, us: u32);
}

/// NCFGR data-bus-width field from DCFG1.DBWDEF (bits 25..27): 4 -> 128-bit,
/// 2 -> 64-bit, else 32-bit (Linux `macb_dbw`).
pub fn dbw_from_dcfg1(dcfg1: u32) -> u32 {
    match (dcfg1 >> 25) & 7 { 4 => 2, 2 => 1, _ => 0 }
}

/// Number of DMA queues: queue 0 plus one per DCFG6 bit 1..15.
pub fn num_queues(dcfg6: u32) -> usize {
    1 + ((dcfg6 >> 1) & 0x7fff).count_ones() as usize
}

/// NCFGR before the link is known: MDC divider, data-bus width, discard FCS,
/// 1536-byte frames, multicast hash on; speed/duplex/GBE clear.
pub fn ncfgr_base(dcfg1: u32) -> u32 {
    (CLK_DIV96 << NCFGR_CLK_SHIFT) | (dbw_from_dcfg1(dcfg1) << NCFGR_DBW_SHIFT)
        | NCFGR_DRFCS | NCFGR_BIG | NCFGR_MTI
}

/// DMACFG for `rx_buf` byte RX buffers: 16-beat bursts, full RX/TX packet
/// buffer memory, little-endian descriptors and data, no checksum offload,
/// 64-bit descriptors when `addr64`.
pub fn dmacfg_value(old: u32, rx_buf: usize, addr64: bool) -> u32 {
    let mut v = old & !(DMACFG_RXBS_MASK | DMACFG_FBLDO_MASK | DMACFG_ENDIA_DESC | DMACFG_ENDIA_PKT
        | DMACFG_TXCOEN | DMACFG_ADDR64 | DMACFG_RXEXT | DMACFG_TXEXT);
    v |= (((rx_buf / 64) as u32) << DMACFG_RXBS_SHIFT) & DMACFG_RXBS_MASK;
    v |= 16 | DMACFG_RXBMS_MASK | DMACFG_TXPBMS;
    if addr64 { v |= DMACFG_ADDR64; }
    v
}

/// DMACFG with the AXI fixed-burst length replaced: 1 (single), 4, 8 or
/// 16 beats (the FBLDO encodings are the beat counts themselves).
pub fn dmacfg_with_burst(v: u32, burst: u32) -> u32 {
    let b = match burst { 1 | 4 | 8 | 16 => burst, _ => 16 };
    (v & !DMACFG_FBLDO_MASK) | b
}

/// Next shorter burst for the TX-DMA-error step-down (16 -> 8 -> 4 -> 1).
pub fn shorter_burst(b: u32) -> u32 { match b { 16 => 8, 8 => 4, _ => 1 } }

// TSR bits (GEM).
pub const TSR_UBR: u32 = 1 << 0;
pub const TSR_COL: u32 = 1 << 1;
pub const TSR_RLE: u32 = 1 << 2;
pub const TSR_TGO: u32 = 1 << 3;
/// Transmit frame corruption: an AXI/AHB error mid-frame.
pub const TSR_TFC: u32 = 1 << 4;
pub const TSR_COMP: u32 = 1 << 5;
pub const TSR_UND: u32 = 1 << 6;
pub const TSR_LCO: u32 = 1 << 7;
/// The DMA got a bus error response (HRESP/RRESP not OK).
pub const TSR_HRESP: u32 = 1 << 8;
/// TSR bits after which the transmitter has halted and needs a TE cycle.
pub const TSR_TX_HALT_ERR: u32 = TSR_TFC | TSR_HRESP | TSR_RLE | TSR_UND;

/// "ubr|tfc|hresp" style names of the set TSR bits ("-" when none).
pub fn tsr_names(tsr: u32, out: &mut dyn core::fmt::Write) {
    const N: [&str; 9] = ["ubr", "col", "rle", "tgo", "tfc", "comp", "und", "lco", "hresp"];
    let mut first = true;
    for (i, n) in N.iter().enumerate() {
        if tsr & (1 << i) != 0 {
            let _ = out.write_str(if first { "" } else { "|" });
            let _ = out.write_str(n);
            first = false;
        }
    }
    if first { let _ = out.write_str("-"); }
}

/// PCIe Device Control for the endpoint (RP1) so that no TLP it receives or
/// sends exceeds what the other side accepts: MPS = min(root port MPS,
/// endpoint MPS supported), and MRRS = that MPS, so even a root complex that
/// splits read completions at its own (larger) MPS never sends the
/// endpoint a completion bigger than its MPS (a malformed TLP, which the
/// GEM sees as a DMA read error). Linux gets the same effect from
/// `pci_configure_mps`. Returns the new DEVCTL value.
pub fn pcie_ep_devctl(rc_devctl: u16, ep_devcap: u32, ep_devctl: u16) -> u16 {
    let rc_mps = (rc_devctl >> 5) & 7;
    let ep_mpss = (ep_devcap & 7) as u16;
    let mps = rc_mps.min(ep_mpss).min(5);
    (ep_devctl & !((7 << 5) | (7 << 12))) | (mps << 5) | (mps << 12)
}

/// Speed/duplex fields for a resolved link (Linux `macb_mac_link_up`).
pub fn ncfgr_for_link(ncfgr: u32, speed: u32, full: bool) -> u32 {
    let mut v = ncfgr & !(NCFGR_SPD | NCFGR_FD | NCFGR_GBE);
    match speed { 1000 => v |= NCFGR_GBE, 100 => v |= NCFGR_SPD, _ => {} }
    if full { v |= NCFGR_FD; }
    v
}

/// MAC from the specific-address-1 registers.
pub fn mac_from_sa(bot: u32, top: u32) -> [u8; 6] {
    let b = bot.to_le_bytes();
    let t = top.to_le_bytes();
    [b[0], b[1], b[2], b[3], t[0], t[1]]
}

pub fn sa_from_mac(m: &[u8; 6]) -> (u32, u32) {
    (u32::from_le_bytes([m[0], m[1], m[2], m[3]]), u16::from_le_bytes([m[4], m[5]]) as u32)
}

/// Usable unicast address: not all-zero, not all-ones, not multicast.
pub fn mac_valid(m: &[u8; 6]) -> bool {
    m.iter().any(|&b| b != 0) && m.iter().any(|&b| b != 0xff) && m[0] & 1 == 0
}

// ── descriptors ─────────────────────────────────────────────────────────────

/// 64-bit-addressing descriptor (DMACFG.ADDR64, no timestamp extension).
#[repr(C)]
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct Desc {
    pub addr: u32,
    pub ctrl: u32,
    pub addr_hi: u32,
    pub rsvd: u32,
}
pub const DESC_SIZE: usize = 16;

// RX word 0 / word 1
pub const RX_USED: u32 = 1 << 0;
pub const RX_WRAP: u32 = 1 << 1;
pub const RX_ADDR_MASK: u32 = !3;
pub const RX_LEN_MASK: u32 = 0x1fff;
pub const RX_SOF: u32 = 1 << 14;
pub const RX_EOF: u32 = 1 << 15;
// TX word 1
pub const TX_LEN_MASK: u32 = 0x3fff;
pub const TX_LAST: u32 = 1 << 15;
pub const TX_ERR_MASK: u32 = (1 << 29) | (1 << 27) | (1 << 26) | (7 << 20);
pub const TX_WRAP: u32 = 1 << 30;
pub const TX_USED: u32 = 1 << 31;

/// Per-descriptor cache maintenance for rings kept in CACHEABLE memory on a
/// non-coherent bus (`macb.dma=cmo`): `(clean, invalidate)` over `(va, len)`.
/// Null = the rings are in Normal-NC / Device memory (or the bus snoops) and
/// plain volatile accesses are enough.
static DESC_CMO: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub type CmoFns = (fn(usize, usize), fn(usize, usize));
static mut CMO_FNS: CmoFns = (cmo_nop, cmo_nop);
fn cmo_nop(_: usize, _: usize) {}

/// Install (or clear) per-descriptor cache maintenance. Call before the rings
/// are initialised; not meant to change while the GEM runs.
pub fn set_desc_cmo(f: Option<CmoFns>) {
    use core::sync::atomic::Ordering;
    match f {
        Some(f) => { unsafe { CMO_FNS = f; } DESC_CMO.store(1, Ordering::Release); }
        None => DESC_CMO.store(0, Ordering::Release),
    }
}
#[inline]
fn cmo() -> Option<CmoFns> {
    if DESC_CMO.load(core::sync::atomic::Ordering::Acquire) == 0 { None } else { Some(unsafe { CMO_FNS }) }
}

unsafe fn rd(d: *mut Desc, i: usize) -> Desc {
    if let Some((_, inv)) = cmo() { inv(d.add(i) as usize, core::mem::size_of::<Desc>()); }
    core::ptr::read_volatile(d.add(i))
}
unsafe fn wr_word(d: *mut Desc, i: usize, word: usize, v: u32) {
    let c = cmo();
    // Fetch the line fresh first: a stale copy of a neighbour descriptor the
    // GEM has since written must not be cleaned back over it.
    if let Some((_, inv)) = c { inv(d.add(i) as usize, core::mem::size_of::<Desc>()); }
    core::ptr::write_volatile((d.add(i) as *mut u32).add(word), v);
    if let Some((clean, _)) = c { clean(d.add(i) as usize, core::mem::size_of::<Desc>()); }
}

#[derive(Debug, PartialEq, Eq)]
pub enum RxEvent {
    Empty,
    Frame { idx: usize, len: usize },
    /// Frame split across buffers (cannot happen with 1536 B frames in
    /// 2 KiB buffers) or an otherwise unusable slot.
    Fragment { idx: usize },
}

/// Slot index a queue pointer register (TBQP/RBQP low word) points at, or
/// None when it lies outside the `n`-descriptor ring at `base_lo`.
pub fn ring_slot(qp_lo: u32, base_lo: u32, n: usize) -> Option<usize> {
    let off = qp_lo.wrapping_sub(base_lo) as usize;
    if off % DESC_SIZE != 0 || off / DESC_SIZE >= n { None } else { Some(off / DESC_SIZE) }
}

/// Polls (1 s apart) that may pass with frames queued, the transmitter idle
/// (TSR.TGO clear) and TBQP not moving before the driver rewinds the queue.
pub const TX_STALL_POLLS: u32 = 3;

/// TX stall detector: frames are in flight, TSTART was rung, yet the
/// controller sits idle on the same descriptor. `tick` is called once per
/// driver-thread poll and answers true when the queue must be rewound.
#[derive(Default, Debug)]
pub struct TxStall {
    last: Option<(usize, usize)>,
    polls: u32,
}
impl TxStall {
    pub fn tick(&mut self, hw_slot: Option<usize>, tail: usize, in_flight: usize, tgo: bool) -> bool {
        if in_flight == 0 || tgo {
            *self = TxStall::default();
            return false;
        }
        let key = (hw_slot.unwrap_or(usize::MAX), tail);
        if self.last == Some(key) { self.polls += 1; } else { self.last = Some(key); self.polls = 0; }
        if self.polls >= TX_STALL_POLLS { *self = TxStall::default(); true } else { false }
    }
}

/// RX ring: word 0 USED set by the controller = frame for software; the
/// driver clears it to give the slot back. The last slot carries WRAP.
pub struct RxRing {
    descs: *mut Desc,
    n: usize,
    head: usize,
}
unsafe impl Send for RxRing {}

impl RxRing {
    /// # Safety
    /// `descs` points at `n` descriptors that stay valid for the ring's life.
    pub unsafe fn init(descs: *mut Desc, n: usize, buf_dma: &dyn Fn(usize) -> u64) -> RxRing {
        for i in 0..n {
            let a = buf_dma(i);
            wr_word(descs, i, 2, (a >> 32) as u32);
            wr_word(descs, i, 3, 0);
            wr_word(descs, i, 1, 0);
            let wrap = if i == n - 1 { RX_WRAP } else { 0 };
            wr_word(descs, i, 0, (a as u32 & RX_ADDR_MASK) | wrap);
        }
        fence(Ordering::SeqCst);
        RxRing { descs, n, head: 0 }
    }
    pub fn len(&self) -> usize { self.n }
    pub fn head(&self) -> usize { self.head }
    /// Descriptor `i` as the CPU sees it (diagnostics).
    pub fn desc(&self, i: usize) -> Desc { unsafe { rd(self.descs, i % self.n) } }
    /// Look at the next slot in order (does not consume it).
    pub fn next(&self) -> RxEvent {
        let d = unsafe { rd(self.descs, self.head) };
        if d.addr & RX_USED == 0 { return RxEvent::Empty; }
        fence(Ordering::Acquire);
        let d = unsafe { rd(self.descs, self.head) };
        if d.ctrl & (RX_SOF | RX_EOF) == (RX_SOF | RX_EOF) {
            RxEvent::Frame { idx: self.head, len: (d.ctrl & RX_LEN_MASK) as usize }
        } else {
            RxEvent::Fragment { idx: self.head }
        }
    }
    /// Give slot `idx` (the head) back to the controller and advance.
    pub fn release(&mut self, idx: usize) {
        debug_assert_eq!(idx, self.head);
        unsafe {
            let d = rd(self.descs, idx);
            wr_word(self.descs, idx, 1, 0);
            fence(Ordering::Release);
            wr_word(self.descs, idx, 0, d.addr & !RX_USED);
        }
        fence(Ordering::SeqCst);
        self.head = (idx + 1) % self.n;
    }
}

/// TX ring: every idle slot holds USED (the controller stops there); posting
/// clears USED, the controller sets it again once the frame left.
pub struct TxRing {
    descs: *mut Desc,
    n: usize,
    head: usize,
    tail: usize,
    pub errors: u64,
}
unsafe impl Send for TxRing {}

impl TxRing {
    /// # Safety
    /// As for [`RxRing::init`].
    pub unsafe fn init(descs: *mut Desc, n: usize, buf_dma: &dyn Fn(usize) -> u64) -> TxRing {
        for i in 0..n {
            let a = buf_dma(i);
            wr_word(descs, i, 0, a as u32);
            wr_word(descs, i, 2, (a >> 32) as u32);
            wr_word(descs, i, 3, 0);
            wr_word(descs, i, 1, TX_USED | Self::wrap_of(n, i));
        }
        fence(Ordering::SeqCst);
        TxRing { descs, n, head: 0, tail: 0, errors: 0 }
    }
    fn wrap_of(n: usize, i: usize) -> u32 { if i == n - 1 { TX_WRAP } else { 0 } }
    pub fn len(&self) -> usize { self.n }
    pub fn in_flight(&self) -> usize { (self.head + self.n - self.tail) % self.n }
    pub fn tail(&self) -> usize { self.tail }
    pub fn head(&self) -> usize { self.head }
    /// Next free slot; one slot always stays idle so head never catches tail.
    pub fn slot(&self) -> Option<usize> {
        if self.in_flight() >= self.n - 1 { None } else { Some(self.head) }
    }
    /// Hand slot `idx` (from [`slot`]) holding a `len`-byte frame to the chip.
    pub fn post(&mut self, idx: usize, len: usize) {
        debug_assert_eq!(idx, self.head);
        fence(Ordering::Release);
        unsafe { wr_word(self.descs, idx, 1, (len as u32 & TX_LEN_MASK) | TX_LAST | Self::wrap_of(self.n, idx)); }
        fence(Ordering::SeqCst);
        self.head = (idx + 1) % self.n;
    }
    /// Descriptor `i` as the controller sees it (diagnostics).
    pub fn desc(&self, i: usize) -> Desc { unsafe { rd(self.descs, i % self.n) } }
    /// After the transmitter halted on an error and TE was cycled (which
    /// moves the controller back to the ring base): every slot back to
    /// USED, head = tail = 0. Returns the frames dropped.
    pub fn reset(&mut self) -> usize {
        let dropped = self.in_flight();
        for i in 0..self.n {
            unsafe { wr_word(self.descs, i, 1, TX_USED | Self::wrap_of(self.n, i)); }
        }
        fence(Ordering::SeqCst);
        self.head = 0;
        self.tail = 0;
        dropped
    }
    /// Retire slots the controller finished; returns how many.
    pub fn reclaim(&mut self) -> usize {
        let mut k = 0;
        while self.tail != self.head {
            let d = unsafe { rd(self.descs, self.tail) };
            if d.ctrl & TX_USED == 0 { break; }
            if d.ctrl & TX_ERR_MASK != 0 { self.errors += 1; }
            self.tail = (self.tail + 1) % self.n;
            k += 1;
        }
        k
    }
}

// ── MDIO / PHY ──────────────────────────────────────────────────────────────

pub const MII_BMCR: u8 = 0;
pub const MII_BMSR: u8 = 1;
pub const MII_PHYID1: u8 = 2;
pub const MII_PHYID2: u8 = 3;
pub const MII_ANAR: u8 = 4;
pub const MII_ANLPAR: u8 = 5;
pub const MII_GBCR: u8 = 9;
pub const MII_GBSR: u8 = 10;
/// Broadcom auxiliary control (shadowed) and shadow-0x1c registers.
pub const MII_BCM_AUXCTL: u8 = 0x18;
pub const MII_BCM_SHD1C: u8 = 0x1c;

pub const BMCR_RESET: u16 = 1 << 15;
pub const BMCR_ANENABLE: u16 = 1 << 12;
pub const BMCR_PDOWN: u16 = 1 << 11;
pub const BMCR_ISOLATE: u16 = 1 << 10;
pub const BMCR_ANRESTART: u16 = 1 << 9;
pub const BMSR_LSTATUS: u16 = 1 << 2;
pub const BMSR_ANEGCOMPLETE: u16 = 1 << 5;
/// 10/100 half+full, selector 802.3.
pub const ANAR_DEFAULT: u16 = 0x01e1;
pub const GBCR_1000FULL: u16 = 1 << 9;
pub const GBCR_1000HALF: u16 = 1 << 8;
pub const GBSR_LP_1000FULL: u16 = 1 << 11;
pub const GBSR_LP_1000HALF: u16 = 1 << 10;

/// AUXCTL shadow 7 (misc control): write enable, RGMII RX skew (RX delay).
pub const AUXCTL_SHDW_MISC: u16 = 0x7;
pub const AUXCTL_MISC_WREN: u16 = 0x8000;
pub const AUXCTL_MISC_RGMII_SKEW_EN: u16 = 0x0100;
/// Shadow 0x1c selector 3 (clock control): GTXCLK delay (TX delay).
pub const SHD_CLK_CTL: u16 = 0x3;
pub const SHD_CLK_GTXCLK_EN: u16 = 1 << 9;

/// BCM54213PE OUI/model (PHYID1:PHYID2 = 0x600d:0x84a2), revision masked.
pub const BCM54213PE_ID: u32 = 0x600d_84a0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhyMode { Rgmii, RgmiiId, RgmiiRxid, RgmiiTxid }

impl PhyMode {
    pub fn parse(s: &[u8]) -> Option<PhyMode> {
        Some(match s {
            b"rgmii" => PhyMode::Rgmii,
            b"rgmii-id" => PhyMode::RgmiiId,
            b"rgmii-rxid" => PhyMode::RgmiiRxid,
            b"rgmii-txid" => PhyMode::RgmiiTxid,
            _ => return None,
        })
    }
    pub fn rx_delay(self) -> bool { matches!(self, PhyMode::RgmiiId | PhyMode::RgmiiRxid) }
    pub fn tx_delay(self) -> bool { matches!(self, PhyMode::RgmiiId | PhyMode::RgmiiTxid) }
    pub fn name(self) -> &'static str {
        match self { PhyMode::Rgmii => "rgmii", PhyMode::RgmiiId => "rgmii-id",
                     PhyMode::RgmiiRxid => "rgmii-rxid", PhyMode::RgmiiTxid => "rgmii-txid" }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdioError { Timeout, NoPhy, ResetStuck }

fn mdio_wait(r: &mut dyn Regs) -> Result<(), MdioError> {
    for _ in 0..1000 {
        if r.r32(NSR) & NSR_MDIO_IDLE != 0 { return Ok(()); }
        r.delay_us(1);
    }
    Err(MdioError::Timeout)
}

pub fn mdio_read(r: &mut dyn Regs, phy: u8, reg: u8) -> Result<u16, MdioError> {
    mdio_wait(r)?;
    r.w32(MAN, MAN_SOF_C22 | MAN_RW_READ | MAN_CODE | ((phy as u32 & 31) << 23) | ((reg as u32 & 31) << 18));
    mdio_wait(r)?;
    Ok(r.r32(MAN) as u16)
}

pub fn mdio_write(r: &mut dyn Regs, phy: u8, reg: u8, v: u16) -> Result<(), MdioError> {
    mdio_wait(r)?;
    r.w32(MAN, MAN_SOF_C22 | MAN_RW_WRITE | MAN_CODE | ((phy as u32 & 31) << 23) | ((reg as u32 & 31) << 18) | v as u32);
    mdio_wait(r)
}

pub fn phy_id(r: &mut dyn Regs, phy: u8) -> Result<u32, MdioError> {
    let hi = mdio_read(r, phy, MII_PHYID1)?;
    let lo = mdio_read(r, phy, MII_PHYID2)?;
    Ok(((hi as u32) << 16) | lo as u32)
}

fn id_present(id: u32) -> bool { id != 0 && id != 0xffff_ffff }

/// The PHY address: `want` (from the DTB) if a PHY answers there, else the
/// first address 0..31 that answers.
pub fn find_phy(r: &mut dyn Regs, want: u8) -> Result<(u8, u32), MdioError> {
    let id = phy_id(r, want)?;
    if id_present(id) { return Ok((want, id)); }
    for a in 0..32u8 {
        if a == want { continue; }
        let id = phy_id(r, a)?;
        if id_present(id) { return Ok((a, id)); }
    }
    Err(MdioError::NoPhy)
}

/// Broadcom RGMII delays (Linux `bcm54xx_config_clock_delay`): the RX delay
/// is AUXCTL misc RGMII_SKEW_EN, the TX delay shadow-0x1c clock GTXCLK_EN.
pub fn bcm_set_delays(r: &mut dyn Regs, phy: u8, mode: PhyMode) -> Result<(), MdioError> {
    mdio_write(r, phy, MII_BCM_AUXCTL, (AUXCTL_SHDW_MISC << 12) | 0x7)?;
    let mut v = mdio_read(r, phy, MII_BCM_AUXCTL)? | AUXCTL_MISC_WREN;
    if mode.rx_delay() { v |= AUXCTL_MISC_RGMII_SKEW_EN } else { v &= !AUXCTL_MISC_RGMII_SKEW_EN }
    mdio_write(r, phy, MII_BCM_AUXCTL, (v & !0x7) | AUXCTL_SHDW_MISC)?;
    mdio_write(r, phy, MII_BCM_SHD1C, (SHD_CLK_CTL & 0x1f) << 10)?;
    let mut c = mdio_read(r, phy, MII_BCM_SHD1C)? & 0x3ff;
    if mode.tx_delay() { c |= SHD_CLK_GTXCLK_EN } else { c &= !SHD_CLK_GTXCLK_EN }
    mdio_write(r, phy, MII_BCM_SHD1C, 0x8000 | ((SHD_CLK_CTL & 0x1f) << 10) | (c & 0x3ff))
}

/// Soft reset (BMCR.RESET, self-clearing within 500 ms), RGMII delays,
/// advertise 10/100/1000 and (re)start autonegotiation.
pub fn phy_setup(r: &mut dyn Regs, phy: u8, mode: PhyMode) -> Result<(), MdioError> {
    mdio_write(r, phy, MII_BMCR, BMCR_RESET)?;
    let mut ok = false;
    for _ in 0..500 {
        r.delay_us(1000);
        if mdio_read(r, phy, MII_BMCR)? & BMCR_RESET == 0 { ok = true; break; }
    }
    if !ok { return Err(MdioError::ResetStuck); }
    bcm_set_delays(r, phy, mode)?;
    mdio_write(r, phy, MII_ANAR, ANAR_DEFAULT)?;
    mdio_write(r, phy, MII_GBCR, GBCR_1000FULL | GBCR_1000HALF)?;
    let bmcr = mdio_read(r, phy, MII_BMCR)? & !(BMCR_PDOWN | BMCR_ISOLATE);
    mdio_write(r, phy, MII_BMCR, bmcr | BMCR_ANENABLE | BMCR_ANRESTART)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Link { pub up: bool, pub speed: u32, pub full: bool }

/// Highest common mode from our advertisement and the partner's.
pub fn resolve(anar: u16, anlpar: u16, gbcr: u16, gbsr: u16) -> (u32, bool) {
    if gbcr & GBCR_1000FULL != 0 && gbsr & GBSR_LP_1000FULL != 0 { return (1000, true); }
    if gbcr & GBCR_1000HALF != 0 && gbsr & GBSR_LP_1000HALF != 0 { return (1000, false); }
    let c = anar & anlpar;
    if c & 0x100 != 0 { (100, true) } else if c & 0x080 != 0 { (100, false) }
    else if c & 0x040 != 0 { (10, true) } else { (10, false) }
}

/// Current link as the PHY reports it (BMSR link status is latched-low, so
/// it is read twice).
pub fn phy_link(r: &mut dyn Regs, phy: u8) -> Result<Link, MdioError> {
    let _ = mdio_read(r, phy, MII_BMSR)?;
    let bmsr = mdio_read(r, phy, MII_BMSR)?;
    if bmsr & BMSR_LSTATUS == 0 || bmsr & BMSR_ANEGCOMPLETE == 0 { return Ok(Link::default()); }
    let anar = mdio_read(r, phy, MII_ANAR)?;
    let lpa = mdio_read(r, phy, MII_ANLPAR)?;
    let gbcr = mdio_read(r, phy, MII_GBCR)?;
    let gbsr = mdio_read(r, phy, MII_GBSR)?;
    let (speed, full) = resolve(anar, lpa, gbcr, gbsr);
    Ok(Link { up: true, speed, full })
}

/// Link polling state machine: feeds [`phy_link`] results, reports a change
/// once, and programs NCFGR speed/duplex (and RE/TE) on every link-up.
#[derive(Default)]
pub struct LinkWatch { pub cur: Link, pub changes: u32 }

impl LinkWatch {
    /// Poll the PHY once. Returns the new link on a change.
    pub fn poll(&mut self, r: &mut dyn Regs, phy: u8) -> Result<Option<Link>, MdioError> {
        let l = phy_link(r, phy)?;
        if l == self.cur { return Ok(None); }
        self.cur = l;
        self.changes += 1;
        if l.up {
            let v = ncfgr_for_link(r.r32(NCFGR), l.speed, l.full);
            r.w32(NCFGR, v);
            let ncr = r.r32(NCR);
            r.w32(NCR, ncr | NCR_RE | NCR_TE);
        }
        Ok(Some(l))
    }
}

// ── DTB ─────────────────────────────────────────────────────────────────────

/// What the DTB says about the (first enabled) GEM.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GemDt {
    pub found: bool,
    pub mac: Option<[u8; 6]>,
    pub phy_mode: Option<PhyMode>,
    pub phy_addr: Option<u8>,
}

#[derive(Clone, Copy, Default)]
struct Frame { gem: bool, disabled: bool, mac: Option<[u8; 6]>, mode: Option<PhyMode>, phy_child: bool, reg: Option<u32> }

fn be32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

/// Walk a flattened device tree for a node compatible with `cdns,macb` or
/// `raspberrypi,rp1-gem` (status not "disabled"), returning its
/// `local-mac-address` (if valid, i.e. the firmware filled it in), `phy-mode`
/// and the `reg` of its `ethernet-phy@` child.
pub fn fdt_find_gem(dtb: &[u8]) -> GemDt {
    let mut out = GemDt::default();
    if be32(dtb, 0) != Some(0xd00d_feed) { return out; }
    let (st, ss) = match (be32(dtb, 8), be32(dtb, 12)) { (Some(a), Some(b)) => (a as usize, b as usize), _ => return out };
    let st_len = be32(dtb, 36).unwrap_or(0) as usize;
    let end = (st + st_len).min(dtb.len());
    let mut stack = [Frame::default(); 16];
    let mut depth = 0usize;
    let mut o = st;
    while o + 4 <= end {
        let tok = be32(dtb, o).unwrap_or(9);
        o += 4;
        match tok {
            1 => {
                let name_end = match dtb[o..end].iter().position(|&b| b == 0) { Some(p) => o + p, None => return out };
                let name = &dtb[o..name_end];
                o = (name_end + 1 + 3) & !3;
                if depth + 1 >= stack.len() { return out; }
                depth += 1;
                let parent_gem = stack[depth - 1].gem && !stack[depth - 1].disabled;
                stack[depth] = Frame { phy_child: parent_gem && name.starts_with(b"ethernet-phy"), ..Frame::default() };
            }
            2 => {
                if depth == 0 { return out; }
                let f = stack[depth];
                depth -= 1;
                if f.phy_child {
                    if let Some(r) = f.reg { if out.phy_addr.is_none() { out.phy_addr = Some(r as u8 & 31); } }
                }
                if f.gem && !f.disabled && !out.found {
                    out.found = true;
                    out.mac = f.mac.filter(mac_valid);
                    out.phy_mode = f.mode;
                    return out;
                }
            }
            3 => {
                let (len, nameoff) = match (be32(dtb, o), be32(dtb, o + 4)) { (Some(a), Some(b)) => (a as usize, b as usize), _ => return out };
                let v = match dtb.get(o + 8..o + 8 + len) { Some(v) => v, None => return out };
                o = (o + 8 + len + 3) & !3;
                let pn = match dtb.get(ss + nameoff..) { Some(s) => &s[..s.iter().position(|&b| b == 0).unwrap_or(s.len())], None => continue };
                let f = &mut stack[depth];
                match pn {
                    b"compatible" => {
                        if v.split(|&b| b == 0).any(|c| c == b"cdns,macb" || c == b"raspberrypi,rp1-gem") { f.gem = true; }
                    }
                    b"status" => { if v.starts_with(b"disabled") { f.disabled = true; } }
                    b"local-mac-address" if v.len() == 6 => {
                        let mut m = [0u8; 6];
                        m.copy_from_slice(v);
                        f.mac = Some(m);
                    }
                    b"phy-mode" | b"phy-connection-type" => {
                        f.mode = PhyMode::parse(&v[..v.iter().position(|&b| b == 0).unwrap_or(v.len())]);
                    }
                    b"reg" if v.len() == 4 => f.reg = be32(v, 0),
                    _ => {}
                }
            }
            4 => {}
            _ => break,
        }
    }
    out
}

// ── Statistics and diagnostics ──────────────────────────────────────────────

/// GEM statistics registers (Linux macb.h); they clear on read.
pub const STAT_OCT_TX_LO: usize = 0x0100;
pub const STAT_OCT_TX_HI: usize = 0x0104;
pub const STAT_TX_FRAMES: usize = 0x0108;
pub const STAT_TX_UNDERRUN: usize = 0x0134;
pub const STAT_OCT_RX_LO: usize = 0x0150;
pub const STAT_OCT_RX_HI: usize = 0x0154;
pub const STAT_RX_FRAMES: usize = 0x0158;
pub const STAT_RX_UNDERSIZE: usize = 0x0184;
pub const STAT_RX_JABBER: usize = 0x018c;
pub const STAT_RX_FCS: usize = 0x0190;
pub const STAT_RX_SYMBOL: usize = 0x0198;
pub const STAT_RX_ALIGN: usize = 0x019c;
pub const STAT_RX_RESOURCE: usize = 0x01a0;
pub const STAT_RX_OVERRUN: usize = 0x01a4;

/// What the MAC itself counted, accumulated across clear-on-read snapshots.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HwStats {
    pub tx_frames: u64,
    pub tx_octets: u64,
    pub tx_underruns: u64,
    pub rx_frames: u64,
    pub rx_octets: u64,
    pub rx_fcs: u64,
    pub rx_symbol: u64,
    pub rx_align: u64,
    pub rx_undersize: u64,
    pub rx_jabber: u64,
    pub rx_resource: u64,
    pub rx_overrun: u64,
}

impl HwStats {
    /// Reads every counter once (clearing it) and adds it in.
    pub fn accumulate(&mut self, r: &mut dyn Regs) {
        let oct = |r: &mut dyn Regs, lo: usize, hi: usize| r.r32(lo) as u64 | ((r.r32(hi) as u64 & 0xffff) << 32);
        self.tx_octets += oct(r, STAT_OCT_TX_LO, STAT_OCT_TX_HI);
        self.tx_frames += r.r32(STAT_TX_FRAMES) as u64;
        self.tx_underruns += r.r32(STAT_TX_UNDERRUN) as u64;
        self.rx_octets += oct(r, STAT_OCT_RX_LO, STAT_OCT_RX_HI);
        self.rx_frames += r.r32(STAT_RX_FRAMES) as u64;
        self.rx_undersize += r.r32(STAT_RX_UNDERSIZE) as u64;
        self.rx_jabber += r.r32(STAT_RX_JABBER) as u64;
        self.rx_fcs += r.r32(STAT_RX_FCS) as u64;
        self.rx_symbol += r.r32(STAT_RX_SYMBOL) as u64;
        self.rx_align += r.r32(STAT_RX_ALIGN) as u64;
        self.rx_resource += r.r32(STAT_RX_RESOURCE) as u64;
        self.rx_overrun += r.r32(STAT_RX_OVERRUN) as u64;
    }
    /// Line-level receive errors: the signature of a wrong RGMII RX skew.
    pub fn rx_line_errors(&self) -> u64 {
        self.rx_fcs + self.rx_symbol + self.rx_align + self.rx_undersize + self.rx_jabber
    }
}

/// Interrupt-status bits seen while the GEM runs without a wired interrupt
/// (the driver thread reads, and thereby clears, ISR).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IsrCounts { pub tcomp: u64, pub rcomp: u64, pub txubr: u64, pub rxubr: u64, pub hresp: u64, pub rovr: u64, pub txerr: u64 }

impl IsrCounts {
    pub fn add(&mut self, isr: u32) {
        let b = |m: u32| (isr & m != 0) as u64;
        self.tcomp += b(INT_TCOMP);
        self.rcomp += b(INT_RCOMP);
        self.txubr += b(INT_TXUBR);
        self.rxubr += b(INT_RXUBR);
        self.hresp += b(INT_HRESP);
        self.rovr += b(INT_ROVR);
        self.txerr += b(INT_TUND | INT_RLE | INT_TXERR);
    }
}

/// Receive-path verdict over one observation window (stats deltas).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxVerdict {
    /// The MAC counted error-free frames.
    Good,
    /// Only errored frames (FCS/symbol/alignment): RGMII RX timing is off.
    BadTiming,
    /// Nothing arrived at the MAC at all.
    Silent,
}

pub fn rx_verdict(delta: &HwStats) -> RxVerdict {
    if delta.rx_frames > 0 { RxVerdict::Good }
    else if delta.rx_line_errors() > 0 { RxVerdict::BadTiming }
    else { RxVerdict::Silent }
}

impl HwStats {
    pub fn delta(&self, earlier: &HwStats) -> HwStats {
        HwStats {
            tx_frames: self.tx_frames - earlier.tx_frames, tx_octets: self.tx_octets - earlier.tx_octets,
            tx_underruns: self.tx_underruns - earlier.tx_underruns, rx_frames: self.rx_frames - earlier.rx_frames,
            rx_octets: self.rx_octets - earlier.rx_octets, rx_fcs: self.rx_fcs - earlier.rx_fcs,
            rx_symbol: self.rx_symbol - earlier.rx_symbol, rx_align: self.rx_align - earlier.rx_align,
            rx_undersize: self.rx_undersize - earlier.rx_undersize, rx_jabber: self.rx_jabber - earlier.rx_jabber,
            rx_resource: self.rx_resource - earlier.rx_resource, rx_overrun: self.rx_overrun - earlier.rx_overrun,
        }
    }
}

impl PhyMode {
    /// The same TX delay with the PHY's RX delay flipped (the retry when the
    /// MAC sees only line errors).
    pub fn flip_rx_delay(self) -> PhyMode {
        match self {
            PhyMode::RgmiiId => PhyMode::RgmiiTxid,
            PhyMode::RgmiiTxid => PhyMode::RgmiiId,
            PhyMode::RgmiiRxid => PhyMode::Rgmii,
            PhyMode::Rgmii => PhyMode::RgmiiRxid,
        }
    }
}

/// BCM54xx counters and the delay bits as latched in the PHY.
pub const MII_BCM_RXERR: u8 = 0x12;
pub const MII_BCM_FALSE_CARRIER: u8 = 0x13;
pub const MII_BCM_RX_NOTOK: u8 = 0x14;
pub const MII_BCM_AUX_STATUS: u8 = 0x19;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PhyDiag { pub rx_err: u16, pub false_carrier: u16, pub rx_notok: u16, pub aux_status: u16, pub misc: u16, pub clk_ctl: u16 }

impl PhyDiag {
    pub fn rx_skew(&self) -> bool { self.misc & AUXCTL_MISC_RGMII_SKEW_EN != 0 }
    pub fn gtxclk_delay(&self) -> bool { self.clk_ctl & SHD_CLK_GTXCLK_EN != 0 }
}

pub fn phy_diag(r: &mut dyn Regs, phy: u8) -> Result<PhyDiag, MdioError> {
    let rx_err = mdio_read(r, phy, MII_BCM_RXERR)?;
    let false_carrier = mdio_read(r, phy, MII_BCM_FALSE_CARRIER)?;
    let rx_notok = mdio_read(r, phy, MII_BCM_RX_NOTOK)?;
    let aux_status = mdio_read(r, phy, MII_BCM_AUX_STATUS)?;
    mdio_write(r, phy, MII_BCM_AUXCTL, (AUXCTL_SHDW_MISC << 12) | 0x7)?;
    let misc = mdio_read(r, phy, MII_BCM_AUXCTL)?;
    mdio_write(r, phy, MII_BCM_SHD1C, (SHD_CLK_CTL & 0x1f) << 10)?;
    let clk_ctl = mdio_read(r, phy, MII_BCM_SHD1C)? & 0x3ff;
    Ok(PhyDiag { rx_err, false_carrier, rx_notok, aux_status, misc, clk_ctl })
}
