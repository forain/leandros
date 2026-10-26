//! Raspberry Pi 5 Ethernet: the Cadence GEM (`raspberrypi,rp1-gem`,
//! `cdns,macb`) inside RP1, with a Broadcom BCM54213PE PHY on its MDIO bus,
//! RGMII with both delays in the PHY (`phy-mode = "rgmii-id"`).
//!
//! # Seam with the RP1 bring-up (lane pipcie)
//!
//! This driver does not touch PCIe. The RP1 probe hands it the controller:
//!
//! ```ignore
//! // RP1 BAR1 is RP1's peripheral window (DT 0xc0_4000_0000); the GEM is at
//! // BAR1 + RP1_GEM_OFFSET, RP1_GEM_SIZE bytes, mapped Uncached (Device).
//! let va = mm::kva::ioremap(bar1_phys + drivers::macb::RP1_GEM_OFFSET,
//!                           drivers::macb::RP1_GEM_SIZE, mm::kva::CacheMode::Uncached)?;
//! drivers::macb::rp1_attach(drivers::macb::Rp1Gem {
//!     mmio_va: va,
//!     dma_offset: drivers::macb::RP1_DMA_OFFSET, // CPU phys -> RP1 bus address
//!     irq_wired: true, // RP1 interrupt RP1_GEM_IRQ (6, level-high) calls rp1_isr
//! });
//! // ... and from the RP1 interrupt demux, for RP1 IRQ 6:
//! drivers::macb::rp1_isr(0);
//! ```
//!
//! `rp1_attach` must run before `netdev::init` (kernel `init.rs`), which
//! calls [`init`]: that is where the controller is brought up and `eth0`
//! registered. Without an attach, or if the GEM does not answer (MID reads
//! 0 / all-ones), nothing happens beyond one `[macb] ...` line. Without an
//! interrupt (`irq_wired: false`) the net daemon polls the ring.
//!
//! RP1's clocks (pclk/hclk/tsu/`clk_eth` tx_clk) and the PHY reset GPIO
//! (`phy-reset-gpios`, RP1 GPIO 32) are left as the firmware set them; the
//! PHY gets a BMCR soft reset over MDIO. With MACB_CAPS_CLK_HW_CHG the GEM
//! switches the RGMII TX clock itself on a speed change.
//!
//! # DMA
//!
//! RP1 reaches system RAM at bus address `phys + 0x10_0000_0000` (pcie2
//! `dma-ranges`), hence 64-bit descriptors (DMACFG.ADDR64). The descriptor
//! rings live below 4 GiB physical so all queues share one TBQPH/RBQPH. The
//! BCM2712 PCIe does not snoop CPU caches: rings are mapped Normal
//! Non-cacheable (`kva::vmap`), packet buffers get explicit cache
//! maintenance (`mm::dma_cache`). On a coherent platform both are plain.

use alloc::format;
use alloc::string::String;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

use crate::macb_hw::{self as hw, Desc, Link, LinkWatch, PhyMode, Regs, RxEvent, RxRing, TxRing};

/// The GEM's offset inside RP1 BAR1 (DT `ethernet@100000`, reg 0xc0_4010_0000).
pub const RP1_GEM_OFFSET: u64 = 0x10_0000;
pub const RP1_GEM_SIZE: usize = 0x4000;
/// The GEM's interrupt number on RP1's interrupt controller (DT `interrupts = <6 4>`).
pub const RP1_GEM_IRQ: u32 = 6;
/// What RP1 adds to a CPU physical address to reach RAM (pcie2 dma-ranges).
pub const RP1_DMA_OFFSET: u64 = 0x10_0000_0000;

/// What the RP1 probe gives this driver.
#[derive(Clone, Copy, Debug)]
pub struct Rp1Gem {
    /// Kernel VA of the GEM registers (Device / Uncached mapping).
    pub mmio_va: usize,
    /// Added to a CPU physical address to form the GEM's DMA address.
    pub dma_offset: u64,
    /// The RP1 interrupt demux will call [`rp1_isr`] for RP1 IRQ 6.
    pub irq_wired: bool,
}

const NUM_RX: usize = 256;
const NUM_TX: usize = 256;
const BUF_SIZE: usize = 2048;
const MAX_FRAME: usize = 1514;
const ETH_ZLEN: usize = 60;
const PAGE: usize = 4096;

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
}

