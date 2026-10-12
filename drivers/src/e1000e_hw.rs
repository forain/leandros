//! Hardware-independent parts of the Intel e1000e driver (`e1000e.rs`): the
//! device table (82574L for QEMU, the PCH-integrated I219 family for real
//! laptops), legacy RX/TX descriptor rings, the HV/I217-class PHY register
//! addressing (pages, MDIC encoding), MAC-address and NVM helpers, link,
//! flow-control and LTR arithmetic, FWSM (management firmware) decoding and
//! the interrupt mask protocol. No MMIO here: everything is exercised on the
//! host by `drivers/e1000e-tests`.
//!
//! Register and bit names follow Intel's datasheets as used by Linux's e1000e
//! driver (GPL; consulted as documentation only, no code copied), so a log
//! line can be matched against `ich8lan.c` / `defines.h`.

#![allow(dead_code)]

use core::sync::atomic::{fence, Ordering};

// ── device table ────────────────────────────────────────────────────────────

/// MAC generation, ordered like Linux's `enum e1000_mac_type` (comparisons
/// such as "SPT or newer" are written `fam >= Family::PchSpt`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Family {
    /// Discrete 82574L/LA (the device QEMU's `-device e1000e` models).
    I82574,
    /// Sunrise Point / Kaby Lake / Lewisburg / Comet Lake (some) PCH.
    PchSpt,
    /// Cannon Point / Ice Point / Comet Lake PCH.
    PchCnp,
    /// Tiger Lake PCH.
    PchTgp,
    /// Alder Lake / Raptor Lake PCH.
    PchAdp,
    /// Meteor Lake and newer (Lunar/Arrow/Panther/Nova Lake).
    PchMtp,
}

