//! Realtek RTL8111/8168 (and RTL8101/8169 family) PCIe gigabit Ethernet.
//!
//! The NIC on most consumer x86 boards of the last fifteen years (PCI
//! 10ec:8168, also 10ec:8161 and the fast-ethernet 10ec:8136). Inert when no
//! such function exists: `init` finds nothing, logs one line and returns.
//!
//! Shape:
//! * `init` (kernel init, boot context): PCI enable, MMIO BAR, chip
//!   identification from the XID in TxConfig (logged with the MAC version
//!   number Linux uses, so a report can be matched against the reference
//!   tables), MAC-side bring-up and reset, MAC address, descriptor rings
//!   (256 RX + 256 TX, buffers below 4 GiB), MSI. Then it starts the `[r8169]`
//!   kernel thread and returns at once.
//! * The `[r8169]` thread: optionally waits for the real root and loads the
//!   PHY patch `/lib/firmware/rtl_nic/<chip>.fw` (`r8169.fw=`), then does what
//!   Linux's open() does — PHY resume, per-chip PHY configuration, PHY soft
//!   reset, chip reset + MAC configuration, autonegotiation — marks the data
//!   path running, and stays as the link monitor (PHYstatus every 250 ms;
//!   up/down transitions with speed/duplex/flow control are logged, and a TX
//!   ring that stops completing is reset).
//! * Data path: polled by the net server (`netdev`), like virtio-net: one
//!   received frame per `poll_receive`, `send_packet` posts a frame and rings
//!   the doorbell without waiting. Checksum offload, VLAN stripping, TSO,
//!   jumbo frames: all off.
//! * Interrupts: MSI, unmasked for link change and error events only (the
//!   data path is polled at the net server's cadence); the handler acks and
//!   counts. Without MSI the driver simply polls.
//!
//! Hardware programming follows Realtek's datasheets as documented by the BSD
//! re(4) driver and Linux's r8169 (GPL; consulted as documentation only, no
//! code copied). Per-chip MAC and PHY tables are implemented for the
//! RTL8168c..8168h families; for other revisions the generic path runs and the
//! log says so.
//!
//! Kernel command line:
//!   r8169.disable=1   do not touch the NIC
//!   r8169.fw=auto|1|0 PHY firmware: auto (default) = load it when the file
//!                     exists, waiting up to 30 s for the root filesystem;
//!                     1 = same but wait up to 120 s; 0 = never
//!   r8169.msi=0       no interrupts at all (pure polling)
//!   r8169.eee=1       keep Energy-Efficient Ethernet (default: off, the
//!                     usual cause of link flaps with these PHYs)
//!   r8169.speed=100   advertise 10/100 only (bad cable / switch diagnosis)
//!   r8169.debug=1     register dumps at link events

#![allow(dead_code)]

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, AtomicU64, AtomicU8, AtomicUsize, Ordering};
use spin::Mutex;

use crate::pci::{self, PciFunction};
use crate::r8169_hw::{self as hw, ChipInfo, FwProgram, RxEvent, RxRing, TxRing};

// ── registers ───────────────────────────────────────────────────────────────

const MAC0: usize = 0x00;
const MAC4: usize = 0x04;
const MAR0: usize = 0x08;
const COUNTER_LO: usize = 0x10;
const COUNTER_HI: usize = 0x14;
const TX_DESC_LO: usize = 0x20;
const TX_DESC_HI: usize = 0x24;
const CHIP_CMD: usize = 0x37;
const TX_POLL: usize = 0x38;
const INTR_MASK: usize = 0x3c;
const INTR_STATUS: usize = 0x3e;
const TX_CONFIG: usize = 0x40;
const RX_CONFIG: usize = 0x44;
const CFG9346: usize = 0x50;
const CONFIG1: usize = 0x52;
const CONFIG2: usize = 0x53;
const CONFIG3: usize = 0x54;
const CONFIG4: usize = 0x55;
const CONFIG5: usize = 0x56;
const PHYAR: usize = 0x60;
const CSIDR: usize = 0x64;
const CSIAR: usize = 0x68;
const PHY_STATUS: usize = 0x6c;
const ERIDR: usize = 0x70;
const ERIAR: usize = 0x74;
const EPHYAR: usize = 0x80;
const OCPDR: usize = 0xb0;
const OCPAR: usize = 0xb4;
const GPHY_OCP: usize = 0xb8;
const DLLPR: usize = 0xd0;
const DBG_REG: usize = 0xd1;
const MCU: usize = 0xd3;
const RX_MAX_SIZE: usize = 0xda;
const CPLUS_CMD: usize = 0xe0;
const INTR_MITIGATE: usize = 0xe2;
const RX_DESC_LO: usize = 0xe4;
const RX_DESC_HI: usize = 0xe8;
const MAX_TX_PACKET: usize = 0xec;
const MISC: usize = 0xf0;
const MISC_1: usize = 0xf2;
const PMCH: usize = 0x6f;
const IBCR0: usize = 0xf8;
const IBCR2: usize = 0xf9;
const IBISR0: usize = 0xfb;
const EEE_LED: usize = 0x1b;

// ChipCmd
const CMD_STOP_REQ: u8 = 0x80;
const CMD_RESET: u8 = 0x10;
const CMD_RX_ENB: u8 = 0x08;
const CMD_TX_ENB: u8 = 0x04;
// TxPoll
const TXPOLL_NPQ: u8 = 0x40;
// interrupt status/mask
const INT_SYSERR: u16 = 0x8000;
const INT_TX_DESC_UNAVAIL: u16 = 0x0080;
const INT_RX_FIFO_OVER: u16 = 0x0040;
const INT_LINK_CHG: u16 = 0x0020;
const INT_RX_OVERFLOW: u16 = 0x0010;
const INT_TX_ERR: u16 = 0x0008;
const INT_TX_OK: u16 = 0x0004;
const INT_RX_ERR: u16 = 0x0002;
const INT_RX_OK: u16 = 0x0001;
/// Events the MSI fires for: link change and errors. The data path is
/// polled, so RxOK/TxOK stay masked (no interrupt per packet).
const IRQ_MASK: u16 = INT_LINK_CHG | INT_RX_ERR | INT_TX_ERR | INT_RX_OVERFLOW | INT_RX_FIFO_OVER | INT_SYSERR;
// Cfg9346
const CFG9346_UNLOCK: u8 = 0xc0;
const CFG9346_LOCK: u8 = 0x00;
// RxConfig
const RX128_INT_EN: u32 = 1 << 15;
const RX_MULTI_EN: u32 = 1 << 14;
const RX_FIFO_THRESH: u32 = 7 << 13;
const RX_EARLY_OFF: u32 = 1 << 11;
const RX_DMA_BURST: u32 = 7 << 8;
const ACCEPT_ERR_MASK: u32 = 0x30;
const ACCEPT_BROADCAST: u32 = 0x08;
const ACCEPT_MULTICAST: u32 = 0x04;
const ACCEPT_MY_PHYS: u32 = 0x02;
const ACCEPT_MASK: u32 = 0x3f;
// TxConfig
const TXCFG_AUTO_FIFO: u32 = 1 << 7;
const TXCFG_EMPTY: u32 = 1 << 11;
const TX_DMA_BURST_SHIFTED: u32 = 7 << 8;
const TX_IFG_SHIFTED: u32 = 3 << 24;
// Config bits
const CFG1_SPEED_DOWN: u8 = 1 << 4;
const CFG2_CLKREQ_EN: u8 = 1 << 7;
const CFG3_JUMBO_EN0: u8 = 1 << 2;
const CFG3_RDY_TO_L23: u8 = 1 << 1;
const CFG3_BEACON_EN: u8 = 1 << 0;
const CFG4_JUMBO_EN1: u8 = 1 << 1;
const CFG5_SPI_EN: u8 = 1 << 3;
const CFG5_ASPM_EN: u8 = 1 << 0;
// CPlusCmd
const CP_NORMAL_MODE: u16 = 1 << 13;
const CP_RX_VLAN: u16 = 1 << 6;
const CP_RX_CHKSUM: u16 = 1 << 5;
const CP_INTT_MASK: u16 = 0x3;
// MCU
const MCU_NOW_IS_OOB: u8 = 1 << 7;
const MCU_TX_EMPTY: u8 = 1 << 5;
const MCU_RX_EMPTY: u8 = 1 << 4;
const MCU_LINK_LIST_RDY: u8 = 1 << 1;
// DLLPR / MISC
const DLLPR_PFM_EN: u8 = 1 << 6;
const DLLPR_TX_10M_PS_EN: u8 = 1 << 7;
const MISC1_PFM_D3COLD_EN: u8 = 1 << 6;
const MISC_TXPLA_RST: u32 = 1 << 29;
const MISC_PWM_EN: u32 = 1 << 22;
const MISC_RXDV_GATED_EN: u32 = 1 << 19;
// ERI
const ERIAR_FLAG: u32 = 0x8000_0000;
const ERI_EXGMAC: u32 = 0;
const ERI_OOB: u32 = 2 << 16;
const ERI_MASK_0001: u32 = 0x1 << 12;
const ERI_MASK_0011: u32 = 0x3 << 12;
const ERI_MASK_1111: u32 = 0xf << 12;
const OCP_STD_PHY_BASE: u32 = 0xa400;
// MII
const MII_BMCR: u32 = 0x00;
const MII_BMSR: u32 = 0x01;
const MII_PHYSID1: u32 = 0x02;
const MII_PHYSID2: u32 = 0x03;
const MII_ADVERTISE: u32 = 0x04;
const MII_LPA: u32 = 0x05;
const MII_CTRL1000: u32 = 0x09;
const MII_STAT1000: u32 = 0x0a;
const BMCR_RESET: u32 = 0x8000;
const BMCR_ANENABLE: u32 = 0x1000;
const BMCR_PDOWN: u32 = 0x0800;
const BMCR_ISOLATE: u32 = 0x0400;
const BMCR_ANRESTART: u32 = 0x0200;

const NUM_RX: usize = 256;
const NUM_TX: usize = 256;
/// Per-descriptor buffer (two per page). A frame larger than this arrives
/// fragmented and is dropped; the stack's MTU is 1500.
const BUF_SIZE: usize = 2048;
/// Frames shorter than the Ethernet minimum are zero-padded (some revisions
/// do not pad by themselves).
const ETH_ZLEN: usize = 60;
const MAX_FRAME: usize = 1514;

// ── logging ─────────────────────────────────────────────────────────────────

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
}

