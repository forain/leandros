//! Intel Wi-Fi PCIe transport ("gen1", 7000/8000 family; the 8265 is
//! family 8000): CSR / periphery (PRPH) / SRAM access, the APM power-up
//! sequence, firmware download over the FH service DMA channel, the legacy
//! single RX queue, the TX scheduler and TFD rings, and host commands.
//!
//! Behavioural reference: Linux drivers/net/wireless/intel/iwlwifi/pcie/
//! (trans.c, rx.c, tx.c) and queue/tx.c, consulted as documentation; this is
//! a small, polled re-implementation for one device and one op mode (MVM).
//!
//! Interrupts are never delivered to the CPU (PCI INTx is disabled and MSI is
//! not enabled): CSR_INT is programmed exactly as Linux does, so its status
//! bits latch the same way, and the driver polls them (firmware chunk done,
//! firmware error) and the RX status block in host memory (closed RB index).
//!
//! All DMA memory is below 4 GiB. Everything here runs with the caller
//! holding the driver's lock; nothing sleeps except the explicit waits.

#![allow(dead_code)]

use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

extern "C" {
    fn serial_print_str_raw(s: *const u8, len: usize);
}

/// One `[iwlwifi] ...` line on the console (and so in the persistent log).
pub fn log(msg: &str) {
    let mut s = String::with_capacity(msg.len() + 12);
    s.push_str("[iwlwifi] ");
    s.push_str(msg);
    s.push('\n');
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

macro_rules! ilog { ($($a:tt)*) => { $crate::iwl_trans::log(&alloc::format!($($a)*)) }; }
pub(crate) use ilog;

pub fn now_ms() -> u64 { sched::monotonic_ns() / 1_000_000 }
pub fn udelay(us: u64) { sched::kthread::udelay(us) }
pub fn msleep(ms: u64) { sched::kthread::msleep(ms) }

// ── CSR (BAR0) ──────────────────────────────────────────────────────────────

pub const CSR_HW_IF_CONFIG_REG: usize = 0x000;
pub const CSR_INT_COALESCING: usize = 0x004;
pub const CSR_INT: usize = 0x008;
pub const CSR_INT_MASK: usize = 0x00c;
pub const CSR_FH_INT_STATUS: usize = 0x010;
pub const CSR_GPIO_IN: usize = 0x018;
pub const CSR_RESET: usize = 0x020;
pub const CSR_GP_CNTRL: usize = 0x024;
pub const CSR_HW_REV: usize = 0x028;
pub const CSR_EEPROM_REG: usize = 0x02c;
pub const CSR_OTP_GP_REG: usize = 0x034;
pub const CSR_GIO_REG: usize = 0x03c;
pub const CSR_GP_UCODE_REG: usize = 0x048;
pub const CSR_GP_DRIVER_REG: usize = 0x050;
pub const CSR_UCODE_DRV_GP1: usize = 0x054;
pub const CSR_UCODE_DRV_GP1_SET: usize = 0x058;
pub const CSR_UCODE_DRV_GP1_CLR: usize = 0x05c;
pub const CSR_UCODE_DRV_GP2: usize = 0x060;
pub const CSR_MBOX_SET_REG: usize = 0x088;
pub const CSR_LED_REG: usize = 0x094;
pub const CSR_HW_RF_ID: usize = 0x09c;
pub const CSR_DRAM_INT_TBL_REG: usize = 0x0a0;
pub const CSR_MAC_SHADOW_REG_CTRL: usize = 0x0a8;
pub const CSR_GIO_CHICKEN_BITS: usize = 0x100;
pub const CSR_ANA_PLL_CFG: usize = 0x20c;
pub const CSR_MONITOR_STATUS_REG: usize = 0x228;
pub const CSR_DBG_HPET_MEM_REG: usize = 0x240;
pub const CSR_DBG_LINK_PWR_MGMT_REG: usize = 0x250;

pub const HW_IF_CONFIG_MSK_MAC_STEP_DASH: u32 = 0x0000_000f;
pub const HW_IF_CONFIG_BIT_RADIO_SI: u32 = 0x0000_0200;
pub const HW_IF_CONFIG_BIT_MAC_SI: u32 = 0x0000_0100;
pub const HW_IF_CONFIG_MSK_PHY_TYPE: u32 = 0x0000_0c00;
pub const HW_IF_CONFIG_MSK_PHY_DASH: u32 = 0x0000_3000;
pub const HW_IF_CONFIG_MSK_PHY_STEP: u32 = 0x0000_c000;
pub const HW_IF_CONFIG_D3_DEBUG: u32 = 0x0000_0200;
pub const HW_IF_CONFIG_BIT_HAP_WAKE_L1A: u32 = 0x0008_0000;
pub const HW_IF_CONFIG_BIT_NIC_READY: u32 = 0x0040_0000;
pub const HW_IF_CONFIG_BIT_NIC_PREPARE_DONE: u32 = 0x0200_0000;
pub const HW_IF_CONFIG_PREPARE: u32 = 0x0800_0000;
pub const HW_IF_CONFIG_ENABLE_PME: u32 = 0x1000_0000;
pub const MBOX_SET_OS_ALIVE: u32 = 1 << 5;

pub const INT_BIT_FH_RX: u32 = 1 << 31;
pub const INT_BIT_HW_ERR: u32 = 1 << 29;
pub const INT_BIT_RX_PERIODIC: u32 = 1 << 28;
pub const INT_BIT_FH_TX: u32 = 1 << 27;
pub const INT_BIT_SCD: u32 = 1 << 26;
pub const INT_BIT_SW_ERR: u32 = 1 << 25;
pub const INT_BIT_RF_KILL: u32 = 1 << 7;
pub const INT_BIT_CT_KILL: u32 = 1 << 6;
pub const INT_BIT_SW_RX: u32 = 1 << 3;
pub const INT_BIT_WAKEUP: u32 = 1 << 1;
pub const INT_BIT_ALIVE: u32 = 1 << 0;
pub const INI_SET_MASK: u32 = INT_BIT_FH_RX | INT_BIT_HW_ERR | INT_BIT_FH_TX | INT_BIT_SW_ERR
    | INT_BIT_RF_KILL | INT_BIT_SW_RX | INT_BIT_WAKEUP | INT_BIT_ALIVE | INT_BIT_RX_PERIODIC;
pub const FH_INT_TX_MASK: u32 = (1 << 1) | (1 << 0);

pub const RESET_FLAG_SW_RESET: u32 = 0x0000_0080;
pub const RESET_FLAG_MASTER_DISABLED: u32 = 0x0000_0100;
pub const RESET_FLAG_STOP_MASTER: u32 = 0x0000_0200;
pub const RESET_LINK_PWR_MGMT_DISABLED: u32 = 0x8000_0000;

pub const GP_CNTRL_MAC_CLOCK_READY: u32 = 0x0000_0001;
pub const GP_CNTRL_INIT_DONE: u32 = 0x0000_0004;
pub const GP_CNTRL_MAC_ACCESS_REQ: u32 = 0x0000_0008;
pub const GP_CNTRL_GOING_TO_SLEEP: u32 = 0x0000_0010;
pub const GP_CNTRL_XTAL_ON: u32 = 0x0000_0400;
pub const GP_CNTRL_MAC_ACCESS_EN: u32 = 0x0000_0001;
/// 1 = the radio is enabled by the hardware RF-kill line (0 = killed).
pub const GP_CNTRL_HW_RF_KILL_SW: u32 = 0x0800_0000;

pub const UCODE_DRV_GP1_BIT_MAC_SLEEP: u32 = 0x1;
pub const UCODE_SW_BIT_RFKILL: u32 = 0x2;
pub const UCODE_DRV_GP1_BIT_CMD_BLOCKED: u32 = 0x4;
pub const UCODE_DRV_GP1_CT_KILL_EXIT: u32 = 0x8;

pub const GIO_REG_VAL_L0S_DISABLED: u32 = 0x2;
pub const GIO_CHICKEN_L1A_NO_L0S_RX: u32 = 0x0080_0000;
pub const DBG_HPET_MEM_REG_VAL: u32 = 0xffff_0000;
pub const HOST_INT_TIMEOUT_DEF: u8 = 0x40;

// HBUS: indirect access to the periphery and to device SRAM.
pub const HBUS_TARG_MEM_RADDR: usize = 0x40c;
pub const HBUS_TARG_MEM_WADDR: usize = 0x410;
pub const HBUS_TARG_MEM_WDAT: usize = 0x418;
pub const HBUS_TARG_MEM_RDAT: usize = 0x41c;
pub const HBUS_TARG_PRPH_WADDR: usize = 0x444;
pub const HBUS_TARG_PRPH_RADDR: usize = 0x448;
pub const HBUS_TARG_PRPH_WDAT: usize = 0x44c;
pub const HBUS_TARG_PRPH_RDAT: usize = 0x450;
pub const HBUS_TARG_WRPTR: usize = 0x460;

// ── FH (flow handler, DMA) ──────────────────────────────────────────────────

const FH_MEM_LOWER_BOUND: usize = 0x1000;
pub const FH_KW_MEM_ADDR_REG: usize = FH_MEM_LOWER_BOUND + 0x97c;
const FH_MEM_CBBC_0_15: usize = FH_MEM_LOWER_BOUND + 0x9d0;
const FH_MEM_CBBC_16_19: usize = FH_MEM_LOWER_BOUND + 0xbf0;
const FH_MEM_CBBC_20_31: usize = FH_MEM_LOWER_BOUND + 0xb20;
const FH_RSCSR_CHNL0: usize = FH_MEM_LOWER_BOUND + 0xbc0;
pub const FH_RSCSR_CHNL0_STTS_WPTR_REG: usize = FH_RSCSR_CHNL0;
pub const FH_RSCSR_CHNL0_RBDCB_BASE_REG: usize = FH_RSCSR_CHNL0 + 0x4;
pub const FH_RSCSR_CHNL0_RBDCB_WPTR_REG: usize = FH_RSCSR_CHNL0 + 0x8;
pub const FH_RSCSR_CHNL0_WPTR: usize = FH_RSCSR_CHNL0_RBDCB_WPTR_REG;
pub const FH_RSCSR_CHNL0_RDPTR: usize = FH_RSCSR_CHNL0 + 0xc;
const FH_MEM_RCSR_CHNL0: usize = FH_MEM_LOWER_BOUND + 0xc00;
pub const FH_MEM_RCSR_CHNL0_CONFIG_REG: usize = FH_MEM_RCSR_CHNL0;
pub const FH_MEM_RCSR_CHNL0_RBDCB_WPTR: usize = FH_MEM_RCSR_CHNL0 + 0x8;
pub const FH_MEM_RCSR_CHNL0_FLUSH_RB_REQ: usize = FH_MEM_RCSR_CHNL0 + 0x10;
pub const FH_MEM_RSSR_RX_STATUS_REG: usize = FH_MEM_LOWER_BOUND + 0xc44;
pub const FH_RSSR_CHNL0_RX_STATUS_CHNL_IDLE: u32 = 0x0100_0000;
const FH_RCSR_RX_CONFIG_CHNL_EN_ENABLE_VAL: u32 = 0x8000_0000;
const FH_RCSR_CHNL0_RX_IGNORE_RXF_EMPTY: u32 = 0x0000_0004;
const FH_RCSR_CHNL0_RX_CONFIG_IRQ_DEST_INT_HOST_VAL: u32 = 0x0000_1000;
const FH_RCSR_RX_CONFIG_REG_VAL_RB_SIZE_4K: u32 = 0;
const FH_RCSR_RX_CONFIG_REG_IRQ_RBTH_POS: u32 = 4;
const FH_RCSR_RX_CONFIG_RBDCB_SIZE_POS: u32 = 20;
const RX_RB_TIMEOUT: u32 = 0x11;
const FH_TFDIB_LOWER_BOUND: usize = FH_MEM_LOWER_BOUND + 0x900;
fn fh_tfdib_ctrl0(ch: usize) -> usize { FH_TFDIB_LOWER_BOUND + 8 * ch }
fn fh_tfdib_ctrl1(ch: usize) -> usize { FH_TFDIB_LOWER_BOUND + 8 * ch + 4 }
const FH_TCSR_LOWER_BOUND: usize = FH_MEM_LOWER_BOUND + 0xd00;
fn fh_tcsr_tx_config(ch: usize) -> usize { FH_TCSR_LOWER_BOUND + 0x20 * ch }
fn fh_tcsr_tx_buf_sts(ch: usize) -> usize { FH_TCSR_LOWER_BOUND + 0x20 * ch + 0x8 }
pub const FH_TCSR_CHNL_NUM: usize = 8;
const FH_TCSR_TX_CONFIG_DMA_CHNL_PAUSE: u32 = 0;
const FH_TCSR_TX_CONFIG_DMA_CHNL_ENABLE: u32 = 0x8000_0000;
const FH_TCSR_TX_CONFIG_DMA_CREDIT_DISABLE: u32 = 0;
const FH_TCSR_TX_CONFIG_DMA_CREDIT_ENABLE: u32 = 0x8;
const FH_TCSR_TX_CONFIG_CIRQ_HOST_ENDTFD: u32 = 0x0010_0000;
const FH_TCSR_TX_BUF_STS_TFDB_VALID: u32 = 0x3;
const FH_TCSR_TX_BUF_STS_POS_TB_NUM: u32 = 20;
const FH_TCSR_TX_BUF_STS_POS_TB_IDX: u32 = 12;
pub const FH_TSSR_TX_STATUS_REG: usize = FH_MEM_LOWER_BOUND + 0xeb0;
pub const FH_TSSR_TX_ERROR_REG: usize = FH_MEM_LOWER_BOUND + 0xeb8;
const FH_SRVC_CHNL: usize = 9;
const FH_SRVC_CHNL_SRAM_ADDR_REG: usize = FH_MEM_LOWER_BOUND + 0x9c8;
pub const FH_TX_CHICKEN_BITS_REG: usize = FH_MEM_LOWER_BOUND + 0xe98;
const FH_TX_CHICKEN_BITS_SCD_AUTO_RETRY_EN: u32 = 0x2;
/// Firmware load handshake register (direct, needs NIC access).
pub const FH_UCODE_LOAD_STATUS: usize = 0x1af0;
/// Largest firmware chunk one service-channel DMA moves.
const FH_MEM_TB_MAX_LENGTH: usize = 0x20000;

fn fh_cbbc_queue(q: usize) -> usize {
    if q < 16 { FH_MEM_CBBC_0_15 + 4 * q } else if q < 20 { FH_MEM_CBBC_16_19 + 4 * (q - 16) } else { FH_MEM_CBBC_20_31 + 4 * (q - 20) }
}

// ── periphery registers ─────────────────────────────────────────────────────

pub const PRPH_MASK: u32 = 0x000f_ffff;
pub const RELEASE_CPU_RESET: u32 = 0x300c;
pub const RELEASE_CPU_RESET_BIT: u32 = 1 << 24;
pub const WFPM_GP2: u32 = 0xa030b4;
pub const WFMP_MAC_ADDR_0: u32 = 0xa03080;
pub const WFMP_MAC_ADDR_1: u32 = 0xa03084;
pub const SB_CPU_1_STATUS: u32 = 0xa01e30;
pub const SB_CPU_2_STATUS: u32 = 0xa01e34;
pub const LMPM_CHICK: u32 = 0xa01ff8;
const LMPM_CHICK_EXTENDED_ADDR_SPACE: u32 = 1 << 0;
const FW_MEM_EXTENDED_START: u32 = 0x40000;
const FW_MEM_EXTENDED_END: u32 = 0x57fff;

const SCD_BASE: u32 = 0xa02c00;
pub const SCD_SRAM_BASE_ADDR: u32 = SCD_BASE;
pub const SCD_DRAM_BASE_ADDR: u32 = SCD_BASE + 0x8;
pub const SCD_TXFACT: u32 = SCD_BASE + 0x10;
pub const SCD_QUEUECHAIN_SEL: u32 = SCD_BASE + 0xe8;
pub const SCD_AGGR_SEL: u32 = SCD_BASE + 0x248;
pub const SCD_GP_CTRL: u32 = SCD_BASE + 0x1a8;
pub const SCD_EN_CTRL: u32 = SCD_BASE + 0x254;
const SCD_GP_CTRL_ENABLE_31_QUEUES: u32 = 1 << 0;
const SCD_GP_CTRL_AUTO_ACTIVE_MODE: u32 = 1 << 18;
fn scd_queue_rdptr(q: u32) -> u32 { if q < 20 { SCD_BASE + 0x68 + q * 4 } else { SCD_BASE + 0x2b4 + (q - 20) * 4 } }
fn scd_queue_status_bits(q: u32) -> u32 { if q < 20 { SCD_BASE + 0x10c + q * 4 } else { SCD_BASE + 0x334 + (q - 20) * 4 } }
const SCD_QUEUE_STTS_REG_POS_TXF: u32 = 0;
const SCD_QUEUE_STTS_REG_POS_ACTIVE: u32 = 3;
const SCD_QUEUE_STTS_REG_POS_WSL: u32 = 4;
const SCD_QUEUE_STTS_REG_POS_SCD_ACT_EN: u32 = 19;
const SCD_QUEUE_STTS_REG_MSK: u32 = 0x017f_0000;
const SCD_CONTEXT_MEM_LOWER_BOUND: u32 = 0x600;
const SCD_TX_STTS_MEM_LOWER_BOUND: u32 = 0x6a0;
const SCD_TRANS_TBL_MEM_LOWER_BOUND: u32 = 0x7e0;
fn scd_context_queue_offset(q: u32) -> u32 { SCD_CONTEXT_MEM_LOWER_BOUND + q * 8 }
fn scd_trans_tbl_offset_queue(q: u32) -> u32 { (SCD_TRANS_TBL_MEM_LOWER_BOUND + q * 2) & 0xfffc }
const SCD_FRAME_LIMIT: u32 = 64;

// ── rings ───────────────────────────────────────────────────────────────────

pub const NUM_QUEUES: usize = 31;
pub const TFD_QUEUE_SIZE: usize = 256;
pub const TFD_SIZE: usize = 128;
pub const CMD_QUEUE: usize = 0;
pub const CMD_FIFO: u32 = 7;
/// Command queue window (slots with their own buffer).
pub const CMD_SLOTS: usize = 32;
pub const RX_QUEUE_SIZE: usize = 256;
const RX_QUEUE_SIZE_LOG: u32 = 8;
pub const RB_SIZE: usize = 4096;
const FIRST_TB_SIZE: usize = 20;
/// Largest TB a gen1 TFD can describe (12-bit length).
const TB_MAX: usize = 4000;
const BC_TABLE_ENTRIES: usize = TFD_QUEUE_SIZE + 64;

pub const RX_FRAME_SIZE_MSK: u32 = 0x3fff;
pub const RX_FRAME_INVALID: u32 = 0x5555_0000;
const RX_FRAME_ALIGN: usize = 0x40;
pub const SEQ_RX_FRAME: u16 = 0x8000;

// ── memory helpers ──────────────────────────────────────────────────────────

#[derive(Clone, Copy)]
pub struct Mmio(pub usize);

impl Mmio {
    #[inline] pub fn r32(&self, o: usize) -> u32 { unsafe { core::ptr::read_volatile((self.0 + o) as *const u32) } }
    #[inline] pub fn w32(&self, o: usize, v: u32) { unsafe { core::ptr::write_volatile((self.0 + o) as *mut u32, v) } }
    #[inline] pub fn w8(&self, o: usize, v: u8) { unsafe { core::ptr::write_volatile((self.0 + o) as *mut u8, v) } }
    pub fn set(&self, o: usize, bits: u32) { self.w32(o, self.r32(o) | bits) }
    pub fn clr(&self, o: usize, bits: u32) { self.w32(o, self.r32(o) & !bits) }
    /// Poll until `(reg & mask) == (bits & mask)`, for at most `timeout_us`.
    pub fn poll(&self, o: usize, bits: u32, mask: u32, timeout_us: u64) -> bool {
        let mut t = 0u64;
        loop {
            if self.r32(o) & mask == bits & mask { return true; }
            if t >= timeout_us { return false; }
            udelay(10);
            t += 10;
        }
    }
}

/// A zeroed, physically contiguous DMA buffer below 4 GiB (whole pages).
pub struct Dma {
    pub phys: usize,
    pub va: usize,
    pub order: usize,
}

impl Dma {
    pub fn alloc(bytes: usize) -> Option<Dma> {
        let pages = (bytes + 4095) / 4096;
        let mut order = 0;
        while (1usize << order) < pages { order += 1; }
        let phys = mm::buddy::alloc_below(order, 1usize << 32)?;
        let va = mm::phys_to_virt(phys);
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, 4096 << order); }
        Some(Dma { phys, va, order })
    }
    pub fn size(&self) -> usize { 4096 << self.order }
    pub fn bytes(&self) -> &[u8] { unsafe { core::slice::from_raw_parts(self.va as *const u8, self.size()) } }
    pub fn bytes_mut(&mut self) -> &mut [u8] { unsafe { core::slice::from_raw_parts_mut(self.va as *mut u8, self.size()) } }
    pub fn rd32(&self, off: usize) -> u32 { unsafe { core::ptr::read_volatile((self.va + off) as *const u32) } }
    pub fn wr32(&self, off: usize, v: u32) { unsafe { core::ptr::write_volatile((self.va + off) as *mut u32, v) } }
    pub fn rd16(&self, off: usize) -> u16 { unsafe { core::ptr::read_volatile((self.va + off) as *const u16) } }
    pub fn wr16(&self, off: usize, v: u16) { unsafe { core::ptr::write_volatile((self.va + off) as *mut u16, v) } }
    pub fn zero(&mut self) { let n = self.size(); unsafe { core::ptr::write_bytes(self.va as *mut u8, 0, n); } }
    pub fn free(self) { mm::buddy::free(self.phys, self.order); }
}