fn log(msg: &str) {
    let mut s = String::with_capacity(msg.len() + 10);
    s.push_str("[macb] ");
    s.push_str(msg);
    s.push('\n');
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

macro_rules! mlog { ($($a:tt)*) => { log(&format!($($a)*)) }; }

fn now_ms() -> u64 { sched::monotonic_ns() / 1_000_000 }

#[derive(Clone, Copy)]
struct Mmio(usize);

impl Regs for Mmio {
    fn r32(&self, off: usize) -> u32 { unsafe { core::ptr::read_volatile((self.0 + off) as *const u32) } }
    fn w32(&mut self, off: usize, v: u32) { unsafe { core::ptr::write_volatile((self.0 + off) as *mut u32, v) } }
    fn delay_us(&mut self, us: u32) { sched::kthread::udelay(us as u64) }
}

// ── state ───────────────────────────────────────────────────────────────────

static ATTACHED: Mutex<Option<(Rp1Gem, hw::GemDt)>> = Mutex::new(None);
static BASE: AtomicUsize = AtomicUsize::new(0);
static PRESENT: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);
static IRQ_ACTIVE: AtomicBool = AtomicBool::new(false);
/// up | full << 1 | speed << 16
static LINK: AtomicU32 = AtomicU32::new(0);
static LINK_GEN: AtomicU32 = AtomicU32::new(0);
static MAC: Mutex<[u8; 6]> = Mutex::new([0; 6]);
static INFO: Mutex<Option<String>> = Mutex::new(None);
static STATE: Mutex<&'static str> = Mutex::new("not attached");

static IRQS: AtomicU64 = AtomicU64::new(0);
static IRQ_ERR: AtomicU64 = AtomicU64::new(0);
static RX_OVR: AtomicU64 = AtomicU64::new(0);
static RX_UBR: AtomicU64 = AtomicU64::new(0);
static HRESP: AtomicU64 = AtomicU64::new(0);
static RX_PKTS: AtomicU64 = AtomicU64::new(0);
static RX_BYTES: AtomicU64 = AtomicU64::new(0);
static RX_FRAGS: AtomicU64 = AtomicU64::new(0);
static TX_PKTS: AtomicU64 = AtomicU64::new(0);
static TX_BYTES: AtomicU64 = AtomicU64::new(0);
static TX_FULL: AtomicU64 = AtomicU64::new(0);

struct Rings {
    rx: RxRing,
    tx: TxRing,
    /// Linear-map VAs of the packet buffers (CPU side).
    rx_va: alloc::vec::Vec<usize>,
    tx_va: alloc::vec::Vec<usize>,
}
static RINGS: Mutex<Option<Rings>> = Mutex::new(None);
/// Serialises NCR read-modify-writes (TSTART from senders vs RE/TE from the
/// link watcher). Lock order: RINGS, then NCR_LOCK.
static NCR_LOCK: Mutex<()> = Mutex::new(());

fn ncr_set(m: &mut Mmio, set: u32, clear: u32) {
    let _g = NCR_LOCK.lock();
    let v = m.r32(hw::NCR);
    m.w32(hw::NCR, (v & !clear) | set);
}