fn log(msg: &str) {
    let mut s = String::with_capacity(msg.len() + 10);
    s.push_str("[r8169] ");
    s.push_str(msg);
    s.push('\n');
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

macro_rules! rlog { ($($a:tt)*) => { log(&format!($($a)*)) }; }

fn now_ms() -> u64 { sched::monotonic_ns() / 1_000_000 }

fn udelay(us: u64) { sched::kthread::udelay(us) }

/// Sleep where sleeping is allowed (the driver thread); spins otherwise.
fn msleep(ms: u64) { sched::kthread::msleep(ms) }

// ── MMIO ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Mmio(usize);

impl Mmio {
    fn r8(&self, o: usize) -> u8 { unsafe { core::ptr::read_volatile((self.0 + o) as *const u8) } }
    fn r16(&self, o: usize) -> u16 { unsafe { core::ptr::read_volatile((self.0 + o) as *const u16) } }
    fn r32(&self, o: usize) -> u32 { unsafe { core::ptr::read_volatile((self.0 + o) as *const u32) } }
    fn w8(&self, o: usize, v: u8) { unsafe { core::ptr::write_volatile((self.0 + o) as *mut u8, v) } }
    fn w16(&self, o: usize, v: u16) { unsafe { core::ptr::write_volatile((self.0 + o) as *mut u16, v) } }
    fn w32(&self, o: usize, v: u32) { unsafe { core::ptr::write_volatile((self.0 + o) as *mut u32, v) } }
    fn set8(&self, o: usize, bits: u8) { self.w8(o, self.r8(o) | bits) }
    fn clr8(&self, o: usize, bits: u8) { self.w8(o, self.r8(o) & !bits) }
    /// Read back to push preceding posted writes to the chip.
    fn commit(&self) { let _ = self.r8(CHIP_CMD); }
    /// Poll `cond` every `us` µs, at most `n` times, until it equals `want`.
    fn wait(&self, what: &str, us: u64, n: u32, want: bool, cond: impl Fn(&Mmio) -> bool) -> bool {
        for _ in 0..n {
            if cond(self) == want { return true; }
            udelay(us);
        }
        if cond(self) == want { return true; }
        rlog!("timeout waiting for {} == {} ({} x {} us)", what, want, n, us);
        false
    }
}

// ── shared state (data path, ISR, /proc) ────────────────────────────────────

/// MMIO base for the ISR and the info/tally readers (0 = no NIC).
static BASE: AtomicUsize = AtomicUsize::new(0);
/// Data path enabled (set after the first full bring-up; cleared around a
/// reset so `send`/`poll` never touch the rings mid-reset).
static RUNNING: AtomicBool = AtomicBool::new(false);
static PRESENT: AtomicBool = AtomicBool::new(false);
/// Bit 0 link up, bit 1 full duplex, bits 8.. Mbit/s.
static LINK: AtomicU32 = AtomicU32::new(0);
/// Bumped at every link-up transition (the net server restarts DHCP on it).
static LINK_GEN: AtomicU32 = AtomicU32::new(0);
static LAST_PHYSTATUS: AtomicU8 = AtomicU8::new(0);
static MAC: Mutex<[u8; 6]> = Mutex::new([0; 6]);
static INFO: Mutex<Option<Info>> = Mutex::new(None);

static IRQS: AtomicU64 = AtomicU64::new(0);
static IRQ_LINKCHG: AtomicU64 = AtomicU64::new(0);
static IRQ_RX_OVERFLOW: AtomicU64 = AtomicU64::new(0);
static IRQ_RX_FIFO: AtomicU64 = AtomicU64::new(0);
static IRQ_ERRORS: AtomicU64 = AtomicU64::new(0);
static IRQ_LAST_STATUS: AtomicU16 = AtomicU16::new(0);
static RX_PKTS: AtomicU64 = AtomicU64::new(0);
static RX_BYTES: AtomicU64 = AtomicU64::new(0);
static RX_ERRS: AtomicU64 = AtomicU64::new(0);
static RX_FRAGS: AtomicU64 = AtomicU64::new(0);
static TX_PKTS: AtomicU64 = AtomicU64::new(0);
static TX_BYTES: AtomicU64 = AtomicU64::new(0);
static TX_FULL: AtomicU64 = AtomicU64::new(0);
static RESETS: AtomicU32 = AtomicU32::new(0);
static LINK_CHANGES: AtomicU32 = AtomicU32::new(0);

/// Static facts for /proc/net/nic.
#[derive(Clone)]
struct Info {
    bdf: (u8, u8, u8),
    did: u16,
    chip: &'static str,
    xid: u16,
    ver: u8,
    irq: String,
    fw: String,
    phy_id: u32,
}

/// The rings and their buffers. Only touched with `RINGS` held.
struct Rings {
    rx: RxRing,
    tx: TxRing,
    rx_desc_phys: u64,
    tx_desc_phys: u64,
    rx_buf_phys: Vec<u64>,
    tx_buf_phys: Vec<u64>,
}

static RINGS: Mutex<Option<Rings>> = Mutex::new(None);
/// Tally-counter DMA block (64 bytes, 64-byte aligned), physical.
static TALLY_PHYS: AtomicU64 = AtomicU64::new(0);
static TALLY_LOCK: Mutex<()> = Mutex::new(());

// ── per-device state owned by the driver thread ─────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dash { None, Dp, Ep }

#[derive(Clone, Copy, PartialEq, Eq)]
enum FwMode { Off, Auto, On }

struct Opts {
    fw: FwMode,
    msi: bool,
    eee: bool,
    gige: bool,
    debug: bool,
}

struct Hw {
    m: Mmio,
    func: PciFunction,
    chip: &'static ChipInfo,
    ver: u8,
    gmii: bool,
    ocp_base: u32,
    cp_cmd: u16,
    dash: Dash,
    dash_enabled: bool,
    ext_cfg: bool,
    fw: Option<FwProgram>,
    opts: Opts,
}

// ── register-space helpers ──────────────────────────────────────────────────

impl Hw {
    fn unlock(&self) { self.m.w8(CFG9346, CFG9346_UNLOCK); }
    fn lock(&self) { self.m.w8(CFG9346, CFG9346_LOCK); }

    // ERI (extended register interface)
    fn eri_write_t(&self, addr: u32, mask: u32, val: u32, ty: u32) {
        let mut cmd = ERIAR_FLAG | ty | mask | addr;
        if ty == ERI_OOB && self.ver == 52 { cmd |= 0xf70 << 18; }
        self.m.w32(ERIDR, val);
        self.m.w32(ERIAR, cmd);
        self.m.wait("ERIAR write", 100, 100, false, |m| m.r32(ERIAR) & ERIAR_FLAG != 0);
    }
    fn eri_read_t(&self, addr: u32, ty: u32) -> u32 {
        let mut cmd = ty | ERI_MASK_1111 | addr;
        if ty == ERI_OOB && self.ver == 52 { cmd |= 0xf70 << 18; }
        self.m.w32(ERIAR, cmd);
        if self.m.wait("ERIAR read", 100, 100, true, |m| m.r32(ERIAR) & ERIAR_FLAG != 0) {
            self.m.r32(ERIDR)
        } else { !0 }
    }
    fn eri_write(&self, addr: u32, mask: u32, val: u32) { self.eri_write_t(addr, mask, val, ERI_EXGMAC) }
    fn eri_read(&self, addr: u32) -> u32 { self.eri_read_t(addr, ERI_EXGMAC) }
    fn eri_w0w1(&self, addr: u32, set: u32, clear: u32) {
        let v = self.eri_read(addr);
        self.eri_write(addr, ERI_MASK_1111, (v & !clear) | set);
    }
    fn eri_set(&self, addr: u32, bits: u32) { self.eri_w0w1(addr, bits, 0) }
    fn eri_clear(&self, addr: u32, bits: u32) { self.eri_w0w1(addr, 0, bits) }

    // MAC OCP (8168g+)
    fn mac_ocp_write(&self, reg: u32, data: u32) {
        self.m.w32(OCPDR, 0x8000_0000 | (reg << 15) | (data & 0xffff));
    }
    fn mac_ocp_read(&self, reg: u32) -> u32 {
        self.m.w32(OCPDR, reg << 15);
        self.m.r32(OCPDR) & 0xffff
    }
    fn mac_ocp_modify(&self, reg: u32, mask: u32, set: u32) {
        let v = self.mac_ocp_read(reg);
        self.mac_ocp_write(reg, (v & !mask) | set);
    }

    // PHY OCP (8168g+)
    fn phy_ocp_write(&self, reg: u32, data: u32) {
        self.m.w32(GPHY_OCP, 0x8000_0000 | (reg << 15) | (data & 0xffff));
        self.m.wait("GPHY_OCP write", 25, 10, false, |m| m.r32(GPHY_OCP) & 0x8000_0000 != 0);
    }
    fn phy_ocp_read(&self, reg: u32) -> u32 {
        self.m.w32(GPHY_OCP, reg << 15);
        if self.m.wait("GPHY_OCP read", 25, 10, true, |m| m.r32(GPHY_OCP) & 0x8000_0000 != 0) {
            self.m.r32(GPHY_OCP) & 0xffff
        } else { 0xffff }
    }

    // Legacy MDIO through PHYAR
    fn mdio_write_legacy(&self, reg: u32, val: u32) {
        self.m.w32(PHYAR, 0x8000_0000 | ((reg & 0x1f) << 16) | (val & 0xffff));
        self.m.wait("PHYAR write", 25, 20, false, |m| m.r32(PHYAR) & 0x8000_0000 != 0);
        udelay(20);
    }
    fn mdio_read_legacy(&self, reg: u32) -> u32 {
        self.m.w32(PHYAR, (reg & 0x1f) << 16);
        let v = if self.m.wait("PHYAR read", 25, 20, true, |m| m.r32(PHYAR) & 0x8000_0000 != 0) {
            self.m.r32(PHYAR) & 0xffff
        } else { 0xffff };
        udelay(20);
        v
    }

    /// MDIO register write in the chip's PHY access scheme. Register 0x1f is
    /// the page select.
    fn phy_write(&mut self, reg: u32, val: u32) {
        match self.ver {
            28 | 31 => {
                self.m.w32(DLLPR, self.m.r32(DLLPR) & !0x0002_0000);
                self.mdio_write_legacy(reg, val);
                self.m.w32(DLLPR, self.m.r32(DLLPR) | 0x0002_0000);
            }
            v if hw::is_8168g_up(v) => {
                if reg == 0x1f {
                    self.ocp_base = if val != 0 { val << 4 } else { OCP_STD_PHY_BASE };
                    return;
                }
                let mut r = reg;
                if self.ocp_base != OCP_STD_PHY_BASE { r = r.wrapping_sub(0x10); }
                if self.ocp_base == OCP_STD_PHY_BASE && r == MII_BMCR && self.ver == 40 {
                    // 8168g: PHY MCU interrupts must be off before power-down.
                    if val & BMCR_RESET != 0 || val & BMCR_PDOWN == 0 {
                        self.eri_set(0x1a8, 0xfc00_0000);
                    } else {
                        self.eri_clear(0x1a8, 0xfc00_0000);
                    }
                }
                self.phy_ocp_write(self.ocp_base + r * 2, val);
            }
            _ => self.mdio_write_legacy(reg, val),
        }
    }

    fn phy_read(&mut self, reg: u32) -> u32 {
        match self.ver {
            28 | 31 => {
                if reg == MII_PHYSID2 { return 0xc912; }
                self.m.w32(DLLPR, self.m.r32(DLLPR) & !0x0002_0000);
                let v = self.mdio_read_legacy(reg);
                self.m.w32(DLLPR, self.m.r32(DLLPR) | 0x0002_0000);
                v
            }
            v if hw::is_8168g_up(v) => {
                if reg == 0x1f {
                    return if self.ocp_base == OCP_STD_PHY_BASE { 0 } else { self.ocp_base >> 4 };
                }
                let mut r = reg;
                if self.ocp_base != OCP_STD_PHY_BASE { r = r.wrapping_sub(0x10); }
                self.phy_ocp_read(self.ocp_base + r * 2)
            }
            _ => self.mdio_read_legacy(reg),
        }
    }

    fn phy_modify(&mut self, reg: u32, mask: u32, set: u32) {
        let v = self.phy_read(reg);
        self.phy_write(reg, (v & !mask) | set);
    }
    fn phy_read_paged(&mut self, page: u32, reg: u32) -> u32 {
        let old = self.phy_read(0x1f);
        self.phy_write(0x1f, page);
        let v = self.phy_read(reg);
        self.phy_write(0x1f, old);
        v
    }
    fn phy_write_paged(&mut self, page: u32, reg: u32, val: u32) {
        let old = self.phy_read(0x1f);
        self.phy_write(0x1f, page);
        self.phy_write(reg, val);
        self.phy_write(0x1f, old);
    }
    fn phy_modify_paged(&mut self, page: u32, reg: u32, mask: u32, set: u32) {
        let old = self.phy_read(0x1f);
        self.phy_write(0x1f, page);
        self.phy_modify(reg, mask, set);
        self.phy_write(0x1f, old);
    }
    /// 8168g+ PHY parameter RAM: index in page 0xa43 reg 0x13, data in 0x14.
    fn phy_param_g(&mut self, parm: u32, mask: u32, set: u32) {
        let old = self.phy_read(0x1f);
        self.phy_write(0x1f, 0x0a43);
        self.phy_write(0x13, parm);
        self.phy_modify(0x14, mask, set);
        self.phy_write(0x1f, old);
    }

    // PCIe PHY (EPHY)
    fn ephy_write(&self, reg: u32, val: u32) {
        self.m.w32(EPHYAR, 0x8000_0000 | (val & 0xffff) | ((reg & 0x1f) << 16));
        self.m.wait("EPHYAR write", 10, 100, false, |m| m.r32(EPHYAR) & 0x8000_0000 != 0);
        udelay(10);
    }
    fn ephy_read(&self, reg: u32) -> u32 {
        self.m.w32(EPHYAR, (reg & 0x1f) << 16);
        if self.m.wait("EPHYAR read", 10, 100, true, |m| m.r32(EPHYAR) & 0x8000_0000 != 0) {
            self.m.r32(EPHYAR) & 0xffff
        } else { 0xffff }
    }
    fn ephy_init(&self, table: &[(u32, u32, u32)]) {
        for &(reg, mask, bits) in table {
            let w = (self.ephy_read(reg) & !mask) | bits;
            self.ephy_write(reg, w);
        }
    }

    // PCI config space through the chip (CSI), for platforms without ECAM.
    fn csi_write(&self, addr: u32, val: u32) {
        let f = self.func.func as u32;
        self.m.w32(CSIDR, val);
        self.m.w32(CSIAR, 0x8000_0000 | (addr & 0xfff) | 0xf000 | (f << 16));
        self.m.wait("CSIAR write", 10, 100, false, |m| m.r32(CSIAR) & 0x8000_0000 != 0);
    }
    fn csi_read(&self, addr: u32) -> u32 {
        let f = self.func.func as u32;
        self.m.w32(CSIAR, (addr & 0xfff) | (f << 16) | 0xf000);
        if self.m.wait("CSIAR read", 10, 100, true, |m| m.r32(CSIAR) & 0x8000_0000 != 0) {
            self.m.r32(CSIDR)
        } else { !0 }
    }

    /// L0s/L1 entrance latency at config offset 0x70f (Realtek-specific).
    fn set_def_aspm_entry_latency(&self) {
        if self.ext_cfg {
            self.func.write8(0x70f, 0x27);
        } else {
            let v = self.csi_read(0x70c);
            self.csi_write(0x70c, (v & !0xff00_0000) | (0x27 << 24));
        }
    }

    fn pcie_cap(&self) -> Option<u16> { self.func.find_capability(pci::cap::PCIE) }
    fn set_clock_request(&self, on: bool) {
        if let Some(c) = self.pcie_cap() {
            let v = self.func.read16(c + 0x10);
            let n = if on { v | 0x0100 } else { v & !0x0100 };
            if n != v { self.func.write16(c + 0x10, n); }
        }
    }

    fn reset_packet_filter(&self) {
        self.eri_clear(0xdc, 1);
        self.eri_set(0xdc, 1);
    }
    fn set_fifo_size(&self, rx_stat: u32, tx_stat: u32, rx_dyn: u32, tx_dyn: u32) {
        self.eri_write(0xc8, ERI_MASK_1111, (rx_stat << 16) | rx_dyn);
        self.eri_write(0xe8, ERI_MASK_1111, (tx_stat << 16) | tx_dyn);
    }
    fn set_pause_thresholds(&self, low: u32, high: u32) {
        self.eri_write(0xcc, ERI_MASK_0001, low);
        self.eri_write(0xd0, ERI_MASK_0001, high);
    }
    fn pcie_state_l2l3_disable(&self) { self.m.clr8(CONFIG3, CFG3_RDY_TO_L23); }

    fn enable_rxdvgate(&self) {
        self.m.w32(MISC, self.m.r32(MISC) | MISC_RXDV_GATED_EN);
        udelay(2000);
        self.wait_txrx_fifo_empty();
    }
    fn disable_rxdvgate(&self) { self.m.w32(MISC, self.m.r32(MISC) & !MISC_RXDV_GATED_EN); }

    fn wait_txrx_fifo_empty(&self) {
        if (40..=52).contains(&self.ver) {
            self.m.wait("TXCFG_EMPTY", 100, 42, true, |m| m.r32(TX_CONFIG) & TXCFG_EMPTY != 0);
            self.m.wait("MCU RXTX_EMPTY", 100, 42, true,
                        |m| m.r8(MCU) & (MCU_TX_EMPTY | MCU_RX_EMPTY) == (MCU_TX_EMPTY | MCU_RX_EMPTY));
        }
    }

    // ── DASH (out-of-band management firmware on 8168dp/ep/fp) ──
    fn dp_ocp_read(&self, reg: u32) -> u32 {
        self.m.w32(OCPAR, (0xf << 12) | (reg & 0xfff));
        if self.m.wait("OCPAR read", 100, 20, true, |m| m.r32(OCPAR) & 0x8000_0000 != 0) {
            self.m.r32(OCPDR)
        } else { !0 }
    }
    fn dp_ocp_write(&self, mask: u32, reg: u32, data: u32) {
        self.m.w32(OCPDR, data);
        self.m.w32(OCPAR, 0x8000_0000 | ((mask & 0xf) << 12) | (reg & 0xfff));
        self.m.wait("OCPAR write", 100, 20, false, |m| m.r32(OCPAR) & 0x8000_0000 != 0);
    }
    fn ep_ocp_read(&self, reg: u32) -> u32 { self.eri_read_t(reg, ERI_OOB) }
    fn ep_ocp_write(&self, mask: u32, reg: u32, data: u32) {
        self.eri_write_t(reg, (mask & 0xf) << 12, data, ERI_OOB);
    }
    fn dash_check(&self) -> bool {
        match self.dash {
            Dash::Dp => self.dp_ocp_read(if self.ver == 31 { 0xb8 } else { 0x10 }) & (1 << 15) != 0,
            Dash::Ep => self.ep_ocp_read(0x128) & 1 != 0,
            Dash::None => false,
        }
    }
    fn ep_stop_cmac(&self) {
        self.m.clr8(IBCR2, 0x01);
        self.m.wait("IBISR0 OOB tx", 50_000, 20, true, |m| m.r8(IBISR0) & 0x20 != 0);
        self.m.set8(IBISR0, 0x20);
        self.m.clr8(IBCR0, 0x01);
    }
    /// Tell the management firmware the OS driver has taken over.
    fn dash_driver_start(&self) {
        match self.dash {
            Dash::Dp => {
                self.eri_write(0xe8, ERI_MASK_0001, 0x05);
                self.dp_ocp_write(0x1, 0x30, 1);
                if self.dash_enabled {
                    let reg = if self.ver == 31 { 0xb8 } else { 0x10 };
                    for _ in 0..10 {
                        if self.dp_ocp_read(reg) & 0x800 != 0 { break; }
                        msleep(10);
                    }
                }
            }
            Dash::Ep => {
                self.ep_ocp_write(0x01, 0x180, 0x05);
                let v = self.ep_ocp_read(0x30);
                self.ep_ocp_write(0x01, 0x30, v | 1);
                if self.dash_enabled {
                    for _ in 0..30 {
                        if self.ep_ocp_read(0x124) & 1 != 0 { break; }
                        msleep(10);
                    }
                }
            }
            Dash::None => {}
        }
    }

    // ── chip reset and probe-time init ──

    fn irq_mask_and_ack(&self) {
        self.m.w16(INTR_MASK, 0);
        self.m.w16(INTR_STATUS, 0xffff);
        self.m.commit();
    }

    fn hw_reset(&self) {
        self.m.w8(CHIP_CMD, CMD_RESET);
        self.m.wait("ChipCmd reset", 100, 100, false, |m| m.r8(CHIP_CMD) & CMD_RESET != 0);
    }

    fn init_rxcfg(&self) {
        let v = match self.ver {
            2..=6 | 10..=17 => RX_FIFO_THRESH | RX_DMA_BURST,
            18..=24 | 34..=36 | 38 => RX128_INT_EN | RX_MULTI_EN | RX_DMA_BURST,
            40..=52 => RX128_INT_EN | RX_MULTI_EN | RX_DMA_BURST | RX_EARLY_OFF,
            _ => RX128_INT_EN | RX_DMA_BURST,
        };
        self.m.w32(RX_CONFIG, v);
    }

    /// Probe-time MAC bring-up of the 8168g-and-later families: take the
    /// MAC away from the out-of-band firmware and wait for the shared FIFO.
    fn hw_initialize(&self) {
        if !(40..=52).contains(&self.ver) { return; }
        if self.ver >= 51 { self.ep_stop_cmac(); }
        self.enable_rxdvgate();
        self.m.clr8(CHIP_CMD, CMD_TX_ENB | CMD_RX_ENB);
        udelay(1000);
        self.m.clr8(MCU, MCU_NOW_IS_OOB);
        self.mac_ocp_modify(0xe8de, 1 << 14, 0);
        self.m.wait("LINK_LIST_RDY", 100, 42, true, |m| m.r8(MCU) & MCU_LINK_LIST_RDY != 0);
        self.mac_ocp_modify(0xe8de, 0, 1 << 15);
        self.m.wait("LINK_LIST_RDY", 100, 42, true, |m| m.r8(MCU) & MCU_LINK_LIST_RDY != 0);
    }

    /// Stop DMA and reset the chip (before (re)programming it).
    fn cleanup(&self) {
        self.irq_mask_and_ack();
        self.m.w32(RX_CONFIG, self.m.r32(RX_CONFIG) & !ACCEPT_MASK);
        match self.ver {
            28 | 31 => {
                self.m.wait("TxPoll NPQ", 20, 2000, false, |m| m.r8(TX_POLL) & TXPOLL_NPQ != 0);
            }
            34..=38 => {
                self.m.set8(CHIP_CMD, CMD_STOP_REQ);
                self.m.wait("TXCFG_EMPTY", 100, 666, true, |m| m.r32(TX_CONFIG) & TXCFG_EMPTY != 0);
            }
            v if v >= 40 => {
                self.enable_rxdvgate();
                udelay(2000);
            }
            _ => {
                self.m.set8(CHIP_CMD, CMD_STOP_REQ);
                udelay(100);
            }
        }
        self.hw_reset();
    }

    /// The MAC address: ERI 0xe0/0xe4 on 8168e-vl and later (what the BIOS
    /// programmed), else the MAC0..5 registers (from the EEPROM/eFuse).
    fn read_mac(&self) -> ([u8; 6], &'static str) {
        if hw::is_8168evl_up(self.ver) && self.ver != 34 {
            let lo = self.eri_read(0xe0);
            let hi = self.eri_read(0xe4);
            let m = [lo as u8, (lo >> 8) as u8, (lo >> 16) as u8, (lo >> 24) as u8, hi as u8, (hi >> 8) as u8];
            if hw::mac_valid(&m) { return (m, "ERI"); }
        }
        let mut m = [0u8; 6];
        for (i, b) in m.iter_mut().enumerate() { *b = self.m.r8(MAC0 + i); }
        if hw::mac_valid(&m) { return (m, "IDR"); }
        // Locally administered, derived from the clock: unique enough for a LAN.
        let t = sched::monotonic_ns();
        ([0x02, 0x52, (t >> 24) as u8, (t >> 16) as u8, (t >> 8) as u8, t as u8], "random")
    }

    fn rar_set(&self, a: &[u8; 6]) {
        self.unlock();
        self.m.w32(MAC4, u16::from_le_bytes([a[4], a[5]]) as u32);
        self.m.commit();
        self.m.w32(MAC0, u32::from_le_bytes([a[0], a[1], a[2], a[3]]));
        self.m.commit();
        if self.ver == 34 {
            self.eri_write(0xe0, ERI_MASK_1111, u32::from_le_bytes([a[0], a[1], a[2], a[3]]));
            self.eri_write(0xe4, ERI_MASK_1111, u16::from_le_bytes([a[4], a[5]]) as u32);
            self.eri_write(0xf0, ERI_MASK_1111, (u16::from_le_bytes([a[0], a[1]]) as u32) << 16);
            self.eri_write(0xf4, ERI_MASK_1111, u32::from_le_bytes([a[2], a[3], a[4], a[5]]));
        }
        self.lock();
    }

    // ── per-chip MAC configuration (hw_start) ──

    fn start_8168cp_common(&self) {
        self.m.set8(CONFIG1, CFG1_SPEED_DOWN);
        self.m.clr8(CONFIG3, CFG3_BEACON_EN);
        self.set_clock_request(false);
    }

    fn start_8168e_2_f_common(&self) {
        self.eri_write(0xc0, ERI_MASK_0011, 0);
        self.eri_write(0xb8, ERI_MASK_1111, 0);
        self.set_fifo_size(0x10, 0x10, 0x02, 0x06);
    }

    fn start_8168f(&self) {
        self.set_def_aspm_entry_latency();
        self.start_8168e_2_f_common();
        self.reset_packet_filter();
        self.eri_set(0x1b0, 1 << 4);
        self.eri_set(0x1d0, (1 << 4) | (1 << 1));
        self.eri_write(0xcc, ERI_MASK_1111, 0x50);
        self.eri_write(0xd0, ERI_MASK_1111, 0x60);
        self.set_clock_request(false);
        self.m.clr8(MCU, MCU_NOW_IS_OOB);
        self.m.set8(DLLPR, DLLPR_PFM_EN);
        self.m.w32(MISC, self.m.r32(MISC) | MISC_PWM_EN);
        self.m.clr8(CONFIG5, CFG5_SPI_EN);
    }

    fn start_8168g(&self) {
        self.set_fifo_size(0x08, 0x10, 0x02, 0x06);
        self.set_pause_thresholds(0x38, 0x48);
        self.set_def_aspm_entry_latency();
        self.reset_packet_filter();
        self.eri_write(0x2f8, ERI_MASK_0011, 0x1d8f);
        self.disable_rxdvgate();
        self.eri_write(0xc0, ERI_MASK_0011, 0);
        self.eri_write(0xb8, ERI_MASK_0011, 0);
        self.eri_w0w1(0x2fc, 0x01, 0x06);
        self.eri_clear(0x1b0, 1 << 12);
        self.pcie_state_l2l3_disable();
    }

    fn start_8168h(&mut self) {
        self.ephy_init(&[
            (0x1e, 0x0800, 0x0001), (0x1d, 0x0000, 0x0800), (0x05, 0xffff, 0x2089),
            (0x06, 0xffff, 0x5881), (0x04, 0xffff, 0x854a), (0x01, 0xffff, 0x068b),
        ]);
        self.set_fifo_size(0x08, 0x10, 0x02, 0x06);
        self.set_pause_thresholds(0x38, 0x48);
        self.set_def_aspm_entry_latency();
        self.reset_packet_filter();
        self.eri_set(0xdc, 0x001c);
        self.eri_write(0x5f0, ERI_MASK_0011, 0x4f87);
        self.disable_rxdvgate();
        self.eri_write(0xc0, ERI_MASK_0011, 0);
        self.eri_write(0xb8, ERI_MASK_0011, 0);
        self.m.clr8(DLLPR, DLLPR_PFM_EN);
        self.m.clr8(MISC_1, MISC1_PFM_D3COLD_EN);
        self.m.clr8(DLLPR, DLLPR_TX_10M_PS_EN);
        self.eri_clear(0x1b0, 1 << 12);
        self.pcie_state_l2l3_disable();
        let saw = self.phy_read_paged(0x0c42, 0x13) & 0x3fff;
        if saw > 0 {
            let ini = (16_000_000 / saw) & 0x0fff;
            self.mac_ocp_modify(0xd412, 0x0fff, ini);
        }
        self.mac_ocp_modify(0xe056, 0x00f0, 0);
        self.mac_ocp_modify(0xe052, 0x6000, 0x8008);
        self.mac_ocp_modify(0xe0d6, 0x01ff, 0x017f);
        self.mac_ocp_modify(0xd420, 0x0fff, 0x047f);
        self.mac_ocp_write(0xe63e, 0x0001);
        self.mac_ocp_write(0xe63e, 0x0000);
        self.mac_ocp_write(0xc094, 0x0000);
        self.mac_ocp_write(0xc09e, 0x0000);
    }

    fn start_8168ep(&self) {
        self.ep_stop_cmac();
        self.set_fifo_size(0x08, 0x10, 0x02, 0x06);
        self.set_pause_thresholds(0x2f, 0x5f);
        self.set_def_aspm_entry_latency();
        self.reset_packet_filter();
        self.eri_write(0x5f0, ERI_MASK_0011, 0x4f87);
        self.disable_rxdvgate();
        self.eri_write(0xc0, ERI_MASK_0011, 0);
        self.eri_write(0xb8, ERI_MASK_0011, 0);
        self.eri_w0w1(0x2fc, 0x01, 0x06);
        self.m.clr8(DLLPR, DLLPR_TX_10M_PS_EN);
        self.pcie_state_l2l3_disable();
    }

    /// Per-revision MAC programming. Returns false when this revision has no
    /// table here (the generic path then continues).
    fn hw_config(&mut self) -> bool {
        match self.ver {
            17 => self.m.clr8(CONFIG3, CFG3_BEACON_EN),
            18 => {
                self.set_def_aspm_entry_latency();
                self.ephy_init(&[(0x01, 0, 0x0001), (0x02, 0x0800, 0x1000), (0x03, 0, 0x0042),
                                 (0x06, 0x0080, 0x0000), (0x07, 0, 0x2000)]);
                self.start_8168cp_common();
            }
            19 => {
                self.set_def_aspm_entry_latency();
                self.m.w8(DBG_REG, 0x06 | 0x10 | 0x08);
                self.ephy_init(&[(0x02, 0x0800, 0x1000), (0x03, 0, 0x0002), (0x06, 0x0080, 0x0000)]);
                self.start_8168cp_common();
            }
            20 | 21 => {
                self.set_def_aspm_entry_latency();
                self.ephy_init(&[(0x01, 0, 0x0001), (0x03, 0x0400, 0x0020)]);
                self.start_8168cp_common();
            }
            22 => { self.set_def_aspm_entry_latency(); self.start_8168cp_common(); }
            23 => { self.set_def_aspm_entry_latency(); self.m.clr8(CONFIG3, CFG3_BEACON_EN); }
            24 => {
                self.set_def_aspm_entry_latency();
                self.m.clr8(CONFIG3, CFG3_BEACON_EN);
                self.m.w8(DBG_REG, 0x20);
            }
            25 | 26 | 31 => { self.set_def_aspm_entry_latency(); self.set_clock_request(false); }
            28 => {
                self.set_def_aspm_entry_latency();
                self.ephy_init(&[(0x0b, 0x0000, 0x0048), (0x19, 0x0020, 0x0050),
                                 (0x0c, 0x0100, 0x0020), (0x10, 0x0004, 0x0000)]);
                self.set_clock_request(true);
            }
            32 | 33 => {
                self.set_def_aspm_entry_latency();
                self.ephy_init(&[
                    (0x00, 0x0200, 0x0100), (0x00, 0x0000, 0x0004), (0x06, 0x0002, 0x0001),
                    (0x06, 0x0000, 0x0030), (0x07, 0x0000, 0x2000), (0x00, 0x0000, 0x0020),
                    (0x03, 0x5800, 0x2000), (0x03, 0x0000, 0x0001), (0x01, 0x0800, 0x1000),
                    (0x07, 0x0000, 0x4000), (0x1e, 0x0000, 0x2000), (0x19, 0xffff, 0xfe6c),
                    (0x0a, 0x0000, 0x0040),
                ]);
                self.set_clock_request(false);
                self.m.w32(MISC, self.m.r32(MISC) | MISC_TXPLA_RST);
                self.m.w32(MISC, self.m.r32(MISC) & !MISC_TXPLA_RST);
                self.m.clr8(CONFIG5, CFG5_SPI_EN);
            }
            34 => {
                self.set_def_aspm_entry_latency();
                self.ephy_init(&[(0x09, 0x0000, 0x0080), (0x19, 0x0000, 0x0224),
                                 (0x00, 0x0000, 0x0004), (0x0c, 0x3df0, 0x0200)]);
                self.start_8168e_2_f_common();
                self.eri_set(0x1d0, 1 << 1);
                self.reset_packet_filter();
                self.eri_set(0x1b0, 1 << 4);
                self.eri_write(0xcc, ERI_MASK_1111, 0x50);
                self.eri_write(0xd0, ERI_MASK_1111, 0x07ff_0060);
                self.set_clock_request(false);
                self.m.clr8(MCU, MCU_NOW_IS_OOB);
                self.m.set8(DLLPR, DLLPR_PFM_EN);
                self.m.w32(MISC, self.m.r32(MISC) | MISC_PWM_EN);
                self.m.clr8(CONFIG5, CFG5_SPI_EN);
            }
            35 | 36 => {
                self.start_8168f();
                self.ephy_init(&[(0x06, 0x00c0, 0x0020), (0x08, 0x0001, 0x0002), (0x09, 0x0000, 0x0080),
                                 (0x19, 0x0000, 0x0224), (0x00, 0x0000, 0x0008), (0x0c, 0x3df0, 0x0200)]);
            }
            38 => {
                self.start_8168f();
                self.pcie_state_l2l3_disable();
                self.ephy_init(&[(0x06, 0x00c0, 0x0020), (0x0f, 0xffff, 0x5200), (0x19, 0x0000, 0x0224),
                                 (0x00, 0x0000, 0x0008), (0x0c, 0x3df0, 0x0200)]);
            }
            40 => {
                self.start_8168g();
                self.ephy_init(&[(0x00, 0x0008, 0x0000), (0x0c, 0x3ff0, 0x0820),
                                 (0x1e, 0x0000, 0x0001), (0x19, 0x8000, 0x0000)]);
            }
            42 | 43 => {
                self.start_8168g();
                self.ephy_init(&[(0x00, 0x0008, 0x0000), (0x0c, 0x3ff0, 0x0820), (0x19, 0xffff, 0x7c00),
                                 (0x1e, 0xffff, 0x20eb), (0x0d, 0xffff, 0x1666), (0x00, 0xffff, 0x10a3),
                                 (0x06, 0xffff, 0xf050), (0x04, 0x0000, 0x0010), (0x1d, 0x4000, 0x0000)]);
            }
            44 => {
                // RTL8411b: the EPHY table; the RX-after-PHY-power-down MCU
                // patch of the reference drivers is not applied.
                self.start_8168g();
                self.ephy_init(&[(0x00, 0x0008, 0x0000), (0x0c, 0x37d0, 0x0820), (0x1e, 0x0000, 0x0001),
                                 (0x19, 0x8021, 0x0000), (0x1e, 0x0000, 0x2000), (0x0d, 0x0100, 0x0200),
                                 (0x00, 0x0000, 0x0080), (0x06, 0x0000, 0x0010), (0x04, 0x0000, 0x0010),
                                 (0x1d, 0x0000, 0x4000)]);
            }
            46 | 48 => self.start_8168h(),
            51 => {
                self.ephy_init(&[(0x00, 0x0000, 0x0080), (0x0d, 0x0100, 0x0200),
                                 (0x19, 0x8021, 0x0000), (0x1e, 0x0000, 0x2000)]);
                self.start_8168ep();
                self.m.clr8(DLLPR, DLLPR_PFM_EN);
                self.m.clr8(MISC_1, MISC1_PFM_D3COLD_EN);
                self.mac_ocp_modify(0xd3e2, 0x0fff, 0x0271);
                self.mac_ocp_modify(0xd3e4, 0x00ff, 0x0000);
                self.mac_ocp_modify(0xe860, 0x0000, 0x0080);
            }
            _ => return false,
        }
        true
    }

    fn hw_aspm_clkreq_disable(&self) {
        if self.ver < 32 { return; }
        if matches!(self.ver, 46 | 48) { self.mac_ocp_modify(0xe092, 0x00ff, 0); }
        self.m.clr8(CONFIG2, CFG2_CLKREQ_EN);
        self.m.clr8(CONFIG5, CFG5_ASPM_EN);
    }

    fn enable_exit_l1(&self) {
        match self.ver {
            34..=36 => self.eri_set(0xd4, 0x1f00),
            37..=38 => self.eri_set(0xd4, 0x0c00),
            v if v >= 40 => self.mac_ocp_modify(0xc0ac, 0, 0x1f80),
            _ => {}
        }
    }

    fn jumbo_off(&self) {
        self.unlock();
        match self.ver {
            17 => self.m.clr8(CONFIG4, 1),
            18..=26 => { self.m.clr8(CONFIG3, CFG3_JUMBO_EN0); self.m.clr8(CONFIG4, CFG4_JUMBO_EN1); }
            28 => self.m.clr8(CONFIG3, CFG3_JUMBO_EN0),
            31..=33 => {
                self.m.w8(MAX_TX_PACKET, 0x3f);
                self.m.clr8(CONFIG3, CFG3_JUMBO_EN0);
                self.m.clr8(CONFIG4, 1);
            }
            _ => {}
        }
        self.lock();
        // PCIe max read request 4096 (Device Control bits 14:12 = 5).
        if self.gmii {
            if let Some(c) = self.pcie_cap() {
                let v = self.func.read16(c + 0x08);
                self.func.write16(c + 0x08, (v & !0x7000) | (5 << 12));
            }
        }
    }

    /// Program the MAC and enable RX/TX (the chip must have been reset by
    /// `cleanup` and the rings re-armed).
    fn hw_start(&mut self, rx_phys: u64, tx_phys: u64) {
        self.unlock();
        self.hw_aspm_clkreq_disable();
        self.m.w16(CPLUS_CMD, self.cp_cmd);
        if matches!(self.ver, 46 | 48) {
            // EEE TX idle timer = MTU + header + 0x20.
            self.mac_ocp_write(0xe048, 1500 + 14 + 0x20);
        }
        if self.ver <= 6 {
            self.m.w8(MAX_TX_PACKET, 0x3f); // EarlyTxThres: no early TX
        } else {
            self.m.w8(MAX_TX_PACKET, if hw::is_8168evl_up(self.ver) { 0x27 } else { 0x3f });
            if !self.hw_config() {
                rlog!("no per-chip MAC table for MAC version {} ({}); generic configuration", self.ver, self.chip.name);
            }
        }
        self.m.w16(INTR_MITIGATE, 0);
        self.enable_exit_l1();
        // ASPM stays off in the chip (the reference drivers only enable it
        // when the platform vouches for it).
        self.m.w16(RX_MAX_SIZE, 0x4000);
        self.m.w32(TX_DESC_HI, (tx_phys >> 32) as u32);
        self.m.w32(TX_DESC_LO, tx_phys as u32);
        self.m.w32(RX_DESC_HI, (rx_phys >> 32) as u32);
        self.m.w32(RX_DESC_LO, rx_phys as u32);
        self.lock();
        self.jumbo_off();
        self.m.commit();

        self.m.w8(CHIP_CMD, CMD_TX_ENB | CMD_RX_ENB);
        self.init_rxcfg();
        let mut txcfg = TX_DMA_BURST_SHIFTED | TX_IFG_SHIFTED;
        if hw::is_8168evl_up(self.ver) { txcfg |= TXCFG_AUTO_FIFO; }
        self.m.w32(TX_CONFIG, txcfg);
        // No RXALL, no RX checksum offload, no VLAN stripping.
        self.m.w32(RX_CONFIG, self.m.r32(RX_CONFIG) & !ACCEPT_ERR_MASK);
        self.cp_cmd &= !(CP_RX_CHKSUM | CP_RX_VLAN);
        self.m.w16(CPLUS_CMD, self.cp_cmd);
        self.m.commit();
        // Unicast to us, broadcast, all multicast.
        self.m.w32(MAR0 + 4, 0xffff_ffff);
        self.m.w32(MAR0, 0xffff_ffff);
        let rc = self.m.r32(RX_CONFIG);
        self.m.w32(RX_CONFIG, (rc & !0x0f) | ACCEPT_BROADCAST | ACCEPT_MULTICAST | ACCEPT_MY_PHYS);
        if !self.opts.eee && (34..=52).contains(&self.ver) && self.ver != 37 && self.ver != 39 {
            // MAC-side TX LPI off.
            if self.ver != 38 { self.m.clr8(EEE_LED, 0x07); }
            self.eri_clear(0x1b0, 0x0003);
        }
        if irq_active() { self.m.w16(INTR_MASK, IRQ_MASK); }
    }

    // ── per-chip PHY configuration ──

    fn apply_firmware(&mut self) {
        let code = match self.fw.as_ref() { Some(f) => f.code.clone(), None => return };
        let ok = hw::fw_run(&code, self, 1_000_000);
        self.ocp_base = OCP_STD_PHY_BASE;
        if !ok { log("PHY firmware did not finish within its step bound (looping?); continuing"); }
        // A PHY soft reset may still be in progress.
        for _ in 0..12 {
            if self.phy_read(MII_BMCR) & BMCR_RESET == 0 { break; }
            msleep(50);
        }
    }

    fn phy_8168g_disable_aldps(&mut self) { self.phy_modify_paged(0x0a43, 0x10, 1 << 2, 0); }
    fn phy_8168g_config_eee(&mut self) { self.phy_modify_paged(0x0a43, 0x11, 0, 1 << 4); }

    /// Returns false when there is no table for this revision.
    fn hw_phy_config(&mut self) -> bool {
        match self.ver {
            40 => {
                self.apply_firmware();
                let r = self.phy_read_paged(0x0a46, 0x10);
                if r & (1 << 8) != 0 { self.phy_modify_paged(0x0bcc, 0x12, 1 << 15, 0); }
                else { self.phy_modify_paged(0x0bcc, 0x12, 0, 1 << 15); }
                let r = self.phy_read_paged(0x0a46, 0x13);
                if r & (1 << 8) != 0 { self.phy_modify_paged(0x0c41, 0x15, 0, 1 << 1); }
                else { self.phy_modify_paged(0x0c41, 0x15, 1 << 1, 0); }
                // PHY auto speed down
                self.phy_modify_paged(0x0a44, 0x11, 0, (1 << 3) | (1 << 2));
                // 10M / ALDPS adjustments
                self.phy_modify_paged(0x0bcc, 0x14, 1 << 8, 0);
                self.phy_modify_paged(0x0a44, 0x11, 0, (1 << 7) | (1 << 6));
                self.phy_param_g(0x8084, 0x6000, 0x0000);
                self.phy_modify_paged(0x0a43, 0x10, 0, 0x1003);
                // EEE auto-fallback
                self.phy_modify_paged(0x0a4b, 0x11, 0, 1 << 2);
                // UC LPF tune
                self.phy_param_g(0x8012, 0x0000, 0x8000);
                self.phy_modify_paged(0x0c42, 0x11, 1 << 13, 1 << 14);
                // switching-regulator efficiency
                for &(r, v) in &[(0x1f, 0x0bcd), (0x14, 0x5065), (0x14, 0xd065), (0x1f, 0x0bc8),
                                 (0x11, 0x5655), (0x1f, 0x0bcd), (0x14, 0x1065), (0x14, 0x9065),
                                 (0x14, 0x1065), (0x1f, 0x0000)] {
                    self.phy_write(r, v);
                }
                self.phy_8168g_disable_aldps();
                self.phy_8168g_config_eee();
            }
            42 | 43 | 44 => {
                self.apply_firmware();
                self.phy_8168g_config_eee();
            }
            46 | 48 => {
                self.apply_firmware();
                // CHIN EST parameter
                self.phy_param_g(0x808a, 0x003f, 0x000a);
                // R-tune and PGA-retune
                self.phy_param_g(0x0811, 0x0000, 0x0800);
                self.phy_modify_paged(0x0a42, 0x16, 0, 0x0002);
                // GPHY 10M
                self.phy_modify_paged(0x0a44, 0x11, 0, 1 << 11);
                // ADC bias current offset from the MAC's eFuse copy
                self.mac_ocp_write(0xdd02, 0x807d);
                let d1 = self.mac_ocp_read(0xdd02);
                let d2 = self.mac_ocp_read(0xdd00);
                let mut ioffset = ((d2 >> 1) & 0x7ff8) | (d2 & 0x0007);
                if d1 & (1 << 7) != 0 { ioffset |= 1 << 15; }
                if ioffset != 0xffff { self.phy_write_paged(0x0bcf, 0x16, ioffset); }
                // TX LPF corner frequency (rlen)
                let data = self.phy_read_paged(0x0bcd, 0x16) & 0x000f;
                let rlen = if data > 3 { data - 3 } else { 0 };
                self.phy_write_paged(0x0bcd, 0x17, rlen | (rlen << 4) | (rlen << 8) | (rlen << 12));
                // PHY PFM mode off, 10M PLL off disabled
                self.phy_modify_paged(0x0a44, 0x11, 1 << 7, 0);
                self.phy_modify_paged(0x0a43, 0x10, 1 << 0, 0);
                self.phy_8168g_disable_aldps();
                self.phy_8168g_config_eee();
            }
            _ => {
                // The 8168d patches are only valid in a PHY state the
                // reference drivers check first; those revisions get none.
                if self.ver >= 29 { self.apply_firmware(); }
                return false;
            }
        }
        true
    }

    /// Autonegotiation: advertise 10/100 half+full, symmetric+asymmetric
    /// pause, and 1000 full (the MACs cannot do 1000 half).
    fn start_aneg(&mut self) {
        self.phy_write(0x1f, 0);
        let adv = self.phy_read(MII_ADVERTISE);
        let new_adv = (adv & !0x0fe0) | 0x01e0 | 0x0c00 | 0x0001;
        self.phy_write(MII_ADVERTISE, new_adv);
        if self.gmii {
            let c = self.phy_read(MII_CTRL1000);
            let want = if self.opts.gige { 0x0200 } else { 0 };
            self.phy_write(MII_CTRL1000, (c & !0x0300) | want);
        }
        if !self.opts.eee && hw::is_8168g_up(self.ver) {
            // EEE advertisement (MMD 7.60) lives at page 0xa5d reg 0x10.
            self.phy_write_paged(0x0a5d, 0x10, 0);
        }
        let bmcr = self.phy_read(MII_BMCR);
        self.phy_write(MII_BMCR, (bmcr & !(BMCR_ISOLATE | BMCR_PDOWN)) | BMCR_ANENABLE | BMCR_ANRESTART);
    }

    fn phy_id(&mut self) -> u32 {
        self.phy_write(0x1f, 0);
        (self.phy_read(MII_PHYSID1) << 16) | self.phy_read(MII_PHYSID2)
    }

    fn phy_dump(&mut self) -> String {
        self.phy_write(0x1f, 0);
        let regs = [MII_BMCR, MII_BMSR, MII_PHYSID1, MII_PHYSID2, MII_ADVERTISE, MII_LPA, MII_CTRL1000, MII_STAT1000];
        let mut s = String::new();
        for r in regs {
            let _ = core::fmt::Write::write_fmt(&mut s, format_args!("{:02x}={:04x} ", r, self.phy_read(r)));
        }
        s
    }

    fn mac_dump(&self) -> String {
        format!("ChipCmd={:02x} TxConfig={:08x} RxConfig={:08x} CPlusCmd={:04x} IntrMask={:04x} IntrStatus={:04x} \
                 PHYstatus={:02x} Config1..5={:02x} {:02x} {:02x} {:02x} {:02x} MCU={:02x} MISC={:08x} TxPoll={:02x}",
                self.m.r8(CHIP_CMD), self.m.r32(TX_CONFIG), self.m.r32(RX_CONFIG), self.m.r16(CPLUS_CMD),
                self.m.r16(INTR_MASK), self.m.r16(INTR_STATUS), self.m.r8(PHY_STATUS),
                self.m.r8(CONFIG1), self.m.r8(CONFIG2), self.m.r8(CONFIG3), self.m.r8(CONFIG4), self.m.r8(CONFIG5),
                self.m.r8(MCU), self.m.r32(MISC), self.m.r8(TX_POLL))
    }

    /// The equivalent of open(): PHY up and configured, chip reset and
    /// programmed, rings armed, autonegotiation started.
    fn up(&mut self) {
        if self.dash != Dash::None { self.dash_driver_start(); }
        self.func.set_bus_master(true);
        // PHY resume (clear power-down) on the standard page.
        self.phy_write(0x1f, 0);
        let bmcr = self.phy_read(MII_BMCR);
        if bmcr & BMCR_PDOWN != 0 { self.phy_write(MII_BMCR, bmcr & !BMCR_PDOWN); msleep(20); }
        let t0 = now_ms();
        if !self.hw_phy_config() {
            rlog!("no per-chip PHY table for MAC version {} ({}); generic PHY path", self.ver, self.chip.name);
        }
        // PHY soft reset, wait for it to finish.
        self.phy_write(0x1f, 0);
        let bmcr = self.phy_read(MII_BMCR);
        let mut ctl = BMCR_RESET;
        if bmcr & BMCR_ANENABLE != 0 { ctl |= BMCR_ANRESTART; }
        self.phy_write(MII_BMCR, bmcr | ctl);
        let mut reset_ok = false;
        for _ in 0..12 {
            msleep(50);
            if self.phy_read(MII_BMCR) & BMCR_RESET == 0 { reset_ok = true; break; }
        }
        if !reset_ok { log("PHY soft reset did not complete in 600 ms"); }
        rlog!("PHY configured in {} ms; PHY id {:08x}", now_ms() - t0, self.phy_id());

        // Reset the chip, re-arm the rings, program the MAC.
        self.reset_datapath();
        self.start_aneg();
        rlog!("autonegotiation started ({}); waiting for link", if self.opts.gige { "10/100/1000" } else { "10/100 only" });
        if self.opts.debug { rlog!("after up: {} | PHY {}", self.mac_dump(), self.phy_dump()); }
    }

    /// Chip reset + MAC programming with fresh rings (also the TX-timeout
    /// recovery). The data path is off for the duration.
    fn reset_datapath(&mut self) {
        RUNNING.store(false, Ordering::SeqCst);
        let mut g = RINGS.lock();
        let (rxp, txp) = match g.as_mut() {
            Some(r) => {
                self.cleanup();
                r.rx.reset();
                r.tx.reset();
                (r.rx_desc_phys, r.tx_desc_phys)
            }
            None => return,
        };
        self.hw_start(rxp, txp);
        drop(g);
        RUNNING.store(true, Ordering::SeqCst);
    }
}

impl hw::FwTarget for Hw {
    fn phy_write(&mut self, reg: u32, val: u32) { Hw::phy_write(self, reg, val) }
    fn phy_read(&mut self, reg: u32) -> u32 { Hw::phy_read(self, reg) }
    fn mcu_write(&mut self, reg: u32, val: u32) {
        if reg == 0x1f { self.ocp_base = val << 4; return; }
        self.mac_ocp_write(self.ocp_base + reg, val);
    }
    fn mcu_read(&mut self, reg: u32) -> u32 { self.mac_ocp_read(self.ocp_base + reg) }
    fn delay_ms(&mut self, ms: u32) { msleep(ms as u64) }
}

// ── interrupt ───────────────────────────────────────────────────────────────

static IRQ_ACTIVE: AtomicBool = AtomicBool::new(false);
fn irq_active() -> bool { IRQ_ACTIVE.load(Ordering::Relaxed) }

extern "C" fn isr(_ctx: usize) -> crate::irq::IrqReturn {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 { return crate::irq::IrqReturn::None; }
    let m = Mmio(base);
    let st = m.r16(INTR_STATUS);
    if st == 0 || st == 0xffff { return crate::irq::IrqReturn::None; }
    m.w16(INTR_STATUS, st);
    IRQS.fetch_add(1, Ordering::Relaxed);
    IRQ_LAST_STATUS.store(st, Ordering::Relaxed);
    if st & INT_LINK_CHG != 0 { IRQ_LINKCHG.fetch_add(1, Ordering::Relaxed); }
    if st & INT_RX_OVERFLOW != 0 { IRQ_RX_OVERFLOW.fetch_add(1, Ordering::Relaxed); }
    if st & INT_RX_FIFO_OVER != 0 { IRQ_RX_FIFO.fetch_add(1, Ordering::Relaxed); }
    if st & (INT_SYSERR | INT_TX_ERR | INT_RX_ERR) != 0 { IRQ_ERRORS.fetch_add(1, Ordering::Relaxed); }
    crate::irq::IrqReturn::Handled
}

// ── probe ───────────────────────────────────────────────────────────────────

const REALTEK: u16 = 0x10ec;
/// (device id, gigabit)
const IDS: &[(u16, bool)] = &[(0x8168, true), (0x8161, true), (0x8136, false), (0x8169, true), (0x8167, true)];

fn cmdline_val<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    // Last occurrence wins, like the kernel's other options.
    cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('='))).last()
}

