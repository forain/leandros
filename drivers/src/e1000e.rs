//! Intel e1000e-family gigabit Ethernet: the PCH-integrated I219 (LM/V) of
//! Skylake..Meteor Lake laptops and desktops, and the discrete 82574L that
//! QEMU models as `-device e1000e` (same descriptor/ring/register core, so the
//! data path is proven end to end in QEMU; the PCH-only parts — management
//! engine handshakes, ULP exit, LANPHYPC, K1, LTR, the SPT ring-flush quirk —
//! only on real hardware). Inert when no such function exists.
//!
//! Shape (the r8169 driver's, `r8169.rs`):
//! * `init` (kernel init, boot context): PCI enable, BAR0, identification and
//!   a register snapshot (FWSM = the ME's view, STRAP, H2ME, EXTCNF_CTRL...),
//!   MAC address (RAL0/RAH0, else the NVM, else random), descriptor rings
//!   (256 RX + 256 TX legacy descriptors, 2 KiB buffers, below 4 GiB), MSI.
//!   Starts the `[e1000e]` kernel thread and returns.
//! * The `[e1000e]` thread: what Linux's probe + open do, every step logged
//!   with its result — PHY workarounds (gate automatic PHY config, ULP exit
//!   through the ME or by software, PHY accessibility with the SMBus /
//!   LANPHYPC fallbacks, PHY reset), PHY id, MAC reset, NVM bank/checksum,
//!   e1000e_reset (packet buffer, flow-control thresholds, the SPT/CNP
//!   descriptor-ring flush, DRV_LOAD for the ME, MAC init, copper link setup
//!   and autonegotiation, EEE advertisement), rings. Then it stays as the
//!   link monitor (MAC STATUS every 250 ms; link-up applies the per-speed
//!   PCH settings: TIPG, EMI RX config, PLL clock gate, K1 clock request, the
//!   SPT 1000 Mb/s pointer gap, LTR, FEXTNVM4/6, flow control, EEE), dumps MAC
//!   and PHY registers at 10/30/120 s without link, resets a TX ring that
//!   stopped completing and reports RX stalls.
//! * Data path (`netdev`): one frame per `poll_receive`, `send_packet` posts a
//!   frame and writes TDT. No offloads.
//! * Interrupts: MSI (x86_64) for RX timer, link change, overrun; NAPI style
//!   (`e1000e_hw::irq_service` / `irq_rearm`): the handler masks everything,
//!   acks what it read and kicks the net daemon, which re-enables the mask
//!   after draining. Without MSI the data path is polled.
//!
//! PHY accesses (MDIC) are made holding the software flag
//! (EXTCNF_CTRL.SWFLAG), which arbitrates the PHY with the ME on "LM" (vPro)
//! parts.
//!
//! Behaviour follows Intel's documentation as implemented by Linux's e1000e
//! (ich8lan.c, netdev.c, phy.c, v6.11; GPL; consulted as documentation only,
//! no code copied).
//!
//! Kernel command line:
//!   e1000e.disable=1   do not touch the NIC
//!   e1000e.msi=0       no interrupt (pure polling)
//!   e1000e.rxirq=0     MSI for link/errors only; RX polled
//!   e1000e.eee=1       advertise Energy-Efficient Ethernet (default: off)
//!   e1000e.speed=100   advertise 10/100 only
//!   e1000e.k1=0        force the K1 power state off (PCH)
//!   e1000e.ulp=0       skip the ULP-exit step (diagnosis only)
//!   e1000e.debug=1     register dumps at link events

#![allow(dead_code)]

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use spin::Mutex;

use crate::e1000e_hw::{self as hw, DevInfo, Family, RxEvent, RxRing, TxRing};
use crate::pci::{self, PciFunction};

// ── MAC registers ───────────────────────────────────────────────────────────

const CTRL: usize = 0x00000;
const STATUS: usize = 0x00008;
const STRAP: usize = 0x0000c;
const EECD: usize = 0x00010;
const EERD: usize = 0x00014; // 82574 only (FEXTNVM5 on PCH)
const FEXTNVM6: usize = 0x00010; // PCH only (EECD on 82574)
const CTRL_EXT: usize = 0x00018;
const MDIC: usize = 0x00020;
const FEXTNVM4: usize = 0x00024;
const FEXTNVM: usize = 0x00028;
const KMRNCTRLSTA: usize = 0x00034;
const VET: usize = 0x00038;
const FEXTNVM3: usize = 0x0003c;
const ICR: usize = 0x000c0;
const ITR: usize = 0x000c4;
const IMS: usize = 0x000d0;
const IMC: usize = 0x000d8;
const IAM: usize = 0x000e0;
const FEXTNVM7: usize = 0x000e4;
const LTRV: usize = 0x000f8;
const RCTL: usize = 0x00100;
const FCTTV: usize = 0x00170;
const TCTL: usize = 0x00400;
const TIPG: usize = 0x00410;
const LEDCTL: usize = 0x00e00;
const EXTCNF_CTRL: usize = 0x00f00;
const EXTCNF_SIZE: usize = 0x00f08;
const PHY_CTRL: usize = 0x00f10;
const PCIEANACFG: usize = 0x00f18;
const IOSFPC: usize = 0x00f28;
const PBA: usize = 0x01000;
const PBECCSTS: usize = 0x0100c;
const FCRTL: usize = 0x02160;
const FCRTH: usize = 0x02168;
const RDBAL: usize = 0x02800;
const RDBAH: usize = 0x02804;
const RDLEN: usize = 0x02808;
const RDH: usize = 0x02810;
const RDT: usize = 0x02818;
const RDTR: usize = 0x02820;
const RXDCTL0: usize = 0x02828;
const RADV: usize = 0x0282c;
const KABGTXD: usize = 0x03004;
const TDBAL: usize = 0x03800;
const TDBAH: usize = 0x03804;
const TDLEN: usize = 0x03808;
const TDH: usize = 0x03810;
const TDT: usize = 0x03818;
const TIDV: usize = 0x03820;
const TXDCTL0: usize = 0x03828;
const TADV: usize = 0x0382c;
const TARC0: usize = 0x03840;
const TXDCTL1: usize = 0x03928;
const TARC1: usize = 0x03940;
const CRCERRS: usize = 0x04000;
const RXERRC: usize = 0x0400c;
const MPC: usize = 0x04010;
const COLC: usize = 0x04028;
const XONRXC: usize = 0x04048;
const XONTXC: usize = 0x0404c;
const XOFFRXC: usize = 0x04050;
const XOFFTXC: usize = 0x04054;
const GPRC: usize = 0x04074;
const BPRC: usize = 0x04078;
const MPRC: usize = 0x0407c;
const GPTC: usize = 0x04080;
const RNBC: usize = 0x040a0;
const RUC: usize = 0x040a4;
const ROC: usize = 0x040ac;
const RXCSUM: usize = 0x05000;
const RFCTL: usize = 0x05008;
const MTA: usize = 0x05200;
const VFTA: usize = 0x05600;
const RAL0: usize = 0x05400;
const RAH0: usize = 0x05404;
const WUC: usize = 0x05800;
const MANC: usize = 0x05820;
const GCR: usize = 0x05b00;
const H2ME: usize = 0x05b50; // PCH (SWSM on the 82574)
const FWSM: usize = 0x05b54;
const FEXTNVM9: usize = 0x05bb4;
const FEXTNVM11: usize = 0x05bbc;
const FFLT_DBG: usize = 0x05f04;
const FCRTV_PCH: usize = 0x05f40;
/// SPT+: the GbE NVM (flash) access registers inside BAR0.
const FLASH_BASE: usize = 0x0e000;
const FL_HSFSTS: usize = FLASH_BASE + 0x04;
const FL_FADDR: usize = FLASH_BASE + 0x08;
const FL_FDATA0: usize = FLASH_BASE + 0x10;

// CTRL
const CTRL_FD: u32 = 1 << 0;
const CTRL_GIO_MASTER_DISABLE: u32 = 1 << 2;
const CTRL_SLU: u32 = 1 << 6;
const CTRL_SPD_100: u32 = 1 << 8;
const CTRL_SPD_1000: u32 = 1 << 9;
const CTRL_FRCSPD: u32 = 1 << 11;
const CTRL_FRCDPX: u32 = 1 << 12;
const CTRL_LANPHYPC_OVERRIDE: u32 = 1 << 16;
const CTRL_LANPHYPC_VALUE: u32 = 1 << 17;
const CTRL_MEHE: u32 = 1 << 19;
const CTRL_RST: u32 = 1 << 26;
const CTRL_RFCE: u32 = 1 << 27;
const CTRL_TFCE: u32 = 1 << 28;
const CTRL_PHY_RST: u32 = 1 << 31;
// STATUS
const STATUS_LAN_INIT_DONE: u32 = 1 << 9;
const STATUS_PHYRA: u32 = 1 << 10;
const STATUS_GIO_MASTER_ENABLE: u32 = 1 << 19;
const STATUS_PCIM_STATE: u32 = 1 << 30;
// CTRL_EXT
const CTRL_EXT_LPCD: u32 = 1 << 2;
const CTRL_EXT_FORCE_SMBUS: u32 = 1 << 11;
const CTRL_EXT_SPD_BYPS: u32 = 1 << 15;
const CTRL_EXT_RO_DIS: u32 = 1 << 17;
const CTRL_EXT_PHYPDEN: u32 = 1 << 20;
const CTRL_EXT_DRV_LOAD: u32 = 1 << 28;
// EXTCNF_CTRL
const EXTCNF_SWFLAG: u32 = 1 << 5; // MDIO_SW_OWNERSHIP on the 82574
const EXTCNF_GATE_PHY_CFG: u32 = 1 << 7;
// H2ME
const H2ME_ULP: u32 = 1 << 11;
const H2ME_ENFORCE_SETTINGS: u32 = 1 << 12;
// FEXTNVM*
const FEXTNVM_SW_CONFIG_ICH8M: u32 = 1 << 27;
const FEXTNVM3_PHY_CFG_COUNTER_MASK: u32 = 0x0c00_0000;
const FEXTNVM3_PHY_CFG_COUNTER_50MSEC: u32 = 0x0800_0000;
const FEXTNVM4_BEACON_DURATION_MASK: u32 = 0x7;
const FEXTNVM4_BEACON_DURATION_8USEC: u32 = 0x7;
const FEXTNVM6_K1_OFF_ENABLE: u32 = 1 << 31;
const FEXTNVM7_SIDE_CLK_UNGATE: u32 = 1 << 2;
const FEXTNVM7_DISABLE_SMB_PERST: u32 = 1 << 5;
const FEXTNVM9_IOSFSB_CLKGATE_DIS: u32 = 1 << 11;
const FEXTNVM9_IOSFSB_CLKREQ_DIS: u32 = 1 << 12;
const FEXTNVM11_DISABLE_MULR_FIX: u32 = 1 << 13;
// PHY_CTRL (MAC side OEM bits)
const PHY_CTRL_D0A_LPLU: u32 = 1 << 1;
const PHY_CTRL_GBE_DISABLE: u32 = 1 << 6;
// RCTL
const RCTL_EN: u32 = 1 << 1;
const RCTL_SBP: u32 = 1 << 2;
const RCTL_UPE: u32 = 1 << 3;
const RCTL_MPE: u32 = 1 << 4;
const RCTL_LPE: u32 = 1 << 5;
const RCTL_LBM_MASK: u32 = 3 << 6;
const RCTL_RDMTS_MASK: u32 = 3 << 8;
const RCTL_DTYP_MASK: u32 = 3 << 10;
const RCTL_MO_MASK: u32 = 3 << 12;
const RCTL_BAM: u32 = 1 << 15;
const RCTL_SZ_MASK: u32 = 3 << 16;
const RCTL_RDMTS_HEX: u32 = 1 << 16;
const RCTL_BSEX: u32 = 1 << 25;
const RCTL_SECRC: u32 = 1 << 26;
// TCTL
const TCTL_EN: u32 = 1 << 1;
const TCTL_PSP: u32 = 1 << 3;
const TCTL_CT: u32 = 0xff << 4;
const TCTL_COLD: u32 = 0x3ff << 12;
const TCTL_RTLC: u32 = 1 << 24;
const TCTL_MULR: u32 = 1 << 28;
const COLLISION_THRESHOLD: u32 = 15;
const COLLISION_DISTANCE: u32 = 63;
// TXDCTL / RXDCTL
const TXDCTL_PTHRESH: u32 = 0x3f;
const TXDCTL_WTHRESH: u32 = 0x3f << 16;
const TXDCTL_FULL_TX_DESC_WB: u32 = 0x0101_0000;
const TXDCTL_MAX_TX_DESC_PREFETCH: u32 = 0x0100_001f;
const RXDCTL_THRESH_UNIT_DESC: u32 = 0x0100_0000;
const TARC0_CB_MULTIQ_MASK: u32 = 0x3000_0000;
const TARC0_CB_MULTIQ_2_REQ: u32 = 0x2000_0000;
// misc
const RFCTL_NFSW_DIS: u32 = 1 << 6;
const RFCTL_NFSR_DIS: u32 = 1 << 7;
const RFCTL_EXTEN: u32 = 1 << 15;
const RXCSUM_TUOFL: u32 = 1 << 9;
const PBECCSTS_ECC_ENABLE: u32 = 1 << 16;
const KABGTXD_BGSQLBIAS: u32 = 0x0005_0000;
const FCRTL_XONE: u32 = 1 << 31;
const PCIE_NO_SNOOP_ALL: u32 = 0x3f;
const GCR_L1_ACT_WITHOUT_L0S_RX: u32 = 1 << 27;
const TXDCTL_COUNT_DESC: u32 = 1 << 22;
const FFLT_DBG_DONT_GATE_WAKE_DMA_CLK: u32 = 1 << 12;
const LTRV_REQ: u32 = 1 << 15;
const LTRV_NOSNOOP_SHIFT: u32 = 16;
const LTRV_SEND: u32 = 1 << 30;
const KMRN_OFFSET_SHIFT: u32 = 16;
const KMRN_REN: u32 = 1 << 21;
const KMRN_K1_CONFIG: u32 = 0x7;
const KMRN_K1_ENABLE: u16 = 0x2;
const EECD_AUTO_RD: u32 = 1 << 9;
const EERD_START: u32 = 1;
const EERD_DONE: u32 = 1 << 1;
// PCI config space
const PCICFG_DESC_RING_STATUS: u16 = 0xe4;
const FLUSH_DESC_REQUIRED: u16 = 0x100;
const PCI_LTR_CAP_LPT: u16 = 0xa8;

// ── PHY registers (`page << 5 | reg`) ──────────────────────────────────────