fn set_state(s: &'static str) { *STATE.lock() = s; }

/// Called by the RP1 probe (see the module docs). Reads what the DTB says
/// about the GEM now, while the boot DTB is certainly mapped.
pub fn rp1_attach(res: Rp1Gem) {
    let dt = unsafe { boot::device_tree::recorded_dtb() }.map(hw::fdt_find_gem).unwrap_or_default();
    mlog!("attached: regs at {:#x}, dma offset {:#x}, irq {}, dtb node {}", res.mmio_va, res.dma_offset,
          if res.irq_wired { "RP1 6" } else { "none (polled)" }, if dt.found { "found" } else { "missing" });
    *ATTACHED.lock() = Some((res, dt));
    set_state("attached");
}

fn cmdline_val<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_whitespace().find_map(|w| w.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
}

/// Bring the controller up (from `netdev::init`). Inert without an attach.
pub fn init(cmdline: &str) {
    let (res, dt) = match ATTACHED.lock().take() { Some(a) => a, None => return };
    if cmdline_val(cmdline, "macb") == Some("0") { log("disabled (macb=0)"); set_state("disabled"); return; }
    let mut m = Mmio(res.mmio_va);
    let mid = m.r32(hw::MID);
    if mid == 0 || mid == 0xffff_ffff || (mid >> 16) & 0xfff < 2 {
        mlog!("no GEM answers (MID {:#010x}); Ethernet stays off", mid);
        set_state("no gem");
        return;
    }
    let dcfg1 = m.r32(hw::DCFG1);
    let dcfg6 = m.r32(hw::DCFG6);
    let queues = hw::num_queues(dcfg6);
    if dcfg6 & hw::DCFG6_DAW64 == 0 && (res.dma_offset >> 32) != 0 {
        mlog!("GEM has no 64-bit DMA (DCFG6 {:#x}) but RAM is at bus {:#x}; Ethernet stays off", dcfg6, res.dma_offset);
        set_state("no 64-bit dma");
        return;
    }

    // Quiesce: RX/TX off, every interrupt masked and acked, status cleared.
    m.w32(hw::NCR, 0);
    m.w32(hw::IDR, hw::INT_ALL);
    for q in 1..queues { m.w32(hw::IDRQ1 + 4 * (q - 1), hw::INT_ALL); }
    let _ = m.r32(hw::ISR);
    m.w32(hw::ISR, hw::INT_ALL);
    m.w32(hw::TSR, 0xffff_ffff);
    m.w32(hw::RSR, 0xffff_ffff);
    m.w32(hw::NCFGR, hw::ncfgr_base(dcfg1));
    m.w32(hw::NCR, hw::NCR_MPE | hw::NCR_CLRSTAT);
    m.w32(hw::USRIO, 1); // RGMII
    m.w32(hw::AMP, 8 | (8 << 8) | (1 << 16));

    // MAC: DTB (firmware fills local-mac-address), else what the boot
    // firmware left in SA1, else a locally administered one.
    let sa = hw::mac_from_sa(m.r32(hw::SA1B), m.r32(hw::SA1T));
    let (mac, src) = if let Some(mac) = dt.mac { (mac, "dtb") }
        else if hw::mac_valid(&sa) { (sa, "SA1 registers") }
        else {
            let t = (sched::monotonic_ns() as u32).to_le_bytes();
            ([0x02, 0x52, 0x50, t[0], t[1], t[2]], "generated")
        };
    let (b, t) = hw::sa_from_mac(&mac);
    m.w32(hw::SA1B, b);
    m.w32(hw::SA1T, t);
    m.w32(hw::HRB, 0xffff_ffff);
    m.w32(hw::HRT, 0xffff_ffff);
    *MAC.lock() = mac;
    mlog!("GEM MID {:#x}, {} queue(s), MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (from {})", mid, queues,
          mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], src);

    let (rings, rx_dma, tx_dma, dummy_dma) = match alloc_rings(res.dma_offset) {
        Some(r) => r,
        None => { log("out of DMA memory for the rings; Ethernet stays off"); set_state("no memory"); return; }
    };
    m.w32(hw::DMACFG, hw::dmacfg_value(m.r32(hw::DMACFG), BUF_SIZE, true));
    m.w32(hw::RBQPH, (rx_dma >> 32) as u32);
    m.w32(hw::TBQPH, (tx_dma >> 32) as u32);
    m.w32(hw::RBQP, rx_dma as u32);
    m.w32(hw::TBQP, tx_dma as u32);
    // Unused priority queues point at a dummy USED|WRAP descriptor.
    for q in 1..queues {
        m.w32(hw::TBQPQ1 + 4 * (q - 1), dummy_dma as u32);
        m.w32(hw::RBQPQ1 + 4 * (q - 1), (dummy_dma + hw::DESC_SIZE as u64) as u32);
    }
    *RINGS.lock() = Some(rings);
    BASE.store(res.mmio_va, Ordering::SeqCst);
    if res.irq_wired {
        IRQ_ACTIVE.store(true, Ordering::SeqCst);
        m.w32(hw::IER, hw::INT_RX_MASK | hw::INT_ERR_MASK);
    }
    *INFO.lock() = Some(format!("gem_mid: {:#x}\nqueues: {}\ndcfg1: {:#x}\ndcfg6: {:#x}\nmac_source: {}\nirq: {}\n",
        mid, queues, dcfg1, dcfg6, src, if res.irq_wired { "RP1 6" } else { "polled" }));
    PRESENT.store(true, Ordering::SeqCst);
    RUNNING.store(true, Ordering::SeqCst);
    set_state("phy");
    let phy_mode = dt.phy_mode.unwrap_or(PhyMode::RgmiiId);
    let phy_addr = dt.phy_addr.unwrap_or(1);
    if sched::kthread::spawn("macb", move || driver_thread(Mmio(res.mmio_va), phy_addr, phy_mode)).is_none() {
        log("could not start the driver thread; link stays down");
        set_state("no thread");
    }
}