fn wmb() { core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst); }

// ── queues ──────────────────────────────────────────────────────────────────

pub struct TxQueue {
    pub id: usize,
    pub tfds: Dma,
    /// One buffer per slot (command queue: CMD_SLOTS of them, indexed by
    /// write pointer modulo the window; data queues: one per TFD).
    pub bufs: Vec<Dma>,
    pub write: usize,
    pub read: usize,
    pub active: bool,
}

impl TxQueue {
    fn new(id: usize, slots: usize) -> Option<TxQueue> {
        let tfds = Dma::alloc(TFD_QUEUE_SIZE * TFD_SIZE)?;
        let mut bufs = Vec::with_capacity(slots);
        for _ in 0..slots { bufs.push(Dma::alloc(4096)?); }
        Some(TxQueue { id, tfds, bufs, write: 0, read: 0, active: false })
    }
    fn slot(&self, idx: usize) -> usize { idx % self.bufs.len() }
    /// Number of TFDs the device still owns.
    pub fn used(&self) -> usize { (self.write + TFD_QUEUE_SIZE - self.read) % TFD_QUEUE_SIZE }
    fn reset(&mut self) {
        self.write = 0;
        self.read = 0;
        self.active = false;
        self.tfds.zero();
    }
    /// Fill the TFD at `self.write` with TBs over `buf[0..len]` (TB0 is the
    /// first FIRST_TB_SIZE bytes, then chunks of at most TB_MAX).
    fn build_tfd(&mut self, len: usize) {
        let idx = self.write;
        let base = self.bufs[self.slot(idx)].phys;
        let tfd = self.tfds.va + idx * TFD_SIZE;
        unsafe { core::ptr::write_bytes(tfd as *mut u8, 0, TFD_SIZE); }
        let mut tbs = 0usize;
        let mut off = 0usize;
        while off < len && tbs < 20 {
            let n = if off == 0 { core::cmp::min(len, FIRST_TB_SIZE) } else { core::cmp::min(len - off, TB_MAX) };
            let addr = (base + off) as u64;
            let tb = tfd + 4 + tbs * 6;
            unsafe {
                core::ptr::write_unaligned(tb as *mut u32, (addr as u32).to_le());
                let hi_n_len = ((n as u16) << 4) | ((addr >> 32) as u16 & 0xf);
                core::ptr::write_unaligned((tb + 4) as *mut u16, hi_n_len.to_le());
            }
            tbs += 1;
            off += n;
        }
        unsafe { core::ptr::write_volatile((tfd + 3) as *mut u8, tbs as u8); }
    }
}