const MII_BMCR: u32 = 0;
const MII_BMSR: u32 = 1;
const MII_PHYSID1: u32 = 2;
const MII_PHYSID2: u32 = 3;
const MII_ADVERTISE: u32 = 4;
const MII_LPA: u32 = 5;
const MII_CTRL1000: u32 = 9;
const MII_STAT1000: u32 = 10;
const BMCR_ANRESTART: u16 = 0x0200;
const BMCR_PDOWN: u16 = 0x0800;
const BMCR_ANENABLE: u16 = 0x1000;
const ADVERTISE_10HALF: u16 = 0x0020;
const ADVERTISE_10FULL: u16 = 0x0040;
const ADVERTISE_100HALF: u16 = 0x0080;
const ADVERTISE_100FULL: u16 = 0x0100;
const ADVERTISE_1000FULL: u16 = 0x0200;
const ADVERTISE_1000HALF: u16 = 0x0100;
const I82577_PHY_CTRL_2: u32 = 18;
const I82577_CFG_REG: u32 = 22;
const I82577_PHY_STATUS_2: u32 = 26;
const I82577_CFG_ASSERT_CRS_ON_TX: u16 = 1 << 15;
const I82577_CFG_ENABLE_DOWNSHIFT: u16 = 3 << 10;
const I82577_PHY_CTRL2_MDIX_CFG_MASK: u16 = 0x0600;
const I82577_PHY_CTRL2_AUTO_MDI_MDIX: u16 = 0x0400;
const I82579_EMI_ADDR: u32 = 0x10;
const I82579_EMI_DATA: u32 = 0x11;
const BM_PORT_GEN_CFG: u32 = hw::phy_reg(769, 17);
const BM_WUC_HOST_WU_BIT: u16 = 1 << 4;
const CV_SMB_CTRL: u32 = hw::phy_reg(769, 23);
const CV_SMB_CTRL_FORCE_SMBUS: u16 = 0x0001;
const BM_PORT_CTRL_27: u32 = hw::phy_reg(769, 27);
const HV_KMRN_MODE_CTRL: u32 = hw::phy_reg(769, 16);
const HV_KMRN_MDIO_SLOW: u16 = 0x0400;
const HV_PM_CTRL: u32 = hw::phy_reg(770, 17);
const HV_PM_CTRL_K1_CLK_REQ: u16 = 0x200;
const HV_PM_CTRL_K1_ENABLE: u16 = 0x4000;
const I82579_LPI_CTRL: u32 = hw::phy_reg(772, 20);
const I82579_LPI_CTRL_100_ENABLE: u16 = 0x2000;
const I82579_LPI_CTRL_1000_ENABLE: u16 = 0x4000;
const I82579_LPI_CTRL_ENABLE_MASK: u16 = 0x6000;
const I217_PLL_CLOCK_GATE_REG: u32 = hw::phy_reg(772, 28);
const I217_PLL_CLOCK_GATE_MASK: u16 = 0x07ff;
const SPT_PTR_GAP_REG: u32 = hw::phy_reg(776, 20);
const I218_ULP_CONFIG1: u32 = hw::phy_reg(779, 16);
const ULP_CONFIG1_START: u16 = 0x0001;
const ULP_CONFIG1_IND: u16 = 0x0004;
const ULP_CONFIG1_STICKY_ULP: u16 = 0x0010;
const ULP_CONFIG1_INBAND_EXIT: u16 = 0x0020;
const ULP_CONFIG1_WOL_HOST: u16 = 0x0040;
const ULP_CONFIG1_RESET_TO_SMBUS: u16 = 0x0100;
const ULP_CONFIG1_EN_ULP_LANPHYPC: u16 = 0x0400;
const ULP_CONFIG1_DIS_CLR_STICKY_ON_PERST: u16 = 0x0800;
const ULP_CONFIG1_DISABLE_SMB_PERST: u16 = 0x1000;
const HV_SMB_ADDR: u32 = hw::phy_reg(768, 26);
const HV_SMB_ADDR_MASK: u16 = 0x007f;
const HV_SMB_ADDR_PEC_EN: u16 = 0x0200;
const HV_SMB_ADDR_VALID: u16 = 0x0080;
const HV_SMB_ADDR_FREQ_MASK: u16 = 0x1100;
const HV_OEM_BITS: u32 = hw::phy_reg(768, 25);
const HV_OEM_BITS_LPLU: u16 = 0x0004;
const HV_OEM_BITS_GBE_DIS: u16 = 0x0040;
const HV_OEM_BITS_RESTART_AN: u16 = 0x0400;
const HV_LED_CONFIG: u32 = hw::phy_reg(768, 30);
// EMI (extended management interface) addresses, I217 class
const I217_EEE_PCS_STATUS: u16 = 0x9401;
const I217_EEE_ADVERTISEMENT: u16 = 0x8001;
const I217_EEE_LP_ABILITY: u16 = 0x8002;
const I217_RX_CONFIG: u16 = 0xb20c;
const EEE_100_SUPPORTED: u16 = 1 << 1;
const EEE_1000_SUPPORTED: u16 = 1 << 2;

// ── sizes ───────────────────────────────────────────────────────────────────

const NUM_RX: usize = 256;
const NUM_TX: usize = 256;
const BUF_SIZE: usize = 2048;
const ETH_ZLEN: usize = 60;
const MAX_FRAME: usize = 1514;
/// MTU 1500 + Ethernet header + FCS: Linux's `max_frame_size` (LTR, FC).
const MAX_FRAME_SIZE: u32 = 1518;
/// 20000 interrupts/s at most (ITR counts 256 ns units).
const ITR_VALUE: u32 = 1_000_000_000 / (20_000 * 256);

const IRQ_EN_RX: u32 = hw::ICR_RXT0 | hw::ICR_RXDMT0;
const IRQ_EN_BASE: u32 = hw::ICR_LSC | hw::ICR_RXO | hw::ICR_RXSEQ;
/// Uncorrectable packet-buffer ECC error (PCH: MEHE stops the MAC; reset).
const ICR_ECCER: u32 = 1 << 22;
static ECC_ERRORS: AtomicU32 = AtomicU32::new(0);

// ── logging ─────────────────────────────────────────────────────────────────

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
}