fn zeroed_page() -> Option<usize> {
    let p = mm::buddy::alloc_below(0, 1 << 32)?;
    let va = mm::phys_to_virt(p);
    unsafe { core::ptr::write_bytes(va as *mut u8, 0, PAGE); }
    mm::dma_cache::sync_for_device(va, PAGE);
    Some(p)
}

/// A descriptor page: zeroed, cleaned out of the cache, then (non-coherent)
/// mapped Normal Non-cacheable. Returns (CPU pointer, physical address).
fn desc_page() -> Option<(*mut Desc, usize)> {
    let p = zeroed_page()?;
    let va = if mm::dma_cache::coherent() { mm::phys_to_virt(p) }
             else { mm::kva::vmap(&[p as u64], mm::kva::CacheMode::WriteCombining)? };
    Some((va as *mut Desc, p))
}

fn alloc_rings(off: u64) -> Option<(Rings, u64, u64, u64)> {
    let (rxd, rxp) = desc_page()?;
    let (txd, txp) = desc_page()?;
    let (dmy, dmyp) = desc_page()?;
    unsafe {
        core::ptr::write_volatile(dmy, Desc { addr: 0, ctrl: hw::TX_USED | hw::TX_WRAP, addr_hi: 0, rsvd: 0 });
        core::ptr::write_volatile(dmy.add(1), Desc { addr: hw::RX_USED | hw::RX_WRAP, ctrl: 0, addr_hi: 0, rsvd: 0 });
    }
    let per_page = PAGE / BUF_SIZE;
    let mut rx_phys = alloc::vec::Vec::with_capacity(NUM_RX);
    for _ in 0..NUM_RX / per_page {
        let p = zeroed_page()?;
        for k in 0..per_page { rx_phys.push(p + k * BUF_SIZE); }
    }
    let mut tx_phys = alloc::vec::Vec::with_capacity(NUM_TX);
    for _ in 0..NUM_TX / per_page {
        let p = zeroed_page()?;
        for k in 0..per_page { tx_phys.push(p + k * BUF_SIZE); }
    }
    let rx = unsafe { RxRing::init(rxd, NUM_RX, &|i| rx_phys[i] as u64 + off) };
    let tx = unsafe { TxRing::init(txd, NUM_TX, &|i| tx_phys[i] as u64 + off) };
    let rx_va = rx_phys.iter().map(|&p| mm::phys_to_virt(p)).collect();
    let tx_va = tx_phys.iter().map(|&p| mm::phys_to_virt(p)).collect();
    Some((Rings { rx, tx, rx_va, tx_va }, rxp as u64 + off, txp as u64 + off, dmyp as u64 + off))
}

fn driver_thread(mut m: Mmio, want: u8, mode: PhyMode) {
    let (addr, id) = match hw::find_phy(&mut m, want) {
        Ok(x) => x,
        Err(e) => { mlog!("no PHY on MDIO ({:?}); link stays down", e); set_state("no phy"); return; }
    };
    let known = if id & !0xf == hw::BCM54213PE_ID { "BCM54213PE" } else { "unknown PHY" };
    match hw::phy_setup(&mut m, addr, mode) {
        Ok(()) => mlog!("PHY {} id {:#010x} at {}, {}, autoneg started", known, id, addr, mode.name()),
        Err(e) => { mlog!("PHY setup failed ({:?}); link stays down", e); set_state("phy failed"); return; }
    }
    if let Some(i) = INFO.lock().as_mut() { let _ = write!(i, "phy: {} {:#010x} @{} {}\n", known, id, addr, mode.name()); }
    set_state("no link");
    let mut w = LinkWatch::default();
    let t0 = now_ms();
    let mut said_nolink = false;
    loop {
        let res = { let _g = NCR_LOCK.lock(); w.poll(&mut m, addr) };
        match res {
            Ok(Some(l)) => on_link(&mut m, l),
            Ok(None) => {}
            Err(e) => mlog!("MDIO error {:?}", e),
        }
        if !w.cur.up && !said_nolink && now_ms() - t0 > 10_000 {
            log("no link after 10 s (cable?)");
            said_nolink = true;
        }
        // TX doorbell retry: frames posted while the controller was halting.
        if let Some(r) = RINGS.lock().as_mut() {
            r.tx.reclaim();
            if r.tx.in_flight() > 0 { ncr_set(&mut m, hw::NCR_TSTART, 0); }
        }
        sched::kthread::msleep(if w.cur.up { 1000 } else { 250 });
    }
}

