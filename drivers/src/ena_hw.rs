//! Amazon Elastic Network Adapter (ENA): the hardware-independent driver core.
//!
//! Everything but PCI probing, BAR mapping and the netdev glue lives here,
//! written against the small [`EnaBus`] trait (register access, DMA memory,
//! time) so the same code runs on the kernel's real BAR (`ena.rs`) and on the
//! Rust mock device in `drivers/ena-tests`. There is no QEMU ENA model; the
//! mock is the test vehicle until real EC2.
//!
//! What it implements (host placement, plus LLQ "device placement" when the
//! device offers it and BAR2 is mapped write-combining):
//!
//! * **Register access**, including *readless MMIO*: EC2 ENA VFs set PCI
//!   revision bit 0, which forbids plain BAR0 reads. Reads then go through
//!   `MMIO_REG_READ` (write `{req_id, offset}`, the device DMA-writes the
//!   value into a host buffer). [`Ena::new`] takes the flag.
//! * **Bring-up** in the order of Linux's `ena_device_init`: wait READY,
//!   device reset, program the admin SQ/CQ and the AENQ, mask the interrupt
//!   (everything is polled), `GET_FEATURE` device attributes / max queues /
//!   LLQ caps, `SET_FEATURE` AENQ groups and MTU, then CREATE_CQ -> CREATE_SQ
//!   for one TX and one RX queue pair (no MSI-X, no RSS).
//! * **Admin queue and AENQ**, both polled by phase bit.
//! * **Data path**: copy-in TX and copy-out RX through 2 KiB slots (one
//!   descriptor per frame), SQ tail doorbells, CQ head doorbells.
//! * **LLQ** (low-latency queue): TX descriptors and the first bytes of the
//!   frame are written as one 128/192/256-byte entry into BAR2, then the SQ
//!   doorbell; the rest of the frame is fetched from the host slot. Caps come
//!   from `GET_FEATURE LLQ`; the choice is [`llq_negotiate`]. Any refusal falls
//!   back to host placement.
//! * **Watchdog**: missing keep-alive for 6 s, a fatal DEV_STS, or an admin
//!   timeout resets the device and re-initialises (Linux `ena_fw_reset_device`).
//!
//! Field offsets follow the public ENA register/descriptor definitions
//! (`ena_regs_defs.h`, `ena_admin_defs.h`, `ena_eth_io_defs.h`) as documented
//! in the Linux/DPDK/FreeBSD drivers; no code is copied.

use alloc::vec::Vec;
use core::sync::atomic::{fence, Ordering};

pub const VENDOR: u16 = 0x1d0f;
/// ENA VF (what EC2 exposes), VF reserved, PF, PF reserved.
pub const DEVICE_IDS: [u16; 4] = [0xec20, 0xec21, 0x0ec2, 0x1ec2];

// ── registers (BAR0) ────────────────────────────────────────────────────────

pub mod reg {
    pub const VERSION: usize = 0x00;
    pub const CONTROLLER_VERSION: usize = 0x04;
    pub const CAPS: usize = 0x08;
    pub const CAPS_EXT: usize = 0x0c;
    pub const AQ_BASE_LO: usize = 0x10;
    pub const AQ_BASE_HI: usize = 0x14;
    pub const AQ_CAPS: usize = 0x18;
    pub const ACQ_BASE_LO: usize = 0x20;
    pub const ACQ_BASE_HI: usize = 0x24;
    pub const ACQ_CAPS: usize = 0x28;
    pub const AQ_DB: usize = 0x2c;
    pub const ACQ_TAIL: usize = 0x30;
    pub const AENQ_CAPS: usize = 0x34;
    pub const AENQ_BASE_LO: usize = 0x38;
    pub const AENQ_BASE_HI: usize = 0x3c;
    pub const AENQ_HEAD_DB: usize = 0x40;
    pub const INTR_MASK: usize = 0x4c;
    pub const DEV_CTL: usize = 0x54;
    pub const DEV_STS: usize = 0x58;
    pub const MMIO_REG_READ: usize = 0x5c;
    pub const MMIO_RESP_LO: usize = 0x60;
    pub const MMIO_RESP_HI: usize = 0x64;
}

pub const STS_READY: u32 = 1 << 0;
pub const STS_RESET_IN_PROGRESS: u32 = 1 << 3;
pub const STS_RESET_FINISHED: u32 = 1 << 4;
pub const STS_FATAL: u32 = 1 << 5;
pub const CTL_RESET: u32 = 1 << 0;
pub const CTL_RESET_REASON_SHIFT: u32 = 28;
/// `ENA_REGS_ADMIN_INTR_MASK`: mask the admin interrupt (polled).
pub const INTR_MASK_ADMIN: u32 = 1;
/// `ENA_ETH_IO_INTR_REG_INTR_UNMASK`: bit 30 of a CQ's interrupt-unmask register
/// (delays 0). The device masks the vector again after each interrupt.
pub const INTR_UNMASK: u32 = 1 << 30;
/// MSI-X table entry of the management / AENQ interrupt; IO queue pair `n` uses `1 + n`.
pub const MSIX_MGMT: u16 = 0;
pub const MSIX_IO0: u16 = 1;

/// Reset reasons (DEV_CTL bits 31:28).
pub mod reset_reason {
    pub const NORMAL: u32 = 0;
    pub const KEEP_ALIVE_TO: u32 = 1;
    pub const ADMIN_TO: u32 = 2;
    pub const DRIVER_INVALID_STATE: u32 = 8;
    pub const SHUTDOWN: u32 = 11;
    pub const GENERIC: u32 = 13;
}

pub mod op {
    pub const CREATE_SQ: u8 = 1;
    pub const DESTROY_SQ: u8 = 2;
    pub const CREATE_CQ: u8 = 3;
    pub const DESTROY_CQ: u8 = 4;
    pub const GET_FEATURE: u8 = 8;
    pub const SET_FEATURE: u8 = 9;
}

pub mod feat {
    pub const DEVICE_ATTRIBUTES: u8 = 1;
    pub const MAX_QUEUES_NUM: u8 = 2;
    pub const LLQ: u8 = 4;
    pub const MAX_QUEUES_EXT: u8 = 7;
    pub const MTU: u8 = 14;
    pub const AENQ_CONFIG: u8 = 26;
}

/// AENQ groups.
pub mod aenq {
    pub const LINK_CHANGE: u16 = 0;
    pub const FATAL_ERROR: u16 = 1;
    pub const WARNING: u16 = 2;
    pub const NOTIFICATION: u16 = 3;
    pub const KEEP_ALIVE: u16 = 4;
}

// ── sizing ──────────────────────────────────────────────────────────────────