impl Family {
    pub fn is_pch(self) -> bool { self >= Family::PchSpt }
    pub fn name(self) -> &'static str {
        match self {
            Family::I82574 => "82574",
            Family::PchSpt => "pch_spt",
            Family::PchCnp => "pch_cnp",
            Family::PchTgp => "pch_tgp",
            Family::PchAdp => "pch_adp",
            Family::PchMtp => "pch_mtp",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DevInfo {
    pub did: u16,
    /// Linux's `E1000_DEV_ID_*` suffix, so a log line maps to `hw.h`.
    pub name: &'static str,
    pub fam: Family,
}

pub const INTEL: u16 = 0x8086;

const fn d(did: u16, name: &'static str, fam: Family) -> DevInfo { DevInfo { did, name, fam } }

/// Every 82574 and I219 PCI id of Linux v6.11's e1000e table, with the board
/// (MAC generation) Linux binds it to. The order is the scan order: the
/// 82574 last, so a laptop that also has an 82574 card uses its onboard port.
pub const DEVICES: &[DevInfo] = &[
    // board_pch_spt
    d(0x156f, "PCH_SPT_I219_LM", Family::PchSpt),
    d(0x1570, "PCH_SPT_I219_V", Family::PchSpt),
    d(0x15b7, "PCH_SPT_I219_LM2", Family::PchSpt),
    d(0x15b8, "PCH_SPT_I219_V2", Family::PchSpt),
    d(0x15b9, "PCH_LBG_I219_LM3", Family::PchSpt),
    d(0x15d7, "PCH_SPT_I219_LM4", Family::PchSpt),
    d(0x15d8, "PCH_SPT_I219_V4", Family::PchSpt),
    d(0x15e3, "PCH_SPT_I219_LM5", Family::PchSpt),
    d(0x15d6, "PCH_SPT_I219_V5", Family::PchSpt),
    d(0x0d53, "PCH_CMP_I219_LM12", Family::PchSpt),
    d(0x0d55, "PCH_CMP_I219_V12", Family::PchSpt),
    // board_pch_cnp
    d(0x15bd, "PCH_CNP_I219_LM6", Family::PchCnp),
    d(0x15be, "PCH_CNP_I219_V6", Family::PchCnp),
    d(0x15bb, "PCH_CNP_I219_LM7", Family::PchCnp),
    d(0x15bc, "PCH_CNP_I219_V7", Family::PchCnp),
    d(0x15df, "PCH_ICP_I219_LM8", Family::PchCnp),
    d(0x15e0, "PCH_ICP_I219_V8", Family::PchCnp),
    d(0x15e1, "PCH_ICP_I219_LM9", Family::PchCnp),
    d(0x15e2, "PCH_ICP_I219_V9", Family::PchCnp),
    d(0x0d4e, "PCH_CMP_I219_LM10", Family::PchCnp),
    d(0x0d4f, "PCH_CMP_I219_V10", Family::PchCnp),
    d(0x0d4c, "PCH_CMP_I219_LM11", Family::PchCnp),
    d(0x0d4d, "PCH_CMP_I219_V11", Family::PchCnp),
    // board_pch_tgp
    d(0x15fb, "PCH_TGP_I219_LM13", Family::PchTgp),
    d(0x15fc, "PCH_TGP_I219_V13", Family::PchTgp),
    d(0x15f9, "PCH_TGP_I219_LM14", Family::PchTgp),
    d(0x15fa, "PCH_TGP_I219_V14", Family::PchTgp),
    d(0x15f4, "PCH_TGP_I219_LM15", Family::PchTgp),
    d(0x15f5, "PCH_TGP_I219_V15", Family::PchTgp),
    // board_pch_adp
    d(0x0dc5, "PCH_RPL_I219_LM23", Family::PchAdp),
    d(0x0dc6, "PCH_RPL_I219_V23", Family::PchAdp),
    d(0x1a1e, "PCH_ADP_I219_LM16", Family::PchAdp),
    d(0x1a1f, "PCH_ADP_I219_V16", Family::PchAdp),
    d(0x1a1c, "PCH_ADP_I219_LM17", Family::PchAdp),
    d(0x1a1d, "PCH_ADP_I219_V17", Family::PchAdp),
    d(0x0dc7, "PCH_RPL_I219_LM22", Family::PchAdp),
    d(0x0dc8, "PCH_RPL_I219_V22", Family::PchAdp),
    // board_pch_mtp
    d(0x550a, "PCH_MTP_I219_LM18", Family::PchMtp),
    d(0x550b, "PCH_MTP_I219_V18", Family::PchMtp),
    d(0x550c, "PCH_MTP_I219_LM19", Family::PchMtp),
    d(0x550d, "PCH_MTP_I219_V19", Family::PchMtp),
    d(0x550e, "PCH_LNP_I219_LM20", Family::PchMtp),
    d(0x550f, "PCH_LNP_I219_V20", Family::PchMtp),
    d(0x5510, "PCH_LNP_I219_LM21", Family::PchMtp),
    d(0x5511, "PCH_LNP_I219_V21", Family::PchMtp),
    d(0x57a0, "PCH_ARL_I219_LM24", Family::PchMtp),
    d(0x57a1, "PCH_ARL_I219_V24", Family::PchMtp),
    d(0x57b3, "PCH_PTP_I219_LM25", Family::PchMtp),
    d(0x57b4, "PCH_PTP_I219_V25", Family::PchMtp),
    d(0x57b5, "PCH_PTP_I219_LM26", Family::PchMtp),
    d(0x57b6, "PCH_PTP_I219_V26", Family::PchMtp),
    d(0x57b7, "PCH_PTP_I219_LM27", Family::PchMtp),
    d(0x57b8, "PCH_PTP_I219_V27", Family::PchMtp),
    d(0x57b9, "PCH_NVL_I219_LM29", Family::PchMtp),
    d(0x57ba, "PCH_NVL_I219_V29", Family::PchMtp),
    // board_82574
    d(0x10d3, "82574L", Family::I82574),
    d(0x10f6, "82574LA", Family::I82574),
];

pub fn lookup(did: u16) -> Option<&'static DevInfo> { DEVICES.iter().find(|x| x.did == did) }

// ── legacy descriptors ──────────────────────────────────────────────────────

/// Legacy receive descriptor (RFCTL.EXTEN = 0, RCTL.DTYP = 0): 16 bytes.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RxDesc {
    pub addr: u64,
    pub len: u16,
    pub csum: u16,
    pub status: u8,
    pub errors: u8,
    pub special: u16,
}

/// Legacy transmit descriptor: `lower` = length | CSO << 16 | CMD << 24,
/// `upper` = STA | CSS << 8 | special << 16.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TxDesc {
    pub addr: u64,
    pub lower: u32,
    pub upper: u32,
}

pub const RXD_STAT_DD: u8 = 0x01;
pub const RXD_STAT_EOP: u8 = 0x02;
/// CRC, symbol, sequence, carrier-extension and RX data errors (Linux's
/// E1000_RXD_ERR_FRAME_ERR_MASK). Checksum-offload errors are not frame
/// errors (and offload is off).
pub const RXD_ERR_FRAME: u8 = 0x01 | 0x02 | 0x04 | 0x10 | 0x80;

pub const TXD_CMD_EOP: u32 = 0x0100_0000;
pub const TXD_CMD_IFCS: u32 = 0x0200_0000;
pub const TXD_CMD_RS: u32 = 0x0800_0000;
pub const TXD_STAT_DD: u32 = 0x1;

unsafe fn rx_rd_status(d: *const RxDesc) -> u8 { core::ptr::read_volatile(core::ptr::addr_of!((*d).status)) }
unsafe fn rx_rd_errors(d: *const RxDesc) -> u8 { core::ptr::read_volatile(core::ptr::addr_of!((*d).errors)) }
unsafe fn rx_rd_len(d: *const RxDesc) -> u16 { core::ptr::read_volatile(core::ptr::addr_of!((*d).len)) }
unsafe fn rx_clear(d: *mut RxDesc) {
    // Status last-read field first, then the rest: the NIC writes status
    // (DD) last, so a cleared status means "not done".
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).status), 0);
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).errors), 0);
    core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).len), 0);
}
unsafe fn rx_wr_addr(d: *mut RxDesc, v: u64) { core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).addr), v) }
unsafe fn tx_rd_upper(d: *const TxDesc) -> u32 { core::ptr::read_volatile(core::ptr::addr_of!((*d).upper)) }
unsafe fn tx_wr_lower(d: *mut TxDesc, v: u32) { core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).lower), v) }
unsafe fn tx_wr_upper(d: *mut TxDesc, v: u32) { core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).upper), v) }
unsafe fn tx_wr_addr(d: *mut TxDesc, v: u64) { core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).addr), v) }