fn log(msg: &str) {
    let mut s = String::with_capacity(msg.len() + 10);
    s.push_str("[e1000e] ");
    s.push_str(msg);
    s.push('\n');
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

macro_rules! elog { ($($a:tt)*) => { log(&format!($($a)*)) }; }

fn now_ms() -> u64 { sched::monotonic_ns() / 1_000_000 }
fn udelay(us: u64) { sched::kthread::udelay(us) }
fn msleep(ms: u64) { sched::kthread::msleep(ms) }

// ── MMIO ────────────────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Mmio(usize);

// HVF-safe on aarch64: always a plain `ldr`/`str`. A volatile access in a
// loop (MTA/VFTA clears, the statistics registers) may otherwise compile to a
// post-index writeback form, which carries no instruction syndrome when it
// traps and kills QEMU under HVF ("Assertion failed: (isv)").
impl Mmio {
    #[inline(always)]
    fn r32(&self, o: usize) -> u32 {
        let a = self.0 + o;
        #[cfg(target_arch = "aarch64")]
        unsafe { let v: u32; core::arch::asm!("ldr {v:w}, [{a}]", a = in(reg) a, v = out(reg) v, options(nostack, readonly)); v }
        #[cfg(not(target_arch = "aarch64"))]
        unsafe { core::ptr::read_volatile(a as *const u32) }
    }
    #[inline(always)]
    fn w32(&self, o: usize, v: u32) {
        let a = self.0 + o;
        #[cfg(target_arch = "aarch64")]
        unsafe { core::arch::asm!("str {v:w}, [{a}]", a = in(reg) a, v = in(reg) v, options(nostack)); }
        #[cfg(not(target_arch = "aarch64"))]
        unsafe { core::ptr::write_volatile(a as *mut u32, v) }
    }
    fn set(&self, o: usize, bits: u32) { self.w32(o, self.r32(o) | bits) }
    fn clr(&self, o: usize, bits: u32) { self.w32(o, self.r32(o) & !bits) }
    /// Push posted writes (Linux's e1e_flush).
    fn flush(&self) { let _ = self.r32(STATUS); }
}

// ── shared state ────────────────────────────────────────────────────────────

static BASE: AtomicUsize = AtomicUsize::new(0);
static RUNNING: AtomicBool = AtomicBool::new(false);
static PRESENT: AtomicBool = AtomicBool::new(false);
/// Bit 0 link up, bit 1 full duplex, bits 8.. Mbit/s.
static LINK: AtomicU32 = AtomicU32::new(0);
static LINK_GEN: AtomicU32 = AtomicU32::new(0);
static LINK_CHANGES: AtomicU32 = AtomicU32::new(0);
static LAST_STATUS: AtomicU32 = AtomicU32::new(0);
static FC_BITS: AtomicU32 = AtomicU32::new(0);
static MAC: Mutex<[u8; 6]> = Mutex::new([0; 6]);
static INFO: Mutex<Option<Info>> = Mutex::new(None);

static IRQ_ACTIVE: AtomicBool = AtomicBool::new(false);
static RX_IRQ: AtomicBool = AtomicBool::new(false);
static IRQ_EN: AtomicU32 = AtomicU32::new(0);
static IRQS: AtomicU64 = AtomicU64::new(0);
static IRQ_RX: AtomicU64 = AtomicU64::new(0);
static IRQ_LSC: AtomicU64 = AtomicU64::new(0);
static IRQ_RXO: AtomicU64 = AtomicU64::new(0);
static IRQ_EMPTY: AtomicU64 = AtomicU64::new(0);
static IRQ_LAST: AtomicU32 = AtomicU32::new(0);
static RX_PKTS: AtomicU64 = AtomicU64::new(0);
static RX_BYTES: AtomicU64 = AtomicU64::new(0);
static RX_ERRS: AtomicU64 = AtomicU64::new(0);
static RX_FRAGS: AtomicU64 = AtomicU64::new(0);
static TX_PKTS: AtomicU64 = AtomicU64::new(0);
static TX_BYTES: AtomicU64 = AtomicU64::new(0);
static TX_FULL: AtomicU64 = AtomicU64::new(0);
static RESETS: AtomicU32 = AtomicU32::new(0);
static RX_STALLS: AtomicU32 = AtomicU32::new(0);
static LAST_RX_POLL_MS: AtomicU64 = AtomicU64::new(0);
static PHY_ERRORS: AtomicU32 = AtomicU32::new(0);
static SEM_FAILURES: AtomicU32 = AtomicU32::new(0);

/// Accumulated clear-on-read statistics registers.
static HWSTATS: Mutex<[u64; STAT_REGS.len()]> = Mutex::new([0; STAT_REGS.len()]);
const STAT_REGS: [(usize, &str); 17] = [
    (GPRC, "rx_good"), (GPTC, "tx_good"), (BPRC, "rx_bcast"), (MPRC, "rx_mcast"), (MPC, "rx_missed"),
    (RNBC, "rx_no_buf"), (CRCERRS, "crc_err"), (RXERRC, "rx_err"), (RUC, "rx_undersize"), (ROC, "rx_oversize"),
    (COLC, "collisions"), (XONRXC, "xon_rx"), (XOFFRXC, "xoff_rx"), (XONTXC, "xon_tx"), (XOFFTXC, "xoff_tx"),
    (0x04088, "rx_octets_lo"), (0x04090, "tx_octets_lo"),
];

#[derive(Clone)]
struct Info {
    bdf: (u8, u8, u8),
    dev: &'static DevInfo,
    rev: u8,
    subsys: (u16, u16),
    irq: String,
    mac_src: &'static str,
    phy_id: u32,
    fwsm: u32,
    nvm: String,
    k1: String,
    /// One `step=result` per bring-up step, in order.
    steps: Vec<(&'static str, String)>,
}

struct Rings {
    rx: RxRing,
    tx: TxRing,
    rx_desc_phys: u64,
    tx_desc_phys: u64,
    rx_buf_phys: Vec<u64>,
    tx_buf_phys: Vec<u64>,
}

static RINGS: Mutex<Option<Rings>> = Mutex::new(None);

// ── per-device state owned by the driver thread ─────────────────────────────

struct Opts {
    msi: bool,
    rxirq: bool,
    eee: bool,
    gige: bool,
    k1_off: bool,
    ulp: bool,
    debug: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PhyErr { Sem, Timeout, Mdic, WrongReg, Unsupported }

struct Hw {
    m: Mmio,
    func: PciFunction,
    dev: &'static DevInfo,
    fam: Family,
    opts: Opts,
    phy_id: u32,
    /// SPT+ NVM bank size in words (0 = unknown / not flash).
    bank_words: u32,
    pba: u32,
    fc_high: u32,
    fc_low: u32,
    fc_pause: u32,
    fc_refresh: u32,
    sem_held: bool,
    steps: Vec<(&'static str, String)>,
}

impl Hw {
    fn pch(&self) -> bool { self.fam.is_pch() }
    fn fwsm(&self) -> u32 { if self.pch() { self.m.r32(FWSM) } else { 0 } }
    fn me_active(&self) -> bool { self.fwsm() & hw::FWSM_FW_VALID != 0 }

    fn step(&mut self, name: &'static str, result: String) {
        elog!("step {}: {}", name, result);
        self.steps.push((name, result));
        if let Some(i) = INFO.lock().as_mut() { i.steps = self.steps.clone(); }
    }

    // ── software/firmware semaphore ────────────────────────────────────────

    /// EXTCNF_CTRL.SWFLAG (PCH: arbitrates PHY and some CSRs with the ME;
    /// 82574: MDIO_SW_OWNERSHIP, same bit).
    fn sem_acquire(&mut self) -> Result<(), PhyErr> {
        if self.sem_held { return Ok(()); }
        let mut ext = self.m.r32(EXTCNF_CTRL);
        let mut waited = 0;
        while ext & EXTCNF_SWFLAG != 0 && waited < 100 {
            udelay(1000);
            waited += 1;
            ext = self.m.r32(EXTCNF_CTRL);
        }
        if ext & EXTCNF_SWFLAG != 0 {
            let n = SEM_FAILURES.fetch_add(1, Ordering::Relaxed);
            if n < 8 { elog!("SWFLAG still set after 100 ms (EXTCNF_CTRL {:08x} FWSM {:08x}): someone else holds the PHY", ext, self.fwsm()); }
            if !self.pch() { self.sem_held = true; return Ok(()); }
            return Err(PhyErr::Sem);
        }
        self.m.w32(EXTCNF_CTRL, ext | EXTCNF_SWFLAG);
        let limit = if self.pch() { 1000 } else { 20 };
        for _ in 0..limit {
            if self.m.r32(EXTCNF_CTRL) & EXTCNF_SWFLAG != 0 { self.sem_held = true; return Ok(()); }
            udelay(1000);
        }
        let ext = self.m.r32(EXTCNF_CTRL);
        if !self.pch() {
            // QEMU's 82574 model does not latch the bit: nobody to arbitrate with.
            self.sem_held = true;
            return Ok(());
        }
        let n = SEM_FAILURES.fetch_add(1, Ordering::Relaxed);
        if n < 8 { elog!("SWFLAG not granted within 1 s (EXTCNF_CTRL {:08x} FWSM {:08x}): firmware or hardware holds it", ext, self.fwsm()); }
        self.m.w32(EXTCNF_CTRL, ext & !EXTCNF_SWFLAG);
        Err(PhyErr::Sem)
    }

    fn sem_release(&mut self) {
        if !self.sem_held { return; }
        self.sem_held = false;
        let ext = self.m.r32(EXTCNF_CTRL);
        if ext & EXTCNF_SWFLAG != 0 { self.m.w32(EXTCNF_CTRL, ext & !EXTCNF_SWFLAG); }
    }

    /// Run `f` holding the semaphore.
    fn locked<T>(&mut self, f: impl FnOnce(&mut Hw) -> T) -> Result<T, PhyErr> {
        let had = self.sem_held;
        self.sem_acquire()?;
        let r = f(self);
        if !had { self.sem_release(); }
        Ok(r)
    }

    // ── MDIC / PHY ─────────────────────────────────────────────────────────

    fn mdic(&mut self, cmd: u32, reg: u32) -> Result<u16, PhyErr> {
        self.m.w32(MDIC, cmd);
        let mut v = 0;
        for _ in 0..1920 {
            udelay(50);
            v = self.m.r32(MDIC);
            if v & hw::MDIC_READY != 0 { break; }
        }
        let r = match hw::mdic_decode(v, reg) {
            hw::MdicResult::Ok(x) => Ok(x),
            hw::MdicResult::NotReady => Err(PhyErr::Timeout),
            hw::MdicResult::Error => Err(PhyErr::Mdic),
            hw::MdicResult::WrongReg(_) => Err(PhyErr::WrongReg),
        };
        if r.is_err() { PHY_ERRORS.fetch_add(1, Ordering::Relaxed); }
        r
    }

    fn mdic_read(&mut self, addr: u32, reg: u32) -> Result<u16, PhyErr> { self.mdic(hw::mdic_read_cmd(addr, reg), reg) }
    fn mdic_write(&mut self, addr: u32, reg: u32, v: u16) -> Result<(), PhyErr> {
        self.mdic(hw::mdic_write_cmd(addr, reg, v), reg).map(|_| ())
    }

    /// PHY register read, semaphore already held.
    fn phy_read_l(&mut self, off: u32) -> Result<u16, PhyErr> {
        if !self.pch() { return self.mdic_read(1, off & hw::MAX_PHY_REG); }
        let a = hw::hv_access(off).map_err(|_| PhyErr::Unsupported)?;
        if let Some(p) = a.page_select { self.mdic_write(1, hw::PHY_PAGE_SELECT, p)?; }
        self.mdic_read(a.addr, a.reg)
    }

    fn phy_write_l(&mut self, off: u32, v: u16) -> Result<(), PhyErr> {
        if !self.pch() { return self.mdic_write(1, off & hw::MAX_PHY_REG, v); }
        let a = hw::hv_access(off).map_err(|_| PhyErr::Unsupported)?;
        if let Some(p) = a.page_select { self.mdic_write(1, hw::PHY_PAGE_SELECT, p)?; }
        self.mdic_write(a.addr, a.reg, v)
    }

    fn phy_read(&mut self, off: u32) -> Result<u16, PhyErr> { self.locked(|h| h.phy_read_l(off))? }
    fn phy_write(&mut self, off: u32, v: u16) -> Result<(), PhyErr> { self.locked(|h| h.phy_write_l(off, v))? }
    fn phy_modify(&mut self, off: u32, clear: u16, set: u16) -> Result<u16, PhyErr> {
        self.locked(|h| {
            let v = h.phy_read_l(off)?;
            let n = (v & !clear) | set;
            h.phy_write_l(off, n)?;
            Ok(n)
        })?
    }

    fn emi_write_l(&mut self, addr: u16, v: u16) -> Result<(), PhyErr> {
        self.phy_write_l(I82579_EMI_ADDR, addr)?;
        self.phy_write_l(I82579_EMI_DATA, v)
    }
    fn emi_read_l(&mut self, addr: u16) -> Result<u16, PhyErr> {
        self.phy_write_l(I82579_EMI_ADDR, addr)?;
        self.phy_read_l(I82579_EMI_DATA)
    }

    fn kmrn_read_l(&mut self, off: u32) -> u16 {
        self.m.w32(KMRNCTRLSTA, (off << KMRN_OFFSET_SHIFT) | KMRN_REN);
        self.m.flush();
        udelay(2);
        self.m.r32(KMRNCTRLSTA) as u16
    }
    fn kmrn_write_l(&mut self, off: u32, v: u16) {
        self.m.w32(KMRNCTRLSTA, (off << KMRN_OFFSET_SHIFT) | v as u32);
        self.m.flush();
        udelay(2);
    }

    /// Both PHY id registers answer (semaphore held). Returns the id.
    fn phy_id_l(&mut self) -> Option<u32> {
        for _ in 0..2 {
            let a = match self.phy_read_l(MII_PHYSID1) { Ok(v) => v, Err(_) => continue };
            let b = match self.phy_read_l(MII_PHYSID2) { Ok(v) => v, Err(_) => continue };
            if hw::phy_id_valid(a, b) { return Some(((a as u32) << 16) | b as u32); }
        }
        None
    }

    /// Linux's e1000_phy_is_accessible_pchlan (semaphore held): the PHY id
    /// answers; then, without an ME, SMBus mode is unforced in PHY and MAC.
    fn phy_accessible_l(&mut self, how: &str) -> bool {
        let id = match self.phy_id_l() {
            Some(id) => id,
            None => { elog!("PHY not accessible ({}): PHY id reads failed or all-ones", how); return false; }
        };
        if self.phy_id == 0 { self.phy_id = id; }
        elog!("PHY accessible ({}): id {:08x} = {}", how, id, hw::phy_name(id));
        if !self.me_active() {
            let r = self.phy_read_l(CV_SMB_CTRL).and_then(|v| self.phy_write_l(CV_SMB_CTRL, v & !CV_SMB_CTRL_FORCE_SMBUS));
            self.m.clr(CTRL_EXT, CTRL_EXT_FORCE_SMBUS);
            elog!("no ME: SMBus mode unforced in PHY ({:?}) and MAC", r.map(|_| "ok"));
        }
        true
    }

    /// FWSM.RSPCIPHY clear = the ME blocks PHY resets. Polls up to 300 ms, as
    /// Linux does.
    fn reset_blocked(&self) -> bool {
        if !self.pch() { return false; }
        for i in 0..31 {
            if self.m.r32(FWSM) & hw::FWSM_RSPCIPHY != 0 { return false; }
            if i < 30 { msleep(10); }
        }
        true
    }

    // ── PCH bring-up pieces ────────────────────────────────────────────────

    fn toggle_lanphypc(&mut self) -> String {
        let v = self.m.r32(FEXTNVM3);
        self.m.w32(FEXTNVM3, (v & !FEXTNVM3_PHY_CFG_COUNTER_MASK) | FEXTNVM3_PHY_CFG_COUNTER_50MSEC);
        let mut c = self.m.r32(CTRL);
        c |= CTRL_LANPHYPC_OVERRIDE;
        c &= !CTRL_LANPHYPC_VALUE;
        self.m.w32(CTRL, c);
        self.m.flush();
        udelay(20);
        c &= !CTRL_LANPHYPC_OVERRIDE;
        self.m.w32(CTRL, c);
        self.m.flush();
        let t0 = now_ms();
        let mut seen = false;
        for _ in 0..21 {
            msleep(5);
            if self.m.r32(CTRL_EXT) & CTRL_EXT_LPCD != 0 { seen = true; break; }
        }
        let dt = now_ms() - t0;
        msleep(30);
        let s = if seen { format!("LANPHYPC toggled, LCD power cycle done after {} ms", dt) }
                else { format!("LANPHYPC toggled, LPCD not seen within {} ms", dt) };
        log(&s);
        s
    }

    /// Linux's e1000_disable_ulp_lpt_lp(force = true): bring the PHY out of
    /// Ultra Low Power. With an ME the ME does it (H2ME request, then wait for
    /// FWSM.ULP_CFG_DONE to clear); without one, by software through the PHY.
    fn ulp_disable(&mut self) -> String {
        let fwsm = self.m.r32(FWSM);
        if fwsm & hw::FWSM_FW_VALID != 0 {
            let h = self.m.r32(H2ME);
            self.m.w32(H2ME, (h & !H2ME_ULP) | H2ME_ENFORCE_SETTINGS);
            let t0 = now_ms();
            let mut i = 0;
            let mut ok = true;
            while self.m.r32(FWSM) & hw::FWSM_ULP_CFG_DONE != 0 {
                if i == 250 { ok = false; break; }
                i += 1;
                msleep(10);
            }
            let dt = now_ms() - t0;
            let h2 = self.m.r32(H2ME);
            self.m.w32(H2ME, h2 & !H2ME_ENFORCE_SETTINGS);
            return if !ok {
                format!("ME path: FAIL, ULP_CFG_DONE still set after {} ms (FWSM {:08x}, H2ME was {:08x})", dt, self.m.r32(FWSM), h)
            } else if fwsm & hw::FWSM_ULP_CFG_DONE == 0 {
                format!("ME path: ok, ULP_CFG_DONE was already clear (H2ME was {:08x})", h)
            } else if dt > 1000 {
                format!("ME path: ok after {} ms (> 1 s: Linux calls this a firmware bug; H2ME was {:08x})", dt, h)
            } else {
                format!("ME path: ok, ULP_CFG_DONE cleared after {} ms (H2ME was {:08x})", dt, h)
            };
        }
        // No ME: undo ULP in the PHY by software.
        if self.sem_acquire().is_err() { return String::from("software path: FAIL, SWFLAG not granted"); }
        let lan = self.toggle_lanphypc();
        let mut out = format!("software path: {}", lan);
        let mut smb = self.phy_read_l(CV_SMB_CTRL);
        if smb.is_err() {
            self.m.set(CTRL_EXT, CTRL_EXT_FORCE_SMBUS);
            msleep(50);
            smb = self.phy_read_l(CV_SMB_CTRL);
            out.push_str("; PHY needed forced SMBus");
        }
        match smb {
            Ok(v) => {
                let _ = self.phy_write_l(CV_SMB_CTRL, v & !CV_SMB_CTRL_FORCE_SMBUS);
                self.m.clr(CTRL_EXT, CTRL_EXT_FORCE_SMBUS);
                let pm = self.phy_read_l(HV_PM_CTRL).map(|p| { let _ = self.phy_write_l(HV_PM_CTRL, p | HV_PM_CTRL_K1_ENABLE); p });
                match self.phy_read_l(I218_ULP_CONFIG1) {
                    Ok(u) => {
                        let n = u & !(ULP_CONFIG1_IND | ULP_CONFIG1_STICKY_ULP | ULP_CONFIG1_RESET_TO_SMBUS
                            | ULP_CONFIG1_WOL_HOST | ULP_CONFIG1_INBAND_EXIT | ULP_CONFIG1_EN_ULP_LANPHYPC
                            | ULP_CONFIG1_DIS_CLR_STICKY_ON_PERST | ULP_CONFIG1_DISABLE_SMB_PERST);
                        let _ = self.phy_write_l(I218_ULP_CONFIG1, n);
                        let _ = self.phy_write_l(I218_ULP_CONFIG1, n | ULP_CONFIG1_START);
                        self.m.clr(FEXTNVM7, FEXTNVM7_DISABLE_SMB_PERST);
                        out.push_str(&format!("; CV_SMB_CTRL {:04x}, HV_PM_CTRL {} (K1 re-enabled), ULP_CONFIG1 {:04x} -> {:04x} + START",
                                              v, hx(pm), u, n));
                    }
                    Err(e) => out.push_str(&format!("; ULP_CONFIG1 read failed ({:?})", e)),
                }
            }
            Err(e) => out.push_str(&format!("; FAIL: CV_SMB_CTRL unreadable even in SMBus mode ({:?})", e)),
        }
        self.sem_release();
        let r = self.phy_hw_reset_ich();
        msleep(50);
        out.push_str(&format!("; PHY reset: {}", r));
        out
    }

    /// Linux's e1000_init_phy_workarounds_pchlan.
    fn phy_workarounds(&mut self) -> String {
        // Gate automatic PHY configuration by hardware.
        self.m.set(EXTCNF_CTRL, EXTCNF_GATE_PHY_CFG);
        let ulp = if self.opts.ulp { self.ulp_disable() } else { String::from("skipped (e1000e.ulp=0)") };
        self.step("ulp_exit", ulp);
        if self.sem_acquire().is_err() { return String::from("FAIL: SWFLAG not granted"); }
        let mut how = "first try";
        let mut ok = self.phy_accessible_l(how);
        if !ok {
            self.m.set(CTRL_EXT, CTRL_EXT_FORCE_SMBUS);
            msleep(50);
            how = "MAC forced to SMBus";
            ok = self.phy_accessible_l(how);
        }
        let mut blocked_toggle = false;
        if !ok {
            if self.reset_blocked() {
                blocked_toggle = true;
                log("required LANPHYPC toggle blocked by ME (FWSM.RSPCIPHY clear)");
            } else {
                self.toggle_lanphypc();
                how = "after LANPHYPC toggle";
                ok = self.phy_accessible_l(how);
                if !ok {
                    self.m.clr(CTRL_EXT, CTRL_EXT_FORCE_SMBUS);
                    how = "after LANPHYPC toggle, SMBus unforced";
                    ok = self.phy_accessible_l(how);
                }
            }
        }
        self.sem_release();
        if !ok {
            return format!("FAIL: PHY not accessible{} (CTRL {:08x} CTRL_EXT {:08x} FWSM {:08x})",
                           if blocked_toggle { ", LANPHYPC toggle blocked by ME" } else { "" },
                           self.m.r32(CTRL), self.m.r32(CTRL_EXT), self.m.r32(FWSM));
        }
        if self.reset_blocked() {
            return format!("ok ({}); PHY reset blocked by ME, PHY left as is", how);
        }
        let r = self.phy_hw_reset_generic();
        let after = if self.reset_blocked() { "; ME blocked PHY access after the reset" } else { "" };
        format!("ok ({}); PHY reset: {}{}", how, r, after)
    }

    /// CTRL.PHY_RST pulse, then wait for the configuration (Linux's
    /// e1000e_phy_hw_reset_generic with the ich8lan get_cfg_done).
    fn phy_hw_reset_generic(&mut self) -> String {
        if self.reset_blocked() { return String::from("skipped, blocked by ME"); }
        if self.sem_acquire().is_err() { return String::from("FAIL: SWFLAG not granted"); }
        let c = self.m.r32(CTRL);
        self.m.w32(CTRL, c | CTRL_PHY_RST);
        self.m.flush();
        udelay(100);
        self.m.w32(CTRL, c);
        self.m.flush();
        udelay(300);
        self.sem_release();
        self.cfg_done()
    }

    /// Linux's e1000_phy_hw_reset_ich8lan: generic reset + post-reset work.
    fn phy_hw_reset_ich(&mut self) -> String {
        let a = self.phy_hw_reset_generic();
        let b = self.post_phy_reset();
        format!("{}; {}", a, b)
    }

    /// e1000_get_cfg_done_ich8lan.
    fn cfg_done(&mut self) -> String {
        msleep(10);
        if !self.pch() {
            for _ in 0..10 {
                if self.m.r32(EECD) & EECD_AUTO_RD != 0 { return String::from("NVM auto-read done"); }
                msleep(1);
            }
            return String::from("NVM auto-read not done (ignored)");
        }
        let t0 = now_ms();
        let mut done = false;
        for _ in 0..1500 {
            if self.m.r32(STATUS) & STATUS_LAN_INIT_DONE != 0 { done = true; break; }
            udelay(150);
        }
        let dt = now_ms() - t0;
        self.m.clr(STATUS, STATUS_LAN_INIT_DONE);
        let st = self.m.r32(STATUS);
        let phyra = st & STATUS_PHYRA != 0;
        if phyra { self.m.w32(STATUS, st & !STATUS_PHYRA); }
        format!("LAN_INIT_DONE {}, PHYRA {}", if done { format!("after {} ms", dt) } else { String::from("NOT set (timeout)") },
                if phyra { "cleared" } else { "not set" })
    }

    /// e1000_post_phy_reset_ich8lan for SPT+.
    fn post_phy_reset(&mut self) -> String {
        if !self.pch() { return String::from("post-reset: n/a"); }
        if self.reset_blocked() { return String::from("post-reset: skipped, blocked by ME"); }
        msleep(10);
        let wu = self.phy_modify(BM_PORT_GEN_CFG, BM_WUC_HOST_WU_BIT, 0);
        let lcd = self.sw_lcd_config();
        let oem = self.oem_bits_config();
        format!("post-reset: host wakeup bit cleared {:?}; LCD config: {}; OEM bits: {}", wu.map(|_| "ok"), lcd, oem)
    }

    /// e1000_write_smbus_addr (semaphore held).
    fn write_smbus_addr_l(&mut self) -> Result<u16, PhyErr> {
        let strap = self.m.r32(STRAP);
        let freq = (strap >> 12) & 3;
        let addr = ((strap & 0x00fe_0000) >> 17) as u16;
        let mut v = self.phy_read_l(HV_SMB_ADDR)?;
        v &= !HV_SMB_ADDR_MASK;
        v |= addr | HV_SMB_ADDR_PEC_EN | HV_SMB_ADDR_VALID;
        if freq != 0 {
            let f = freq - 1;
            v &= !HV_SMB_ADDR_FREQ_MASK;
            v |= ((f & 1) as u16) << 8;
            v |= ((f & 2) as u16) << 11;
        }
        self.phy_write_l(HV_SMB_ADDR, v)?;
        Ok(v)
    }

    /// e1000_sw_lcd_config_ich8lan: when the NVM says software configures
    /// the PHY (FEXTNVM.SW_CONFIG), replay the NVM's extended configuration
    /// region into it.
    fn sw_lcd_config(&mut self) -> String {
        if self.m.r32(FEXTNVM) & FEXTNVM_SW_CONFIG_ICH8M == 0 { return String::from("not needed (hardware configures the PHY)"); }
        let ext = self.m.r32(EXTCNF_CTRL);
        let size = (self.m.r32(EXTCNF_SIZE) & 0x00ff_0000) >> 16;
        if size == 0 { return String::from("SW_CONFIG set but the region is empty"); }
        let base_words = (((ext & 0x0fff_0000) >> 16) << 1) as u16;
        // NVM first (no PHY semaphore needed for flash reads).
        let mut pairs = Vec::new();
        for i in 0..size as u16 {
            let d = self.nvm_read(base_words + i * 2);
            let a = self.nvm_read(base_words + i * 2 + 1);
            match (d, a) { (Some(d), Some(a)) => pairs.push((d, a)), _ => return format!("FAIL: NVM read at word {:#x}", base_words + i * 2) }
        }
        let r = self.locked(|h| -> Result<(u16, usize), PhyErr> {
            let smb = h.write_smbus_addr_l()?;
            let led = h.m.r32(LEDCTL) as u16;
            h.phy_write_l(HV_LED_CONFIG, led)?;
            let mut page = 0u16;
            let mut n = 0;
            for &(data, addr) in &pairs {
                if addr as u32 == hw::PHY_PAGE_SELECT { page = data; continue; }
                let reg = (addr as u32 & hw::MAX_PHY_REG) | page as u32;
                match h.phy_write_l(reg, data) {
                    Ok(()) => n += 1,
                    Err(PhyErr::Unsupported) => elog!("LCD config: entry page {} reg {} data {:04x} skipped (page not supported)",
                                                      reg >> 5, reg & 0x1f, data),
                    Err(e) => return Err(e),
                }
            }
            Ok((smb, n))
        });
        match r {
            Ok(Ok((smb, n))) => format!("{} PHY writes from NVM word {:#x} ({} entries), SMBus addr reg {:04x}", n, base_words, size, smb),
            Ok(Err(e)) => format!("FAIL ({:?})", e),
            Err(e) => format!("FAIL ({:?})", e),
        }
    }

    /// e1000_oem_bits_config_ich8lan(d0): carry the MAC's GbE-disable / LPLU
    /// bits into the PHY when software owns them.
    fn oem_bits_config(&mut self) -> String {
        if self.m.r32(FEXTNVM) & FEXTNVM_SW_CONFIG_ICH8M == 0 { return String::from("not needed"); }
        let pc = self.m.r32(PHY_CTRL);
        let blocked = self.reset_blocked();
        let r = self.locked(|h| -> Result<u16, PhyErr> {
            let mut oem = h.phy_read_l(HV_OEM_BITS)?;
            oem &= !(HV_OEM_BITS_GBE_DIS | HV_OEM_BITS_LPLU);
            if pc & PHY_CTRL_GBE_DISABLE != 0 { oem |= HV_OEM_BITS_GBE_DIS; }
            if pc & PHY_CTRL_D0A_LPLU != 0 { oem |= HV_OEM_BITS_LPLU; }
            if !blocked { oem |= HV_OEM_BITS_RESTART_AN; }
            h.phy_write_l(HV_OEM_BITS, oem)?;
            Ok(oem)
        });
        match r {
            Ok(Ok(v)) => format!("PHY_CTRL {:08x} -> HV_OEM_BITS {:04x}{}", pc, v,
                                 if pc & PHY_CTRL_GBE_DISABLE != 0 { " (GIGABIT DISABLED by platform)" } else { "" }),
            Ok(Err(e)) | Err(e) => format!("FAIL ({:?})", e),
        }
    }

    // ── NVM ────────────────────────────────────────────────────────────────

    /// One 32-bit read of the SPT+ GbE flash at byte offset `off`.
    fn flash_read32(&self, off: u32) -> Option<u32> {
        for _ in 0..10 {
            udelay(1);
            // cycle init
            let st = self.m.r32(FL_HSFSTS) & 0xffff;
            if st & (1 << 14) == 0 { return None; } // flash descriptor not valid
            let st2 = st | 0x2 | 0x4; // clear FCERR, DAEL
            self.m.w32(FL_HSFSTS, st2 & 0xffff);
            if st & (1 << 5) != 0 {
                let mut free = false;
                for _ in 0..10_000 {
                    if self.m.r32(FL_HSFSTS) & (1 << 5) == 0 { free = true; break; }
                    udelay(1);
                }
                if !free { return None; }
            }
            self.m.w32(FL_HSFSTS, (st2 | 1) & 0xffff); // flash cycle done (W1C)
            // 4-byte read cycle
            let mut ctl = self.m.r32(FL_HSFSTS) >> 16;
            ctl &= !(3 << 1 | 3 << 8);
            ctl |= 3 << 8; // byte count 4
            self.m.w32(FL_HSFSTS, ctl << 16);
            self.m.w32(FL_FADDR, off & 0x00ff_ffff);
            let ctl = (self.m.r32(FL_HSFSTS) >> 16) | 1; // go
            self.m.w32(FL_HSFSTS, ctl << 16);
            let mut st = 0;
            for _ in 0..100_000 {
                st = self.m.r32(FL_HSFSTS);
                if st & 1 != 0 { break; }
                udelay(1);
            }
            if st & 1 != 0 && st & 2 == 0 { return Some(self.m.r32(FL_FDATA0)); }
            if st & 2 == 0 { return None; } // not an error, just never done
        }
        None
    }

    fn nvm_read(&mut self, word: u16) -> Option<u16> {
        if !self.pch() {
            self.m.w32(EERD, ((word as u32) << 2) | EERD_START);
            for _ in 0..100_000 {
                let v = self.m.r32(EERD);
                if v & EERD_DONE != 0 { return Some((v >> 16) as u16); }
                udelay(5);
            }
            return None;
        }
        if self.bank_words == 0 { return None; }
        let bank = self.nvm_bank()?;
        let act = bank * self.bank_words + word as u32;
        let dw = self.flash_read32((act & !1) * 2)?;
        Some(if act & 1 == 0 { dw as u16 } else { (dw >> 16) as u16 })
    }

    /// Valid NVM bank (0/1) from the signature in word 0x13.
    fn nvm_bank(&self) -> Option<u32> {
        for bank in 0..2 {
            let w = hw::ICH_NVM_SIG_WORD + bank * self.bank_words;
            if let Some(dw) = self.flash_read32(w * 2) {
                if hw::nvm_sig_valid(dw) { return Some(bank); }
            }
        }
        None
    }

    fn nvm_summary(&mut self) -> String {
        let mut s = String::new();
        if self.pch() {
            let strap = self.m.r32(STRAP);
            self.bank_words = hw::spt_flash_bank_words(strap);
            match self.nvm_bank() {
                Some(b) => s.push_str(&format!("bank {} valid ({} words/bank, STRAP {:08x})", b, self.bank_words, strap)),
                None => { s.push_str(&format!("no valid bank found (STRAP {:08x}, HSFSTS {:08x})", strap, self.m.r32(FL_HSFSTS))); return s; }
            }
        }
        let mut words = Vec::with_capacity(64);
        for i in 0..64u16 {
            match self.nvm_read(i) { Some(w) => words.push(w), None => { s.push_str(&format!("; read failed at word {}", i)); return s; } }
        }
        let sum = hw::nvm_checksum(&words);
        let mac = hw::mac_from_nvm([words[0], words[1], words[2]]);
        s.push_str(&format!("{}checksum {:04x} ({}); NVM MAC {}; word 0x1b (K1 cfg) {:04x}",
                            if s.is_empty() { "" } else { "; " }, sum,
                            if sum == hw::NVM_SUM { "valid" } else { "INVALID, Linux would refuse the NIC" }, mac_str(&mac),
                            words[0x1b]));
        s
    }

    // ── MAC reset / init (e1000e_reset) ────────────────────────────────────

    fn disable_pcie_master(&self) -> bool {
        self.m.set(CTRL, CTRL_GIO_MASTER_DISABLE);
        for _ in 0..800 {
            if self.m.r32(STATUS) & STATUS_GIO_MASTER_ENABLE == 0 { return true; }
            udelay(100);
        }
        false
    }

    /// e1000_reset_hw_ich8lan / e1000_reset_hw_82571: full MAC (and PHY,
    /// unless the ME blocks it) reset.
    fn reset_hw(&mut self) -> String {
        let master = self.disable_pcie_master();
        self.m.w32(IMC, !0);
        self.m.w32(RCTL, 0);
        self.m.w32(TCTL, TCTL_PSP);
        self.m.flush();
        msleep(10);
        let mut ctrl = self.m.r32(CTRL);
        let phy_rst = self.pch() && !self.reset_blocked();
        if phy_rst { ctrl |= CTRL_PHY_RST; }
        let sem = self.sem_acquire().is_ok();
        self.m.w32(CTRL, ctrl | CTRL_RST);
        // No flush here: reading during the reset can hang the MAC.
        msleep(20);
        // The reset clears the flag in hardware; forget our claim.
        self.sem_held = false;
        if self.m.r32(EXTCNF_CTRL) & EXTCNF_SWFLAG != 0 {
            if self.pch() {
                // Linux relies on the reset clearing it; if it did not, the
                // next PHY access will say so (SWFLAG still set after 100 ms).
                elog!("SWFLAG still set right after the MAC reset (EXTCNF_CTRL {:08x})", self.m.r32(EXTCNF_CTRL));
            } else {
                self.m.clr(EXTCNF_CTRL, EXTCNF_SWFLAG);
            }
        }
        let mut out = format!("{}{}{}", if master { "" } else { "PCIe master disable timed out; " },
                              if phy_rst { "MAC+PHY reset" } else if self.pch() { "MAC reset (PHY reset blocked by ME)" } else { "MAC reset" },
                              if sem { "" } else { " (without SWFLAG)" });
        if phy_rst {
            let c = self.cfg_done();
            let p = self.post_phy_reset();
            out.push_str(&format!("; {}; {}", c, p));
        } else if !self.pch() {
            let c = self.cfg_done();
            out.push_str(&format!("; {}", c));
        }
        self.m.w32(IMC, !0);
        let _ = self.m.r32(ICR);
        self.m.set(KABGTXD, KABGTXD_BGSQLBIAS);
        out
    }

    /// e1000_flush_desc_rings (SPT+): the descriptor rings must be empty
    /// before a reset, or the MAC can enter a unit hang only a PCI reset
    /// clears. Caller holds the ring lock (`r`).
    fn flush_desc_rings(&mut self, r: &mut Rings) -> String {
        self.m.set(FEXTNVM11, FEXTNVM11_DISABLE_MULR_FIX);
        let tdlen = self.m.r32(TDLEN);
        let hang = self.func.read16(PCICFG_DESC_RING_STATUS);
        if hang & FLUSH_DESC_REQUIRED == 0 || tdlen == 0 {
            return format!("not needed (DESC_RING_STATUS {:04x}, TDLEN {})", hang, tdlen);
        }
        let tdba = self.m.r32(TDBAL) as u64 | (self.m.r32(TDBAH) as u64) << 32;
        if tdba != r.tx_desc_phys {
            return format!("REQUIRED but TDBA {:#x} is not our ring: skipped", tdba);
        }
        // TX: one dummy 512-byte descriptor pointing at the ring itself.
        self.m.set(TCTL, TCTL_EN);
        let tdt = (self.m.r32(TDT) as usize) % NUM_TX;
        unsafe {
            let d = r.tx.descs.add(tdt);
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).addr), r.tx_desc_phys);
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).lower), hw::TXD_CMD_IFCS | 512);
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).upper), 0);
        }
        core::sync::atomic::fence(Ordering::SeqCst);
        self.m.w32(TDT, ((tdt + 1) % NUM_TX) as u32);
        udelay(250);
        // Our ring writes a slot's buffer address only at init: put it back.
        unsafe {
            let d = r.tx.descs.add(tdt);
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).addr), r.tx_buf_phys[tdt]);
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).lower), 0);
            core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).upper), 0);
        }
        let hang2 = self.func.read16(PCICFG_DESC_RING_STATUS);
        let mut s = format!("REQUIRED (DESC_RING_STATUS {:04x}): TX flushed -> {:04x}", hang, hang2);
        if hang2 & FLUSH_DESC_REQUIRED != 0 {
            let rctl = self.m.r32(RCTL);
            self.m.w32(RCTL, rctl & !RCTL_EN);
            self.m.flush();
            udelay(150);
            let rxdctl = (self.m.r32(RXDCTL0) & 0xffff_c000) | 0x1f | (1 << 8) | RXDCTL_THRESH_UNIT_DESC;
            self.m.w32(RXDCTL0, rxdctl);
            self.m.w32(RCTL, rctl | RCTL_EN);
            self.m.flush();
            udelay(150);
            self.m.w32(RCTL, rctl & !RCTL_EN);
            s.push_str(&format!("; RX flushed -> {:04x}", self.func.read16(PCICFG_DESC_RING_STATUS)));
        }
        s
    }

    /// e1000_initialize_hw_bits_ich8lan.
    fn init_hw_bits(&mut self) {
        if !self.pch() {
            // 82574: Linux's e1000_initialize_hw_bits_82571 (82574 subset).
            self.m.set(TXDCTL0, 1 << 22);
            self.m.set(TXDCTL1, 1 << 22);
            self.m.set(TARC0, 1 << 23 | 1 << 24 | 1 << 25 | 1 << 26);
            let mut t1 = self.m.r32(TARC1) | 1 << 24 | 1 << 25 | 1 << 26;
            if self.m.r32(TCTL) & TCTL_MULR != 0 { t1 &= !(1 << 28); } else { t1 |= 1 << 28; }
            self.m.w32(TARC1, t1);
            self.m.set(CTRL_EXT, 1 << 22);
            return;
        }
        self.m.set(CTRL_EXT, 1 << 22 | CTRL_EXT_PHYPDEN);
        self.m.set(TXDCTL0, 1 << 22);
        self.m.set(TXDCTL1, 1 << 22);
        self.m.set(TARC0, 1 << 23 | 1 << 24 | 1 << 26 | 1 << 27);
        let mut t1 = self.m.r32(TARC1);
        if self.m.r32(TCTL) & TCTL_MULR != 0 { t1 &= !(1 << 28); } else { t1 |= 1 << 28; }
        t1 |= 1 << 24 | 1 << 26 | 1 << 30;
        self.m.w32(TARC1, t1);
        self.m.set(RFCTL, RFCTL_NFSW_DIS | RFCTL_NFSR_DIS);
        self.m.set(PBECCSTS, PBECCSTS_ECC_ENABLE);
        self.m.set(CTRL, CTRL_MEHE);
    }

    /// Copper link: MAC set-link-up, PHY CRS/downshift/MDI-X, advertisement
    /// (10/100/1000 + symmetric/asymmetric pause), restart autonegotiation.
    fn setup_link(&mut self) -> String {
        if self.reset_blocked() { return String::from("skipped: PHY owned by the ME (SOL/IDE-R session?)"); }
        let mut c = self.m.r32(CTRL);
        c |= CTRL_SLU;
        c &= !(CTRL_FRCSPD | CTRL_FRCDPX);
        self.m.w32(CTRL, c);
        let gige = self.opts.gige;
        let pch = self.pch();
        let r = self.locked(|h| -> Result<(u16, u16, u16), PhyErr> {
            if pch {
                let cfg = h.phy_read_l(I82577_CFG_REG)?;
                h.phy_write_l(I82577_CFG_REG, cfg | I82577_CFG_ASSERT_CRS_ON_TX | I82577_CFG_ENABLE_DOWNSHIFT)?;
                let c2 = h.phy_read_l(I82577_PHY_CTRL_2)?;
                h.phy_write_l(I82577_PHY_CTRL_2, (c2 & !I82577_PHY_CTRL2_MDIX_CFG_MASK) | I82577_PHY_CTRL2_AUTO_MDI_MDIX)?;
            }
            let mut adv = h.phy_read_l(MII_ADVERTISE)?;
            adv &= !(ADVERTISE_10HALF | ADVERTISE_10FULL | ADVERTISE_100HALF | ADVERTISE_100FULL | hw::ADV_PAUSE | hw::ADV_ASYM);
            adv |= ADVERTISE_10HALF | ADVERTISE_10FULL | ADVERTISE_100HALF | ADVERTISE_100FULL | hw::ADV_PAUSE | hw::ADV_ASYM;
            h.phy_write_l(MII_ADVERTISE, adv)?;
            let mut g = h.phy_read_l(MII_CTRL1000)?;
            g &= !(ADVERTISE_1000FULL | ADVERTISE_1000HALF);
            if gige { g |= ADVERTISE_1000FULL; }
            h.phy_write_l(MII_CTRL1000, g)?;
            let mut bmcr = h.phy_read_l(MII_BMCR)?;
            bmcr &= !BMCR_PDOWN;
            bmcr |= BMCR_ANENABLE | BMCR_ANRESTART;
            h.phy_write_l(MII_BMCR, bmcr)?;
            if pch {
                // Pause time also in the PHY's port control page.
                h.phy_write_l(BM_PORT_CTRL_27, 0xffff)?;
            }
            Ok((adv, g, bmcr))
        });
        // Flow control timers and thresholds (fc = full).
        self.m.w32(FCTTV, self.fc_pause);
        if self.pch() { self.m.w32(FCRTV_PCH, self.fc_refresh); }
        self.m.w32(FCRTL, self.fc_low | FCRTL_XONE);
        self.m.w32(FCRTH, self.fc_high);
        match r {
            Ok(Ok((adv, g, bmcr))) => format!("autonegotiation restarted (ADV {:04x} 1000T_CTRL {:04x} BMCR {:04x}{})", adv, g, bmcr,
                                              if gige { "" } else { ", gigabit not advertised: e1000e.speed=100" }),
            Ok(Err(e)) | Err(e) => format!("FAIL: PHY access ({:?})", e),
        }
    }

    /// e1000_init_hw_ich8lan (+ the 82574 equivalent).
    fn init_hw(&mut self, mac: &[u8; 6]) -> String {
        self.init_hw_bits();
        let (ral, rah) = hw::ral_rah_of(mac);
        self.m.w32(RAL0, ral);
        self.m.flush();
        self.m.w32(RAH0, rah);
        self.m.flush();
        for i in 0..32 { self.m.w32(MTA + 4 * i, 0); }
        for i in 0..128 { self.m.w32(VFTA + 4 * i, 0); }
        let link = self.setup_link();
        if !self.pch() {
            // e1000_init_hw_82571, 82574 branch.
            let t = self.m.r32(TXDCTL0);
            self.m.w32(TXDCTL0, (t & !TXDCTL_WTHRESH) | TXDCTL_FULL_TX_DESC_WB | TXDCTL_COUNT_DESC);
            self.m.set(GCR, GCR_L1_ACT_WITHOUT_L0S_RX);
            stats_poll(self.m);
            return link;
        }
        for reg in [TXDCTL0, TXDCTL1] {
            let mut t = self.m.r32(reg);
            t = (t & !TXDCTL_WTHRESH) | TXDCTL_FULL_TX_DESC_WB;
            t = (t & !TXDCTL_PTHRESH) | TXDCTL_MAX_TX_DESC_PREFETCH;
            self.m.w32(reg, t);
        }
        // PCIe snoop: Linux writes ~PCIE_NO_SNOOP_ALL into GCR (all other
        // bits set); mirrored as is.
        let gcr = self.m.r32(GCR);
        self.m.w32(GCR, (gcr & !PCIE_NO_SNOOP_ALL) | !PCIE_NO_SNOOP_ALL);
        if self.fam >= Family::PchTgp { self.m.set(FFLT_DBG, FFLT_DBG_DONT_GATE_WAKE_DMA_CLK); }
        self.m.set(CTRL_EXT, CTRL_EXT_RO_DIS);
        stats_poll(self.m);
        link
    }

    /// Linux's e1000e_reset: packet buffer, flow-control thresholds, ring
    /// flush (SPT+), MAC reset, DRV_LOAD, init_hw, EEE advertisement.
    fn full_reset(&mut self, rings: Option<&mut Rings>, mac: &[u8; 6]) -> String {
        self.m.w32(PBA, self.pba);
        if self.pch() { self.m.set(FEXTNVM11, FEXTNVM11_DISABLE_MULR_FIX); }
        let flush = match rings {
            Some(r) if self.pch() => self.flush_desc_rings(r),
            _ => String::from("-"),
        };
        let reset = self.reset_hw();
        self.m.set(CTRL_EXT, CTRL_EXT_DRV_LOAD);
        self.m.w32(WUC, 0);
        let link = self.init_hw(mac);
        self.m.w32(VET, 0x8100);
        let mut eee = String::from("-");
        if self.pch() {
            let adv = if self.opts.eee { EEE_100_SUPPORTED | EEE_1000_SUPPORTED } else { 0 };
            eee = match self.locked(|h| h.emi_write_l(I217_EEE_ADVERTISEMENT, adv)) {
                Ok(Ok(())) => format!("EEE advertisement {:#x}{}", adv, if adv == 0 { " (off; e1000e.eee=1 enables)" } else { "" }),
                Ok(Err(e)) | Err(e) => format!("EEE advertisement FAIL ({:?})", e),
            };
            if !IRQ_ACTIVE.load(Ordering::Relaxed) {
                self.m.set(FEXTNVM7, FEXTNVM7_SIDE_CLK_UNGATE);
                self.m.set(FEXTNVM9, FEXTNVM9_IOSFSB_CLKGATE_DIS | FEXTNVM9_IOSFSB_CLKREQ_DIS);
            }
            if self.opts.k1_off { eee.push_str(&format!("; {}", self.configure_k1(false))); }
        }
        format!("ring flush: {}; {}; DRV_LOAD set; link setup: {}; {}", flush, reset, link, eee)
    }

    /// e1000_configure_k1_ich8lan.
    fn configure_k1(&mut self, enable: bool) -> String {
        let r = self.locked(|h| {
            let mut k = h.kmrn_read_l(KMRN_K1_CONFIG);
            let before = k;
            if enable { k |= KMRN_K1_ENABLE; } else { k &= !KMRN_K1_ENABLE; }
            h.kmrn_write_l(KMRN_K1_CONFIG, k);
            udelay(40);
            let ctrl_ext = h.m.r32(CTRL_EXT);
            let ctrl = h.m.r32(CTRL);
            h.m.w32(CTRL, (ctrl & !(CTRL_SPD_1000 | CTRL_SPD_100)) | CTRL_FRCSPD);
            h.m.w32(CTRL_EXT, ctrl_ext | CTRL_EXT_SPD_BYPS);
            h.m.flush();
            udelay(40);
            h.m.w32(CTRL, ctrl);
            h.m.w32(CTRL_EXT, ctrl_ext);
            h.m.flush();
            udelay(40);
            (before, k)
        });
        match r {
            Ok((b, a)) => format!("K1 {} (Kumeran K1 config {:04x} -> {:04x})", if enable { "enabled" } else { "disabled" }, b, a),
            Err(e) => format!("K1 config FAIL ({:?})", e),
        }
    }

    fn k1_state(&mut self) -> String {
        if !self.pch() { return String::from("n/a"); }
        let r = self.locked(|h| h.kmrn_read_l(KMRN_K1_CONFIG));
        let pm = self.phy_read(HV_PM_CTRL);
        match r {
            Ok(k) => format!("Kumeran K1 {} ({:04x}), PHY HV_PM_CTRL {}", if k & KMRN_K1_ENABLE != 0 { "enabled" } else { "disabled" }, k, hx(pm)),
            Err(e) => format!("unknown ({:?})", e),
        }
    }

    /// e1000_lv_jumbo_workaround_ich8lan(false), which Linux runs at every
    /// open on 82579 and newer: program the MAC/PHY receive path for
    /// standard frames (FIFO/Kumeran/PHY values, RX path gated meanwhile).
    fn jumbo_workaround_off(&mut self) -> String {
        if !self.pch() { return String::from("n/a"); }
        self.m.clr(FFLT_DBG, 0xf << 14);
        self.m.clr(RCTL, RCTL_SECRC);
        let r = self.locked(|h| -> Result<(), PhyErr> {
            let gate = h.phy_read_l(hw::phy_reg(769, 20))?;
            h.phy_write_l(hw::phy_reg(769, 20), gate | 1 << 14)?;
            let body = h.jumbo_off_body_l();
            // Ungate whatever happened above: a gated RX path is silent death.
            let mut ungate = h.phy_write_l(hw::phy_reg(769, 20), gate & !(1 << 14));
            for _ in 0..3 {
                if ungate.is_ok() { break; }
                udelay(1000);
                ungate = h.phy_write_l(hw::phy_reg(769, 20), gate & !(1 << 14));
            }
            if let Err(e) = ungate { elog!("CRITICAL: PHY receive path left gated (769.20 write failed: {:?})", e); }
            body.and(ungate)
        });
        match r { Ok(Ok(())) => String::from("ok"), Ok(Err(e)) | Err(e) => format!("FAIL ({:?})", e) }
    }

    fn jumbo_off_body_l(&mut self) -> Result<(), PhyErr> {
        let h = self;
        {
            let k = h.kmrn_read_l(0x1);
            h.kmrn_write_l(0x1, k & !1);
            let k = h.kmrn_read_l(0x10);
            h.kmrn_write_l(0x10, (k & !(0xf << 8)) | 0xb << 8);
            let d = h.phy_read_l(hw::phy_reg(769, 23))?;
            h.phy_write_l(hw::phy_reg(769, 23), d & !(0x7f << 5))?;
            let d = h.phy_read_l(hw::phy_reg(769, 16))?;
            h.phy_write_l(hw::phy_reg(769, 16), d | 1 << 13)?;
            let d = h.phy_read_l(SPT_PTR_GAP_REG)?;
            h.phy_write_l(SPT_PTR_GAP_REG, (d & !(0x3ff << 2)) | 0x8 << 2)?;
            h.phy_write_l(hw::phy_reg(776, 23), 0x7e00)?;
            let d = h.phy_read_l(HV_PM_CTRL)?;
            h.phy_write_l(HV_PM_CTRL, d & !(1 << 10))?;
            Ok(())
        }
    }

    /// Ring-independent part of e1000_configure, run before `configure`
    /// (it sleeps and takes the PHY semaphore, so not under the ring lock).
    fn pre_configure(&mut self) {
        if self.pch() {
            let j = self.jumbo_workaround_off();
            if !j.starts_with("ok") { elog!("standard-frame receive workaround: {}", j); }
        }
    }

    // ── rings (e1000_configure) ────────────────────────────────────────────

    fn configure(&mut self, r: &mut Rings) {
        r.rx.reset();
        r.tx.reset();
        // TX
        self.m.w32(TDBAL, r.tx_desc_phys as u32);
        self.m.w32(TDBAH, (r.tx_desc_phys >> 32) as u32);
        self.m.w32(TDLEN, (NUM_TX * 16) as u32);
        self.m.w32(TDH, 0);
        self.m.w32(TDT, 0);
        self.m.w32(TIDV, 8);
        self.m.w32(TADV, 32);
        self.m.w32(TXDCTL1, self.m.r32(TXDCTL0));
        let mut tctl = self.m.r32(TCTL);
        tctl &= !(TCTL_CT | TCTL_COLD);
        tctl |= TCTL_PSP | TCTL_RTLC | (COLLISION_THRESHOLD << 4) | (COLLISION_DISTANCE << 12);
        self.m.w32(TCTL, tctl);
        if self.fam == Family::PchSpt {
            // SPT/KBL silicon errata: data corruption and TX hang avoidance.
            self.m.set(IOSFPC, RCTL_RDMTS_HEX);
            let t = self.m.r32(TARC0);
            self.m.w32(TARC0, (t & !TARC0_CB_MULTIQ_MASK) | TARC0_CB_MULTIQ_2_REQ);
        }
        // RX control (receiver kept off until the ring is programmed)
        let mut rctl = self.m.r32(RCTL);
        rctl &= !(RCTL_EN | RCTL_MO_MASK | RCTL_SBP | RCTL_LPE | RCTL_LBM_MASK | RCTL_RDMTS_MASK | RCTL_DTYP_MASK
                  | RCTL_SZ_MASK | RCTL_BSEX | RCTL_UPE);
        rctl |= RCTL_BAM | RCTL_MPE | RCTL_SECRC; // 2048-byte buffers, all multicast, CRC stripped
        self.m.clr(RFCTL, RFCTL_EXTEN); // legacy descriptors
        self.m.w32(RCTL, rctl);
        self.m.flush();
        udelay(10_000);
        self.m.w32(RDTR, 0);
        self.m.w32(RADV, 8);
        self.m.w32(ITR, ITR_VALUE);
        self.m.w32(IAM, 0);
        self.m.w32(RDBAL, r.rx_desc_phys as u32);
        self.m.w32(RDBAH, (r.rx_desc_phys >> 32) as u32);
        self.m.w32(RDLEN, (NUM_RX * 16) as u32);
        self.m.w32(RDH, 0);
        self.m.w32(RDT, 0);
        self.m.clr(RXCSUM, RXCSUM_TUOFL);
        self.m.w32(RCTL, rctl | RCTL_EN);
        self.m.w32(RDT, r.rx.initial_tail());
        // A link that is already up (the monitor does the per-speed work).
        if self.m.r32(STATUS) & hw::STATUS_LU != 0 { self.m.set(TCTL, TCTL_EN); }
        if IRQ_ACTIVE.load(Ordering::Relaxed) { self.m.w32(IMS, IRQ_EN.load(Ordering::Relaxed)); }
    }

    // ── link-change work ───────────────────────────────────────────────────

    /// LTR (PCH): tell the platform how long the RX buffer can wait.
    fn platform_pm(&mut self, link: Option<u32>) -> String {
        let mut reg = LTRV_SEND;
        let mut enc = 0u16;
        if let Some(speed) = link {
            reg |= LTRV_REQ | (LTRV_REQ << LTRV_NOSNOOP_SHIFT);
            let rxa = self.m.r32(PBA) & 0xffff;
            let ms = self.func.read16(PCI_LTR_CAP_LPT);
            let mn = self.func.read16(PCI_LTR_CAP_LPT + 2);
            match hw::ltr_encode(rxa, MAX_FRAME_SIZE, speed, ms, mn) {
                Some(e) => enc = e,
                None => return format!("LTR: invalid scale (rxa {} KiB, speed {})", rxa, speed),
            }
        }
        reg |= enc as u32 | (enc as u32) << LTRV_NOSNOOP_SHIFT;
        self.m.w32(LTRV, reg);
        format!("LTRV {:08x}", reg)
    }

    fn fextnvm_link_bits(&mut self) {
        let v = self.m.r32(FEXTNVM4);
        self.m.w32(FEXTNVM4, (v & !FEXTNVM4_BEACON_DURATION_MASK) | FEXTNVM4_BEACON_DURATION_8USEC);
        let mut f6 = self.m.r32(FEXTNVM6);
        if self.fam == Family::PchSpt {
            if self.m.r32(PCIEANACFG) & FEXTNVM6_K1_OFF_ENABLE != 0 { f6 |= FEXTNVM6_K1_OFF_ENABLE; } else { f6 &= !FEXTNVM6_K1_OFF_ENABLE; }
        }
        self.m.w32(FEXTNVM6, f6);
    }

    /// Link went up: the per-speed PCH settings of
    /// e1000_check_for_copper_link_ich8lan and the watchdog, flow control,
    /// transmitter on. Returns a summary.
    fn on_link_up(&mut self, speed: u32, full: bool) -> String {
        let mut notes: Vec<String> = Vec::new();
        if self.pch() {
            if self.me_active() && self.m.r32(STATUS) & STATUS_PCIM_STATE != 0 {
                let t0 = now_ms();
                let mut n = 0;
                while self.m.r32(STATUS) & STATUS_PCIM_STATE != 0 && n < 100 { msleep(10); n += 1; }
                let cleared = self.m.r32(STATUS) & STATUS_PCIM_STATE == 0;
                notes.push(format!("ME DMoff {} after {} ms", if cleared { "exited" } else { "NOT exited" }, now_ms() - t0));
                if cleared { notes.push(format!("PHY reset (as Linux): {}", self.phy_hw_reset_ich())); }
            }
            let mut tipg = self.m.r32(TIPG) & !0x3ff;
            let emi_val;
            if !full && speed == 10 { tipg |= 0xff; emi_val = 0; }
            else if full && speed != 1000 { tipg |= 0xc; emi_val = 1; }
            else { tipg |= 0x08; emi_val = 1; }
            self.m.w32(TIPG, tipg);
            let spt = self.fam >= Family::PchSpt;
            let r = self.locked(|h| -> Result<(), PhyErr> {
                h.emi_write_l(I217_RX_CONFIG, emi_val)?;
                let mut pll = h.phy_read_l(I217_PLL_CLOCK_GATE_REG)?;
                pll &= !I217_PLL_CLOCK_GATE_MASK;
                pll |= if speed == 1000 { 0xfa } else { 0x3e8 };
                h.phy_write_l(I217_PLL_CLOCK_GATE_REG, pll)?;
                if speed == 1000 {
                    let pm = h.phy_read_l(HV_PM_CTRL)?;
                    h.phy_write_l(HV_PM_CTRL, pm | HV_PM_CTRL_K1_CLK_REQ)?;
                }
                if spt {
                    if speed == 1000 {
                        let d = h.phy_read_l(SPT_PTR_GAP_REG)?;
                        if (d >> 2) & 0x3ff < 0x18 { h.phy_write_l(SPT_PTR_GAP_REG, (d & !(0x3ff << 2)) | (0x18 << 2))?; }
                    } else {
                        h.phy_write_l(SPT_PTR_GAP_REG, 0xc023)?;
                    }
                }
                Ok(())
            });
            notes.push(format!("TIPG {:08x}, EMI/PLL/K1-clkreq/ptr-gap {:?}", tipg, r.map(|x| x.map(|_| "ok"))));
            self.fextnvm_link_bits();
            notes.push(self.platform_pm(Some(speed)));
            notes.push(self.set_eee());
        }
        // Collision distance + flow control resolution.
        let tctl = self.m.r32(TCTL);
        self.m.w32(TCTL, (tctl & !TCTL_COLD) | (COLLISION_DISTANCE << 12));
        let ad = self.phy_read(MII_ADVERTISE);
        let lp = self.phy_read(MII_LPA);
        let (rx, tx) = match (ad, lp) { (Ok(a), Ok(l)) => hw::fc_resolve(a, l), _ => (false, false) };
        let mut c = self.m.r32(CTRL) & !(CTRL_RFCE | CTRL_TFCE);
        if rx { c |= CTRL_RFCE; }
        if tx { c |= CTRL_TFCE; }
        self.m.w32(CTRL, c);
        FC_BITS.store((rx as u32) | (tx as u32) << 1, Ordering::Relaxed);
        notes.push(format!("flow control rx {} tx {} (ADV {} LPA {})", on(rx), on(tx), hx(ad), hx(lp)));
        self.m.set(TCTL, TCTL_EN);
        notes.join("; ")
    }

    fn on_link_down(&mut self) {
        if self.pch() {
            self.fextnvm_link_bits();
            let _ = self.platform_pm(None);
        }
        FC_BITS.store(0, Ordering::Relaxed);
    }

    /// e1000_set_eee_pchlan after link up.
    fn set_eee(&mut self) -> String {
        let eee = self.opts.eee;
        let r = self.locked(|h| -> Result<(u16, u16), PhyErr> {
            let mut lpi = h.phy_read_l(I82579_LPI_CTRL)?;
            lpi &= !I82579_LPI_CTRL_ENABLE_MASK;
            let mut lp_ab = 0;
            if eee {
                lp_ab = h.emi_read_l(I217_EEE_LP_ABILITY)?;
                let adv = h.emi_read_l(I217_EEE_ADVERTISEMENT)?;
                if adv & lp_ab & EEE_1000_SUPPORTED != 0 { lpi |= I82579_LPI_CTRL_1000_ENABLE; }
                if adv & lp_ab & EEE_100_SUPPORTED != 0 && h.phy_read_l(MII_LPA)? & ADVERTISE_100FULL != 0 {
                    lpi |= I82579_LPI_CTRL_100_ENABLE;
                }
            }
            let _ = h.emi_read_l(I217_EEE_PCS_STATUS)?;
            h.phy_write_l(I82579_LPI_CTRL, lpi)?;
            Ok((lpi, lp_ab))
        });
        match r {
            Ok(Ok((lpi, lp))) => format!("EEE LPI_CTRL {:04x} (partner EEE {:04x})", lpi, lp),
            Ok(Err(e)) | Err(e) => format!("EEE FAIL ({:?})", e),
        }
    }

    // ── diagnostics ────────────────────────────────────────────────────────

    fn mac_dump(&self) -> String {
        let m = self.m;
        let mut s = format!("CTRL {:08x} STATUS {:08x} CTRL_EXT {:08x} RCTL {:08x} TCTL {:08x} IMS {:08x} RDH {} RDT {} TDH {} TDT {} PBA {:08x}",
                            m.r32(CTRL), m.r32(STATUS), m.r32(CTRL_EXT), m.r32(RCTL), m.r32(TCTL), m.r32(IMS),
                            m.r32(RDH), m.r32(RDT), m.r32(TDH), m.r32(TDT), m.r32(PBA));
        if self.pch() {
            s.push_str(&format!(" FWSM {:08x} EXTCNF_CTRL {:08x} H2ME {:08x} PHY_CTRL {:08x} FEXTNVM6 {:08x} FEXTNVM7 {:08x} LTRV {:08x} DESC_RING_STATUS {:04x}",
                                m.r32(FWSM), m.r32(EXTCNF_CTRL), m.r32(H2ME), m.r32(PHY_CTRL), m.r32(FEXTNVM6), m.r32(FEXTNVM7),
                                m.r32(LTRV), self.func.read16(PCICFG_DESC_RING_STATUS)));
        }
        s
    }

    fn phy_dump(&mut self) -> String {
        let mut regs: Vec<(&str, u32)> = alloc::vec![("BMCR", MII_BMCR), ("BMSR", MII_BMSR), ("ADV", MII_ADVERTISE), ("LPA", MII_LPA),
                                                     ("1000CTRL", MII_CTRL1000), ("1000STAT", MII_STAT1000)];
        if self.pch() {
            regs.extend_from_slice(&[("STATUS2", I82577_PHY_STATUS_2), ("CTRL2", I82577_PHY_CTRL_2), ("CFG", I82577_CFG_REG),
                                     ("CV_SMB_CTRL", CV_SMB_CTRL), ("ULP_CONFIG1", I218_ULP_CONFIG1), ("HV_PM_CTRL", HV_PM_CTRL),
                                     ("OEM_BITS", HV_OEM_BITS), ("PORT_GEN_CFG", BM_PORT_GEN_CFG)]);
        }
        let mut s = String::new();
        for (name, off) in regs {
            match self.phy_read(off) {
                Ok(v) => s.push_str(&format!("{}={:04x} ", name, v)),
                Err(e) => s.push_str(&format!("{}=ERR({:?}) ", name, e)),
            }
        }
        s
    }

    fn reset_datapath(&mut self, why: &str) {
        let n = RESETS.fetch_add(1, Ordering::Relaxed) + 1;
        elog!("{}: resetting (reset #{}): {}", why, n, self.mac_dump());
        RUNNING.store(false, Ordering::SeqCst);
        // Data-path calls check RUNNING before taking the ring lock; let any
        // that were already past the check finish.
        msleep(2);
        let mac = *MAC.lock();
        // Ring flush (SPT+) under the lock (microsecond waits only), the
        // sleeping MAC/PHY work outside it, then the rings again.
        let flush = match RINGS.lock().as_mut() {
            Some(r) if self.pch() => { self.m.set(FEXTNVM11, FEXTNVM11_DISABLE_MULR_FIX); self.flush_desc_rings(r) }
            _ => String::from("-"),
        };
        let s = self.full_reset(None, &mac);
        self.pre_configure();
        if let Some(r) = RINGS.lock().as_mut() { self.configure(r); }
        elog!("reset #{} done: ring flush: {}; {}", n, flush, s);
        LINK.store(0, Ordering::SeqCst);
        RUNNING.store(true, Ordering::SeqCst);
    }
}