fn on_link(m: &mut Mmio, l: Link) {
    // LinkWatch already programmed NCFGR and enabled RX/TX on link-up.
    if l.up {
        LINK.store(1 | ((l.full as u32) << 1) | (l.speed << 16), Ordering::SeqCst);
        LINK_GEN.fetch_add(1, Ordering::SeqCst);
        set_state("link up");
        mlog!("link up {} Mbit/s {} duplex (NCFGR {:#010x})", l.speed, if l.full { "full" } else { "half" }, m.r32(hw::NCFGR));
    } else {
        ncr_set(m, 0, hw::NCR_RE | hw::NCR_TE);
        LINK.store(0, Ordering::SeqCst);
        set_state("no link");
        log("link down");
    }
    crate::netdev::kick();
}

/// RP1 IRQ 6 handler (called by the RP1 interrupt demux).
pub extern "C" fn rp1_isr(_ctx: usize) -> crate::irq::IrqReturn {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 || !IRQ_ACTIVE.load(Ordering::Relaxed) { return crate::irq::IrqReturn::None; }
    let mut m = Mmio(base);
    let st = m.r32(hw::ISR);
    if st == 0 { return crate::irq::IrqReturn::None; }
    m.w32(hw::ISR, st); // clear-on-write configurations; ignored when clear-on-read
    IRQS.fetch_add(1, Ordering::Relaxed);
    if st & hw::INT_ROVR != 0 { RX_OVR.fetch_add(1, Ordering::Relaxed); }
    if st & hw::INT_RXUBR != 0 { RX_UBR.fetch_add(1, Ordering::Relaxed); }
    if st & hw::INT_HRESP != 0 { HRESP.fetch_add(1, Ordering::Relaxed); }
    if st & (hw::INT_TUND | hw::INT_RLE | hw::INT_TXERR) != 0 { IRQ_ERR.fetch_add(1, Ordering::Relaxed); }
    if st & hw::INT_RX_MASK != 0 {
        // NAPI style: RX stays masked until the daemon drained the ring.
        m.w32(hw::IDR, hw::INT_RX_MASK);
        crate::netdev::kick_from_irq();
    }
    crate::irq::IrqReturn::Handled
}

// ── netdev entry points ─────────────────────────────────────────────────────

pub fn present() -> bool { PRESENT.load(Ordering::Relaxed) }
pub fn state() -> &'static str { *STATE.lock() }
pub fn mac_address() -> Option<[u8; 6]> { if present() { Some(*MAC.lock()) } else { None } }
pub fn link() -> (bool, u32, bool) {
    let l = LINK.load(Ordering::Relaxed);
    (l & 1 != 0, l >> 16, l & 2 != 0)
}
pub fn link_generation() -> u32 { LINK_GEN.load(Ordering::Relaxed) }
pub fn rx_irq_driven() -> bool { IRQ_ACTIVE.load(Ordering::Relaxed) && RUNNING.load(Ordering::Relaxed) }