/// What the next RX descriptor holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxEvent {
    /// Not written back yet (DD clear): nothing received.
    Empty,
    /// A whole frame of `len` bytes (FCS stripped by RCTL.SECRC).
    Frame { idx: usize, len: usize },
    /// A frame with a frame error (`errors` byte); drop it.
    Error { idx: usize, errors: u8 },
    /// Part of a frame larger than one buffer (no EOP). Long-packet receive
    /// is off, so this means a misconfiguration; dropped.
    Fragment { idx: usize, status: u8 },
}

/// RX ring over `n` legacy descriptors, each pointing at its own buffer.
///
/// Ownership is by index, not by a bit: the NIC may fill the descriptors from
/// RDH up to RDT - 1. The driver keeps exactly one descriptor back (RDT
/// trails `cur` by one), so RDH == RDT never means "full" to the NIC.
pub struct RxRing {
    pub descs: *mut RxDesc,
    pub n: usize,
    pub cur: usize,
    pub buf_size: u32,
}

unsafe impl Send for RxRing {}

impl RxRing {
    /// Point every descriptor at its buffer, clear its status.
    ///
    /// # Safety
    /// `descs` must be valid for `n` descriptors.
    pub unsafe fn init(descs: *mut RxDesc, n: usize, buf_size: u32, buf_phys: &dyn Fn(usize) -> u64) -> Self {
        for i in 0..n {
            let dd = descs.add(i);
            rx_wr_addr(dd, buf_phys(i));
            rx_clear(dd);
        }
        fence(Ordering::Release);
        RxRing { descs, n, cur: 0, buf_size }
    }