fn on(b: bool) -> &'static str { if b { "on" } else { "off" } }

/// A PHY register value for the log: 4 hex digits, or the error.
fn hx(r: Result<u16, PhyErr>) -> String {
    match r { Ok(v) => format!("{:04x}", v), Err(e) => format!("ERR({:?})", e) }
}

fn mac_str(m: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

/// Fold the clear-on-read statistics registers into the totals.
fn stats_poll(m: Mmio) -> [u64; STAT_REGS.len()] {
    let mut g = HWSTATS.lock();
    for (i, &(reg, _)) in STAT_REGS.iter().enumerate() {
        g[i] += m.r32(reg) as u64;
        if reg == 0x04088 || reg == 0x04090 { let _ = m.r32(reg + 4); }
    }
    *g
}

fn stat(name: &str) -> u64 {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 { return 0; }
    let s = stats_poll(Mmio(base));
    STAT_REGS.iter().position(|&(_, n)| n == name).map_or(0, |i| s[i])
}

// ── interrupt ───────────────────────────────────────────────────────────────

impl hw::IcrRegs for Mmio {
    fn icr(&mut self) -> u32 { self.r32(ICR) }
    fn ack(&mut self, bits: u32) { self.w32(ICR, bits) }
    fn mask_all(&mut self) { self.w32(IMC, !0) }
    fn unmask(&mut self, bits: u32) { self.w32(IMS, bits) }
}

extern "C" fn isr(_ctx: usize) -> crate::irq::IrqReturn {
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 { return crate::irq::IrqReturn::None; }
    let mut m = Mmio(base);
    let st = match hw::irq_service(&mut m) {
        Some(st) => st,
        None => { IRQ_EMPTY.fetch_add(1, Ordering::Relaxed); return crate::irq::IrqReturn::None; }
    };
    IRQS.fetch_add(1, Ordering::Relaxed);
    IRQ_LAST.store(st, Ordering::Relaxed);
    if st & (hw::ICR_RXT0 | hw::ICR_RXDMT0) != 0 { IRQ_RX.fetch_add(1, Ordering::Relaxed); }
    if st & hw::ICR_LSC != 0 { IRQ_LSC.fetch_add(1, Ordering::Relaxed); }
    if st & hw::ICR_RXO != 0 { IRQ_RXO.fetch_add(1, Ordering::Relaxed); }
    if st & ICR_ECCER != 0 { ECC_ERRORS.fetch_add(1, Ordering::Relaxed); }
    crate::netdev::kick_from_irq();
    crate::irq::IrqReturn::Handled
}

// ── probe ───────────────────────────────────────────────────────────────────

fn cmdline_val<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('='))).last()
}

