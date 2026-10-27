//! RP1 south bridge (Raspberry Pi 5), behind BCM2712 pcie2. Minimal probe
//! plus the interrupt plumbing later RP1 drivers (GEM Ethernet, xHCI) use.
//!
//! Interrupt path: RP1 source `n` (0..61, Linux `rp1.h` numbering, the
//! `interrupts = <n ..>` cell of the RP1 DTB nodes) → RP1 MSI-X vector `n`
//! → memory write of data `n` to PCI 0xff_ffff_f000 → inbound RC_BAR2 →
//! MIP0 INT_RAISE → GIC SPI 128+n (INTID 160+n), edge.
//! RP1 turns level sources into edges itself: MSIX_CFG(n).IACK_EN makes it
//! wait for an IACK write before it can send vector `n` again, so after a
//! level source's handler ran `irq_ack(n)` re-arms it (done automatically by
//! the trampoline below when the source was requested as level).
//!
//! API:
//! - `probe()` — after `brcmstb_pcie::init`; maps BAR1, prints `[rp1] chip_id=..`,
//!   sets up MIP0 and the MSI-X table (all vectors masked), then the clocks
//!   (`rp1_clk`), GPIO (`rp1_gpio`), PWR LED, fan (`rp1_fan`) and the GEM. Idempotent.
//! - `bar1() -> Option<(usize, u64)>` — VA and size of the peripheral BAR;
//!   a peripheral at DTB address 0xc0_4000_0000 + X is at `bar1().0 + X`.
//! - `request_irq(n, level, handler, ctx) -> Result<IrqHandle, IrqError>`
//!   (task context); `irq_ack(n)`; `free_irq(n, handle)`.
//! - `dma_addr(pa)` — bus address RP1 must use for CPU physical address `pa`.

use crate::brcmstb_hw as hw;
use crate::irq::{IrqError, IrqHandle, IrqHandler, IrqReturn};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

static BAR1: AtomicUsize = AtomicUsize::new(0);
static BAR1_SIZE: AtomicU64 = AtomicU64::new(0);
static MSIX_TABLE: AtomicUsize = AtomicUsize::new(0);
static MSIX_ENTRIES: AtomicUsize = AtomicUsize::new(0);
static PROBED: AtomicBool = AtomicBool::new(false);

const N: usize = hw::MIP0_VECTORS as usize;
static USER_HANDLER: [AtomicUsize; N] = [const { AtomicUsize::new(0) }; N];
static USER_CTX: [AtomicUsize; N] = [const { AtomicUsize::new(0) }; N];
static LEVEL: [AtomicBool; N] = [const { AtomicBool::new(false) }; N];

fn log(s: &str) { crate::pci::serial_debug(s) }
fn hex(v: u64) { crate::pci::serial_debug_hex_64(v) }

pub fn bar1() -> Option<(usize, u64)> {
    let va = BAR1.load(Ordering::Acquire);
    if va == 0 { None } else { Some((va, BAR1_SIZE.load(Ordering::Relaxed))) }
}

pub const fn dma_addr(pa: u64) -> u64 { hw::dma_addr(pa) }

fn rp1() -> Option<crate::pci::PciFunction> { crate::pci::find_function(hw::RP1_VENDOR, hw::RP1_DEVICE) }