    /// RDT to program after `init`/`reset` (RDH = 0): every descriptor but
    /// the last one is the NIC's.
    pub fn initial_tail(&self) -> u32 { (self.n - 1) as u32 }

    /// Clear every status and restart at 0 (after a MAC reset, which zeroes
    /// RDH/RDT); program RDT = `initial_tail()` afterwards.
    pub fn reset(&mut self) {
        unsafe { for i in 0..self.n { rx_clear(self.descs.add(i)); } }
        fence(Ordering::Release);
        self.cur = 0;
    }

    /// Descriptors written back (DD set) and not yet consumed.
    pub fn filled(&self) -> usize {
        (0..self.n).filter(|&i| unsafe { rx_rd_status(self.descs.add(i)) } & RXD_STAT_DD != 0).count()
    }

    pub fn next(&self) -> RxEvent {
        let idx = self.cur;
        let dp = unsafe { self.descs.add(idx) };
        let status = unsafe { rx_rd_status(dp) };
        if status & RXD_STAT_DD == 0 { return RxEvent::Empty; }
        // Nothing else of the descriptor or buffer before DD was seen.
        fence(Ordering::Acquire);
        let errors = unsafe { rx_rd_errors(dp) };
        if status & RXD_STAT_EOP == 0 { return RxEvent::Fragment { idx, status }; }
        if errors & RXD_ERR_FRAME != 0 { return RxEvent::Error { idx, errors }; }
        let len = unsafe { rx_rd_len(dp) } as usize;
        if len == 0 { return RxEvent::Error { idx, errors }; }
        RxEvent::Frame { idx, len: len.min(self.buf_size as usize) }
    }

    /// Recycle descriptor `idx` (the one `next` returned) and advance.
    /// Returns the new RDT: the descriptor kept back so far (`idx - 1`) goes
    /// to the NIC, `idx` becomes the one kept back.
    pub fn release(&mut self, idx: usize) -> u32 {
        debug_assert_eq!(idx, self.cur);
        unsafe { rx_clear(self.descs.add(idx)); }
        fence(Ordering::Release);
        self.cur = (self.cur + 1) % self.n;
        idx as u32
    }
}

/// TX ring. `cur` and `dirty` are free-running: slots `dirty..cur` are posted
/// and not reclaimed yet. TDT = `cur % n`.
pub struct TxRing {
    pub descs: *mut TxDesc,
    pub n: usize,
    pub cur: u64,
    pub dirty: u64,
}

unsafe impl Send for TxRing {}

impl TxRing {
    /// # Safety
    /// `descs` must be valid for `n` descriptors.
    pub unsafe fn init(descs: *mut TxDesc, n: usize, buf_phys: &dyn Fn(usize) -> u64) -> Self {
        for i in 0..n {
            let dd = descs.add(i);
            tx_wr_addr(dd, buf_phys(i));
            tx_wr_lower(dd, 0);
            tx_wr_upper(dd, 0);
        }
        TxRing { descs, n, cur: 0, dirty: 0 }
    }

    pub fn in_flight(&self) -> usize { (self.cur - self.dirty) as usize }

    /// The slot for the next frame, None when full (one slot stays free so
    /// TDT never catches up with TDH).
    pub fn slot(&self) -> Option<usize> {
        if self.in_flight() >= self.n - 1 { None } else { Some((self.cur % self.n as u64) as usize) }
    }