/// Probe and bring up the first Realtek NIC, if any. Boot context; returns
/// quickly (the PHY and link work happens in the `[r8169]` thread).
pub fn init(cmdline: &str) {
    if cmdline_val(cmdline, "r8169.disable") == Some("1") {
        log("disabled (r8169.disable=1)");
        return;
    }
    let mut found = None;
    for &(did, gmii) in IDS {
        if let Some(d) = pci::find_device(REALTEK, did) { found = Some((d, gmii)); break; }
    }
    let (dev, gmii) = match found {
        Some(x) => x,
        None => { log("no Realtek RTL8168/8111/8101 (10ec:8168/8161/8136/8169) found; driver inert"); return; }
    };
    let func = dev.function();
    let opts = Opts {
        fw: match cmdline_val(cmdline, "r8169.fw") { Some("0") => FwMode::Off, Some("1") => FwMode::On, _ => FwMode::Auto },
        msi: cmdline_val(cmdline, "r8169.msi") != Some("0"),
        eee: cmdline_val(cmdline, "r8169.eee") == Some("1"),
        gige: cmdline_val(cmdline, "r8169.speed") != Some("100"),
        debug: cmdline_val(cmdline, "r8169.debug") == Some("1"),
    };
    rlog!("found 10ec:{:04x} at {:02x}:{:02x}.{} (rev {:02x}, subsystem {:04x}:{:04x})",
          dev.device_id, dev.bus, dev.dev, dev.func, func.read8(0x08), func.read16(0x2c), func.read16(0x2e));

    // Power state D0 (firmware may have left it in D3hot).
    if let Some(pm) = func.find_capability(pci::cap::PM) {
        let pmcsr = func.read16(pm + 4);
        if pmcsr & 3 != 0 {
            rlog!("PCI power state D{} -> D0", pmcsr & 3);
            func.write16(pm + 4, pmcsr & !3);
            msleep(10);
        }
    }
    // ASPM off on our end of the link (L0s/L1 cause random stalls on these).
    if let Some(c) = func.find_capability(pci::cap::PCIE) {
        let lnkctl = func.read16(c + 0x10);
        if lnkctl & 3 != 0 {
            rlog!("PCIe ASPM {} -> off", match lnkctl & 3 { 1 => "L0s", 2 => "L1", _ => "L0s+L1" });
            func.write16(c + 0x10, lnkctl & !3);
        }
    }
    func.enable_mem_and_bus_master();

    // First memory BAR (BAR2 on the 8168; BAR0 is I/O).
    let mut mapped = None;
    for i in 0..6 {
        if let Some(b) = func.bar(i) {
            if !b.io && b.base != 0 {
                if let Some((va, size)) = func.map_bar(i, mm::kva::CacheMode::Uncached) {
                    mapped = Some((i, b.base, va, size));
                }
                break;
            }
        }
    }
    let (bar_i, bar_phys, va, bar_size) = match mapped {
        Some(x) => x,
        None => { log("no usable memory BAR; giving up"); return; }
    };
    let m = Mmio(va);
    let txconfig = m.r32(TX_CONFIG);
    if txconfig == !0 {
        rlog!("TxConfig reads all-ones (BAR{} at {:#x}): device not responding; giving up", bar_i, bar_phys);
        return;
    }
    let xid = hw::xid_of(txconfig);
    let chip = hw::identify(xid, gmii);
    rlog!("BAR{} {:#x} ({} KiB) -> chip {} XID {:03x} MAC version {} (TxConfig {:08x}){}",
          bar_i, bar_phys, bar_size / 1024, chip.name, xid, chip.ver, txconfig,
          if chip.ver == hw::VER_NONE { "; UNKNOWN revision, trying the generic path" } else { "" });
    if chip.ver >= 61 {
        log("RTL8125-class MACs are not supported by this driver");
        return;
    }

    let dash = match chip.ver { 28 | 31 => Dash::Dp, 51 | 52 => Dash::Ep, _ => Dash::None };
    let mut hwst = Hw {
        m, func, chip, ver: chip.ver, gmii, ocp_base: OCP_STD_PHY_BASE,
        cp_cmd: m.r16(CPLUS_CMD) & (CP_NORMAL_MODE | CP_RX_VLAN | CP_RX_CHKSUM | CP_INTT_MASK),
        dash, dash_enabled: false, ext_cfg: func.has_extended_config(), fw: None, opts,
    };
    hwst.dash_enabled = hwst.dash_check();
    if dash != Dash::None { rlog!("DASH management firmware {}", if hwst.dash_enabled { "enabled" } else { "disabled" }); }

    hwst.init_rxcfg();
    hwst.irq_mask_and_ack();
    hwst.hw_initialize();
    hwst.hw_reset();
    // Keep the PLL running in D3hot (and in D3cold only for DASH chips), as
    // the reference drivers program it at probe.
    match hwst.ver {
        2..=24 | 28 | 31 | 38 => {}
        _ => {
            let v = m.r8(PMCH) | 0x40;
            m.w8(PMCH, if hwst.dash_enabled { v | 0x80 } else { v & !0x80 });
        }
    }

    let (mac, src) = hwst.read_mac();
    hwst.rar_set(&mac);
    *MAC.lock() = mac;
    rlog!("MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (from {})", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5], src);

    // Rings and buffers, below 4 GiB (every revision can do 64-bit DMA, but
    // there is no reason to depend on it).
    let rings = match alloc_rings() {
        Some(r) => r,
        None => { log("out of DMA memory for the rings; giving up"); return; }
    };
    rlog!("rings: RX {} x {} B at {:#x}, TX {} x {} B at {:#x}", NUM_RX, BUF_SIZE, rings.rx_desc_phys,
          NUM_TX, BUF_SIZE, rings.tx_desc_phys);
    *RINGS.lock() = Some(rings);
    if let Some(p) = mm::buddy::alloc_below(0, 1 << 32) {
        unsafe { core::ptr::write_bytes(mm::phys_to_virt(p) as *mut u8, 0, 4096); }
        TALLY_PHYS.store(p as u64, Ordering::Relaxed);
    }
    BASE.store(va, Ordering::SeqCst);

    // Interrupt: MSI (x86_64). The handle is kept for the kernel's lifetime.
    let irq_desc = if !hwst.opts.msi {
        String::from("none (r8169.msi=0, polled)")
    } else {
        match crate::irq::request_msi(func, isr, 0) {
            Ok(h) => {
                let v = h.vector();
                core::mem::forget(h);
                IRQ_ACTIVE.store(true, Ordering::SeqCst);
                match v { Some(v) => format!("MSI vector {:#04x}", v), None => String::from("MSI") }
            }
            Err(e) => format!("none ({:?}; polled)", e),
        }
    };
    rlog!("interrupt: {}", irq_desc);

    *INFO.lock() = Some(Info {
        bdf: (dev.bus, dev.dev, dev.func), did: dev.device_id, chip: chip.name, xid, ver: chip.ver,
        irq: irq_desc, fw: String::from("pending"), phy_id: 0,
    });
    PRESENT.store(true, Ordering::SeqCst);

    if sched::kthread::spawn("r8169", move || driver_thread(hwst)).is_none() {
        log("could not start the driver thread; NIC stays down");
    }
}

