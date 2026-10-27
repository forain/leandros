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
    /// Kernel VA of RP1's eth_cfg block (RP1 0x4010_4000), read for the
    /// diagnostics only; 0 when BAR1 does not reach it.
    pub eth_cfg_va: usize,
}

/// eth_cfg sits right after the GEM's 16 KiB window.
pub const RP1_ETH_CFG_OFFSET: u64 = 0x10_4000;

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
/// MAC-side counters (clear-on-read registers, accumulated) and, when no
/// interrupt is wired, the ISR bits the driver thread saw.
static HW_STATS: Mutex<hw::HwStats> = Mutex::new(hw::HwStats {
    tx_frames: 0, tx_octets: 0, tx_underruns: 0, rx_frames: 0, rx_octets: 0, rx_fcs: 0, rx_symbol: 0,
    rx_align: 0, rx_undersize: 0, rx_jabber: 0, rx_resource: 0, rx_overrun: 0 });
static ISR_SEEN: Mutex<hw::IsrCounts> = Mutex::new(hw::IsrCounts {
    tcomp: 0, rcomp: 0, txubr: 0, rxubr: 0, hresp: 0, rovr: 0, txerr: 0 });
static PHY_DIAG: Mutex<Option<hw::PhyDiag>> = Mutex::new(None);
static PHY_MODE_NOW: Mutex<&'static str> = Mutex::new("");
static ETH_CFG_VA: AtomicUsize = AtomicUsize::new(0);
// TX error recovery (lane pigem-tx): halts seen in TSR, frames dropped by
// the ring resets, current AXI burst, the first failed descriptor, and the
// PCIe payload fixup summary.
static TX_ERRS: AtomicU64 = AtomicU64::new(0);
static TX_DROPS: AtomicU64 = AtomicU64::new(0);
static TX_ERR_TSR: AtomicU32 = AtomicU32::new(0);
static BURST: AtomicU32 = AtomicU32::new(16);
static BURST_AUTO: AtomicBool = AtomicBool::new(false);
static TX_RECOVER: AtomicBool = AtomicBool::new(true);
static QUEUES: AtomicUsize = AtomicUsize::new(1);
static TXD_AT_ERR: Mutex<Option<hw::Desc>> = Mutex::new(None);
static PCIE_FIX: Mutex<Option<String>> = Mutex::new(None);

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

    // MAC: DTB (firmware fills local-mac-address), else the board OTP
    // (firmware mailbox, Pi 5), else what the boot
    // firmware left in SA1, else a locally administered one.
    let sa = hw::mac_from_sa(m.r32(hw::SA1B), m.r32(hw::SA1T));
    let (mac, src) = if let Some(mac) = dt.mac { (mac, "dtb") }
        else if let Some(mac) = crate::bcm2712_otp::eth_mac() { (mac, "otp") }
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
    // RP1 PCIe payload sizes before any DMA (`macb.mps=0` leaves them).
    let fix = pcie_fixup(cmdline);
    mlog!("pcie payload: {}", fix);
    *PCIE_FIX.lock() = Some(fix);
    // `macb.burst=1|4|8|16` pins the AXI burst (default 16, as Linux);
    // `macb.burstauto=1` steps it down after repeated TX DMA errors;
    // `macb.txrecover=0` leaves a halted transmitter halted.
    let burst = cmdline_val(cmdline, "macb.burst").and_then(|v| v.parse::<u32>().ok())
        .filter(|b| matches!(b, 1 | 4 | 8 | 16)).unwrap_or(16);
    BURST.store(burst, Ordering::Relaxed);
    BURST_AUTO.store(cmdline_val(cmdline, "macb.burstauto") == Some("1"), Ordering::Relaxed);
    TX_RECOVER.store(cmdline_val(cmdline, "macb.txrecover") != Some("0"), Ordering::Relaxed);
    QUEUES.store(queues, Ordering::Relaxed);
    m.w32(hw::DMACFG, hw::dmacfg_with_burst(hw::dmacfg_value(m.r32(hw::DMACFG), BUF_SIZE, true), burst));
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
    // `macb.phy=rgmii|rgmii-id|rgmii-rxid|rgmii-txid` pins the PHY delays
    // (no automatic RX-delay retry then).
    let pinned = cmdline_val(cmdline, "macb.phy").and_then(|v| PhyMode::parse(v.as_bytes()));
    let phy_mode = pinned.or(dt.phy_mode).unwrap_or(PhyMode::RgmiiId);
    let phy_addr = dt.phy_addr.unwrap_or(1);
    ETH_CFG_VA.store(res.eth_cfg_va, Ordering::Relaxed);
    let polled = !res.irq_wired;
    if sched::kthread::spawn("macb", move || driver_thread(Mmio(res.mmio_va), phy_addr, phy_mode, pinned.is_some(), polled)).is_none() {
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

fn driver_thread(mut m: Mmio, want: u8, mut mode: PhyMode, pinned: bool, polled: bool) {
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
    *PHY_MODE_NOW.lock() = mode.name();
    let mut w = LinkWatch::default();
    let t0 = now_ms();
    let mut said_nolink = false;
    // Diagnostics / RX-delay retry state, per link-up.
    let mut up_at: Option<u64> = None;
    let mut diag_n = 0u32;
    let mut win_start = (0u64, hw::HwStats::default());
    let mut flips = 0u32;
    let mut rx_ok_said = false;
    loop {
        let res = { let _g = NCR_LOCK.lock(); w.poll(&mut m, addr) };
        match res {
            Ok(Some(l)) => on_link(&mut m, l),
            Ok(None) => {}
            Err(e) => mlog!("MDIO error {:?}", e),
        }
        sample(&mut m, polled);
        if w.cur.up {
            let now = now_ms();
            let t_up = *up_at.get_or_insert_with(|| { win_start = (now, *HW_STATS.lock()); now });
            // Diag at +10 s and again at +40 s (after any TX recovery).
            if diag_n < 2 && now - t_up >= if diag_n == 0 { 10_000 } else { 40_000 } {
                diag_n += 1;
                let pd = { let _g = NCR_LOCK.lock(); hw::phy_diag(&mut m, addr).ok() };
                *PHY_DIAG.lock() = pd;
                log(&diag_line(&m, mode, pd, if diag_n == 1 { 10 } else { 40 }));
            }
            if now - win_start.0 >= 6_000 {
                let cur = *HW_STATS.lock();
                let d = cur.delta(&win_start.1);
                match hw::rx_verdict(&d) {
                    hw::RxVerdict::BadTiming if !pinned && flips < 3 => {
                        let next = mode.flip_rx_delay();
                        mlog!("rx: {} line errors (fcs {} sym {} align {}) and no good frame under {}; trying {}",
                              d.rx_line_errors(), d.rx_fcs, d.rx_symbol, d.rx_align, mode.name(), next.name());
                        let r = { let _g = NCR_LOCK.lock(); hw::bcm_set_delays(&mut m, addr, next) };
                        if r.is_ok() { mode = next; *PHY_MODE_NOW.lock() = mode.name(); }
                        flips += 1;
                    }
                    hw::RxVerdict::Good if flips > 0 && !rx_ok_said => {
                        rx_ok_said = true;
                        mlog!("rx: clean frames under {}", mode.name());
                    }
                    _ => {}
                }
                win_start = (now, cur);
            }
        } else {
            up_at = None;
            diag_n = 0;
        }
        if !w.cur.up && !said_nolink && now_ms() - t0 > 10_000 {
            log("no link after 10 s (cable?)");
            said_nolink = true;
        }
        tx_check(&mut m);
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
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 { return; }
    let mut m = Mmio(base);
    sample(&mut m, !IRQ_ACTIVE.load(Ordering::Relaxed));
    let s = *HW_STATS.lock();
    let i = *ISR_SEEN.lock();
    let _ = write!(out, "hw_tx_frames: {}\nhw_tx_octets: {}\nhw_tx_underruns: {}\nhw_rx_frames: {}\nhw_rx_octets: {}\n\
hw_rx_fcs_errors: {}\nhw_rx_symbol_errors: {}\nhw_rx_align_errors: {}\nhw_rx_undersize: {}\nhw_rx_jabbers: {}\n\
hw_rx_resource_errors: {}\nhw_rx_overruns: {}\n",
        s.tx_frames, s.tx_octets, s.tx_underruns, s.rx_frames, s.rx_octets, s.rx_fcs, s.rx_symbol, s.rx_align,
        s.rx_undersize, s.rx_jabber, s.rx_resource, s.rx_overrun);
    let _ = write!(out, "isr_tcomp: {}\nisr_rcomp: {}\nisr_txubr: {}\nisr_rxubr: {}\nisr_hresp: {}\nisr_rovr: {}\nisr_txerr: {}\n",
        i.tcomp, i.rcomp, i.txubr, i.rxubr, i.hresp, i.rovr, i.txerr);
    for (n, o) in [("ncr", hw::NCR), ("ncfgr", hw::NCFGR), ("nsr", hw::NSR), ("usrio", hw::USRIO), ("dmacfg", hw::DMACFG),
                   ("tsr", hw::TSR), ("rsr", hw::RSR), ("tbqp", hw::TBQP), ("rbqp", hw::RBQP), ("tbqph", hw::TBQPH),
                   ("rbqph", hw::RBQPH), ("amp", hw::AMP), ("imr", hw::IMR)] {
        let _ = write!(out, "reg_{}: {:#010x}\n", n, m.r32(o));
    }
    let _ = write!(out, "phy_mode: {}\n", *PHY_MODE_NOW.lock());
    if let Some(p) = *PHY_DIAG.lock() {
        let _ = write!(out, "phy_rx_err: {}\nphy_false_carrier: {}\nphy_rx_notok: {:#06x}\nphy_aux_status: {:#06x}\nphy_misc: {:#06x}\nphy_clk_ctl: {:#06x}\n",
            p.rx_err, p.false_carrier, p.rx_notok, p.aux_status, p.misc, p.clk_ctl);
    }
    let _ = write!(out, "eth_cfg: {}\n", eth_cfg_words());
}

/// Folds the clear-on-read statistics into [`HW_STATS`]; without a wired
/// interrupt also reads (clears) ISR into [`ISR_SEEN`].
fn sample(m: &mut Mmio, polled: bool) {
    HW_STATS.lock().accumulate(m);
    if polled {
        let st = m.r32(hw::ISR);
        if st != 0 {
            m.w32(hw::ISR, st);
            ISR_SEEN.lock().add(st);
            if st & hw::INT_HRESP != 0 { HRESP.fetch_add(1, Ordering::Relaxed); }
            if st & hw::INT_RXUBR != 0 { RX_UBR.fetch_add(1, Ordering::Relaxed); }
        }
    }
}

/// RP1 eth_cfg words 0x00..0x1c (read-only peek; "-" when not mapped).
fn eth_cfg_words() -> String {
    let va = ETH_CFG_VA.load(Ordering::Relaxed);
    if va == 0 { return String::from("-"); }
    let mut s = String::new();
    for k in 0..8 {
        let v = unsafe { core::ptr::read_volatile((va + 4 * k) as *const u32) };
        let _ = write!(s, "{}{:08x}", if k == 0 { "" } else { " " }, v);
    }
    s
}

/// One line that tells, ten seconds after link-up, which side of the wire
/// the frames stop at: MAC counters, polled ISR bits, DMA/queue registers,
/// PHY counters with the latched delay bits, and RP1 eth_cfg.
fn diag_line(m: &Mmio, mode: PhyMode, pd: Option<hw::PhyDiag>, secs: u32) -> String {
    let s = *HW_STATS.lock();
    let i = *ISR_SEEN.lock();
    let mut l = format!("diag +{}s: hw tx {} urun {} | hw rx {} fcs {} sym {} align {} short {} res {} ovr {} | \
isr tcomp {} rcomp {} txubr {} hresp {} | sw tx {} rx {} | TSR {:#x} RSR {:#x} NSR {:#x} NCR {:#x} USRIO {:#x} DMACFG {:#x} TBQP {:#x} RBQP {:#x} | {}",
        secs, s.tx_frames, s.tx_underruns, s.rx_frames, s.rx_fcs, s.rx_symbol, s.rx_align, s.rx_undersize, s.rx_resource,
        s.rx_overrun, i.tcomp, i.rcomp, i.txubr, i.hresp, TX_PKTS.load(Ordering::Relaxed), RX_PKTS.load(Ordering::Relaxed),
        m.r32(hw::TSR), m.r32(hw::RSR), m.r32(hw::NSR), m.r32(hw::NCR), m.r32(hw::USRIO), m.r32(hw::DMACFG),
        m.r32(hw::TBQP), m.r32(hw::RBQP), mode.name());
    match pd {
        Some(p) => { let _ = write!(l, " phy rxerr {} fcar {} notok {:#x} aux {:#x} rxskew {} txdly {}", p.rx_err,
                                     p.false_carrier, p.rx_notok, p.aux_status, p.rx_skew() as u8, p.gtxclk_delay() as u8); }
        None => l.push_str(" phy diag failed"),
    }
    let _ = write!(l, " | eth_cfg {}", eth_cfg_words());
    l.push_str(" | tsr ");
    hw::tsr_names(m.r32(hw::TSR), &mut l);
    let _ = write!(l, " TBQPH {:#x} RBQPH {:#x}", m.r32(hw::TBQPH), m.r32(hw::RBQPH));
    if QUEUES.load(Ordering::Relaxed) > 1 { let _ = write!(l, " Q1TBQP {:#x}", m.r32(hw::TBQPQ1)); }
    let _ = write!(l, " | txerr {} drop {} burst {} errtsr ", TX_ERRS.load(Ordering::Relaxed),
                   TX_DROPS.load(Ordering::Relaxed), BURST.load(Ordering::Relaxed));
    hw::tsr_names(TX_ERR_TSR.load(Ordering::Relaxed), &mut l);
    if let Some(d) = *TXD_AT_ERR.lock() {
        let _ = write!(l, " errtxd {:08x} {:08x} {:08x}", d.addr, d.ctrl, d.addr_hi);
    }
    if let Some(r) = RINGS.lock().as_ref() {
        let d = r.tx.desc(0);
        let _ = write!(l, " txd0 {:08x} {:08x} {:08x}", d.addr, d.ctrl, d.addr_hi);
    }
    if let Some(p) = PCIE_FIX.lock().as_ref() { let _ = write!(l, " | pcie {}", p); }
    l
}

/// RP1's PCIe Device Control: MPS = min(root port MPS, RP1 MPS supported)
/// and MRRS = MPS (see `hw::pcie_ep_devctl`). Returns a log summary.
fn pcie_fixup(cmdline: &str) -> String {
    let ep = match crate::pci::find_function(0x1de4, 0x0001) { Some(f) => f, None => return String::from("no rp1 function") };
    let Some(ec) = ep.find_capability(0x10) else { return String::from("rp1 has no pcie cap") };
    let rc = crate::pci::PciFunction { bus: 0, dev: 0, func: 0 };
    let rcap = rc.find_capability(0x10).unwrap_or(0xac);
    let rc_dc = rc.read16(rcap + 8);
    let cap = ep.read32(ec + 4);
    let old = ep.read16(ec + 8);
    if rc_dc == 0xffff || cap == 0xffff_ffff || old == 0xffff {
        return format!("unreadable (rc devctl {:#x} rp1 devcap {:#x} devctl {:#x})", rc_dc, cap, old);
    }
    let off = cmdline_val(cmdline, "macb.mps") == Some("0");
    let new = hw::pcie_ep_devctl(rc_dc, cap, old);
    if new != old && !off { ep.write16(ec + 8, new); }
    let now = ep.read16(ec + 8);
    format!("rc devctl {:#06x} (mps {}) rp1 devcap {:#x} devctl {:#06x}->{:#06x} (mps {} mrrs {}){}", rc_dc,
            128u32 << ((rc_dc >> 5) & 7), cap, old, now, 128u32 << ((now >> 5) & 7), 128u32 << ((now >> 12) & 7),
            if off { " [macb.mps=0]" } else { "" })
}

/// A transmitter that halted on an error (TSR tfc/hresp/rle/und) stays
/// halted: Linux's `macb_tx_error_task` cycles TE (which moves the queue
/// back to its base), gives every slot back, clears TSR and restarts.
fn tx_check(m: &mut Mmio) {
    let tsr = m.r32(hw::TSR);
    if tsr & hw::TSR_TX_HALT_ERR == 0 { return; }
    let n = TX_ERRS.fetch_add(1, Ordering::Relaxed) + 1;
    TX_ERR_TSR.fetch_or(tsr, Ordering::Relaxed);
    let mut g = RINGS.lock();
    let Some(r) = g.as_mut() else { return };
    let d = r.tx.desc(r.tx.tail());
    TXD_AT_ERR.lock().get_or_insert(d);
    if !TX_RECOVER.load(Ordering::Relaxed) {
        m.w32(hw::TSR, tsr);
        return;
    }
    ncr_set(m, 0, hw::NCR_TE);
    let mut spins = 0;
    while m.r32(hw::TSR) & hw::TSR_TGO != 0 && spins < 100 { sched::kthread::udelay(10); spins += 1; }
    let dropped = r.tx.reset();
    TX_DROPS.fetch_add(dropped as u64, Ordering::Relaxed);
    m.w32(hw::TSR, tsr);
    let mut note = String::new();
    if BURST_AUTO.load(Ordering::Relaxed) && tsr & (hw::TSR_TFC | hw::TSR_HRESP) != 0 && n % 2 == 0 {
        let b = BURST.load(Ordering::Relaxed);
        let nb = hw::shorter_burst(b);
        if nb != b {
            m.w32(hw::DMACFG, hw::dmacfg_with_burst(m.r32(hw::DMACFG), nb));
            BURST.store(nb, Ordering::Relaxed);
            let _ = write!(note, ", burst {} -> {}", b, nb);
        }
    }
    ncr_set(m, hw::NCR_TE, 0);
    if n <= 3 || !note.is_empty() {
        let mut names = String::new();
        hw::tsr_names(tsr, &mut names);
        mlog!("tx halted (TSR {:#x} {}), desc {:08x} {:08x} {:08x}; TE cycled, {} frame(s) dropped{}",
              tsr, names, d.addr, d.ctrl, d.addr_hi, dropped, note);
    }
}