    /// Hand the frame already copied into slot `idx`'s buffer (`len` bytes)
    /// to the NIC. Returns the TDT value to write (the doorbell).
    pub fn post(&mut self, idx: usize, len: usize) -> u32 {
        debug_assert_eq!(Some(idx), self.slot());
        unsafe {
            let dd = self.descs.add(idx);
            tx_wr_upper(dd, 0);
            tx_wr_lower(dd, (len as u32 & 0xffff) | TXD_CMD_EOP | TXD_CMD_IFCS | TXD_CMD_RS);
        }
        fence(Ordering::Release);
        self.cur += 1;
        self.tail()
    }

    pub fn tail(&self) -> u32 { (self.cur % self.n as u64) as u32 }

    /// Reclaim slots the NIC has written back (DD). Returns how many.
    pub fn reclaim(&mut self) -> usize {
        let mut k = 0;
        while self.dirty != self.cur {
            let idx = (self.dirty % self.n as u64) as usize;
            if unsafe { tx_rd_upper(self.descs.add(idx)) } & TXD_STAT_DD == 0 { break; }
            self.dirty += 1;
            k += 1;
        }
        if k > 0 { fence(Ordering::Acquire); }
        k
    }

    /// Forget everything in flight (after a MAC reset zeroed TDH/TDT).
    pub fn reset(&mut self) {
        unsafe {
            for i in 0..self.n {
                let dd = self.descs.add(i);
                tx_wr_lower(dd, 0);
                tx_wr_upper(dd, 0);
            }
        }
        self.cur = 0;
        self.dirty = 0;
    }
}

// ── PHY register addressing ─────────────────────────────────────────────────
//
// PCH PHYs (82577 .. I219, Linux's "HV" access) have a 32-register MII space
// per page. A register is named `page << 5 | reg`. Pages >= 768 live at PHY
// address 1, everything else at address 2. Registers 0..15 are the same on
// every page; for 16..31 the page is first selected by writing `page << 5`
// to register 31 at address 1. Page 768 is selected as page 0. Page 800 (host
// wakeup) and pages 1..767 (debug) need other access sequences; the driver
// does not use them.

pub const PHY_PAGE_SHIFT: u32 = 5;
pub const MAX_PHY_REG: u32 = 0x1f;
pub const MAX_PHY_MULTI_PAGE_REG: u32 = 0xf;
pub const HV_INTC_FC_PAGE_START: u32 = 768;
pub const BM_WUC_PAGE: u32 = 800;
pub const PHY_PAGE_SELECT: u32 = 0x1f;

pub const fn phy_reg(page: u32, reg: u32) -> u32 { (page << PHY_PAGE_SHIFT) | (reg & MAX_PHY_REG) }

/// One HV PHY register access as MDIC transactions: an optional page-select
/// write (value, at address 1, register 31) then the access itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HvAccess {
    pub page_select: Option<u16>,
    pub addr: u32,
    pub reg: u32,
}

pub fn hv_access(off: u32) -> Result<HvAccess, &'static str> {
    let page = (off >> PHY_PAGE_SHIFT) & 0xffff;
    let reg = off & MAX_PHY_REG;
    if page == BM_WUC_PAGE { return Err("wakeup page 800 not supported"); }
    if page > 0 && page < HV_INTC_FC_PAGE_START { return Err("debug pages 1..767 not supported"); }
    let addr = if page >= HV_INTC_FC_PAGE_START { 1 } else { 2 };
    let sel = if page == HV_INTC_FC_PAGE_START { 0 } else { page };
    let page_select = if reg > MAX_PHY_MULTI_PAGE_REG { Some((sel << PHY_PAGE_SHIFT) as u16) } else { None };
    Ok(HvAccess { page_select, addr, reg })
}

// MDIC (MDI control register) encoding.
pub const MDIC_REG_SHIFT: u32 = 16;
pub const MDIC_PHY_SHIFT: u32 = 21;
pub const MDIC_OP_WRITE: u32 = 0x0400_0000;
pub const MDIC_OP_READ: u32 = 0x0800_0000;
pub const MDIC_READY: u32 = 0x1000_0000;
pub const MDIC_ERROR: u32 = 0x4000_0000;