fn alloc_rings() -> Option<Rings> {
    let lim = 1usize << 32;
    let rxd = mm::buddy::alloc_below(0, lim)?;
    let txd = mm::buddy::alloc_below(0, lim)?;
    unsafe {
        core::ptr::write_bytes(mm::phys_to_virt(rxd) as *mut u8, 0, 4096);
        core::ptr::write_bytes(mm::phys_to_virt(txd) as *mut u8, 0, 4096);
    }
    let per_page = 4096 / BUF_SIZE;
    let mut rx_buf_phys = Vec::with_capacity(NUM_RX);
    for _ in 0..NUM_RX / per_page {
        let p = mm::buddy::alloc_below(0, lim)?;
        for k in 0..per_page { rx_buf_phys.push((p + k * BUF_SIZE) as u64); }
    }
    let mut tx_buf_phys = Vec::with_capacity(NUM_TX);
    for _ in 0..NUM_TX / per_page {
        let p = mm::buddy::alloc_below(0, lim)?;
        unsafe { core::ptr::write_bytes(mm::phys_to_virt(p) as *mut u8, 0, 4096); }
        for k in 0..per_page { tx_buf_phys.push((p + k * BUF_SIZE) as u64); }
    }
    let rx_descs = mm::phys_to_virt(rxd) as *mut hw::Desc;
    let tx_descs = mm::phys_to_virt(txd) as *mut hw::Desc;
    let rx = unsafe { RxRing::init(rx_descs, NUM_RX, BUF_SIZE as u32, &|i| rx_buf_phys[i]) };
    let tx = unsafe { TxRing::init(tx_descs, NUM_TX, &|i| tx_buf_phys[i]) };
    Some(Rings { rx, tx, rx_desc_phys: rxd as u64, tx_desc_phys: txd as u64, rx_buf_phys, tx_buf_phys })
}