pub fn send_packet(buf: &[u8]) -> bool {
    if !RUNNING.load(Ordering::Acquire) || buf.is_empty() || buf.len() > MAX_FRAME { return false; }
    let mut m = Mmio(BASE.load(Ordering::Relaxed));
    let mut g = RINGS.lock();
    let r = match g.as_mut() { Some(r) => r, None => return false };
    r.tx.reclaim();
    let idx = match r.tx.slot() {
        Some(i) => i,
        None => { TX_FULL.fetch_add(1, Ordering::Relaxed); return false; }
    };
    let len = buf.len().max(ETH_ZLEN);
    let dst = r.tx_va[idx];
    unsafe {
        core::ptr::copy_nonoverlapping(buf.as_ptr(), dst as *mut u8, buf.len());
        if buf.len() < ETH_ZLEN { core::ptr::write_bytes((dst as *mut u8).add(buf.len()), 0, ETH_ZLEN - buf.len()); }
    }
    mm::dma_cache::sync_for_device(dst, len);
    r.tx.post(idx, len);
    ncr_set(&mut m, hw::NCR_TSTART, 0);
    TX_PKTS.fetch_add(1, Ordering::Relaxed);
    TX_BYTES.fetch_add(buf.len() as u64, Ordering::Relaxed);
    true
}

pub fn tx_ready() -> bool {
    RINGS.lock().as_mut().map(|r| { r.tx.reclaim(); r.tx.in_flight() < NUM_TX - 8 }).unwrap_or(false)
}

pub fn poll_receive(out: &mut [u8]) -> Option<usize> {
    if !RUNNING.load(Ordering::Acquire) { return None; }
    let mut g = RINGS.lock();
    let r = g.as_mut()?;
    r.tx.reclaim();
    for _ in 0..NUM_RX {
        match r.rx.next() {
            RxEvent::Empty => return None,
            RxEvent::Frame { idx, len } => {
                let n = len.min(out.len()).min(BUF_SIZE);
                let src = r.rx_va[idx];
                mm::dma_cache::sync_for_cpu(src, n);
                unsafe { core::ptr::copy_nonoverlapping(src as *const u8, out.as_mut_ptr(), n); }
                r.rx.release(idx);
                RX_PKTS.fetch_add(1, Ordering::Relaxed);
                RX_BYTES.fetch_add(n as u64, Ordering::Relaxed);
                return Some(n);
            }
            RxEvent::Fragment { idx } => { RX_FRAGS.fetch_add(1, Ordering::Relaxed); r.rx.release(idx); }
        }
    }
    None
}

/// Re-enable the RX interrupt after a drain; true if a frame is already waiting.
pub fn rx_rearm() -> bool {
    if !RUNNING.load(Ordering::Acquire) { return false; }
    let g = RINGS.lock();
    let r = match g.as_ref() { Some(r) => r, None => return false };
    if IRQ_ACTIVE.load(Ordering::Relaxed) {
        core::sync::atomic::fence(Ordering::SeqCst);
        Mmio(BASE.load(Ordering::Relaxed)).w32(hw::IER, hw::INT_RX_MASK);
    }
    !matches!(r.rx.next(), RxEvent::Empty)
}

pub fn info_lines(out: &mut String) {
    let _ = write!(out, "state: {}\n", state());
    if let Some(i) = INFO.lock().as_ref() { out.push_str(i); }
    let (up, sp, fd) = link();
    let _ = write!(out, "link: {} {} {}\nlink_gen: {}\n", if up { "up" } else { "down" }, sp,
                   if fd { "full" } else { "half" }, link_generation());
    let _ = write!(out, "rx_packets: {}\nrx_bytes: {}\nrx_frags: {}\ntx_packets: {}\ntx_bytes: {}\ntx_full: {}\n",
        RX_PKTS.load(Ordering::Relaxed), RX_BYTES.load(Ordering::Relaxed), RX_FRAGS.load(Ordering::Relaxed),
        TX_PKTS.load(Ordering::Relaxed), TX_BYTES.load(Ordering::Relaxed), TX_FULL.load(Ordering::Relaxed));
    let tx_err = RINGS.lock().as_ref().map(|r| r.tx.errors).unwrap_or(0);
    let _ = write!(out, "irqs: {}\nrx_overruns: {}\nrx_used_bit_read: {}\nhresp: {}\ntx_irq_errors: {}\ntx_desc_errors: {}\n",
        IRQS.load(Ordering::Relaxed), RX_OVR.load(Ordering::Relaxed), RX_UBR.load(Ordering::Relaxed),
        HRESP.load(Ordering::Relaxed), IRQ_ERR.load(Ordering::Relaxed), tx_err);
}