pub fn mdic_read_cmd(addr: u32, reg: u32) -> u32 { ((reg & MAX_PHY_REG) << MDIC_REG_SHIFT) | (addr << MDIC_PHY_SHIFT) | MDIC_OP_READ }
pub fn mdic_write_cmd(addr: u32, reg: u32, v: u16) -> u32 {
    v as u32 | ((reg & MAX_PHY_REG) << MDIC_REG_SHIFT) | (addr << MDIC_PHY_SHIFT) | MDIC_OP_WRITE
}

/// Outcome of a completed-or-not MDIC transaction (`mdic` = last value read).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MdicResult {
    Ok(u16),
    NotReady,
    Error,
    WrongReg(u32),
}

pub fn mdic_decode(mdic: u32, reg: u32) -> MdicResult {
    if mdic & MDIC_READY == 0 { return MdicResult::NotReady; }
    if mdic & MDIC_ERROR != 0 { return MdicResult::Error; }
    let r = (mdic >> MDIC_REG_SHIFT) & MAX_PHY_REG;
    if r != reg & MAX_PHY_REG { return MdicResult::WrongReg(r); }
    MdicResult::Ok(mdic as u16)
}

/// PHY ids (PHYSID1 << 16 | PHYSID2, revision nibble masked).
pub const PHY_REVISION_MASK: u32 = 0xffff_fff0;
pub const I217_E_PHY_ID: u32 = 0x0154_00a0;
pub const I82579_E_PHY_ID: u32 = 0x0154_0090;
pub const BME1000_E_PHY_ID: u32 = 0x0141_0cb0;

pub fn phy_name(id: u32) -> &'static str {
    match id & PHY_REVISION_MASK {
        I217_E_PHY_ID => "I217/I218/I219 (e1000_phy_i217)",
        I82579_E_PHY_ID => "82579",
        BME1000_E_PHY_ID => "BME1000 (82574)",
        0 | 0xffff_fff0 => "none (PHY not answering)",
        _ => "unknown",
    }
}

/// A PHY id read back as all-ones or zero is a PHY that did not answer.
pub fn phy_id_valid(id1: u16, id2: u16) -> bool { id1 != 0xffff && id2 != 0xffff && (id1 != 0 || id2 != 0) }

// ── MAC address / NVM ──────────────────────────────────────────────────────

pub const RAH_AV: u32 = 0x8000_0000;

pub fn mac_valid(m: &[u8; 6]) -> bool { m.iter().any(|&b| b != 0) && m[0] & 1 == 0 && *m != [0xff; 6] }

/// MAC from RAL0/RAH0 (loaded from the NVM by the hardware at power-on).
pub fn mac_from_ral(ral: u32, rah: u32) -> [u8; 6] {
    [ral as u8, (ral >> 8) as u8, (ral >> 16) as u8, (ral >> 24) as u8, rah as u8, (rah >> 8) as u8]
}

pub fn ral_rah_of(m: &[u8; 6]) -> (u32, u32) {
    let ral = m[0] as u32 | (m[1] as u32) << 8 | (m[2] as u32) << 16 | (m[3] as u32) << 24;
    let mut rah = m[4] as u32 | (m[5] as u32) << 8;
    if ral != 0 || rah != 0 { rah |= RAH_AV; }
    (ral, rah)
}

/// MAC from NVM words 0..2 (little endian within each word).
pub fn mac_from_nvm(w: [u16; 3]) -> [u8; 6] {
    [w[0] as u8, (w[0] >> 8) as u8, w[1] as u8, (w[1] >> 8) as u8, w[2] as u8, (w[2] >> 8) as u8]
}

pub const NVM_CHECKSUM_REG: usize = 0x3f;
pub const NVM_SUM: u16 = 0xbaba;