// ── driver thread ───────────────────────────────────────────────────────────

fn load_firmware(hw: &mut Hw) -> String {
    let name = match hw.chip.fw { Some(n) => n, None => return String::from("none needed for this revision") };
    if hw.opts.fw == FwMode::Off { return format!("{} skipped (r8169.fw=0)", name); }
    let wait_s = if hw.opts.fw == FwMode::On { 120 } else { 30 };
    let t0 = now_ms();
    while !crate::firmware::root_ready() {
        if now_ms() - t0 > wait_s * 1000 {
            return format!("{} not loaded: root filesystem not mounted within {} s", name, wait_s);
        }
        msleep(250);
    }
    let path = format!("/lib/firmware/{}", name);
    let fw = match crate::firmware::request_firmware(&path, 256 * 1024) {
        Ok(f) => f,
        Err(e) => return format!("{} not loaded ({:?}); continuing without it", path, e),
    };
    let data = match fw.to_vec() {
        Some(d) => d,
        None => return format!("{} not loaded (out of memory)", path),
    };
    match hw::fw_parse(&data, name) {
        Ok(p) => {
            let s = format!("{} loaded ({} bytes, version '{}', {} actions) after {} ms", path, data.len(),
                            p.version_str(), p.code.len(), now_ms() - t0);
            hw.fw = Some(p);
            s
        }
        Err(e) => format!("{} rejected: {}; continuing without it", path, e),
    }
}