pub struct RxQueue {
    bd: Dma,
    stts: Dma,
    bufs: Vec<Dma>,
    pub read: usize,
    write: usize,
    write_actual: usize,
}

/// One received firmware packet, copied out of the RB.
pub struct RxPacket {
    pub cmd: u8,
    pub group: u8,
    pub seq: u16,
    /// Payload after the 4-byte command header.
    pub data: Vec<u8>,
}

impl RxPacket {
    pub fn is_response(&self) -> bool { self.seq & SEQ_RX_FRAME == 0 }
    pub fn queue(&self) -> usize { ((self.seq >> 8) & 0x1f) as usize }
    pub fn index(&self) -> usize { (self.seq & 0xff) as usize }
}

/// Firmware image piece to download: device address and bytes.
pub struct FwChunk<'a> {
    pub addr: u32,
    pub data: &'a [u8],
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransError {
    NotReady,
    NoAccess,
    Timeout(&'static str),
    RfKill,
    NoMemory,
    FwError,
    Busy,
}

pub struct Trans {
    pub m: Mmio,
    pub hw_rev: u32,
    pub hw_rf_id: u32,
    rx: RxQueue,
    kw: Dma,
    bc: Dma,
    dummy_tfds: Dma,
    /// Firmware download bounce buffer (FH_MEM_TB_MAX_LENGTH, or one page).
    fwbuf: Dma,
    pub txq: Vec<Option<TxQueue>>,
    pub scd_base: u32,
    /// Latched CSR_INT bits seen by `poll_int` (for the log).
    pub int_seen: u32,
    pub fw_error: bool,
    /// CSR_HW_IF_CONFIG radio bits from the firmware PHY config.
    pub phy_config: u32,
    pub rx_packets: u64,
}

impl Trans {
    pub fn new(m: Mmio, data_queues: &[usize]) -> Option<Trans> {
        let rx = RxQueue {
            bd: Dma::alloc(RX_QUEUE_SIZE * 4)?,
            stts: Dma::alloc(16)?,
            bufs: { let mut v = Vec::with_capacity(RX_QUEUE_SIZE); for _ in 0..RX_QUEUE_SIZE { v.push(Dma::alloc(RB_SIZE)?); } v },
            read: 0, write: 0, write_actual: 0,
        };
        let fwbuf = Dma::alloc(FH_MEM_TB_MAX_LENGTH).or_else(|| Dma::alloc(4096))?;
        let mut txq: Vec<Option<TxQueue>> = (0..NUM_QUEUES).map(|_| None).collect();
        txq[CMD_QUEUE] = Some(TxQueue::new(CMD_QUEUE, CMD_SLOTS)?);
        for &q in data_queues { txq[q] = Some(TxQueue::new(q, TFD_QUEUE_SIZE)?); }
        Some(Trans {
            m, hw_rev: 0, hw_rf_id: 0, rx,
            kw: Dma::alloc(4096)?,
            bc: Dma::alloc(NUM_QUEUES * BC_TABLE_ENTRIES * 2)?,
            dummy_tfds: Dma::alloc(TFD_QUEUE_SIZE * TFD_SIZE)?,
            fwbuf, txq, scd_base: 0, int_seen: 0, fw_error: false, phy_config: 0, rx_packets: 0,
        })
    }

    // ── NIC access and indirect registers ──

    /// Wake the MAC and keep it awake (MAC_ACCESS_REQ) until released.
    pub fn grab(&self) -> bool {
        self.m.set(CSR_GP_CNTRL, GP_CNTRL_MAC_ACCESS_REQ);
        udelay(2);
        let ok = self.m.poll(CSR_GP_CNTRL, GP_CNTRL_MAC_ACCESS_EN,
                             GP_CNTRL_MAC_CLOCK_READY | GP_CNTRL_GOING_TO_SLEEP, 15000);
        if !ok {
            ilog!("timeout waiting for MAC access (CSR_GP_CNTRL {:08x})", self.m.r32(CSR_GP_CNTRL));
            self.m.clr(CSR_GP_CNTRL, GP_CNTRL_MAC_ACCESS_REQ);
        }
        ok
    }
    pub fn release(&self) { self.m.clr(CSR_GP_CNTRL, GP_CNTRL_MAC_ACCESS_REQ); }

    pub fn read_prph_nograb(&self, reg: u32) -> u32 {
        self.m.w32(HBUS_TARG_PRPH_RADDR, (reg & PRPH_MASK) | (3 << 24));
        self.m.r32(HBUS_TARG_PRPH_RDAT)
    }
    pub fn write_prph_nograb(&self, reg: u32, v: u32) {
        self.m.w32(HBUS_TARG_PRPH_WADDR, (reg & PRPH_MASK) | (3 << 24));
        self.m.w32(HBUS_TARG_PRPH_WDAT, v);
    }
    pub fn read_prph(&self, reg: u32) -> u32 {
        if !self.grab() { return 0x5a5a_5a5a; }
        let v = self.read_prph_nograb(reg);
        self.release();
        v
    }
    pub fn write_prph(&self, reg: u32, v: u32) {
        if self.grab() { self.write_prph_nograb(reg, v); self.release(); }
    }
    pub fn set_bits_prph(&self, reg: u32, bits: u32) {
        if self.grab() { let v = self.read_prph_nograb(reg); self.write_prph_nograb(reg, v | bits); self.release(); }
    }
    pub fn clear_bits_prph(&self, reg: u32, bits: u32) {
        if self.grab() { let v = self.read_prph_nograb(reg); self.write_prph_nograb(reg, v & !bits); self.release(); }
    }
    /// CSR/FH register access that needs the MAC awake.
    pub fn read_direct(&self, o: usize) -> u32 {
        if !self.grab() { return 0x5a5a_5a5a; }
        let v = self.m.r32(o);
        self.release();
        v
    }
    pub fn write_direct(&self, o: usize, v: u32) {
        if self.grab() { self.m.w32(o, v); self.release(); }
    }
    /// Read `n` dwords of device SRAM.
    pub fn read_mem(&self, addr: u32, out: &mut [u32]) -> bool {
        if !self.grab() { return false; }
        self.m.w32(HBUS_TARG_MEM_RADDR, addr);
        for v in out.iter_mut() { *v = self.m.r32(HBUS_TARG_MEM_RDAT); }
        self.release();
        true
    }
    /// Write dwords (or zeros when `vals` is None) to device SRAM.
    pub fn write_mem(&self, addr: u32, vals: Option<&[u32]>, n: usize) -> bool {
        if !self.grab() { return false; }
        self.m.w32(HBUS_TARG_MEM_WADDR, addr);
        for i in 0..n { self.m.w32(HBUS_TARG_MEM_WDAT, vals.map_or(0, |v| v[i])); }
        self.release();
        true
    }
    pub fn read_mem32(&self, addr: u32) -> u32 { let mut v = [0u32]; self.read_mem(addr, &mut v); v[0] }
    pub fn write_mem32(&self, addr: u32, v: u32) { self.write_mem(addr, Some(&[v]), 1); }

    pub fn rfkill_on(&self) -> bool { self.m.r32(CSR_GP_CNTRL) & GP_CNTRL_HW_RF_KILL_SW == 0 }

    // ── interrupts (masked to the CPU; status polled) ──

    pub fn disable_interrupts(&self) {
        self.m.w32(CSR_INT_MASK, 0);
        self.m.w32(CSR_INT, 0xffff_ffff);
        self.m.w32(CSR_FH_INT_STATUS, 0xffff_ffff);
    }
    pub fn enable_interrupts(&self) { self.m.w32(CSR_INT_MASK, INI_SET_MASK); }

    /// Read and acknowledge CSR_INT. Firmware error bits set `fw_error`.
    pub fn poll_int(&mut self) -> u32 {
        let v = self.m.r32(CSR_INT);
        if v == 0 || v == 0xffff_ffff { return if v == 0xffff_ffff { 0 } else { 0 }; }
        self.m.w32(CSR_INT, v);
        if v & INT_BIT_FH_TX != 0 { self.m.w32(CSR_FH_INT_STATUS, FH_INT_TX_MASK); }
        if v & (INT_BIT_SW_ERR | INT_BIT_HW_ERR) != 0 && !self.fw_error {
            self.fw_error = true;
            ilog!("firmware error interrupt (CSR_INT {:08x}{}{})", v,
                  if v & INT_BIT_SW_ERR != 0 { ", SW_ERR" } else { "" },
                  if v & INT_BIT_HW_ERR != 0 { ", HW_ERR" } else { "" });
        }
        self.int_seen |= v;
        v
    }

    // ── power-up ──

    fn set_hw_ready(&self) -> bool {
        self.m.set(CSR_HW_IF_CONFIG_REG, HW_IF_CONFIG_BIT_NIC_READY);
        let ok = self.m.poll(CSR_HW_IF_CONFIG_REG, HW_IF_CONFIG_BIT_NIC_READY, HW_IF_CONFIG_BIT_NIC_READY, 50);
        if ok { self.m.set(CSR_MBOX_SET_REG, MBOX_SET_OS_ALIVE); }
        ok
    }

    /// Take ownership of the NIC from the ME/AMT side (NIC_READY handshake).
    pub fn prepare_card_hw(&self) -> Result<(), TransError> {
        if self.set_hw_ready() { return Ok(()); }
        self.m.set(CSR_DBG_LINK_PWR_MGMT_REG, RESET_LINK_PWR_MGMT_DISABLED);
        udelay(1500);
        for _ in 0..10 {
            self.m.set(CSR_HW_IF_CONFIG_REG, HW_IF_CONFIG_PREPARE);
            let mut t = 0;
            while t < 150_000 {
                if self.set_hw_ready() { return Ok(()); }
                udelay(200);
                t += 200;
            }
            msleep(25);
        }
        ilog!("could not take ownership of the NIC (HW_IF_CONFIG {:08x})", self.m.r32(CSR_HW_IF_CONFIG_REG));
        Err(TransError::NotReady)
    }

    pub fn sw_reset(&self, retake: bool) -> Result<(), TransError> {
        self.m.set(CSR_RESET, RESET_FLAG_SW_RESET);
        udelay(6000);
        if retake { self.prepare_card_hw() } else { Ok(()) }
    }

    /// INIT_DONE and wait for the MAC clock (D0U -> D0A).
    pub fn finish_nic_init(&self) -> Result<(), TransError> {
        self.m.set(CSR_GP_CNTRL, GP_CNTRL_INIT_DONE);
        udelay(2);
        if !self.m.poll(CSR_GP_CNTRL, GP_CNTRL_MAC_CLOCK_READY, GP_CNTRL_MAC_CLOCK_READY, 25_000) {
            ilog!("MAC clock not ready after 25 ms (CSR_GP_CNTRL {:08x})", self.m.r32(CSR_GP_CNTRL));
            return Err(TransError::Timeout("MAC clock ready"));
        }
        Ok(())
    }

    /// The basic power-up (iwl_pcie_apm_init) for family 8000.
    pub fn apm_init(&self, func: &crate::pci::PciFunction) -> Result<(), TransError> {
        self.m.set(CSR_GIO_CHICKEN_BITS, GIO_CHICKEN_L1A_NO_L0S_RX);
        self.m.set(CSR_DBG_HPET_MEM_REG, DBG_HPET_MEM_REG_VAL);
        self.m.set(CSR_HW_IF_CONFIG_REG, HW_IF_CONFIG_BIT_HAP_WAKE_L1A);
        // apm_config: L0s always disabled on our side.
        self.m.set(CSR_GIO_REG, GIO_REG_VAL_L0S_DISABLED);
        let _ = func;
        self.finish_nic_init()
    }

    pub fn apm_stop_master(&self) {
        self.m.set(CSR_RESET, RESET_FLAG_STOP_MASTER);
        if !self.m.poll(CSR_RESET, RESET_FLAG_MASTER_DISABLED, RESET_FLAG_MASTER_DISABLED, 100) {
            log("bus master disable timed out (100 us)");
        }
    }

    pub fn apm_stop(&self) {
        self.apm_stop_master();
        let _ = self.sw_reset(false);
        self.m.clr(CSR_GP_CNTRL, GP_CNTRL_INIT_DONE);
    }

    /// Bring the NIC to D0A with nothing running (iwl_trans_pcie_start_hw).
    pub fn start_hw(&self, func: &crate::pci::PciFunction) -> Result<(), TransError> {
        self.prepare_card_hw()?;
        self.sw_reset(true)?;
        self.apm_init(func)?;
        Ok(())
    }

    /// Stop DMA, reset, back to low power (iwl_trans_pcie_stop_device).
    pub fn stop_device(&mut self) {
        self.disable_interrupts();
        // TX: deactivate FIFOs, stop the DMA channels.
        if self.grab() {
            self.write_prph_nograb(SCD_TXFACT, 0);
            let mut mask = 0;
            for ch in 0..FH_TCSR_CHNL_NUM {
                self.m.w32(fh_tcsr_tx_config(ch), 0);
                mask |= (1u32 << ch) << 16;
            }
            if !self.m.poll(FH_TSSR_TX_STATUS_REG, mask, mask, 5000) {
                ilog!("TX DMA channels did not go idle ({:08x})", self.m.r32(FH_TSSR_TX_STATUS_REG));
            }
            // RX DMA off.
            self.m.w32(FH_MEM_RCSR_CHNL0_CONFIG_REG, 0);
            if !self.m.poll(FH_MEM_RSSR_RX_STATUS_REG, FH_RSSR_CHNL0_RX_STATUS_CHNL_IDLE, FH_RSSR_CHNL0_RX_STATUS_CHNL_IDLE, 1000) {
                log("RX DMA did not go idle");
            }
            self.release();
        }
        for q in self.txq.iter_mut().flatten() { q.reset(); }
        self.m.clr(CSR_GP_CNTRL, GP_CNTRL_MAC_ACCESS_REQ);
        self.apm_stop();
        let _ = self.sw_reset(true);
        self.disable_interrupts();
        self.fw_error = false;
    }

    // ── nic_init: RX + TX rings ──

    fn rx_init(&mut self) {
        let rx = &mut self.rx;
        rx.read = 0;
        rx.write = 0;
        rx.write_actual = 0;
        rx.stts.zero();
        // Restock: every slot owns the buffer with its index.
        while (rx.read + RX_QUEUE_SIZE - rx.write - 1) % RX_QUEUE_SIZE > 0 {
            let i = rx.write;
            rx.bd.wr32(i * 4, (rx.bufs[i].phys >> 8) as u32);
            rx.write = (rx.write + 1) % RX_QUEUE_SIZE;
        }
        wmb();
        let bd_phys = rx.bd.phys;
        let stts_phys = rx.stts.phys;
        if !self.grab() { return; }
        let m = self.m;
        m.w32(FH_MEM_RCSR_CHNL0_CONFIG_REG, 0);
        m.w32(FH_MEM_RCSR_CHNL0_RBDCB_WPTR, 0);
        m.w32(FH_MEM_RCSR_CHNL0_FLUSH_RB_REQ, 0);
        m.w32(FH_RSCSR_CHNL0_RDPTR, 0);
        m.w32(FH_RSCSR_CHNL0_RBDCB_WPTR_REG, 0);
        m.w32(FH_RSCSR_CHNL0_RBDCB_BASE_REG, (bd_phys >> 8) as u32);
        m.w32(FH_RSCSR_CHNL0_STTS_WPTR_REG, (stts_phys >> 4) as u32);
        m.w32(FH_MEM_RCSR_CHNL0_CONFIG_REG,
              FH_RCSR_RX_CONFIG_CHNL_EN_ENABLE_VAL | FH_RCSR_CHNL0_RX_IGNORE_RXF_EMPTY
              | FH_RCSR_CHNL0_RX_CONFIG_IRQ_DEST_INT_HOST_VAL | FH_RCSR_RX_CONFIG_REG_VAL_RB_SIZE_4K
              | (RX_RB_TIMEOUT << FH_RCSR_RX_CONFIG_REG_IRQ_RBTH_POS)
              | (RX_QUEUE_SIZE_LOG << FH_RCSR_RX_CONFIG_RBDCB_SIZE_POS));
        self.release();
        self.m.w8(CSR_INT_COALESCING, HOST_INT_TIMEOUT_DEF);
        self.rx_update_wptr();
    }

    fn rx_update_wptr(&mut self) {
        let wa = self.rx.write & !7;
        if wa != self.rx.write_actual {
            self.rx.write_actual = wa;
            self.m.w32(FH_RSCSR_CHNL0_WPTR, wa as u32);
        }
    }

    fn tx_init(&mut self) {
        self.bc.zero();
        if !self.grab() { return; }
        self.write_prph_nograb(SCD_TXFACT, 0);
        self.m.w32(FH_KW_MEM_ADDR_REG, (self.kw.phys >> 4) as u32);
        for q in 0..NUM_QUEUES {
            let phys = match &self.txq[q] { Some(t) => t.tfds.phys, None => self.dummy_tfds.phys };
            self.m.w32(fh_cbbc_queue(q), (phys >> 8) as u32);
        }
        let v = self.read_prph_nograb(SCD_GP_CTRL);
        self.write_prph_nograb(SCD_GP_CTRL, v | SCD_GP_CTRL_AUTO_ACTIVE_MODE | SCD_GP_CTRL_ENABLE_31_QUEUES);
        self.release();
        for q in self.txq.iter_mut().flatten() { q.reset(); }
    }

    /// apm_init + op-mode NIC config + rings + shadow registers.
    fn nic_init(&mut self, func: &crate::pci::PciFunction) -> Result<(), TransError> {
        self.apm_init(func)?;
        // iwl_mvm_nic_config: MAC step/dash and the radio type/step/dash.
        let pc = self.phy_config;
        let mut v = self.hw_rev & HW_IF_CONFIG_MSK_MAC_STEP_DASH;
        v |= (pc & 0x3) << 10;
        v |= ((pc >> 2) & 0x3) << 14;
        v |= ((pc >> 4) & 0x3) << 12;
        let mask = HW_IF_CONFIG_MSK_MAC_STEP_DASH | HW_IF_CONFIG_MSK_PHY_TYPE | HW_IF_CONFIG_MSK_PHY_STEP
            | HW_IF_CONFIG_MSK_PHY_DASH | HW_IF_CONFIG_BIT_RADIO_SI | HW_IF_CONFIG_BIT_MAC_SI | HW_IF_CONFIG_D3_DEBUG;
        let cur = self.m.r32(CSR_HW_IF_CONFIG_REG);
        self.m.w32(CSR_HW_IF_CONFIG_REG, (cur & !mask) | v);
        self.rx_init();
        self.tx_init();
        self.m.set(CSR_MAC_SHADOW_REG_CTRL, 0x800f_ffff);
        Ok(())
    }

    // ── firmware download ──

    fn load_chunk(&mut self, addr: u32, data: &[u8]) -> Result<(), TransError> {
        let buf = self.fwbuf.va;
        unsafe { core::ptr::copy_nonoverlapping(data.as_ptr(), buf as *mut u8, data.len()); }
        wmb();
        let phys = self.fwbuf.phys as u64;
        // Clear a stale completion first.
        self.m.w32(CSR_INT, INT_BIT_FH_TX);
        self.m.w32(CSR_FH_INT_STATUS, FH_INT_TX_MASK);
        if !self.grab() { return Err(TransError::NoAccess); }
        let m = self.m;
        m.w32(fh_tcsr_tx_config(FH_SRVC_CHNL), FH_TCSR_TX_CONFIG_DMA_CHNL_PAUSE);
        m.w32(FH_SRVC_CHNL_SRAM_ADDR_REG, addr);
        m.w32(fh_tfdib_ctrl0(FH_SRVC_CHNL), phys as u32);
        m.w32(fh_tfdib_ctrl1(FH_SRVC_CHNL), (((phys >> 32) as u32 & 0xf) << 28) | data.len() as u32);
        m.w32(fh_tcsr_tx_buf_sts(FH_SRVC_CHNL),
              (1 << FH_TCSR_TX_BUF_STS_POS_TB_NUM) | (1 << FH_TCSR_TX_BUF_STS_POS_TB_IDX) | FH_TCSR_TX_BUF_STS_TFDB_VALID);
        m.w32(fh_tcsr_tx_config(FH_SRVC_CHNL),
              FH_TCSR_TX_CONFIG_DMA_CHNL_ENABLE | FH_TCSR_TX_CONFIG_DMA_CREDIT_DISABLE | FH_TCSR_TX_CONFIG_CIRQ_HOST_ENDTFD);
        self.release();
        let t0 = now_ms();
        loop {
            let v = self.m.r32(CSR_INT);
            if v & INT_BIT_FH_TX != 0 {
                self.m.w32(CSR_INT, INT_BIT_FH_TX);
                self.m.w32(CSR_FH_INT_STATUS, FH_INT_TX_MASK);
                return Ok(());
            }
            if now_ms() - t0 > 5000 {
                ilog!("firmware chunk to {:#x} ({} bytes) not acknowledged in 5 s: CSR_INT {:08x} FH_INT {:08x} GP_CNTRL {:08x}",
                      addr, data.len(), v, self.m.r32(CSR_FH_INT_STATUS), self.m.r32(CSR_GP_CNTRL));
                return Err(TransError::Timeout("firmware chunk DMA"));
            }
            udelay(20);
        }
    }

    fn load_section(&mut self, s: &FwChunk) -> Result<(), TransError> {
        let max = self.fwbuf.size().min(FH_MEM_TB_MAX_LENGTH);
        let mut off = 0;
        while off < s.data.len() {
            let n = (s.data.len() - off).min(max);
            let dst = s.addr.wrapping_add(off as u32);
            let ext = (FW_MEM_EXTENDED_START..=FW_MEM_EXTENDED_END).contains(&dst);
            if ext { self.set_bits_prph(LMPM_CHICK, LMPM_CHICK_EXTENDED_ADDR_SPACE); }
            let r = self.load_chunk(dst, &s.data[off..off + n]);
            if ext { self.clear_bits_prph(LMPM_CHICK, LMPM_CHICK_EXTENDED_ADDR_SPACE); }
            r?;
            off += n;
        }
        Ok(())
    }

    fn load_cpu_sections(&mut self, secs: &[FwChunk], cpu: u32) -> Result<(), TransError> {
        let shift = if cpu == 1 { 0 } else { 16 };
        let mut sec_num: u32 = 1;
        for s in secs {
            self.load_section(s)?;
            let v = self.read_direct(FH_UCODE_LOAD_STATUS);
            self.write_direct(FH_UCODE_LOAD_STATUS, v | (sec_num << shift));
            sec_num = (sec_num << 1) | 1;
        }
        self.enable_interrupts();
        self.write_direct(FH_UCODE_LOAD_STATUS, if cpu == 1 { 0xffff } else { 0xffff_ffff });
        Ok(())
    }

    /// Start a firmware image (iwl_trans_pcie_start_fw + load_given_ucode_8000).
    /// The caller then waits for the ALIVE notification on the RX queue.
    pub fn start_fw(&mut self, func: &crate::pci::PciFunction, cpu1: &[FwChunk], cpu2: &[FwChunk],
                    run_in_rfkill: bool) -> Result<(), TransError> {
        self.prepare_card_hw()?;
        self.m.w32(CSR_INT, 0xffff_ffff);
        self.disable_interrupts();
        if self.rfkill_on() && !run_in_rfkill { return Err(TransError::RfKill); }
        self.m.w32(CSR_UCODE_DRV_GP1_CLR, UCODE_SW_BIT_RFKILL);
        self.m.w32(CSR_UCODE_DRV_GP1_CLR, UCODE_DRV_GP1_BIT_CMD_BLOCKED);
        self.m.w32(CSR_INT, 0xffff_ffff);
        self.fw_error = false;
        self.nic_init(func)?;
        self.m.w32(CSR_INT_MASK, INT_BIT_FH_TX);
        self.m.w32(CSR_UCODE_DRV_GP1_CLR, UCODE_SW_BIT_RFKILL);
        self.m.w32(CSR_UCODE_DRV_GP1_CLR, UCODE_SW_BIT_RFKILL);
        let t0 = now_ms();
        let gp2 = self.read_prph(WFPM_GP2);
        self.write_prph(WFPM_GP2, 0x0101_0101);
        self.write_prph(RELEASE_CPU_RESET, RELEASE_CPU_RESET_BIT);
        self.load_cpu_sections(cpu1, 1)?;
        self.load_cpu_sections(cpu2, 2)?;
        let bytes: usize = cpu1.iter().chain(cpu2.iter()).map(|s| s.data.len()).sum();
        ilog!("firmware image downloaded: {} + {} sections, {} KiB in {} ms (WFPM_GP2 was {:08x})",
              cpu1.len(), cpu2.len(), bytes / 1024, now_ms() - t0, gp2);
        if self.rfkill_on() && !run_in_rfkill { return Err(TransError::RfKill); }
        Ok(())
    }

    /// After ALIVE: the TX scheduler and the command queue (iwl_pcie_tx_start).
    pub fn fw_alive(&mut self, alive_scd_base: u32) {
        self.scd_base = self.read_prph(SCD_SRAM_BASE_ADDR);
        if alive_scd_base != 0 && alive_scd_base != self.scd_base {
            ilog!("SCD base from ALIVE {:#x} != register {:#x}", alive_scd_base, self.scd_base);
        }
        let clear = (scd_trans_tbl_offset_queue(NUM_QUEUES as u32) - SCD_CONTEXT_MEM_LOWER_BOUND) / 4;
        self.write_mem(self.scd_base + SCD_CONTEXT_MEM_LOWER_BOUND, None, clear as usize);
        self.write_prph(SCD_DRAM_BASE_ADDR, (self.bc.phys >> 10) as u32);
        self.txq_enable_scd(CMD_QUEUE, CMD_FIFO, 0);
        self.write_prph(SCD_TXFACT, 0xff);
        for ch in 0..FH_TCSR_CHNL_NUM {
            self.write_direct(fh_tcsr_tx_config(ch), FH_TCSR_TX_CONFIG_DMA_CHNL_ENABLE | FH_TCSR_TX_CONFIG_DMA_CREDIT_ENABLE);
        }
        let v = self.read_direct(FH_TX_CHICKEN_BITS_REG);
        self.write_direct(FH_TX_CHICKEN_BITS_REG, v | FH_TX_CHICKEN_BITS_SCD_AUTO_RETRY_EN);
        self.enable_interrupts();
    }

    /// Program the scheduler for a queue ourselves (only the command queue;
    /// data queues are configured by the firmware via SCD_QUEUE_CFG, with
    /// `txq_enable_fw`).
    fn txq_enable_scd(&mut self, q: usize, fifo: u32, ssn: usize) {
        let qid = q as u32;
        if q == CMD_QUEUE { self.write_prph(SCD_EN_CTRL, 0); }
        self.write_prph(scd_queue_status_bits(qid), 1 << SCD_QUEUE_STTS_REG_POS_SCD_ACT_EN);
        if q != CMD_QUEUE { self.set_bits_prph(SCD_QUEUECHAIN_SEL, 1 << qid); }
        self.clear_bits_prph(SCD_AGGR_SEL, 1 << qid);
        if let Some(t) = self.txq[q].as_mut() { t.read = ssn & 0xff; t.write = ssn & 0xff; t.active = true; }
        self.write_direct(HBUS_TARG_WRPTR, (ssn as u32 & 0xff) | (qid << 8));
        self.write_prph(scd_queue_rdptr(qid), ssn as u32);
        self.write_mem32(self.scd_base + scd_context_queue_offset(qid), 0);
        self.write_mem32(self.scd_base + scd_context_queue_offset(qid) + 4,
                         (SCD_FRAME_LIMIT & 0x7f) | ((SCD_FRAME_LIMIT << 16) & 0x007f_0000));
        self.write_prph(scd_queue_status_bits(qid),
                        (1 << SCD_QUEUE_STTS_REG_POS_ACTIVE) | (fifo << SCD_QUEUE_STTS_REG_POS_TXF)
                        | (1 << SCD_QUEUE_STTS_REG_POS_WSL) | SCD_QUEUE_STTS_REG_MSK);
        if q == CMD_QUEUE { self.write_prph(SCD_EN_CTRL, 1 << qid); }
    }

    /// Host side of a firmware-configured (DQA) queue: pointers only
    /// (iwl_trans_txq_enable_cfg with no scheduler config). Returns false
    /// when the queue has no ring.
    pub fn txq_enable_fw(&mut self, q: usize, ssn: usize) -> bool {
        let t = match self.txq[q].as_mut() { Some(t) => t, None => return false };
        let mut ssn = ssn & 0xff;
        // 8000-family scheduler bug: moving the write pointer by a multiple
        // of 0x40 gets it stuck.
        if (ssn.wrapping_sub(t.write)) & 0x3f == 0 && ssn != t.write { ssn = (ssn + 1) & 0xff; }
        t.read = ssn;
        t.write = ssn;
        t.active = true;
        self.write_direct(HBUS_TARG_WRPTR, ssn as u32 | ((q as u32) << 8));
        true
    }

    // ── commands ──

    /// Queue a host command. `wide` = (group, version) for the wide header.
    /// Returns the sequence number the response will carry.
    pub fn send_cmd(&mut self, cmd: u8, wide: Option<(u8, u8)>, payload: &[u8]) -> Result<u16, TransError> {
        let q = self.txq[CMD_QUEUE].as_mut().ok_or(TransError::NotReady)?;
        if !q.active { return Err(TransError::NotReady); }
        if q.used() >= CMD_SLOTS - 1 { return Err(TransError::Busy); }
        let idx = q.write;
        let seq: u16 = ((CMD_QUEUE as u16 & 0x1f) << 8) | (idx as u16 & 0xff);
        let hdr_len = if wide.is_some() { 8 } else { 4 };
        let total = hdr_len + payload.len();
        if total > 4096 { return Err(TransError::Busy); }
        let slot = q.slot(idx);
        {
            let b = q.bufs[slot].bytes_mut();
            b[0] = cmd;
            match wide {
                Some((group, ver)) => {
                    b[1] = group;
                    b[2..4].copy_from_slice(&seq.to_le_bytes());
                    b[4..6].copy_from_slice(&(payload.len() as u16).to_le_bytes());
                    b[6] = 0;
                    b[7] = ver;
                }
                None => {
                    b[1] = 0;
                    b[2..4].copy_from_slice(&seq.to_le_bytes());
                }
            }
            b[hdr_len..total].copy_from_slice(payload);
            // Linux pads short commands so TB0 always has FIRST_TB_SIZE.
            for x in &mut b[total..total.max(FIRST_TB_SIZE)] { *x = 0; }
        }
        let len = total.max(FIRST_TB_SIZE);
        q.build_tfd(len);
        q.write = (q.write + 1) % TFD_QUEUE_SIZE;
        let w = q.write;
        wmb();
        self.m.w32(HBUS_TARG_WRPTR, w as u32 | ((CMD_QUEUE as u32) << 8));
        Ok(seq)
    }

    /// Reclaim command-queue TFDs up to and including `index`.
    pub fn cmd_done(&mut self, index: usize) {
        if let Some(q) = self.txq[CMD_QUEUE].as_mut() { q.read = (index + 1) % TFD_QUEUE_SIZE; }
    }

    /// Queue a TX command + frame on data queue `q`: `cmd` is the TX_CMD
    /// payload (struct iwl_tx_cmd with the 802.11 header appended), `frame_body`
    /// the rest of the MPDU. `bc_len` is the scheduler byte count (MPDU length
    /// plus crypto overhead) and `sta_id` the station.
    pub fn tx(&mut self, q: usize, tx_cmd: &[u8], body: &[u8], bc_len: usize, sta_id: u8) -> Result<usize, TransError> {
        let bc = &self.bc;
        let t = self.txq[q].as_mut().ok_or(TransError::NotReady)?;
        if !t.active { return Err(TransError::NotReady); }
        if t.used() >= TFD_QUEUE_SIZE - 16 { return Err(TransError::Busy); }
        let idx = t.write;
        let seq: u16 = ((q as u16 & 0x1f) << 8) | (idx as u16 & 0xff);
        let total = 4 + tx_cmd.len() + body.len();
        if total > 4096 { return Err(TransError::Busy); }
        let slot = t.slot(idx);
        {
            let b = t.bufs[slot].bytes_mut();
            b[0] = 0x1c; // TX_CMD
            b[1] = 0;
            b[2..4].copy_from_slice(&seq.to_le_bytes());
            b[4..4 + tx_cmd.len()].copy_from_slice(tx_cmd);
            b[4 + tx_cmd.len()..total].copy_from_slice(body);
        }
        t.build_tfd(total);
        // Scheduler byte-count table (dword units on this family).
        let len = (bc_len + 4 + 4 + 3) / 4;
        let ent: u16 = (len as u16 & 0xfff) | ((sta_id as u16) << 12);
        let base = q * BC_TABLE_ENTRIES * 2;
        bc.wr16(base + idx * 2, ent);
        if idx < 64 { bc.wr16(base + (TFD_QUEUE_SIZE + idx) * 2, ent); }
        t.write = (t.write + 1) % TFD_QUEUE_SIZE;
        let w = t.write;
        wmb();
        self.m.w32(HBUS_TARG_WRPTR, w as u32 | ((q as u32) << 8));
        Ok(idx)
    }

    /// TX completion: the firmware is done with TFDs before `ssn`.
    pub fn tx_reclaim(&mut self, q: usize, ssn: usize) {
        if let Some(t) = self.txq[q].as_mut() { t.read = ssn % TFD_QUEUE_SIZE; }
    }

    // ── RX ──

    /// Collect every packet the firmware closed since the last call.
    pub fn rx_poll(&mut self, out: &mut Vec<RxPacket>) {
        let closed = (self.rx.stts.rd16(0) & 0x0fff) as usize % RX_QUEUE_SIZE;
        let mut i = self.rx.read;
        if i == closed { return; }
        while i != closed {
            let buf = self.rx.bufs[i].bytes();
            let mut off = 0usize;
            while off + 8 < RB_SIZE {
                let lnf = u32::from_le_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]]);
                if lnf == RX_FRAME_INVALID { break; }
                let flen = (lnf & RX_FRAME_SIZE_MSK) as usize; // header + payload
                let len = flen + 4;
                if flen < 4 || off + len > RB_SIZE { break; }
                let cmd = buf[off + 4];
                let group = buf[off + 5];
                let seq = u16::from_le_bytes([buf[off + 6], buf[off + 7]]);
                out.push(RxPacket { cmd, group, seq, data: buf[off + 8..off + len].to_vec() });
                self.rx_packets += 1;
                off += (len + RX_FRAME_ALIGN - 1) & !(RX_FRAME_ALIGN - 1);
            }
            // Mark the buffer consumed (a stale header must not be parsed
            // again if the firmware closes an RB without writing it).
            self.rx.bufs[i].wr32(0, RX_FRAME_INVALID);
            i = (i + 1) % RX_QUEUE_SIZE;
        }
        self.rx.read = i;
        // Restock: slot `write` gets its own buffer back.
        while (self.rx.read + RX_QUEUE_SIZE - self.rx.write - 1) % RX_QUEUE_SIZE > 0 {
            let w = self.rx.write;
            let phys = self.rx.bufs[w].phys;
            self.rx.bd.wr32(w * 4, (phys >> 8) as u32);
            self.rx.write = (w + 1) % RX_QUEUE_SIZE;
        }
        wmb();
        self.rx_update_wptr();
    }

    // ── diagnostics ──

    /// Key CSR values as one line.
    pub fn csr_dump(&self) -> String {
        let mut s = String::new();
        let regs: [(&str, usize); 12] = [
            ("HW_IF_CONFIG", CSR_HW_IF_CONFIG_REG), ("INT", CSR_INT), ("INT_MASK", CSR_INT_MASK),
            ("FH_INT", CSR_FH_INT_STATUS), ("GPIO_IN", CSR_GPIO_IN), ("RESET", CSR_RESET),
            ("GP_CNTRL", CSR_GP_CNTRL), ("HW_REV", CSR_HW_REV), ("GP_DRIVER", CSR_GP_DRIVER_REG),
            ("UCODE_GP1", CSR_UCODE_DRV_GP1), ("UCODE_GP2", CSR_UCODE_DRV_GP2), ("HW_RF_ID", CSR_HW_RF_ID),
        ];
        for (n, o) in regs.iter() { let _ = write!(s, "{}={:08x} ", n, self.m.r32(*o)); }
        s
    }

    /// Dump a firmware error-event table from SRAM.
    pub fn dump_error_table(&self, what: &str, addr: u32) {
        if addr == 0 || addr == 0xffff_ffff { ilog!("{} error table: no address", what); return; }
        let mut v = [0u32; 32];
        if !self.read_mem(addr, &mut v) { ilog!("{} error table at {:#x}: no NIC access", what, addr); return; }
        if what == "LMAC" {
            ilog!("{} error table at {:#x}: valid {:#x} error_id {:#010x} ({}) trm_hw {:08x}/{:08x} blink2 {:#x} ilink1 {:#x} ilink2 {:#x} data1 {:#x} data2 {:#x} data3 {:#x}",
                  what, addr, v[0], v[1], error_name(v[1]), v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9]);
            ilog!("  bcon_time {:#x} tsf {:08x}{:08x} gp1 {:#x} gp2 {:#x} fw_rev_type {:#x} ver {}.{} hw_ver {:#x} brd_ver {:#x} log_pc {:#x} hcmd {:#x} isr {:08x} {:08x} {:08x} {:08x} {:08x} last_cmd {:#x}",
                  v[10], v[12], v[11], v[13], v[14], v[15], v[16], v[17], v[18], v[19], v[20], v[23], v[24], v[25], v[26], v[27], v[28], v[29]);
        } else {
            ilog!("{} error table at {:#x}: valid {:#x} error_id {:#010x} ({}) blink1 {:#x} blink2 {:#x} ilink1 {:#x} ilink2 {:#x} data1 {:#x} data2 {:#x} data3 {:#x} ver {}.{} frame_ptr {:#x} stack_ptr {:#x} cmd_hdr {:#x} sysassert {:#x}",
                  what, addr, v[0], v[1], error_name(v[1]), v[2], v[3], v[4], v[5], v[6], v[7], v[8], v[9], v[10], v[11], v[12], v[13], v[14]);
        }
    }
}