/// Sum of words 0..=0x3f; a valid image sums to 0xBABA.
pub fn nvm_checksum(words: &[u16]) -> u16 { words.iter().take(NVM_CHECKSUM_REG + 1).fold(0u16, |a, &w| a.wrapping_add(w)) }

/// SPT+ GbE NVM: size from the STRAP register (NVMS field, 4 KiB units),
/// two banks; returns the bank size in 16-bit words.
pub fn spt_flash_bank_words(strap: u32) -> u32 {
    let size = (((strap >> 1) & 0x1f) + 1) * 4096;
    size / 2 / 2
}

pub const ICH_NVM_SIG_WORD: u32 = 0x13;

/// Valid-bank signature: bits 15:14 of word 0x13 == 10b (`dword` is the
/// 32-bit flash read at word 0x13, so the word's high byte is bits 15:8).
pub fn nvm_sig_valid(dword: u32) -> bool { ((dword >> 8) & 0xc0) == 0x80 }

// ── link, flow control, LTR ─────────────────────────────────────────────────

pub const STATUS_FD: u32 = 0x1;
pub const STATUS_LU: u32 = 0x2;
pub const STATUS_SPEED_MASK: u32 = 0xc0;

/// (link up, Mbit/s, full duplex) from the MAC STATUS register.
pub fn link_from_status(st: u32) -> (bool, u32, bool) {
    let speed = match st & STATUS_SPEED_MASK { 0x00 => 10, 0x40 => 100, _ => 1000 };
    (st & STATUS_LU != 0, speed, st & STATUS_FD != 0)
}

pub const ADV_PAUSE: u16 = 0x0400;
pub const ADV_ASYM: u16 = 0x0800;

/// IEEE 802.3 Annex 28B pause resolution from our advertisement and the
/// partner's (MII registers 4 and 5): (honour received PAUSE = CTRL.RFCE,
/// send PAUSE = CTRL.TFCE).
pub fn fc_resolve(adv: u16, lpa: u16) -> (bool, bool) {
    let (lp, la) = (adv & ADV_PAUSE != 0, adv & ADV_ASYM != 0);
    let (rp, ra) = (lpa & ADV_PAUSE != 0, lpa & ADV_ASYM != 0);
    if lp && rp { (true, true) }
    else if !lp && la && rp && ra { (false, true) }
    else if lp && la && !rp && ra { (true, false) }
    else { (false, false) }
}

pub const LTR_VALUE_MASK: u32 = 0x3ff;
pub const LTR_SCALE_SHIFT: u32 = 10;
pub const LTR_SCALE_MAX: u32 = 5;

fn ltr_decode(enc: u16) -> u64 {
    (enc as u64 & LTR_VALUE_MASK as u64) << (5 * ((enc as u32 >> LTR_SCALE_SHIFT) & 7))
}

/// Latency tolerance to report (LTRV value field, same for snoop and
/// no-snoop) on PCH parts once the link is up, as `e1000_platform_pm_pch_lpt`
/// computes it: the time the RX packet buffer (`rxa_kb` KiB, less one frame)
/// takes to fill at `speed` Mbit/s, capped at the platform maximum read from
/// the LTR capability (`max_snoop`, `max_nosnoop`). None = invalid scale.
pub fn ltr_encode(rxa_kb: u32, max_frame: u32, speed: u32, max_snoop: u16, max_nosnoop: u16) -> Option<u16> {
    if speed == 0 { return None; }
    let rxa = rxa_kb as u64 * 512;
    let mut value = if rxa > max_frame as u64 { (rxa - max_frame as u64) * (16000 / speed as u64) } else { 0 };
    let mut scale = 0u32;
    while value > LTR_VALUE_MASK as u64 {
        scale += 1;
        value = value.div_ceil(32);
    }
    if scale > LTR_SCALE_MAX { return None; }
    let mut enc = ((scale << LTR_SCALE_SHIFT) as u64 | value) as u16;
    let max_enc = max_snoop.max(max_nosnoop);
    if ltr_decode(enc) > ltr_decode(max_enc) { enc = max_enc; }
    Some(enc)
}