fn driver_thread(mut hw: Hw) {
    let fw_msg = load_firmware(&mut hw);
    rlog!("firmware: {}", fw_msg);
    hw.up();
    let id = hw.phy_id();
    if let Some(i) = INFO.lock().as_mut() { i.fw = fw_msg; i.phy_id = id; }

    let t_up = now_ms();
    let mut last_up = false;
    let mut reported_nolink = 0u32;
    let mut tx_stall_since: Option<(u64, u64)> = None; // (since ms, dirty at that time)
    loop {
        msleep(250);
        let ps = hw.m.r8(PHY_STATUS);
        LAST_PHYSTATUS.store(ps, Ordering::Relaxed);
        let (up, speed, full) = hw::decode_phystatus(ps);
        if up != last_up {
            LINK_CHANGES.fetch_add(1, Ordering::Relaxed);
            if up {
                LINK.store(1 | if full { 2 } else { 0 } | (speed << 8), Ordering::SeqCst);
                LINK_GEN.fetch_add(1, Ordering::SeqCst);
                hw.phy_write(0x1f, 0);
                let lpa = hw.phy_read(MII_LPA);
                let s1000 = hw.phy_read(MII_STAT1000);
                rlog!("eth0: link up {} Mbps {}-duplex, flow control rx {} tx {} ({} ms after autoneg start; partner LPA {:04x} 1000STAT {:04x})",
                      speed, if full { "full" } else { "half" },
                      if ps & hw::PHYS_RXFLOW != 0 { "on" } else { "off" },
                      if ps & hw::PHYS_TXFLOW != 0 { "on" } else { "off" },
                      now_ms() - t_up, lpa, s1000);
                if hw.ver == 34 || hw.ver == 38 || hw.ver == 35 || hw.ver == 36 { link_chg_patch(&hw, speed); }
            } else {
                LINK.store(0, Ordering::SeqCst);
                rlog!("eth0: link down (PHYstatus {:02x})", ps);
            }
            if hw.opts.debug { rlog!("link event: {} | PHY {}", hw.mac_dump(), hw.phy_dump()); }
            last_up = up;
        }
        if !up {
            let waited = now_ms() - t_up;
            let marks = [10_000u64, 30_000, 120_000];
            if (reported_nolink as usize) < marks.len() && waited >= marks[reported_nolink as usize] {
                reported_nolink += 1;
                rlog!("no link after {} s: {} | PHY {}", waited / 1000, hw.mac_dump(), hw.phy_dump());
            }
        }
        // TX watchdog: frames posted but none completing for 5 s.
        let (inflight, dirty) = match RINGS.try_lock() {
            Some(g) => g.as_ref().map_or((0, 0), |r| (r.tx.in_flight(), r.tx.dirty)),
            None => continue,
        };
        if inflight == 0 || !up {
            tx_stall_since = None;
        } else {
            match tx_stall_since {
                Some((since, d)) if d == dirty => {
                    if now_ms() - since > 5000 {
                        let n = RESETS.fetch_add(1, Ordering::Relaxed) + 1;
                        rlog!("TX timeout ({} frames stuck for 5 s), resetting the chip (reset #{}): {}", inflight, n, hw.mac_dump());
                        hw.reset_datapath();
                        tx_stall_since = None;
                    }
                }
                _ => tx_stall_since = Some((now_ms(), dirty)),
            }
        }
    }
}