/// Names of the common firmware assert codes (fw/dump.c advanced_lookup).
pub fn error_name(id: u32) -> &'static str {
    match id & 0x0fff_ffff {
        0x0 => "OK",
        0x5 => "NMI_INTERRUPT_WDG",
        0x34 => "SYSASSERT",
        0x35 => "UCODE_VERSION_MISMATCH",
        0x37 => "BAD_COMMAND",
        0x38 => "BAD_COMMAND",
        0x70 => "NMI_INTERRUPT_DATA_ACTION_PT",
        0x71 => "NMI_TRM_HW_ERR",
        0x72 => "NMI_INTERRUPT_TRM",
        0x73 => "NMI_INTERRUPT_BREAK_POINT",
        0x74 => "NMI_INTERRUPT_WDG_RXF_FULL",
        0x75 => "NMI_INTERRUPT_WDG_NO_RBD_RXF_FULL",
        0x76 => "NMI_INTERRUPT_HOST",
        0x77 => "NMI_INTERRUPT_LMAC_FATAL",
        0x78 => "NMI_INTERRUPT_UMAC_FATAL",
        0x84 => "NMI_INTERRUPT_UNKNOWN",
        0x86 => "NMI_INTERRUPT_INST_ACTION_PT",
        0x9 => "ADVANCED_SYSASSERT",
        _ => "?",
    }
}