// ── FWSM: the management engine's view ──────────────────────────────────────

pub const FWSM_RSPCIPHY: u32 = 0x0000_0040;
pub const FWSM_WLOCK_MAC_MASK: u32 = 0x0380;
pub const FWSM_WLOCK_MAC_SHIFT: u32 = 7;
pub const FWSM_ULP_CFG_DONE: u32 = 0x0000_0400;
pub const FWSM_FW_VALID: u32 = 0x0000_8000;
pub const FWSM_PCIM2PCI: u32 = 0x0100_0000;
pub const FWSM_MODE_MASK: u32 = 0xe;
pub const FWSM_MODE_SHIFT: u32 = 1;
pub const ICH_MNG_IAMT_MODE: u32 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Fwsm {
    /// Management firmware (ME) is running and owns part of the NIC.
    pub fw_valid: bool,
    pub mode: u32,
    /// Clear = the ME blocks PHY resets (a SOL/IDE-R session, or the ME is
    /// still configuring the PHY).
    pub phy_reset_allowed: bool,
    pub ulp_cfg_done: bool,
    pub wlock_mac: u32,
    pub pcim2pci: bool,
}

pub fn fwsm_decode(v: u32) -> Fwsm {
    Fwsm {
        fw_valid: v & FWSM_FW_VALID != 0,
        mode: (v & FWSM_MODE_MASK) >> FWSM_MODE_SHIFT,
        phy_reset_allowed: v & FWSM_RSPCIPHY != 0,
        ulp_cfg_done: v & FWSM_ULP_CFG_DONE != 0,
        wlock_mac: (v & FWSM_WLOCK_MAC_MASK) >> FWSM_WLOCK_MAC_SHIFT,
        pcim2pci: v & FWSM_PCIM2PCI != 0,
    }
}

pub fn fwsm_mode_name(mode: u32) -> &'static str {
    match mode { 0 => "none", 1 => "ASF", ICH_MNG_IAMT_MODE => "iAMT", 3 => "mode3", 4 => "mode4", _ => "other" }
}

// ── interrupt mask protocol ─────────────────────────────────────────────────
//
// The MAC asserts its interrupt while ICR & IMS is non-zero; an MSI is sent
// on the rising edge of that condition. ICR bits latch whether or not they
// are enabled. The handler reads ICR, then masks everything (IMC = all) and
// acks exactly what it read (write-1-to-clear: a bit that latched after the
// read stays for the next look) and wakes the net daemon. The daemon drains
// the ring and re-enables the mask (IMS = enabled set): a cause that latched
// while masked makes ICR & IMS non-zero again, which is a new edge, so
// nothing is lost; the daemon also looks at the ring once more after the
// unmask (a frame that landed in between is caught either way).

pub trait IcrRegs {
    fn icr(&mut self) -> u32;
    fn ack(&mut self, bits: u32);
    fn mask_all(&mut self);
    fn unmask(&mut self, bits: u32);
}

/// The interrupt handler's part. None = nothing was pending (another path
/// acked it first, or the device is gone).
pub fn irq_service<R: IcrRegs + ?Sized>(r: &mut R) -> Option<u32> {
    let st = r.icr();
    if st == 0 || st == !0 { return None; }
    r.mask_all();
    r.ack(st);
    Some(st)
}

/// The daemon's part at the end of a pass.
pub fn irq_rearm<R: IcrRegs + ?Sized>(r: &mut R, en: u32) {
    if en != 0 { r.unmask(en); }
}

pub const ICR_TXDW: u32 = 0x01;
pub const ICR_LSC: u32 = 0x04;
pub const ICR_RXSEQ: u32 = 0x08;
pub const ICR_RXDMT0: u32 = 0x10;
pub const ICR_RXO: u32 = 0x40;
pub const ICR_RXT0: u32 = 0x80;