fn link_chg_patch(hw: &Hw, speed: u32) {
    if hw.ver == 34 || hw.ver == 38 {
        let (a, b) = match speed { 1000 => (0x11, 0x05), 100 => (0x1f, 0x05), _ => (0x1f, 0x3f) };
        hw.eri_write(0x1bc, ERI_MASK_1111, a);
        hw.eri_write(0x1dc, ERI_MASK_1111, b);
    } else {
        let (a, b) = if speed == 1000 { (0x11, 0x05) } else { (0x1f, 0x3f) };
        hw.eri_write(0x1bc, ERI_MASK_1111, a);
        hw.eri_write(0x1dc, ERI_MASK_1111, b);
    }
    hw.reset_packet_filter();
}

// ── data path (net server) ──────────────────────────────────────────────────

/// A Realtek NIC was found and set up (the link may still be down).
pub fn present() -> bool { PRESENT.load(Ordering::Relaxed) }
pub fn mac_address() -> Option<[u8; 6]> { if present() { Some(*MAC.lock()) } else { None } }
/// The detected chip's name, if a Realtek NIC was set up.
pub fn chip_name() -> Option<&'static str> { INFO.lock().as_ref().map(|i| i.chip) }

/// (up, Mbit/s, full duplex)
pub fn link() -> (bool, u32, bool) {
    let l = LINK.load(Ordering::Relaxed);
    (l & 1 != 0, l >> 8, l & 2 != 0)
}
pub fn link_generation() -> u32 { LINK_GEN.load(Ordering::Relaxed) }