/// Probe RP1 once the root complex is up. Never touches anything if it is not.
pub fn probe() {
    if !crate::brcmstb_pcie::is_up() || PROBED.swap(true, Ordering::AcqRel) { return; }
    let Some(f) = rp1() else { log("[rp1] FAIL not enumerated\n"); return };
    f.enable_mem_and_bus_master();
    // BAR1 as stage 15 placed and read it back (no live re-sizing here).
    let Some((pa, size)) = crate::brcmstb_pcie::rp1_bar1() else { log("[rp1] FAIL bar1 outside rc window\n"); return };
    let Some(va) = mm::kva::ioremap(pa, size as usize, mm::kva::CacheMode::Uncached) else { log("[rp1] FAIL bar1 map\n"); return };
    let (chip, plat) = unsafe {
        (leandros_lib::mmio::read((va + hw::RP1_SYSINFO_CHIP_ID) as *const u32),
         leandros_lib::mmio::read((va + hw::RP1_SYSINFO_PLATFORM) as *const u32))
    };
    log("[rp1] bar1=");
    hex(pa);
    log(" chip_id=");
    hex(chip as u64);
    log(" platform=");
    hex(plat as u64);
    if chip != hw::RP1_C0_CHIP_ID { log(" (unexpected)\n"); return; }
    log("\n");
    BAR1_SIZE.store(size, Ordering::Relaxed);
    BAR1.store(va, Ordering::Release);
    if !mip_init() { log("[rp1] FAIL mip map\n"); return; }
    match msix_init(&f) {
        Some(n) => { log("[rp1] msix vectors="); hex(n as u64); log(" via mip0 spi 128\n"); }
        None => log("[rp1] FAIL msix\n"),
    }
    crate::rp1_gpio::init(va);
    let clocks_ok = crate::rp1_clk::init(va);
    crate::pi5_board::rp1_ready();
    crate::rp1_fan::init(va);
    attach_gem(va, clocks_ok);
    crate::rp1_usb::attach(va, size);
}

/// `rp1.irq=1`: deliver the GEM interrupt over MSI-X → MIP → GIC instead of
/// polling (default polled until the MSI path is hardware-verified).
pub static GEM_IRQ: AtomicBool = AtomicBool::new(false);

/// Pulses the BCM54213PE reset (RP1 GPIO 32, active low, 5 ms) and hands the
/// GEM to `macb`, unless its clocks could not be enabled (an unclocked GEM
/// would stall the bus on the first register read).
fn attach_gem(bar1: usize, clocks_ok: bool) {
    if !clocks_ok { log("[rp1] FAIL gem clocks off, macb not attached\n"); return; }
    if crate::rp1_gpio::reset_pulse_active_low(crate::rp1_hw::ETH_PHY_RESET_GPIO, 5) {
        log("[rp1] eth phy reset pulsed (gpio32)\n");
    }
    let irq_wired = GEM_IRQ.load(Ordering::Relaxed)
        && request_irq(crate::macb::RP1_GEM_IRQ, true, crate::macb::rp1_isr, 0).map(core::mem::forget).is_ok();
    crate::macb::rp1_attach(crate::macb::Rp1Gem {
        mmio_va: bar1 + crate::macb::RP1_GEM_OFFSET as usize,
        dma_offset: crate::macb::RP1_DMA_OFFSET,
        irq_wired,
    });
}

/// MIP0: every vector edge-triggered, delivered to the host, masked for the
/// VPU (Linux `irq-bcm2712-mip.c`).
fn mip_init() -> bool {
    let Some(m) = mm::kva::ioremap(hw::MIP0_REGS, hw::MIP_SIZE as usize, mm::kva::CacheMode::Uncached) else { return false };
    let w = |o: usize, v: u32| unsafe { leandros_lib::mmio::write((m + o) as *mut u32, v) };
    w(hw::MIP_INT_MASKL_HOST, 0);
    w(hw::MIP_INT_MASKH_HOST, 0);
    w(hw::MIP_INT_MASKL_VPU, !0);
    w(hw::MIP_INT_MASKH_VPU, !0);
    w(hw::MIP_INT_CFGL_HOST, !0);
    w(hw::MIP_INT_CFGH_HOST, !0);
    true
}

/// Maps the MSI-X table, points every entry at MIP0 vector = entry index,
/// keeps every entry masked and enables MSI-X. Returns the table size.
fn msix_init(f: &crate::pci::PciFunction) -> Option<usize> {
    let cap = f.find_capability(0x11)?;
    let ctrl = f.read16(cap + 2);
    let n = (ctrl & 0x7ff) as usize + 1;
    let tbl = f.read32(cap + 4);
    let bir = (tbl & 7) as usize;
    let (bva, bsz) = if bir == 1 { bar1()? } else { f.map_bar(bir, mm::kva::CacheMode::Uncached)? };
    let off = (tbl & !7) as usize;
    if off as u64 + 16 * n as u64 > bsz { return None; }
    let t = bva + off;
    for e in 0..n.min(N) {
        let (addr, data) = hw::mip_msg(e as u32);
        let p = (t + 16 * e) as *mut u32;
        unsafe {
            leandros_lib::mmio::write(p.add(3), 1);
            leandros_lib::mmio::write(p, addr as u32);
            leandros_lib::mmio::write(p.add(1), (addr >> 32) as u32);
            leandros_lib::mmio::write(p.add(2), data);
        }
    }
    f.write16(cap + 2, (ctrl & !0x4000) | 0x8000); // enable, function mask off
    f.set_intx_disable(true);
    MSIX_TABLE.store(t, Ordering::Release);
    MSIX_ENTRIES.store(n, Ordering::Release);
    Some(n)
}