/// Probe and set up the first supported Intel NIC, if any. Boot context;
/// returns quickly (PHY, ME and link work happens in the `[e1000e]` thread).
pub fn init(cmdline: &str) {
    if cmdline_val(cmdline, "e1000e.disable") == Some("1") {
        log("disabled (e1000e.disable=1)");
        return;
    }
    let mut found = None;
    for d in hw::DEVICES {
        if let Some(p) = pci::find_device(hw::INTEL, d.did) { found = Some((p, d)); break; }
    }
    let (pdev, dev) = match found {
        Some(x) => x,
        None => { log("no Intel I219 / 82574 (8086:15xx, 8086:10d3) found; driver inert"); return; }
    };
    let func = pdev.function();
    let opts = Opts {
        msi: cmdline_val(cmdline, "e1000e.msi") != Some("0"),
        rxirq: cmdline_val(cmdline, "e1000e.rxirq") != Some("0"),
        eee: cmdline_val(cmdline, "e1000e.eee") == Some("1"),
        gige: cmdline_val(cmdline, "e1000e.speed") != Some("100"),
        k1_off: cmdline_val(cmdline, "e1000e.k1") == Some("0"),
        ulp: cmdline_val(cmdline, "e1000e.ulp") != Some("0"),
        debug: cmdline_val(cmdline, "e1000e.debug") == Some("1"),
    };
    let rev = func.read8(0x08);
    let subsys = (func.read16(0x2c), func.read16(0x2e));
    elog!("found 8086:{:04x} {} (family {}) at {:02x}:{:02x}.{} rev {:02x} subsystem {:04x}:{:04x}",
          dev.did, dev.name, dev.fam.name(), pdev.bus, pdev.dev, pdev.func, rev, subsys.0, subsys.1);

    if let Some(pm) = func.find_capability(pci::cap::PM) {
        let pmcsr = func.read16(pm + 4);
        if pmcsr & 3 != 0 {
            elog!("PCI power state D{} -> D0", pmcsr & 3);
            func.write16(pm + 4, pmcsr & !3);
            msleep(10);
        }
    }
    if dev.fam == Family::I82574 {
        // Linux disables L0s and L1 on the 82574 (FLAG2_DISABLE_ASPM_*).
        if let Some(c) = func.find_capability(pci::cap::PCIE) {
            let lnkctl = func.read16(c + 0x10);
            if lnkctl & 3 != 0 { func.write16(c + 0x10, lnkctl & !3); elog!("PCIe ASPM {:x} -> off", lnkctl & 3); }
        }
    }
    func.enable_mem_and_bus_master();
    let bar = match func.bar(0) {
        Some(b) if !b.io && b.base != 0 => b,
        _ => { log("BAR0 is not a usable memory BAR; giving up"); return; }
    };
    let (va, size) = match func.map_bar(0, mm::kva::CacheMode::Uncached) {
        Some(x) => x,
        None => { elog!("could not map BAR0 {:#x}; giving up", bar.base); return; }
    };
    let m = Mmio(va);
    let status = m.r32(STATUS);
    if status == !0 {
        elog!("STATUS reads all-ones (BAR0 {:#x}): device not responding; giving up", bar.base);
        return;
    }
    let pch = dev.fam.is_pch();
    elog!("BAR0 {:#x} ({} KiB): CTRL {:08x} STATUS {:08x} CTRL_EXT {:08x} EXTCNF_CTRL {:08x} RCTL {:08x} TCTL {:08x}",
          bar.base, size / 1024, m.r32(CTRL), status, m.r32(CTRL_EXT), m.r32(EXTCNF_CTRL), m.r32(RCTL), m.r32(TCTL));
    let fwsm = if pch { m.r32(FWSM) } else { 0 };
    if pch {
        let f = hw::fwsm_decode(fwsm);
        elog!("ME/firmware: FWSM {:08x} = {} (mode {} {}), PHY reset {}, ULP_CFG_DONE {}, WLOCK_MAC {}, PCIM2PCI {}; H2ME {:08x} MANC {:08x} STRAP {:08x} FEXTNVM {:08x} PHY_CTRL {:08x}",
              fwsm, if f.fw_valid { "ME firmware VALID (manageability active)" } else { "no ME firmware" }, f.mode,
              hw::fwsm_mode_name(f.mode), if f.phy_reset_allowed { "allowed" } else { "BLOCKED" }, f.ulp_cfg_done,
              f.wlock_mac, f.pcim2pci, m.r32(H2ME), m.r32(MANC), m.r32(STRAP), m.r32(FEXTNVM), m.r32(PHY_CTRL));
        elog!("link at probe: {}", {
            let (up, sp, fd) = hw::link_from_status(status);
            if up { format!("up {} Mbps {} duplex (firmware left it up)", sp, if fd { "full" } else { "half" }) } else { String::from("down") }
        });
    }

    let mut hwst = Hw {
        m, func, dev, fam: dev.fam, opts, phy_id: 0, bank_words: 0,
        pba: if pch { 26 } else { 32 },
        fc_high: 0, fc_low: 0, fc_pause: 0, fc_refresh: 0, sem_held: false, steps: Vec::new(),
    };
    if pch {
        hwst.bank_words = hw::spt_flash_bank_words(m.r32(STRAP));
        hwst.fc_high = 0x5c20;
        hwst.fc_low = 0x5048;
        hwst.fc_pause = 0xffff;
        hwst.fc_refresh = 0xffff;
    } else {
        let pba_bytes = hwst.pba << 10;
        let hwm = ((pba_bytes * 9 / 10).min(pba_bytes - MAX_FRAME_SIZE)) & 0xfff8;
        hwst.fc_high = hwm;
        hwst.fc_low = hwm - 8;
        hwst.fc_pause = 0x0680;
    }
    // Interrupts off until the rings run.
    m.w32(IMC, !0);
    let _ = m.r32(ICR);

    // MAC address: RAL0/RAH0 (loaded from the NVM by the hardware), else the
    // NVM itself, else a random locally administered one.
    let ral_mac = hw::mac_from_ral(m.r32(RAL0), m.r32(RAH0));
    let (mac, src) = if hw::mac_valid(&ral_mac) {
        (ral_mac, "RAL0/RAH0")
    } else {
        let w = [hwst.nvm_read(0), hwst.nvm_read(1), hwst.nvm_read(2)];
        let nvm_mac = match w { [Some(a), Some(b), Some(c)] => Some(hw::mac_from_nvm([a, b, c])), _ => None };
        match nvm_mac {
            Some(nm) if hw::mac_valid(&nm) => (nm, "NVM (RAL0 was empty)"),
            _ => {
                let t = sched::monotonic_ns();
                let r = [0x02, 0x1e, (t >> 8) as u8, (t >> 16) as u8, (t >> 24) as u8, t as u8 ^ 0x5a];
                (r, "RANDOM (RAL0 and NVM unusable)")
            }
        }
    };
    *MAC.lock() = mac;
    elog!("MAC {} (from {})", mac_str(&mac), src);

    let rings = match alloc_rings() {
        Some(r) => r,
        None => { log("out of DMA memory for the rings; giving up"); return; }
    };
    elog!("rings: RX {} x {} B at {:#x}, TX {} x {} B at {:#x}", NUM_RX, BUF_SIZE, rings.rx_desc_phys, NUM_TX, BUF_SIZE, rings.tx_desc_phys);
    *RINGS.lock() = Some(rings);
    BASE.store(va, Ordering::SeqCst);

    let irq_desc = if !hwst.opts.msi {
        String::from("none (e1000e.msi=0, polled)")
    } else {
        match crate::irq::request_msi(func, isr, 0) {
            Ok(h) => {
                let v = h.vector();
                core::mem::forget(h);
                let mut en = IRQ_EN_BASE;
                if pch { en |= ICR_ECCER; }
                if hwst.opts.rxirq { en |= IRQ_EN_RX; }
                IRQ_EN.store(en, Ordering::SeqCst);
                IRQ_ACTIVE.store(true, Ordering::SeqCst);
                RX_IRQ.store(hwst.opts.rxirq, Ordering::SeqCst);
                let rx = if hwst.opts.rxirq { "RX + link/errors" } else { "link/errors only, RX polled (e1000e.rxirq=0)" };
                match v { Some(v) => format!("MSI vector {:#04x} ({})", v, rx), None => format!("MSI ({})", rx) }
            }
            Err(e) => format!("none ({:?}; polled)", e),
        }
    };
    elog!("interrupt: {}", irq_desc);

    *INFO.lock() = Some(Info {
        bdf: (pdev.bus, pdev.dev, pdev.func), dev, rev, subsys, irq: irq_desc, mac_src: src, phy_id: 0, fwsm,
        nvm: String::from("pending"), k1: String::from("pending"), steps: Vec::new(),
    });
    PRESENT.store(true, Ordering::SeqCst);

    if sched::kthread::spawn("e1000e", move || driver_thread(hwst)).is_none() {
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
    let rx = unsafe { RxRing::init(mm::phys_to_virt(rxd) as *mut hw::RxDesc, NUM_RX, BUF_SIZE as u32, &|i| rx_buf_phys[i]) };
    let tx = unsafe { TxRing::init(mm::phys_to_virt(txd) as *mut hw::TxDesc, NUM_TX, &|i| tx_buf_phys[i]) };
    Some(Rings { rx, tx, rx_desc_phys: rxd as u64, tx_desc_phys: txd as u64, rx_buf_phys, tx_buf_phys })
}

// ── driver thread ───────────────────────────────────────────────────────────

fn bring_up(hw: &mut Hw) {
    let t0 = now_ms();
    let mac = *MAC.lock();
    if hw.pch() {
        let r = hw.phy_workarounds();
        hw.step("phy_init", r);
    }
    // PHY id (also for the 82574).
    if hw.phy_id == 0 {
        let id = hw.locked(|h| h.phy_id_l());
        match id {
            Ok(Some(id)) => hw.phy_id = id,
            _ if hw.pch() => {
                // Linux: slow MDIO mode, then read again.
                let slow = hw.phy_modify(HV_KMRN_MODE_CTRL, 0, HV_KMRN_MDIO_SLOW);
                let id2 = hw.locked(|h| h.phy_id_l());
                elog!("PHY id unreadable; MDIO slow mode {:?}; retry {:?}", slow.map(|_| "set"), id2);
                if let Ok(Some(id)) = id2 { hw.phy_id = id; }
            }
            _ => {}
        }
    }
    let pid = hw.phy_id;
    hw.step("phy_id", format!("{:08x} {}{}", pid, hw::phy_name(pid),
                              if hw.pch() && pid & hw::PHY_REVISION_MASK != hw::I217_E_PHY_ID { " (UNEXPECTED for an I219)" } else { "" }));
    if let Some(i) = INFO.lock().as_mut() { i.phy_id = pid; }
    // Known state before reading the NVM (as Linux's probe).
    let r = hw.reset_hw();
    hw.step("reset_hw", r);
    let nvm = hw.nvm_summary();
    hw.step("nvm", nvm.clone());
    if let Some(i) = INFO.lock().as_mut() { i.nvm = nvm; }
    let k1 = hw.k1_state();
    hw.step("k1", k1.clone());
    if let Some(i) = INFO.lock().as_mut() { i.k1 = k1; }
    // e1000e_reset + open.
    let r = hw.full_reset(None, &mac);
    hw.step("reset", r);
    hw.pre_configure();
    {
        let mut g = RINGS.lock();
        if let Some(rg) = g.as_mut() { hw.configure(rg); }
    }
    hw.step("rings", format!("RX/TX enabled, {} RX buffers posted; {}", NUM_RX - 1, hw.mac_dump()));
    RUNNING.store(true, Ordering::SeqCst);
    elog!("up in {} ms: waiting for link", now_ms() - t0);
}

fn driver_thread(mut hw: Hw) {
    bring_up(&mut hw);
    let t_up = now_ms();
    let mut last_up = false;
    let mut reported_nolink = 0usize;
    let mut tx_stall_since: Option<(u64, u64)> = None;
    let mut rx_seen = RX_PKTS.load(Ordering::Relaxed);
    let mut rx_still_since = now_ms();
    let mut missed_base: Option<u64> = None;
    let mut rx_stall_soft = false;
    let mut stats_at = now_ms();
    let mut stats_last = (0u64, 0u64, 0u64);
    let mut phy_checked = 0u64;
    let mut phy_link_reported = false;
    let mut last_mode = (0u32, false);
    let mut lsc_seen = IRQ_LSC.load(Ordering::Relaxed);
    let mut ecc_seen = 0u32;
    loop {
        msleep(250);
        let now = now_ms();
        let st = hw.m.r32(STATUS);
        LAST_STATUS.store(st, Ordering::Relaxed);
        let (up, speed, full) = hw::link_from_status(st);
        let lsc = IRQ_LSC.load(Ordering::Relaxed);
        let ecc = ECC_ERRORS.load(Ordering::Relaxed);
        if ecc != ecc_seen {
            ecc_seen = ecc;
            hw.reset_datapath("uncorrectable packet-buffer ECC error (ICR.ECCER)");
            last_up = false;
            continue;
        }
        // Re-run the per-speed work on a renegotiation or a quick
        // down/up (a link-change interrupt) the 250 ms poll did not see.
        let renegotiated = up && last_up && ((speed, full) != last_mode || lsc != lsc_seen);
        lsc_seen = lsc;
        if renegotiated {
            let s = hw.on_link_up(speed, full);
            LINK.store(1 | if full { 2 } else { 0 } | (speed << 8), Ordering::SeqCst);
            last_mode = (speed, full);
            elog!("eth0: link renegotiated: {} Mbps {}-duplex: {}", speed, if full { "full" } else { "half" }, s);
        }
        if up != last_up {
            last_mode = (speed, full);
            LINK_CHANGES.fetch_add(1, Ordering::Relaxed);
            if up {
                let s = hw.on_link_up(speed, full);
                LINK.store(1 | if full { 2 } else { 0 } | (speed << 8), Ordering::SeqCst);
                LINK_GEN.fetch_add(1, Ordering::SeqCst);
                let lpa = hw.phy_read(MII_LPA);
                let s1000 = hw.phy_read(MII_STAT1000);
                elog!("eth0: link up {} Mbps {}-duplex ({} ms after bring-up; partner LPA {} 1000STAT {}): {}",
                      speed, if full { "full" } else { "half" }, now - t_up, hx(lpa), hx(s1000), s);
            } else {
                LINK.store(0, Ordering::SeqCst);
                hw.on_link_down();
                elog!("eth0: link down (STATUS {:08x})", st);
            }
            if hw.opts.debug { elog!("link event: {} | PHY {}", hw.mac_dump(), hw.phy_dump()); }
            last_up = up;
        }
        if !up && hw.pch() && now - phy_checked >= 2000 {
            // The MAC's STATUS.LU is what the driver acts on; say so when the
            // PHY already reports a link the MAC does not show.
            phy_checked = now;
            let b = hw.phy_read(MII_BMSR).and_then(|_| hw.phy_read(MII_BMSR));
            if let Ok(b) = b {
                if b & 0x4 != 0 && !phy_link_reported {
                    phy_link_reported = true;
                    elog!("PHY reports link (BMSR {:04x}) but MAC STATUS.LU is 0: {} | PHY {}", b, hw.mac_dump(), hw.phy_dump());
                }
            }
        }
        if !up {
            let waited = now - t_up;
            let marks = [10_000u64, 30_000, 120_000];
            if reported_nolink < marks.len() && waited >= marks[reported_nolink] {
                reported_nolink += 1;
                elog!("no link after {} s: {} | PHY {}", waited / 1000, hw.mac_dump(), hw.phy_dump());
            }
        }
        // RX watchdog: no frame consumed for 2 s while the MAC counts missed
        // frames.
        let rxp = RX_PKTS.load(Ordering::Relaxed);
        if rxp != rx_seen || !up {
            rx_seen = rxp;
            rx_still_since = now;
            missed_base = None;
            rx_stall_soft = false;
        } else if now - rx_still_since >= 2000 {
            let missed = stat("rx_missed") + stat("rx_no_buf");
            match missed_base {
                None => missed_base = Some(missed),
                Some(b) if missed > b => {
                    let ring = RINGS.try_lock().and_then(|g| g.as_ref().map(|r| (r.rx.filled(), !matches!(r.rx.next(), RxEvent::Empty))));
                    let n = RX_STALLS.fetch_add(1, Ordering::Relaxed) + 1;
                    match ring {
                        Some((filled, true)) => {
                            elog!("RX backlog #{}: {} frames waiting, none consumed for {} ms (net daemon last polled {} ms ago); waking it",
                                  n, filled, now - rx_still_since, now.saturating_sub(LAST_RX_POLL_MS.load(Ordering::Relaxed)));
                            crate::netdev::kick();
                        }
                        _ if !rx_stall_soft => {
                            elog!("RX stall #{}: no frame for {} ms, missed/no-buffer {} -> {}; re-arming RDT and the mask: {}",
                                  n, now - rx_still_since, b, missed, hw.mac_dump());
                            if let Some(g) = RINGS.try_lock() {
                                if let Some(r) = g.as_ref() { hw.m.w32(RDT, ((r.rx.cur + NUM_RX - 1) % NUM_RX) as u32); }
                            }
                            if IRQ_ACTIVE.load(Ordering::Relaxed) { hw.m.w32(IMS, IRQ_EN.load(Ordering::Relaxed)); }
                            crate::netdev::kick();
                            rx_stall_soft = true;
                        }
                        _ => {
                            hw.reset_datapath("RX stall persists");
                            rx_stall_soft = false;
                            last_up = false;
                        }
                    }
                    missed_base = Some(missed);
                    rx_still_since = now;
                }
                _ => {}
            }
        }
        // TX watchdog: frames posted but none completing for 5 s.
        let tx = RINGS.try_lock().and_then(|g| g.as_ref().map(|r| (r.tx.in_flight(), r.tx.dirty)));
        if let Some((inflight, dirty)) = tx {
            if inflight == 0 || !up {
                tx_stall_since = None;
            } else {
                match tx_stall_since {
                    Some((since, d)) if d == dirty => {
                        if now - since > 5000 {
                            hw.reset_datapath(&format!("TX timeout ({} frames stuck for 5 s)", inflight));
                            tx_stall_since = None;
                            last_up = false;
                        }
                    }
                    _ => tx_stall_since = Some((now, dirty)),
                }
            }
        }
        // Periodic line while traffic moves (every 30 s).
        if now - stats_at >= 30_000 {
            let s = (RX_PKTS.load(Ordering::Relaxed), TX_PKTS.load(Ordering::Relaxed), IRQS.load(Ordering::Relaxed));
            if s != stats_last {
                let hs = stats_poll(hw.m);
                elog!("stats 30 s: rx +{} tx +{} irqs +{}; totals hw rx_good {} tx_good {} rx_missed {} rx_no_buf {} crc_err {}; phy errors {} swflag failures {}",
                      s.0 - stats_last.0, s.1 - stats_last.1, s.2 - stats_last.2, hs[0], hs[1], hs[4], hs[5], hs[6],
                      PHY_ERRORS.load(Ordering::Relaxed), SEM_FAILURES.load(Ordering::Relaxed));
                stats_last = s;
            }
            stats_at = now;
        }
    }
}

// ── data path (net server) ──────────────────────────────────────────────────

pub fn present() -> bool { PRESENT.load(Ordering::Relaxed) }
pub fn mac_address() -> Option<[u8; 6]> { if present() { Some(*MAC.lock()) } else { None } }
/// The bound device's name (Linux's id suffix), if one was set up.
pub fn chip_name() -> Option<&'static str> { INFO.lock().as_ref().map(|i| i.dev.name) }

pub fn link() -> (bool, u32, bool) {
    let l = LINK.load(Ordering::Relaxed);
    (l & 1 != 0, l >> 8, l & 2 != 0)
}
pub fn link_generation() -> u32 { LINK_GEN.load(Ordering::Relaxed) }

pub fn send_packet(buf: &[u8]) -> bool {
    if !RUNNING.load(Ordering::Acquire) || buf.len() > MAX_FRAME || buf.is_empty() { return false; }
    let base = BASE.load(Ordering::Relaxed);
    let mut g = RINGS.lock();
    let r = match g.as_mut() { Some(r) => r, None => return false };
    r.tx.reclaim();
    let idx = match r.tx.slot() {
        Some(i) => i,
        None => { TX_FULL.fetch_add(1, Ordering::Relaxed); return false; }
    };
    let len = buf.len().max(ETH_ZLEN);
    unsafe {
        let dst = mm::phys_to_virt(r.tx_buf_phys[idx] as usize) as *mut u8;
        core::ptr::copy_nonoverlapping(buf.as_ptr(), dst, buf.len());
        if buf.len() < ETH_ZLEN { core::ptr::write_bytes(dst.add(buf.len()), 0, ETH_ZLEN - buf.len()); }
    }
    let tail = r.tx.post(idx, len);
    Mmio(base).w32(TDT, tail);
    TX_PKTS.fetch_add(1, Ordering::Relaxed);
    TX_BYTES.fetch_add(buf.len() as u64, Ordering::Relaxed);
    true
}

pub fn poll_receive(out: &mut [u8]) -> Option<usize> {
    if !RUNNING.load(Ordering::Acquire) { return None; }
    LAST_RX_POLL_MS.store(now_ms(), Ordering::Relaxed);
    let base = BASE.load(Ordering::Relaxed);
    let mut g = RINGS.lock();
    let r = g.as_mut()?;
    r.tx.reclaim();
    for _ in 0..NUM_RX {
        match r.rx.next() {
            RxEvent::Empty => return None,
            RxEvent::Frame { idx, len } => {
                let n = len.min(out.len());
                unsafe {
                    let src = mm::phys_to_virt(r.rx_buf_phys[idx] as usize) as *const u8;
                    core::ptr::copy_nonoverlapping(src, out.as_mut_ptr(), n);
                }
                let tail = r.rx.release(idx);
                Mmio(base).w32(RDT, tail);
                RX_PKTS.fetch_add(1, Ordering::Relaxed);
                RX_BYTES.fetch_add(n as u64, Ordering::Relaxed);
                return Some(n);
            }
            RxEvent::Error { idx, .. } => { RX_ERRS.fetch_add(1, Ordering::Relaxed); let t = r.rx.release(idx); Mmio(base).w32(RDT, t); }
            RxEvent::Fragment { idx, .. } => { RX_FRAGS.fetch_add(1, Ordering::Relaxed); let t = r.rx.release(idx); Mmio(base).w32(RDT, t); }
        }
    }
    None
}

/// End of a net-daemon pass: re-enable the interrupt mask, then look at the
/// ring once more (true = a frame is already waiting: poll again).
pub fn rx_rearm() -> bool {
    if !RUNNING.load(Ordering::Acquire) { return false; }
    let base = BASE.load(Ordering::Relaxed);
    if base == 0 { return false; }
    let g = RINGS.lock();
    let r = match g.as_ref() { Some(r) => r, None => return false };
    if !RUNNING.load(Ordering::Acquire) { return false; }
    core::sync::atomic::fence(Ordering::SeqCst);
    if IRQ_ACTIVE.load(Ordering::Relaxed) {
        let mut m = Mmio(base);
        hw::irq_rearm(&mut m, IRQ_EN.load(Ordering::Relaxed));
    }
    !matches!(r.rx.next(), RxEvent::Empty)
}

pub fn rx_irq_driven() -> bool { RX_IRQ.load(Ordering::Relaxed) && RUNNING.load(Ordering::Relaxed) }

// ── /proc/net/nic ───────────────────────────────────────────────────────────

pub fn info_lines(out: &mut String) {
    use core::fmt::Write;
    let info = match INFO.lock().clone() { Some(i) => i, None => return };
    let mac = *MAC.lock();
    let (up, speed, full) = link();
    let st = LAST_STATUS.load(Ordering::Relaxed);
    let fc = FC_BITS.load(Ordering::Relaxed);
    let _ = writeln!(out, "driver: e1000e");
    let _ = writeln!(out, "pci: {:02x}:{:02x}.{} 8086:{:04x} rev {:02x} subsystem {:04x}:{:04x}", info.bdf.0, info.bdf.1, info.bdf.2,
                     info.dev.did, info.rev, info.subsys.0, info.subsys.1);
    let _ = writeln!(out, "chip: {}", info.dev.name);
    let _ = writeln!(out, "family: {}", info.dev.fam.name());
    let _ = writeln!(out, "phy_id: {:08x} ({})", info.phy_id, hw::phy_name(info.phy_id));
    if info.dev.fam.is_pch() {
        let base = BASE.load(Ordering::Relaxed);
        let fwsm = if base != 0 { Mmio(base).r32(FWSM) } else { info.fwsm };
        let f = hw::fwsm_decode(fwsm);
        let _ = writeln!(out, "me: {} (FWSM {:08x}, mode {}, phy reset {}, at probe {:08x})",
                         if f.fw_valid { "active" } else { "none" }, fwsm, hw::fwsm_mode_name(f.mode),
                         if f.phy_reset_allowed { "allowed" } else { "blocked" }, info.fwsm);
    }
    let _ = writeln!(out, "nvm: {}", info.nvm);
    let _ = writeln!(out, "k1: {}", info.k1);
    // Each bring-up step, cut to 160 characters (the full text is in the
    // kernel log; netcheck reads this file into 8 KiB).
    for (k, v) in &info.steps {
        let cut = v.char_indices().nth(160).map_or(v.len(), |(i, _)| i);
        let _ = writeln!(out, "init_{}: {}{}", k, &v[..cut], if cut < v.len() { " ..." } else { "" });
    }
    let _ = writeln!(out, "mac: {} ({})", mac_str(&mac), info.mac_src);
    let _ = writeln!(out, "running: {}", if RUNNING.load(Ordering::Relaxed) { "yes" } else { "no" });
    let _ = writeln!(out, "link: {}", if up { "up" } else { "down" });
    let _ = writeln!(out, "speed: {}", speed);
    let _ = writeln!(out, "duplex: {}", if !up { "-" } else if full { "full" } else { "half" });
    let _ = writeln!(out, "flow_control: rx {} tx {}", on(fc & 1 != 0), on(fc & 2 != 0));
    let _ = writeln!(out, "phystatus: STATUS={:08x}", st);
    let _ = writeln!(out, "link_changes: {}", LINK_CHANGES.load(Ordering::Relaxed));
    let _ = writeln!(out, "irq: {}", info.irq);
    let _ = writeln!(out, "irq_count: {} (rx {} lsc {} rxo {} empty {} last_icr {:08x} mask {:08x})",
                     IRQS.load(Ordering::Relaxed), IRQ_RX.load(Ordering::Relaxed), IRQ_LSC.load(Ordering::Relaxed),
                     IRQ_RXO.load(Ordering::Relaxed), IRQ_EMPTY.load(Ordering::Relaxed), IRQ_LAST.load(Ordering::Relaxed),
                     IRQ_EN.load(Ordering::Relaxed));
    let _ = writeln!(out, "rx_packets: {}", RX_PKTS.load(Ordering::Relaxed));
    let _ = writeln!(out, "rx_bytes: {}", RX_BYTES.load(Ordering::Relaxed));
    let _ = writeln!(out, "rx_errors: {}", RX_ERRS.load(Ordering::Relaxed));
    let _ = writeln!(out, "rx_fragments_dropped: {}", RX_FRAGS.load(Ordering::Relaxed));
    let _ = writeln!(out, "tx_packets: {}", TX_PKTS.load(Ordering::Relaxed));
    let _ = writeln!(out, "tx_bytes: {}", TX_BYTES.load(Ordering::Relaxed));
    let _ = writeln!(out, "tx_ring_full: {}", TX_FULL.load(Ordering::Relaxed));
    let _ = writeln!(out, "resets: {} (rx stalls {})", RESETS.load(Ordering::Relaxed), RX_STALLS.load(Ordering::Relaxed));
    let _ = writeln!(out, "phy_errors: {} swflag_failures: {} ecc_errors: {}", PHY_ERRORS.load(Ordering::Relaxed),
                     SEM_FAILURES.load(Ordering::Relaxed), ECC_ERRORS.load(Ordering::Relaxed));
    if let Some(g) = RINGS.try_lock() {
        if let Some(r) = g.as_ref() {
            let _ = writeln!(out, "rings: rx_cur {} tx_cur {} tx_dirty {} tx_in_flight {}", r.rx.cur, r.tx.cur, r.tx.dirty, r.tx.in_flight());
        }
    }
    let base = BASE.load(Ordering::Relaxed);
    if base != 0 {
        let s = stats_poll(Mmio(base));
        let mut line = String::from("hw_stats:");
        for (i, &(_, name)) in STAT_REGS.iter().enumerate() { let _ = write!(line, " {} {}", name, s[i]); }
        let _ = writeln!(out, "{}", line);
    }
}