pub const ADMIN_DEPTH: u16 = 32;
pub const AENQ_DEPTH: u16 = 32;
pub const ADMIN_ENTRY: usize = 64;
pub const AENQ_ENTRY: usize = 64;
pub const TX_DEPTH: u16 = 128;
pub const RX_DEPTH: u16 = 256;
/// One DMA slot per frame (MTU 1500 + headers fits with room).
pub const SLOT: usize = 2048;
pub const MIN_FRAME: usize = 60;
const TX_DESC: usize = 16;
const TX_CDESC: usize = 8;
const RX_DESC: usize = 16;
const RX_CDESC: usize = 16;
/// No keep-alive for this long means the device (or its firmware) is gone.
pub const KEEPALIVE_TIMEOUT_MS: u64 = 6000;
const ADMIN_TIMEOUT_MS_DEFAULT: u64 = 3000;

// ── bus and DMA memory ──────────────────────────────────────────────────────

/// A zeroed, page-aligned, physically contiguous DMA buffer (below 2^48).
#[derive(Clone, Copy)]
pub struct DmaBuf {
    pub va: *mut u8,
    pub pa: u64,
    pub len: usize,
}
unsafe impl Send for DmaBuf {}

impl DmaBuf {
    pub fn r8(&self, o: usize) -> u8 { debug_assert!(o < self.len); unsafe { core::ptr::read_volatile(self.va.add(o)) } }
    pub fn r16(&self, o: usize) -> u16 { debug_assert!(o + 2 <= self.len); unsafe { core::ptr::read_volatile(self.va.add(o) as *const u16) } }
    pub fn r32(&self, o: usize) -> u32 { debug_assert!(o + 4 <= self.len); unsafe { core::ptr::read_volatile(self.va.add(o) as *const u32) } }
    pub fn w8(&self, o: usize, v: u8) { debug_assert!(o < self.len); unsafe { core::ptr::write_volatile(self.va.add(o), v) } }
    pub fn w16(&self, o: usize, v: u16) { debug_assert!(o + 2 <= self.len); unsafe { core::ptr::write_volatile(self.va.add(o) as *mut u16, v) } }
    pub fn w32(&self, o: usize, v: u32) { debug_assert!(o + 4 <= self.len); unsafe { core::ptr::write_volatile(self.va.add(o) as *mut u32, v) } }
    pub fn copy_in(&self, o: usize, src: &[u8]) {
        assert!(o + src.len() <= self.len);
        unsafe { core::ptr::copy_nonoverlapping(src.as_ptr(), self.va.add(o), src.len()) }
    }
    pub fn copy_out(&self, o: usize, dst: &mut [u8]) {
        assert!(o + dst.len() <= self.len);
        unsafe { core::ptr::copy_nonoverlapping(self.va.add(o), dst.as_mut_ptr(), dst.len()) }
    }
    pub fn zero(&self) { unsafe { core::ptr::write_bytes(self.va, 0, self.len) } }
}

/// What the core needs from its environment.
pub trait EnaBus {
    /// Raw BAR0 register read. Never called while readless mode is on, except
    /// by the readless machinery itself (it only writes).
    fn read32(&self, off: usize) -> u32;
    fn write32(&self, off: usize, v: u32);
    /// Zeroed DMA memory, at most one page (`len <= 4096`) is guaranteed.
    fn dma_alloc(&self, len: usize) -> Option<DmaBuf>;
    fn udelay(&self, us: u64);
    fn now_ms(&self) -> u64;
    /// Length of the write-combining BAR2 mapping (LLQ device memory); 0 when
    /// there is none, which keeps the driver in host placement.
    fn llq_bar_len(&self) -> usize { 0 }
    /// Store `data` into BAR2 at `off` (write-combining memory).
    fn llq_write(&self, _off: usize, _data: &[u8]) {}
    /// Write barrier: drain write-combining buffers before a doorbell.
    fn wmb(&self) { fence(Ordering::SeqCst) }
}

// ── LLQ negotiation ─────────────────────────────────────────────────────────

/// `ena_admin_feature_llq_desc`: what the device supports (bit masks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LlqCaps {
    pub max_queues: u32,
    pub max_depth: u32,
    /// Header location: bit 0 inline in the entry, bit 1 separate buffer.
    pub loc: u16,
    /// Entry size: bit 0 128 B, bit 1 192 B, bit 2 256 B.
    pub entry: u16,
    /// Descriptors before the header: bit n = 1<<n descs (1, 2, 4, 8 -> bits 0..3 as values 1,2,4,8).
    pub before: u16,
    /// Stride: bit 0 single descriptor per entry, bit 1 multiple.
    pub stride: u16,
}

pub const LLQ_LOC_INLINE: u16 = 1;
const LLQ_FEATURE_BYTES: usize = 24;
/// Smallest pushed-header window worth using.
const LLQ_MIN_HEADER: usize = 64;

impl LlqCaps {
    /// Parse the feature body (`w16(o)`/`w32(o)` read the control buffer).
    pub fn parse(r32: impl Fn(usize) -> u32, r16: impl Fn(usize) -> u16) -> LlqCaps {
        LlqCaps { max_queues: r32(0), max_depth: r32(4), loc: r16(8), entry: r16(12), before: r16(16), stride: r16(20) }
    }
}

/// The LLQ layout the driver picked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LlqCfg {
    /// Bytes per ring entry: 128, 192 or 256.
    pub entry_size: usize,
    /// Descriptor slots before the pushed header: 1, 2, 4 or 8.
    pub before: usize,
    /// Descriptors-stride control: multiple descriptors per entry.
    pub stride_multi: bool,
}

impl LlqCfg {
    /// Bytes of an entry left for the pushed header.
    pub fn header_space(&self) -> usize { self.entry_size - self.before * 16 }
    fn entry_bit(&self) -> u16 { match self.entry_size { 128 => 1, 192 => 2, _ => 4 } }
}