fn msix_cfg(n: u32, alias: usize, bits: u32) {
    if let Some((va, _)) = bar1() {
        unsafe { leandros_lib::mmio::write((va + hw::rp1_msix_cfg(n, alias)) as *mut u32, bits) }
    }
}

fn entry_mask(n: u32, masked: bool) {
    let t = MSIX_TABLE.load(Ordering::Acquire);
    if t == 0 || n as usize >= MSIX_ENTRIES.load(Ordering::Relaxed) { return; }
    unsafe { leandros_lib::mmio::write((t + 16 * n as usize + 12) as *mut u32, masked as u32) }
}

/// Re-arms level source `n` after its handler cleared the device condition.
pub fn irq_ack(n: u32) { msix_cfg(n, hw::RP1_REG_SET, hw::RP1_MSIX_CFG_IACK); }

extern "C" fn trampoline(n: usize) -> IrqReturn {
    let h = USER_HANDLER[n].load(Ordering::Acquire);
    let r = if h == 0 { IrqReturn::None } else {
        let f: IrqHandler = unsafe { core::mem::transmute::<usize, IrqHandler>(h) };
        f(USER_CTX[n].load(Ordering::Relaxed))
    };
    if LEVEL[n].load(Ordering::Relaxed) { irq_ack(n as u32); }
    r
}

/// Attaches `handler(ctx)` to RP1 interrupt source `n`. `level` = the
/// source is level-triggered (DTB flag 4, e.g. GEM and xHCI). Task context.
pub fn request_irq(n: u32, level: bool, handler: IrqHandler, ctx: usize) -> Result<IrqHandle, IrqError> {
    if bar1().is_none() || MSIX_TABLE.load(Ordering::Acquire) == 0 { return Err(IrqError::Unsupported); }
    if n >= hw::RP1_INT_END || n as usize >= MSIX_ENTRIES.load(Ordering::Relaxed) { return Err(IrqError::Unsupported); }
    let i = n as usize;
    if USER_HANDLER[i].load(Ordering::Acquire) != 0 { return Err(IrqError::Busy); }
    USER_CTX[i].store(ctx, Ordering::Relaxed);
    LEVEL[i].store(level, Ordering::Relaxed);
    USER_HANDLER[i].store(handler as usize, Ordering::Release);
    let h = match crate::irq::request_irq_edge(hw::mip_intid(n), trampoline, i) {
        Ok(h) => h,
        Err(e) => { USER_HANDLER[i].store(0, Ordering::Release); return Err(e); }
    };
    if level {
        msix_cfg(n, hw::RP1_REG_SET, hw::RP1_MSIX_CFG_IACK_EN);
    } else {
        msix_cfg(n, hw::RP1_REG_CLR, hw::RP1_MSIX_CFG_IACK_EN);
    }
    msix_cfg(n, hw::RP1_REG_SET, hw::RP1_MSIX_CFG_ENABLE);
    entry_mask(n, false);
    Ok(h)
}

/// Detaches source `n` (masks it in RP1 and the MSI-X table first).
pub fn free_irq(n: u32, h: IrqHandle) {
    if n as usize >= N { return; }
    entry_mask(n, true);
    msix_cfg(n, hw::RP1_REG_CLR, hw::RP1_MSIX_CFG_ENABLE);
    crate::irq::free_irq(h);
    USER_HANDLER[n as usize].store(0, Ordering::Release);
}