/// Post one frame. False if the NIC is not running, the frame is too big or
/// the ring is full (the frame is dropped; TCP retransmits).
pub fn send_packet(buf: &[u8]) -> bool {
    if !RUNNING.load(Ordering::Acquire) || buf.len() > MAX_FRAME || buf.is_empty() { return false; }
    let base = BASE.load(Ordering::Relaxed);
    let mut g = RINGS.lock();
    let r = match g.as_mut() { Some(r) => r, None => return false };
    r.tx.reclaim();
    let idx = match r.tx.slot() {
        Some(i) => i,
        None => {
            TX_FULL.fetch_add(1, Ordering::Relaxed);
            Mmio(base).w8(TX_POLL, TXPOLL_NPQ);
            return false;
        }
    };
    let len = buf.len().max(ETH_ZLEN);
    unsafe {
        let dst = mm::phys_to_virt(r.tx_buf_phys[idx] as usize) as *mut u8;
        core::ptr::copy_nonoverlapping(buf.as_ptr(), dst, buf.len());
        if buf.len() < ETH_ZLEN { core::ptr::write_bytes(dst.add(buf.len()), 0, ETH_ZLEN - buf.len()); }
    }
    r.tx.post(idx, len);
    Mmio(base).w8(TX_POLL, TXPOLL_NPQ);
    TX_PKTS.fetch_add(1, Ordering::Relaxed);
    TX_BYTES.fetch_add(buf.len() as u64, Ordering::Relaxed);
    true
}

/// Take the next received frame, if any, into `out`; returns its length.
pub fn poll_receive(out: &mut [u8]) -> Option<usize> {
    if !RUNNING.load(Ordering::Acquire) { return None; }
    let mut g = RINGS.lock();
    let r = g.as_mut()?;
    // Completed TX slots are reclaimed here too, and a doorbell re-rung for
    // anything still pending (the chip can miss a TxPoll written while it is
    // busy).
    r.tx.reclaim();
    if r.tx.in_flight() > 0 { Mmio(BASE.load(Ordering::Relaxed)).w8(TX_POLL, TXPOLL_NPQ); }
    for _ in 0..NUM_RX {
        match r.rx.next() {
            RxEvent::Empty => return None,
            RxEvent::Frame { idx, len } => {
                let n = len.min(out.len());
                unsafe {
                    let src = mm::phys_to_virt(r.rx_buf_phys[idx] as usize) as *const u8;
                    core::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), n);
                }
                r.rx.release(idx);
                RX_PKTS.fetch_add(1, Ordering::Relaxed);
                RX_BYTES.fetch_add(n as u64, Ordering::Relaxed);
                return Some(n);
            }
            RxEvent::Error { idx, .. } => { RX_ERRS.fetch_add(1, Ordering::Relaxed); r.rx.release(idx); }
            RxEvent::Fragment { idx, .. } => { RX_FRAGS.fetch_add(1, Ordering::Relaxed); r.rx.release(idx); }
        }
    }
    None
}

// ── diagnostics ─────────────────────────────────────────────────────────────

/// Hardware tally counters: (tx_ok, rx_ok, tx_err, rx_err, rx_missed,
/// align_err, rx_unicast, rx_broadcast, rx_multicast, tx_aborted, tx_underrun).
fn tally() -> Option<[u64; 11]> {
    let base = BASE.load(Ordering::Relaxed);
    let phys = TALLY_PHYS.load(Ordering::Relaxed);
    if base == 0 || phys == 0 || !RUNNING.load(Ordering::Relaxed) { return None; }
    let _g = TALLY_LOCK.lock();
    let m = Mmio(base);
    m.w32(COUNTER_HI, (phys >> 32) as u32);
    m.w32(COUNTER_LO, (phys as u32) | 0x8);
    let mut ok = false;
    for _ in 0..1000 {
        if m.r32(COUNTER_LO) & 0x8 == 0 { ok = true; break; }
        udelay(10);
    }
    if !ok { return None; }
    let p = mm::phys_to_virt(phys as usize) as *const u8;
    unsafe {
        let r64 = |o: usize| core::ptr::read_volatile(p.add(o) as *const u64);
        let r32 = |o: usize| core::ptr::read_volatile(p.add(o) as *const u32) as u64;
        let r16 = |o: usize| core::ptr::read_volatile(p.add(o) as *const u16) as u64;
        Some([r64(0), r64(8), r64(16), r32(24), r16(28), r16(30), r64(40), r64(48), r32(56), r16(60), r16(62)])
    }
}

/// `key: value` lines for /proc/net/nic.
pub fn info_lines(out: &mut String) {
    use core::fmt::Write;
    let info = match INFO.lock().clone() { Some(i) => i, None => return };
    let mac = *MAC.lock();
    let (up, speed, full) = link();
    let ps = LAST_PHYSTATUS.load(Ordering::Relaxed);
    let _ = writeln!(out, "driver: r8169");
    let _ = writeln!(out, "pci: {:02x}:{:02x}.{} 10ec:{:04x}", info.bdf.0, info.bdf.1, info.bdf.2, info.did);
    let _ = writeln!(out, "chip: {}", info.chip);
    let _ = writeln!(out, "xid: {:03x}", info.xid);
    let _ = writeln!(out, "mac_version: {}", info.ver);
    let _ = writeln!(out, "phy_id: {:08x}", info.phy_id);
    let _ = writeln!(out, "firmware: {}", info.fw);
    let _ = writeln!(out, "mac: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]);
    let _ = writeln!(out, "running: {}", if RUNNING.load(Ordering::Relaxed) { "yes" } else { "no" });
    let _ = writeln!(out, "link: {}", if up { "up" } else { "down" });
    let _ = writeln!(out, "speed: {}", speed);
    let _ = writeln!(out, "duplex: {}", if !up { "-" } else if full { "full" } else { "half" });
    let _ = writeln!(out, "flow_control: rx {} tx {}", if ps & hw::PHYS_RXFLOW != 0 { "on" } else { "off" },
                     if ps & hw::PHYS_TXFLOW != 0 { "on" } else { "off" });
    let _ = writeln!(out, "phystatus: {:02x}", ps);
    let _ = writeln!(out, "link_changes: {}", LINK_CHANGES.load(Ordering::Relaxed));
    let _ = writeln!(out, "irq: {}", info.irq);
    let _ = writeln!(out, "irq_count: {} (link {} rx_overflow {} rx_fifo {} errors {} last_status {:04x})",
                     IRQS.load(Ordering::Relaxed), IRQ_LINKCHG.load(Ordering::Relaxed),
                     IRQ_RX_OVERFLOW.load(Ordering::Relaxed), IRQ_RX_FIFO.load(Ordering::Relaxed),
                     IRQ_ERRORS.load(Ordering::Relaxed), IRQ_LAST_STATUS.load(Ordering::Relaxed));
    let _ = writeln!(out, "rx_packets: {}", RX_PKTS.load(Ordering::Relaxed));
    let _ = writeln!(out, "rx_bytes: {}", RX_BYTES.load(Ordering::Relaxed));
    let _ = writeln!(out, "rx_errors: {}", RX_ERRS.load(Ordering::Relaxed));
    let _ = writeln!(out, "rx_fragments_dropped: {}", RX_FRAGS.load(Ordering::Relaxed));
    let _ = writeln!(out, "tx_packets: {}", TX_PKTS.load(Ordering::Relaxed));
    let _ = writeln!(out, "tx_bytes: {}", TX_BYTES.load(Ordering::Relaxed));
    let _ = writeln!(out, "tx_ring_full: {}", TX_FULL.load(Ordering::Relaxed));
    let _ = writeln!(out, "resets: {}", RESETS.load(Ordering::Relaxed));
    if let Some(g) = RINGS.try_lock() {
        if let Some(r) = g.as_ref() {
            let _ = writeln!(out, "rings: rx_cur {} tx_cur {} tx_dirty {} tx_in_flight {}", r.rx.cur, r.tx.cur, r.tx.dirty, r.tx.in_flight());
        }
    }
    if let Some(t) = tally() {
        let _ = writeln!(out, "hw_tally: tx_ok {} rx_ok {} tx_err {} rx_err {} rx_missed {} align_err {} rx_unicast {} rx_broadcast {} rx_multicast {} tx_aborted {} tx_underrun {}",
                         t[0], t[1], t[2], t[3], t[4], t[5], t[6], t[7], t[8], t[9], t[10]);
    }
}