/// Choose the entry layout from the device caps: inline header (the only
/// location implemented), 128 B entries preferred, then 256, then 192;
/// 2 descriptors before the header preferred (Linux's default), then 1, 4, 8;
/// multiple-descriptor stride preferred. `None` when any dimension has no
/// usable option (the caller then uses host placement).
pub fn llq_negotiate(c: &LlqCaps) -> Option<LlqCfg> {
    if c.max_queues == 0 || c.max_depth == 0 || c.loc & LLQ_LOC_INLINE == 0 { return None; }
    let stride_multi = if c.stride & 2 != 0 { true } else if c.stride & 1 != 0 { false } else { return None };
    for (bit, size) in [(1u16, 128usize), (4, 256), (2, 192)] {
        if c.entry & bit == 0 { continue; }
        for before in [2usize, 1, 4, 8] {
            if c.before & before as u16 == 0 || size < before * 16 + LLQ_MIN_HEADER { continue; }
            return Some(LlqCfg { entry_size: size, before, stride_multi });
        }
    }
    None
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Error {
    NoMem,
    /// Device never became ready / a wait ran out.
    Timeout(&'static str),
    /// Admin command completed with this non-zero status.
    Admin { opcode: u8, status: u8 },
    Unsupported(&'static str),
    Fatal,
}

#[derive(Default, Clone, Copy, Debug)]
pub struct Stats {
    pub tx_pkts: u64,
    pub tx_bytes: u64,
    pub tx_full: u64,
    pub rx_pkts: u64,
    pub rx_bytes: u64,
    pub rx_drops: u64,
    pub aenq_events: u64,
    pub keepalives: u64,
    pub resets: u32,
    pub admin_cmds: u64,
    pub admin_timeouts: u64,
    pub readless_reads: u64,
    pub readless_timeouts: u64,
}

/// Device facts learnt at bring-up, for the log and /proc/net/nic.
#[derive(Default, Clone, Copy, Debug)]
pub struct DevInfo {
    pub version: u32,
    pub controller_version: u32,
    pub caps: u32,
    pub dma_width: u32,
    pub max_mtu: u32,
    pub impl_id: u32,
    pub tx_depth: u16,
    pub rx_depth: u16,
    pub max_tx_depth: u32,
    pub max_rx_depth: u32,
    pub max_header: u32,
    pub enabled_groups: u32,
    /// First words of the LLQ feature, when the device answered it.
    pub llq: Option<[u32; 4]>,
    /// The LLQ layout in use (None = host placement).
    pub llq_cfg: Option<LlqCfg>,
}

// ── queue state ─────────────────────────────────────────────────────────────

struct IoQ {
    sq: DmaBuf,
    cq: DmaBuf,
    /// Pages holding the packet slots (`SLOT` bytes each).
    pages: Vec<DmaBuf>,
    depth: u16,
    /// Free-running producer / consumer counters; the ring position is
    /// `n & (depth - 1)` and the phase flips whenever that wraps to 0.
    sq_tail: u16,
    sq_phase: bool,
    cq_head: u16,
    cq_phase: bool,
    sq_db: usize,
    cq_db: usize,
    /// Interrupt-unmask register of the CQ (MSI-X mode), BAR0 offset; 0 when polled.
    cq_unmask: usize,
    free: Vec<u16>,
}

impl IoQ {
    fn slot(&self, i: u16) -> (DmaBuf, usize) {
        let per = 4096 / SLOT;
        (self.pages[i as usize / per], (i as usize % per) * SLOT)
    }
    fn slot_pa(&self, i: u16) -> u64 { let (p, o) = self.slot(i); p.pa + o as u64 }
    fn reset_state(&mut self) {
        self.sq.zero();
        self.cq.zero();
        self.sq_tail = 0;
        self.sq_phase = true;
        self.cq_head = 0;
        self.cq_phase = true;
    }
    fn mask(&self) -> u16 { self.depth - 1 }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Watchdog {
    Ok,
    /// A reset + re-init ran and succeeded.
    Reset(&'static str),
    /// A reset was needed and the re-init failed; the device is down.
    ResetFailed(&'static str),
}

pub struct Ena<B: EnaBus> {
    pub bus: B,
    readless: bool,
    resp: DmaBuf,
    read_seq: u16,
    ctrl: DmaBuf,
    aq: DmaBuf,
    aq_tail: u16,
    aq_phase: bool,
    acq: DmaBuf,
    acq_head: u16,
    acq_phase: bool,
    aenq: DmaBuf,
    aenq_head: u16,
    aenq_phase: bool,
    cmd_id: u16,
    admin_timeout_ms: u64,
    tx: IoQ,
    rx: IoQ,
    /// Bytes of the frame being assembled across RX descriptors.
    rx_acc: usize,
    rx_acc_buf: Vec<u8>,
    pub mac: [u8; 6],
    pub info: DevInfo,
    pub stats: Stats,
    link_up: bool,
    link_gen: u32,
    keepalive_ms: u64,
    admin_failed: bool,
    fatal_event: bool,
    running: bool,
    down_since: u64,
    /// DEV_CTL reason for the reset the next `init` performs.
    next_reason: u32,
    llq_allowed: bool,
    /// The device refused a DEV-placement SQ: stay on host placement.
    llq_refused: bool,
    llq: Option<LlqCfg>,
    llq_off: usize,
    /// Interrupt-driven completions requested (MSI-X vectors are set up by the glue).
    msix_allowed: bool,
    /// The device refused an interrupt-mode CQ (or gave no unmask register): polled.
    msix_refused: bool,
    /// The queues were created in interrupt mode.
    msix: bool,
}

fn page_bytes(p: &DmaBuf, off: usize, n: usize) -> Vec<u8> {
    let mut v = alloc::vec![0u8; n];
    p.copy_out(off, &mut v);
    v
}

fn alloc_pages(bus: &impl EnaBus, n: usize) -> Option<Vec<DmaBuf>> {
    let mut v = Vec::with_capacity(n);
    for _ in 0..n { v.push(bus.dma_alloc(4096)?); }
    Some(v)
}

fn mk_q<B: EnaBus>(bus: &B, depth: u16, sq_entry: usize, cq_entry: usize) -> Option<IoQ> {
    let sq = bus.dma_alloc(depth as usize * sq_entry)?;
    let cq = bus.dma_alloc(depth as usize * cq_entry)?;
    let pages = alloc_pages(bus, depth as usize * SLOT / 4096)?;
    Some(IoQ {
        sq, cq, pages, depth, sq_tail: 0, sq_phase: true, cq_head: 0, cq_phase: true,
        sq_db: 0, cq_db: 0, cq_unmask: 0, free: Vec::new(),
    })
}

impl<B: EnaBus> Ena<B> {
    /// Allocate every buffer (nothing touches the hardware yet). `readless`
    /// is PCI revision bit 0.
    pub fn new(bus: B, readless: bool) -> Result<Self, Error> {
        let nomem = Error::NoMem;
        let resp = bus.dma_alloc(4096).ok_or(nomem)?;
        let ctrl = bus.dma_alloc(4096).ok_or(nomem)?;
        let aq = bus.dma_alloc(ADMIN_DEPTH as usize * ADMIN_ENTRY).ok_or(nomem)?;
        let acq = bus.dma_alloc(ADMIN_DEPTH as usize * ADMIN_ENTRY).ok_or(nomem)?;
        let aenq = bus.dma_alloc(AENQ_DEPTH as usize * AENQ_ENTRY).ok_or(nomem)?;
        let tx = mk_q(&bus, TX_DEPTH, TX_DESC, TX_CDESC).ok_or(nomem)?;
        let rx = mk_q(&bus, RX_DEPTH, RX_DESC, RX_CDESC).ok_or(nomem)?;
        Ok(Ena {
            bus, readless, resp, read_seq: 0, ctrl, aq, aq_tail: 0, aq_phase: true, acq,
            acq_head: 0, acq_phase: true, aenq, aenq_head: 0, aenq_phase: true, cmd_id: 0,
            admin_timeout_ms: ADMIN_TIMEOUT_MS_DEFAULT, tx, rx, rx_acc: 0,
            rx_acc_buf: alloc::vec![0u8; SLOT * 2], mac: [0; 6], info: DevInfo::default(),
            stats: Stats::default(), link_up: false, link_gen: 0, keepalive_ms: 0,
            admin_failed: false, fatal_event: false, running: false, down_since: 0,
            next_reason: reset_reason::NORMAL, llq_allowed: true, llq_refused: false, llq: None, llq_off: 0,
            msix_allowed: false, msix_refused: false, msix: false,
        })
    }

    /// `ena.llq=0`: never use LLQ.
    pub fn set_llq_allowed(&mut self, on: bool) { self.llq_allowed = on; }
    pub fn llq_active(&self) -> bool { self.llq.is_some() }
    /// Create the IO CQs in interrupt mode (the caller has MSI-X vectors 0 and 1
    /// programmed). The device refusing falls back to polling on its own.
    pub fn set_msix_allowed(&mut self, on: bool) { self.msix_allowed = on; }
    /// The IO queues run in interrupt mode (CQ interrupts, admin interrupt unmasked).
    pub fn msix_active(&self) -> bool { self.msix }
    fn msix_wanted(&self) -> bool { self.msix_allowed && !self.msix_refused }
    /// Unmask the IO queue pair's interrupt (the device masks it after firing). Any
    /// context that holds the device; a no-op when polled.
    pub fn irq_unmask(&self) {
        if !self.msix { return; }
        self.wr(self.rx.cq_unmask, INTR_UNMASK);
        if self.tx.cq_unmask != self.rx.cq_unmask { self.wr(self.tx.cq_unmask, INTR_UNMASK); }
    }
    /// NAPI-style re-arm after draining: unmask, then report work that slipped in.
    pub fn irq_rearm(&mut self) -> bool {
        self.irq_unmask();
        self.rx_pending()
    }
    pub fn readless(&self) -> bool { self.readless }
    pub fn running(&self) -> bool { self.running }
    pub fn link_up(&self) -> bool { self.running && self.link_up }
    pub fn link_gen(&self) -> u32 { self.link_gen }

    // ── register access ─────────────────────────────────────────────────────

    /// Read a BAR0 register: direct, or via the readless mechanism.
    pub fn rd(&mut self, off: usize) -> u32 {
        if !self.readless { return self.bus.read32(off); }
        self.stats.readless_reads += 1;
        self.read_seq = self.read_seq.wrapping_add(1);
        let seq = self.read_seq;
        // A request id the device cannot have written yet.
        self.resp.w16(0, seq.wrapping_add(0xdead));
        self.resp.w16(2, 0);
        fence(Ordering::SeqCst);
        self.bus.write32(reg::MMIO_REG_READ, ((off as u32) << 16) | seq as u32);
        for _ in 0..25_000 {
            if self.resp.r16(0) == seq {
                fence(Ordering::SeqCst);
                if self.resp.r16(2) as usize == off { return self.resp.r32(4); }
                break;
            }
            self.bus.udelay(1);
        }
        self.stats.readless_timeouts += 1;
        !0
    }

    fn wr(&self, off: usize, v: u32) { self.bus.write32(off, v) }

    fn setup_readless(&mut self) {
        if !self.readless { return; }
        self.resp.zero();
        self.wr(reg::MMIO_RESP_LO, self.resp.pa as u32);
        self.wr(reg::MMIO_RESP_HI, (self.resp.pa >> 32) as u32);
    }

    fn wait_sts(&mut self, what: &'static str, timeout_ms: u64, f: impl Fn(u32) -> bool) -> Result<u32, Error> {
        let t0 = self.bus.now_ms();
        loop {
            let s = self.rd(reg::DEV_STS);
            if s != !0 && f(s) { return Ok(s); }
            if self.bus.now_ms().saturating_sub(t0) > timeout_ms { return Err(Error::Timeout(what)); }
            self.bus.udelay(500);
        }
    }

    /// Reset the device and leave it quiesced (DMA stopped).
    pub fn dev_reset(&mut self, reason: u32) -> Result<(), Error> {
        let caps = self.rd(reg::CAPS);
        let to = (((caps >> 1) & 0x1f) as u64).max(1) * 100;
        self.wr(reg::DEV_CTL, CTL_RESET | (reason << CTL_RESET_REASON_SHIFT));
        self.wait_sts("reset start", to, |s| s & STS_RESET_IN_PROGRESS != 0)?;
        self.wr(reg::DEV_CTL, 0);
        self.wait_sts("reset finish", to, |s| s & STS_RESET_IN_PROGRESS == 0)?;
        self.stats.resets += 1;
        // The reset cleared the readless response address.
        self.setup_readless();
        Ok(())
    }

    // ── admin queue ─────────────────────────────────────────────────────────

    fn admin_reset_state(&mut self) {
        self.aq.zero();
        self.acq.zero();
        self.aenq.zero();
        self.aq_tail = 0;
        self.aq_phase = true;
        self.acq_head = 0;
        self.acq_phase = true;
        self.aenq_head = 0;
        self.aenq_phase = true;
    }

    fn admin_program(&mut self) {
        self.admin_reset_state();
        self.wr(reg::AQ_BASE_LO, self.aq.pa as u32);
        self.wr(reg::AQ_BASE_HI, (self.aq.pa >> 32) as u32);
        self.wr(reg::AQ_CAPS, ADMIN_DEPTH as u32 | (ADMIN_ENTRY as u32) << 16);
        self.wr(reg::ACQ_BASE_LO, self.acq.pa as u32);
        self.wr(reg::ACQ_BASE_HI, (self.acq.pa >> 32) as u32);
        self.wr(reg::ACQ_CAPS, ADMIN_DEPTH as u32 | (ADMIN_ENTRY as u32) << 16);
        self.wr(reg::AENQ_BASE_LO, self.aenq.pa as u32);
        self.wr(reg::AENQ_BASE_HI, (self.aenq.pa >> 32) as u32);
        self.wr(reg::AENQ_CAPS, AENQ_DEPTH as u32 | (AENQ_ENTRY as u32) << 16);
        // Polled: mask the admin interrupt; MSI-X: vector 0 carries AENQ events.
        self.wr(reg::INTR_MASK, if self.msix_wanted() { 0 } else { INTR_MASK_ADMIN });
        // Hand the device the whole AENQ.
        self.wr(reg::AENQ_HEAD_DB, AENQ_DEPTH as u32);
    }

    /// Submit one admin command and wait (polling) for its completion. `cmd`
    /// is the 64-byte entry with the opcode-specific bytes filled in;
    /// `ctrl_flags` is OR-ed into the flags byte (bit 1: control buffer).
    /// Returns the completion entry.
    fn admin(&mut self, opcode: u8, ctrl_flags: u8, mut cmd: [u8; ADMIN_ENTRY]) -> Result<[u8; ADMIN_ENTRY], Error> {
        let id = self.cmd_id & 0x0fff;
        self.cmd_id = self.cmd_id.wrapping_add(1);
        cmd[0..2].copy_from_slice(&id.to_le_bytes());
        cmd[2] = opcode;
        cmd[3] = ctrl_flags | self.aq_phase as u8;
        let slot = (self.aq_tail & (ADMIN_DEPTH - 1)) as usize * ADMIN_ENTRY;
        self.aq.copy_in(slot, &cmd);
        fence(Ordering::Release);
        self.aq_tail = self.aq_tail.wrapping_add(1);
        if self.aq_tail & (ADMIN_DEPTH - 1) == 0 { self.aq_phase = !self.aq_phase; }
        self.wr(reg::AQ_DB, self.aq_tail as u32);
        self.stats.admin_cmds += 1;

        let t0 = self.bus.now_ms();
        let cslot = (self.acq_head & (ADMIN_DEPTH - 1)) as usize * ADMIN_ENTRY;
        loop {
            if self.acq.r8(cslot + 3) & 1 == self.acq_phase as u8 {
                fence(Ordering::Acquire);
                break;
            }
            if self.bus.now_ms().saturating_sub(t0) > self.admin_timeout_ms {
                self.stats.admin_timeouts += 1;
                self.admin_failed = true;
                return Err(Error::Timeout("admin command"));
            }
            self.bus.udelay(50);
        }
        let mut out = [0u8; ADMIN_ENTRY];
        self.acq.copy_out(cslot, &mut out);
        self.acq_head = self.acq_head.wrapping_add(1);
        if self.acq_head & (ADMIN_DEPTH - 1) == 0 { self.acq_phase = !self.acq_phase; }
        if u16::from_le_bytes([out[0], out[1]]) & 0x0fff != id {
            self.admin_failed = true;
            return Err(Error::Timeout("admin command id mismatch"));
        }
        if out[2] != 0 { return Err(Error::Admin { opcode, status: out[2] }); }
        Ok(out)
    }

    fn ctrl_fill(cmd: &mut [u8; ADMIN_ENTRY], pa: u64, len: u32) {
        cmd[4..8].copy_from_slice(&len.to_le_bytes());
        cmd[8..12].copy_from_slice(&(pa as u32).to_le_bytes());
        cmd[12..14].copy_from_slice(&((pa >> 32) as u16).to_le_bytes());
    }

    /// GET_FEATURE with the answer in the control buffer (`self.ctrl`).
    fn get_feature(&mut self, id: u8, version: u8) -> Result<(), Error> {
        self.ctrl.zero();
        let mut cmd = [0u8; ADMIN_ENTRY];
        Self::ctrl_fill(&mut cmd, self.ctrl.pa, 128);
        cmd[16] = 1; // select: current value
        cmd[17] = id;
        cmd[18] = version;
        self.admin(op::GET_FEATURE, 1 << 1, cmd)?;
        Ok(())
    }

    /// SET_FEATURE LLQ: the same structure with the `*_enabled` words filled.
    fn set_llq(&mut self, c: &LlqCaps, cfg: LlqCfg) -> Result<(), Error> {
        let mut p = [0u8; LLQ_FEATURE_BYTES];
        p[0..4].copy_from_slice(&c.max_queues.to_le_bytes());
        p[4..8].copy_from_slice(&c.max_depth.to_le_bytes());
        let w = |p: &mut [u8; LLQ_FEATURE_BYTES], o: usize, v: u16| p[o..o + 2].copy_from_slice(&v.to_le_bytes());
        w(&mut p, 8, c.loc);
        w(&mut p, 10, LLQ_LOC_INLINE);
        w(&mut p, 12, c.entry);
        w(&mut p, 14, cfg.entry_bit());
        w(&mut p, 16, c.before);
        w(&mut p, 18, cfg.before as u16);
        w(&mut p, 20, c.stride);
        w(&mut p, 22, if cfg.stride_multi { 2 } else { 1 });
        self.set_feature(feat::LLQ, &p)
    }

    fn set_feature(&mut self, id: u8, payload: &[u8]) -> Result<(), Error> {
        let mut cmd = [0u8; ADMIN_ENTRY];
        cmd[17] = id;
        cmd[20..20 + payload.len()].copy_from_slice(payload);
        self.admin(op::SET_FEATURE, 0, cmd)?;
        Ok(())
    }

    /// SET_FEATURE MTU.
    pub fn set_mtu(&mut self, mtu: u32) -> Result<(), Error> { self.set_feature(feat::MTU, &mtu.to_le_bytes()) }

    // ── bring-up ────────────────────────────────────────────────────────────

    /// Full bring-up (also the re-init after a reset). On error the device is
    /// left reset.
    pub fn init(&mut self) -> Result<(), Error> {
        self.running = false;
        let (mut l0, mut m0) = (self.llq_refused, self.msix_refused);
        let mut r = self.init_inner();
        // The device refused the device-placement queue or an interrupt-mode CQ:
        // redo the bring-up on host memory / polled.
        for _ in 0..2 {
            if r.is_ok() || !((self.llq_refused && !l0) || (self.msix_refused && !m0)) { break; }
            l0 = self.llq_refused; m0 = self.msix_refused;
            r = self.init_inner();
        }
        if r.is_err() {
            let _ = self.dev_reset(reset_reason::GENERIC);
        } else {
            self.keepalive_ms = self.bus.now_ms();
            self.admin_failed = false;
            self.fatal_event = false;
            self.running = true;
        }
        r
    }

    fn init_inner(&mut self) -> Result<(), Error> {
        self.setup_readless();
        self.info.version = self.rd(reg::VERSION);
        self.info.controller_version = self.rd(reg::CONTROLLER_VERSION);
        self.info.caps = self.rd(reg::CAPS);
        if self.info.version == !0 { return Err(Error::Timeout("version read")); }
        self.info.dma_width = (self.info.caps >> 8) & 0xff;
        if self.info.dma_width < 32 { return Err(Error::Unsupported("dma width")); }
        self.admin_timeout_ms = match (self.info.caps >> 16) & 0xf { 0 => ADMIN_TIMEOUT_MS_DEFAULT, n => n as u64 * 100 };
        self.wait_sts("device ready", 3000, |s| s & STS_READY != 0)?;
        let reason = core::mem::replace(&mut self.next_reason, reset_reason::NORMAL);
        self.dev_reset(reason)?;
        self.admin_program();

        // Device attributes: MAC, max MTU, address width.
        self.get_feature(feat::DEVICE_ATTRIBUTES, 0)?;
        self.info.impl_id = self.ctrl.r32(0);
        self.ctrl.copy_out(24, &mut self.mac);
        self.info.max_mtu = self.ctrl.r32(32);
        let aw = self.ctrl.r32(16);
        if aw != 0 && aw < self.info.dma_width { self.info.dma_width = aw; }

        // Queue limits: the extended descriptor first, the legacy one if refused.
        let (txd, rxd, hdr) = match self.get_feature(feat::MAX_QUEUES_EXT, 1) {
            Ok(()) => (self.ctrl.r32(20), self.ctrl.r32(28), self.ctrl.r32(36)),
            Err(Error::Admin { .. }) => {
                self.get_feature(feat::MAX_QUEUES_NUM, 0)?;
                let d = self.ctrl.r32(4);
                (d, d, self.ctrl.r32(24))
            }
            Err(e) => return Err(e),
        };
        self.info.max_tx_depth = txd;
        self.info.max_rx_depth = rxd;
        self.info.max_header = hdr;
        let pow2 = |want: u16, max: u32| -> u16 {
            let mut d = want;
            while d as u32 > max && d > 2 { d /= 2; }
            d
        };
        self.info.tx_depth = pow2(TX_DEPTH, txd.max(2));
        self.info.rx_depth = pow2(RX_DEPTH, rxd.max(2));

        // LLQ: parse the caps; use device placement when a layout fits, BAR2
        // is mapped and neither the knob nor an earlier refusal says no.
        self.llq = None;
        self.info.llq_cfg = None;
        let mut llq_caps = None;
        self.info.llq = match self.get_feature(feat::LLQ, 0) {
            Ok(()) => {
                let (c, d) = (&self.ctrl, &self.ctrl);
                llq_caps = Some(LlqCaps::parse(|o| c.r32(o), |o| d.r16(o)));
                Some([self.ctrl.r32(0), self.ctrl.r32(4), self.ctrl.r32(8), self.ctrl.r32(12)])
            }
            Err(_) => None,
        };
        if let (Some(caps), true) = (llq_caps, self.llq_allowed && !self.llq_refused && self.bus.llq_bar_len() > 0) {
            if let Some(cfg) = llq_negotiate(&caps) {
                self.set_llq(&caps, cfg)?;
                self.llq = Some(cfg);
                self.info.llq_cfg = Some(cfg);
                self.info.tx_depth = pow2(self.info.tx_depth, caps.max_depth);
            }
        }

        // AENQ groups: everything the device offers that we understand.
        self.get_feature(feat::AENQ_CONFIG, 0)?;
        let supported = self.ctrl.r32(0);
        let want = (1u32 << aenq::LINK_CHANGE) | (1 << aenq::FATAL_ERROR) | (1 << aenq::WARNING)
            | (1 << aenq::NOTIFICATION) | (1 << aenq::KEEP_ALIVE);
        let en = supported & want;
        self.info.enabled_groups = en;
        let mut p = [0u8; 8];
        p[4..8].copy_from_slice(&en.to_le_bytes());
        self.set_feature(feat::AENQ_CONFIG, &p)?;

        // MTU 1500; a device that refuses keeps its default.
        match self.set_mtu(1500) { Ok(()) | Err(Error::Admin { .. }) => {} Err(e) => return Err(e) }

        self.create_queues()?;
        let (rx_n, tx_n) = (self.info.rx_depth, self.info.tx_depth);
        // RX: post every slot (one short of full so head != tail is "work").
        self.rx.free.clear();
        for i in 0..rx_n - 1 { self.post_rx(i); }
        self.rx_acc = 0;
        fence(Ordering::Release);
        self.wr(self.rx.sq_db, self.rx.sq_tail as u32);
        self.tx.free = (0..tx_n).rev().collect();

        // No cable to pull on EC2: the link is up once the queues are.
        if !self.link_up { self.link_gen = self.link_gen.wrapping_add(1); }
        self.link_up = true;
        Ok(())
    }

    fn create_queues(&mut self) -> Result<(), Error> {
        self.msix = false;
        let (tx_n, rx_n) = (self.info.tx_depth, self.info.rx_depth);
        self.tx.depth = tx_n;
        self.rx.depth = rx_n;
        self.tx.reset_state();
        self.rx.reset_state();
        for dir in 0..2 {
            let (words, n, cq_pa, sq_pa) = if dir == 0 {
                ((TX_CDESC / 4) as u8, tx_n, self.tx.cq.pa, self.tx.sq.pa)
            } else {
                ((RX_CDESC / 4) as u8, rx_n, self.rx.cq.pa, self.rx.sq.pa)
            };
            // CREATE_CQ: polled (interrupt_mode_enabled = 0), or interrupt mode
            // (bit 5 of cq_caps_1) with the MSI-X table entry of the queue pair.
            let irq = self.msix_wanted();
            let mut cmd = [0u8; ADMIN_ENTRY];
            cmd[4] = if irq { 1 << 5 } else { 0 };
            cmd[5] = words & 0x1f;
            cmd[6..8].copy_from_slice(&n.to_le_bytes());
            if irq { cmd[8..12].copy_from_slice(&(MSIX_IO0 as u32).to_le_bytes()); }
            cmd[12..16].copy_from_slice(&(cq_pa as u32).to_le_bytes());
            cmd[16..18].copy_from_slice(&((cq_pa >> 32) as u16).to_le_bytes());
            let r = match self.admin(op::CREATE_CQ, 0, cmd) {
                Err(Error::Admin { .. }) if irq => { self.msix_refused = true; return Err(Error::Unsupported("MSI-X CQ refused")); }
                r => r?,
            };
            let cq_idx = u16::from_le_bytes([r[8], r[9]]);
            let cq_head_db = u32::from_le_bytes([r[16], r[17], r[18], r[19]]) as usize;
            let cq_unmask = u32::from_le_bytes([r[20], r[21], r[22], r[23]]) as usize;
            if irq && cq_unmask == 0 { self.msix_refused = true; return Err(Error::Unsupported("no CQ interrupt-unmask register")); }
            // CREATE_SQ: direction in bits 7:5, host placement (1), descriptor
            // completion policy (0), physically contiguous.
            // Device placement (3) for the LLQ TX queue: the ring lives in BAR2,
            // no host SQ address.
            let dev = dir == 0 && self.llq.is_some();
            let mut cmd = [0u8; ADMIN_ENTRY];
            cmd[4] = (if dir == 0 { 1 } else { 2 }) << 5;
            cmd[6] = if dev { 3 } else { 1 };
            cmd[7] = 1;
            cmd[8..10].copy_from_slice(&cq_idx.to_le_bytes());
            cmd[10..12].copy_from_slice(&n.to_le_bytes());
            if !dev {
                cmd[12..16].copy_from_slice(&(sq_pa as u32).to_le_bytes());
                cmd[16..18].copy_from_slice(&((sq_pa >> 32) as u16).to_le_bytes());
            }
            let r = match self.admin(op::CREATE_SQ, 0, cmd) {
                Err(Error::Admin { .. }) if dev => { self.llq_refused = true; return Err(Error::Unsupported("LLQ SQ refused")); }
                r => r?,
            };
            let sq_db = u32::from_le_bytes([r[12], r[13], r[14], r[15]]) as usize;
            if dev { self.llq_off = u32::from_le_bytes([r[16], r[17], r[18], r[19]]) as usize; }
            let q = if dir == 0 { &mut self.tx } else { &mut self.rx };
            q.cq_db = cq_head_db;
            q.sq_db = sq_db;
            q.cq_unmask = if irq { cq_unmask } else { 0 };
        }
        self.msix = self.msix_wanted();
        self.irq_unmask();
        Ok(())
    }

    // ── AENQ ────────────────────────────────────────────────────────────────

    /// Consume pending asynchronous events. Returns how many.
    pub fn poll_aenq(&mut self) -> u32 {
        let mut n = 0;
        loop {
            let slot = (self.aenq_head & (AENQ_DEPTH - 1)) as usize * AENQ_ENTRY;
            if self.aenq.r8(slot + 4) & 1 != self.aenq_phase as u8 { break; }
            fence(Ordering::Acquire);
            let group = self.aenq.r16(slot);
            let payload0 = self.aenq.r32(slot + 16);
            match group {
                aenq::LINK_CHANGE => {
                    let up = payload0 & 1 != 0;
                    if up && !self.link_up { self.link_gen = self.link_gen.wrapping_add(1); }
                    self.link_up = up;
                }
                aenq::KEEP_ALIVE => {
                    self.keepalive_ms = self.bus.now_ms();
                    self.stats.keepalives += 1;
                }
                aenq::FATAL_ERROR => self.fatal_event = true,
                _ => {}
            }
            self.stats.aenq_events += 1;
            n += 1;
            self.aenq_head = self.aenq_head.wrapping_add(1);
            if self.aenq_head & (AENQ_DEPTH - 1) == 0 { self.aenq_phase = !self.aenq_phase; }
        }
        if n > 0 { self.wr(reg::AENQ_HEAD_DB, self.aenq_head as u32); }
        n
    }

    // ── data path ───────────────────────────────────────────────────────────

    pub fn tx_ready(&mut self) -> bool {
        if !self.running { return false; }
        self.reap_tx();
        self.tx.free.len() > self.info.tx_depth as usize / 8
    }

    fn reap_tx(&mut self) {
        let mut n = 0;
        loop {
            let q = &mut self.tx;
            let slot = (q.cq_head & q.mask()) as usize * TX_CDESC;
            if q.cq.r8(slot + 3) & 1 != q.cq_phase as u8 { break; }
            fence(Ordering::Acquire);
            let id = q.cq.r16(slot);
            if id < q.depth { q.free.push(id); }
            q.cq_head = q.cq_head.wrapping_add(1);
            if q.cq_head & q.mask() == 0 { q.cq_phase = !q.cq_phase; }
            n += 1;
        }
        if n > 0 && self.tx.cq_db != 0 { self.wr(self.tx.cq_db, self.tx.cq_head as u32); }
    }

    /// Queue one frame. False when down, oversized, or the TX ring is full.
    pub fn send(&mut self, frame: &[u8]) -> bool {
        if !self.running || frame.is_empty() || frame.len() > SLOT { return false; }
        self.reap_tx();
        let id = match self.tx.free.pop() {
            Some(i) => i,
            None => { self.stats.tx_full += 1; return false; }
        };
        let len = frame.len().max(MIN_FRAME);
        if let Some(cfg) = self.llq { return self.send_llq(cfg, id, frame, len); }
        let (page, off) = self.tx.slot(id);
        page.copy_in(off, frame);
        if frame.len() < len { for i in frame.len()..len { page.w8(off + i, 0); } }
        let pa = self.tx.slot_pa(id);
        let q = &mut self.tx;
        let d = (q.sq_tail & q.mask()) as usize * TX_DESC;
        // word0: length | req_id[15:10] << 16 | phase << 24 | first | last | comp_req
        let w0 = len as u32 | ((id as u32 >> 10) & 0x3f) << 16 | (q.sq_phase as u32) << 24
            | 1 << 26 | 1 << 27 | 1 << 28;
        q.sq.w32(d + 4, (id as u32 & 0x3ff) << 22);
        q.sq.w32(d + 8, pa as u32);
        q.sq.w32(d + 12, (pa >> 32) as u32 & 0xffff);
        fence(Ordering::Release);
        q.sq.w32(d, w0);
        q.sq_tail = q.sq_tail.wrapping_add(1);
        if q.sq_tail & q.mask() == 0 { q.sq_phase = !q.sq_phase; }
        fence(Ordering::SeqCst);
        let (db, tail) = (q.sq_db, q.sq_tail);
        self.wr(db, tail as u32);
        self.stats.tx_pkts += 1;
        self.stats.tx_bytes += len as u64;
        true
    }

    /// LLQ transmit: one entry = [meta desc][packet desc][pad descs] followed by
    /// the first bytes of the frame (the pushed header); the remainder is read
    /// by the device from the host slot, right after the pushed bytes.
    fn send_llq(&mut self, cfg: LlqCfg, id: u16, frame: &[u8], len: usize) -> bool {
        let (page, off) = self.tx.slot(id);
        page.copy_in(off, frame);
        for i in frame.len()..len { page.w8(off + i, 0); }
        let mut hs = cfg.header_space();
        if self.info.max_header != 0 { hs = hs.min(self.info.max_header as usize); }
        let hl = len.min(hs);
        let pa = self.tx.slot_pa(id) + hl as u64;
        let phase = self.tx.sq_phase as u32;
        let mut e = [0u8; 256];
        let mut pkt = 0;
        if cfg.before >= 2 {
            // Meta descriptor, no offloads: meta bit 23, phase bit 24.
            e[0..4].copy_from_slice(&(1u32 << 23 | phase << 24).to_le_bytes());
            pkt = 1;
        }
        let d = pkt * 16;
        let w0 = len as u32 | ((id as u32 >> 10) & 0x3f) << 16 | phase << 24 | 1 << 26 | 1 << 27 | 1 << 28;
        e[d..d + 4].copy_from_slice(&w0.to_le_bytes());
        e[d + 4..d + 8].copy_from_slice(&((id as u32 & 0x3ff) << 22).to_le_bytes());
        e[d + 8..d + 12].copy_from_slice(&(pa as u32).to_le_bytes());
        e[d + 12..d + 16].copy_from_slice(&(((pa >> 32) as u32 & 0xffff) | (hl as u32) << 24).to_le_bytes());
        let h0 = cfg.before * 16;
        e[h0..h0 + hl].copy_from_slice(&page_bytes(&page, off, hl));
        let q = &mut self.tx;
        let at = self.llq_off + (q.sq_tail & q.mask()) as usize * cfg.entry_size;
        self.bus.llq_write(at, &e[..cfg.entry_size]);
        q.sq_tail = q.sq_tail.wrapping_add(1);
        if q.sq_tail & q.mask() == 0 { q.sq_phase = !q.sq_phase; }
        // Flush the write-combining buffers before the device is told.
        self.bus.wmb();
        let (db, tail) = (q.sq_db, q.sq_tail);
        self.wr(db, tail as u32);
        self.stats.tx_pkts += 1;
        self.stats.tx_bytes += len as u64;
        true
    }

    fn post_rx(&mut self, id: u16) {
        let pa = self.rx.slot_pa(id);
        let q = &mut self.rx;
        let d = (q.sq_tail & q.mask()) as usize * RX_DESC;
        q.sq.w16(d, SLOT as u16);
        q.sq.w16(d + 4, id);
        q.sq.w32(d + 8, pa as u32);
        q.sq.w16(d + 12, (pa >> 32) as u16);
        fence(Ordering::Release);
        // ctrl: phase | first (bit 2) | last (bit 3) | comp_req (bit 4)
        q.sq.w8(d + 3, q.sq_phase as u8 | 1 << 2 | 1 << 3 | 1 << 4);
        q.sq_tail = q.sq_tail.wrapping_add(1);
        if q.sq_tail & q.mask() == 0 { q.sq_phase = !q.sq_phase; }
    }

    /// Copy the next received frame into `out`.
    pub fn recv(&mut self, out: &mut [u8]) -> Option<usize> {
        if !self.running { return None; }
        loop {
            let q = &mut self.rx;
            let c = (q.cq_head & q.mask()) as usize * RX_CDESC;
            let w0 = q.cq.r32(c);
            if (w0 >> 24) & 1 != q.cq_phase as u32 { return None; }
            fence(Ordering::Acquire);
            let first = w0 >> 26 & 1 != 0;
            let last = w0 >> 27 & 1 != 0;
            let len = q.cq.r16(c + 4) as usize;
            let id = q.cq.r16(c + 6);
            q.cq_head = q.cq_head.wrapping_add(1);
            if q.cq_head & q.mask() == 0 { q.cq_phase = !q.cq_phase; }
            if id >= q.depth || len > SLOT {
                self.stats.rx_drops += 1;
                self.rx_acc = 0;
                continue;
            }
            if first { self.rx_acc = 0; }
            let (page, off) = q.slot(id);
            let room = self.rx_acc_buf.len() - self.rx_acc;
            let take = len.min(room);
            let (acc, buf) = (self.rx_acc, &mut self.rx_acc_buf);
            page.copy_out(off, &mut buf[acc..acc + take]);
            self.rx_acc += take;
            // Give the slot back and tell the device.
            self.post_rx(id);
            fence(Ordering::SeqCst);
            self.wr(self.rx.sq_db, self.rx.sq_tail as u32);
            if self.rx.cq_db != 0 { self.wr(self.rx.cq_db, self.rx.cq_head as u32); }
            if !last { continue; }
            let total = self.rx_acc;
            self.rx_acc = 0;
            if total > out.len() || total == 0 {
                self.stats.rx_drops += 1;
                continue;
            }
            out[..total].copy_from_slice(&self.rx_acc_buf[..total]);
            self.stats.rx_pkts += 1;
            self.stats.rx_bytes += total as u64;
            return Some(total);
        }
    }

    /// A completion is waiting (no side effects).
    pub fn rx_pending(&self) -> bool {
        let q = &self.rx;
        let c = (q.cq_head & q.mask()) as usize * RX_CDESC;
        (q.cq.r32(c) >> 24) & 1 == q.cq_phase as u32
    }

    // ── watchdog / reset ────────────────────────────────────────────────────

    /// Run periodically (~10 Hz): drain the AENQ, then check device health.
    /// A sick device is reset and re-initialised; a device that failed to
    /// come back is retried every second.
    pub fn watchdog(&mut self) -> Watchdog {
        let now = self.bus.now_ms();
        if !self.running {
            if now.saturating_sub(self.down_since) < 1000 { return Watchdog::Ok; }
            self.down_since = now;
            return match self.init() {
                Ok(()) => Watchdog::Reset("retry"),
                Err(_) => Watchdog::ResetFailed("retry"),
            };
        }
        self.poll_aenq();
        let sts = self.rd(reg::DEV_STS);
        let (why, reason) = if sts != !0 && sts & STS_FATAL != 0 || self.fatal_event {
            ("fatal error", reset_reason::GENERIC)
        } else if self.admin_failed {
            ("admin queue timeout", reset_reason::ADMIN_TO)
        } else if now.saturating_sub(self.keepalive_ms) > KEEPALIVE_TIMEOUT_MS {
            ("keep-alive timeout", reset_reason::KEEP_ALIVE_TO)
        } else {
            return Watchdog::Ok;
        };
        self.running = false;
        self.next_reason = reason;
        let r = self.init();
        if r.is_err() { self.down_since = self.bus.now_ms(); return Watchdog::ResetFailed(why); }
        // Same carrier, but DHCP must restart: queues were rebuilt.
        self.link_gen = self.link_gen.wrapping_add(1);
        Watchdog::Reset(why)
    }

    /// Stop the device (poweroff / kexec): reset it so it stops DMA.
    pub fn shutdown(&mut self) -> Result<(), Error> {
        self.running = false;
        self.dev_reset(reset_reason::SHUTDOWN)
    }
}
