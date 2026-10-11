//! Hardware-independent parts of the Realtek RTL8169/8168/8101 driver
//! (`r8169.rs`): chip identification, the descriptor-ring bookkeeping, the
//! interrupt status/mask protocol and the PHY-firmware ("rtl_nic/*.fw")
//! parser and interpreter.
//!
//! Nothing here touches MMIO, so the same file is compiled into the host
//! test crate `drivers/r8169-tests` (`cd drivers/r8169-tests && cargo test`)
//! and exercised against a simulated NIC there.
//!
//! Hardware facts are from Realtek's datasheets as documented by the BSD
//! re(4) driver and Linux's r8169 (used as documentation only; this is an
//! independent implementation).

#![allow(dead_code)]

use core::sync::atomic::{fence, Ordering};

// ── chip identification ─────────────────────────────────────────────────────

/// One row of the XID table. `xid` is `(TxConfig >> 20) & 0xfcf`; a row
/// matches when `xid & mask == val`. `ver` is the MAC version number in the
/// numbering Linux and the Realtek vendor driver use (RTL_GIGA_MAC_VER_xx),
/// so log lines can be compared with theirs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChipInfo {
    pub mask: u16,
    pub val: u16,
    pub ver: u8,
    pub name: &'static str,
    /// PHY/MAC patch file under /lib/firmware, if the chip has one.
    pub fw: Option<&'static str>,
}

const fn c(mask: u16, val: u16, ver: u8, name: &'static str, fw: Option<&'static str>) -> ChipInfo {
    ChipInfo { mask, val, ver, name, fw }
}

/// MAC version 0: unknown chip.
pub const VER_NONE: u8 = 0;

/// First match wins, so more specific rows come first (same order as the
/// reference drivers). The RTL8125/8126 rows are absent: those are separate
/// PCI ids this driver does not claim.
pub static CHIPS: &[ChipInfo] = &[
    // 8168fp / 8117
    c(0x7cf, 0x54b, 52, "RTL8168fp/RTL8117", None),
    c(0x7cf, 0x54a, 52, "RTL8168fp/RTL8117", Some("rtl_nic/rtl8168fp-3.fw")),
    // 8168ep
    c(0x7cf, 0x502, 51, "RTL8168ep/8111ep", None),
    // 8168h
    c(0x7cf, 0x541, 46, "RTL8168h/8111h", Some("rtl_nic/rtl8168h-2.fw")),
    c(0x7cf, 0x6c0, 46, "RTL8168M", Some("rtl_nic/rtl8168h-2.fw")),
    // 8168g
    c(0x7cf, 0x5c8, 44, "RTL8411b", Some("rtl_nic/rtl8411-2.fw")),
    c(0x7cf, 0x509, 42, "RTL8168gu/8111gu", Some("rtl_nic/rtl8168g-3.fw")),
    c(0x7cf, 0x4c0, 40, "RTL8168g/8111g", Some("rtl_nic/rtl8168g-2.fw")),
    // 8168f
    c(0x7c8, 0x488, 38, "RTL8411", Some("rtl_nic/rtl8411-1.fw")),
    c(0x7cf, 0x481, 36, "RTL8168f/8111f", Some("rtl_nic/rtl8168f-2.fw")),
    c(0x7cf, 0x480, 35, "RTL8168f/8111f", Some("rtl_nic/rtl8168f-1.fw")),
    // 8168e
    c(0x7c8, 0x2c8, 34, "RTL8168evl/8111evl", Some("rtl_nic/rtl8168e-3.fw")),
    c(0x7cf, 0x2c1, 32, "RTL8168e/8111e", Some("rtl_nic/rtl8168e-1.fw")),
    c(0x7c8, 0x2c0, 33, "RTL8168e/8111e", Some("rtl_nic/rtl8168e-2.fw")),
    // 8168d
    c(0x7cf, 0x281, 25, "RTL8168d/8111d", Some("rtl_nic/rtl8168d-1.fw")),
    c(0x7c8, 0x280, 26, "RTL8168d/8111d", Some("rtl_nic/rtl8168d-2.fw")),
    // 8168dp
    c(0x7cf, 0x28a, 28, "RTL8168dp/8111dp", None),
    c(0x7cf, 0x28b, 31, "RTL8168dp/8111dp", None),
    // 8168c
    c(0x7cf, 0x3c9, 23, "RTL8168cp/8111cp", None),
    c(0x7cf, 0x3c8, 18, "RTL8168cp/8111cp", None),
    c(0x7c8, 0x3c8, 24, "RTL8168cp/8111cp", None),
    c(0x7cf, 0x3c0, 19, "RTL8168c/8111c", None),
    c(0x7cf, 0x3c2, 20, "RTL8168c/8111c", None),
    c(0x7cf, 0x3c3, 21, "RTL8168c/8111c", None),
    c(0x7c8, 0x3c0, 22, "RTL8168c/8111c", None),
    // 8168b
    c(0x7c8, 0x380, 17, "RTL8168b/8111b", None),
    // 8101 family (fast ethernet)
    c(0x7c8, 0x448, 39, "RTL8106e", Some("rtl_nic/rtl8106e-1.fw")),
    c(0x7c8, 0x440, 37, "RTL8402", Some("rtl_nic/rtl8402-1.fw")),
    c(0x7cf, 0x409, 29, "RTL8105e", Some("rtl_nic/rtl8105e-1.fw")),
    c(0x7c8, 0x408, 30, "RTL8105e", Some("rtl_nic/rtl8105e-1.fw")),
    c(0x7cf, 0x349, 8, "RTL8102e", None),
    c(0x7cf, 0x249, 8, "RTL8102e", None),
    c(0x7cf, 0x348, 7, "RTL8102e", None),
    c(0x7cf, 0x248, 7, "RTL8102e", None),
    c(0x7cf, 0x240, 14, "RTL8401", None),
    c(0x7c8, 0x348, 9, "RTL8102e/RTL8103e", None),
    c(0x7c8, 0x248, 9, "RTL8102e/RTL8103e", None),
    c(0x7c8, 0x340, 10, "RTL8101e/RTL8100e", None),
    // 8110 family (PCI)
    c(0xfc8, 0x980, 6, "RTL8169sc/8110sc", None),
    c(0xfc8, 0x180, 5, "RTL8169sc/8110sc", None),
    c(0xfc8, 0x100, 4, "RTL8169sb/8110sb", None),
    c(0xfc8, 0x040, 3, "RTL8110s", None),
    c(0xfc8, 0x008, 2, "RTL8169s", None),
];

static UNKNOWN: ChipInfo = c(0, 0, VER_NONE, "unknown", None);
static RTL8106EUS: ChipInfo = c(0, 0, 43, "RTL8106eus", Some("rtl_nic/rtl8106e-2.fw"));
static RTL8107E: ChipInfo = c(0, 0, 48, "RTL8107e", Some("rtl_nic/rtl8107e-2.fw"));

/// The XID field of a TxConfig register value.
pub fn xid_of(txconfig: u32) -> u16 { ((txconfig >> 20) & 0xfcf) as u16 }

/// Identify a chip by XID. `gmii` is false for a fast-ethernet-only part
/// (PCI id 10ec:8136), which turns two gigabit MACs into their 100 Mbit
/// siblings.
pub fn identify(xid: u16, gmii: bool) -> &'static ChipInfo {
    let p = CHIPS.iter().find(|ci| xid & ci.mask == ci.val).unwrap_or(&UNKNOWN);
    if p.ver == 42 && !gmii { return &RTL8106EUS; }
    if p.ver == 46 && !gmii { return &RTL8107E; }
    p
}

/// Families with the OCP-mapped PHY and MAC MCU (8168g and later).
pub fn is_8168g_up(ver: u8) -> bool { ver >= 40 }
/// 8168e-vl .. 8117, minus the 8106e.
pub fn is_8168evl_up(ver: u8) -> bool { (34..=52).contains(&ver) && ver != 39 }

// ── descriptors ─────────────────────────────────────────────────────────────

/// Descriptor ownership: set = the NIC owns it.
pub const DESC_OWN: u32 = 1 << 31;
/// Last descriptor of the ring: the NIC wraps to the base after it.
pub const DESC_RING_END: u32 = 1 << 30;
pub const DESC_FIRST: u32 = 1 << 29;
pub const DESC_LAST: u32 = 1 << 28;

/// RX status (opts1, NIC-written).
pub const RX_RES: u32 = 1 << 21;   // receive error summary
pub const RX_RWT: u32 = 1 << 22;   // watchdog timer expired (frame too long)
pub const RX_RUNT: u32 = 1 << 20;
pub const RX_CRC: u32 = 1 << 19;
/// Frame length incl. the 4-byte FCS.
pub const RX_LEN_MASK: u32 = 0x3fff;

/// Hardware descriptor (TX and RX share the layout): 16 bytes, rings 256-byte
/// aligned.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Desc {
    pub opts1: u32,
    pub opts2: u32,
    pub addr: u64,
}

unsafe fn rd_opts1(d: *const Desc) -> u32 { core::ptr::read_volatile(core::ptr::addr_of!((*d).opts1)) }
unsafe fn wr_opts1(d: *mut Desc, v: u32) { core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).opts1), v) }
unsafe fn wr_opts2(d: *mut Desc, v: u32) { core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).opts2), v) }
unsafe fn wr_addr(d: *mut Desc, v: u64) { core::ptr::write_volatile(core::ptr::addr_of_mut!((*d).addr), v) }

/// What the next RX descriptor holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RxEvent {
    /// The NIC still owns it: nothing received.
    Empty,
    /// A whole frame of `len` bytes (FCS stripped) in buffer `idx`.
    Frame { idx: usize, len: usize },
    /// A frame the NIC flagged as bad (`status` = opts1); drop it.
    Error { idx: usize, status: u32 },
    /// A frame split over several buffers (larger than one buffer: a jumbo
    /// frame on the wire). Dropped, as the reference drivers do.
    Fragment { idx: usize, status: u32 },
}

/// RX ring bookkeeping over `n` descriptors at `descs`, each pointing at a
/// buffer of `buf_size` bytes.
pub struct RxRing {
    pub descs: *mut Desc,
    pub n: usize,
    pub cur: usize,
    pub buf_size: u32,
}

unsafe impl Send for RxRing {}

impl RxRing {
    /// Point every descriptor at its buffer and hand it to the NIC.
    ///
    /// # Safety
    /// `descs` must be valid for `n` descriptors.
    pub unsafe fn init(descs: *mut Desc, n: usize, buf_size: u32, buf_phys: &dyn Fn(usize) -> u64) -> Self {
        let r = RxRing { descs, n, cur: 0, buf_size };
        for i in 0..n {
            let d = descs.add(i);
            wr_addr(d, buf_phys(i));
        }
        r.give_all();
        r
    }

    fn eor(&self, idx: usize) -> u32 { if idx == self.n - 1 { DESC_RING_END } else { 0 } }

    /// Return every descriptor to the NIC and restart at index 0 (after a
    /// chip reset).
    pub fn give_all(&self) {
        unsafe {
            for i in 0..self.n {
                let d = self.descs.add(i);
                wr_opts2(d, 0);
                fence(Ordering::Release);
                wr_opts1(d, DESC_OWN | self.eor(i) | self.buf_size);
            }
        }
    }

    pub fn reset(&mut self) { self.cur = 0; self.give_all(); }

    /// Descriptors the NIC has filled and handed back (OWN clear) that the
    /// driver has not consumed yet: the RX watchdog's "is anybody draining".
    pub fn filled(&self) -> usize {
        (0..self.n).filter(|&i| unsafe { rd_opts1(self.descs.add(i)) } & DESC_OWN == 0).count()
    }

    /// Look at the next descriptor. Anything but `Empty` must be followed by
    /// `release(idx)` once the buffer has been consumed.
    pub fn next(&self) -> RxEvent {
        let idx = self.cur;
        let status = unsafe { rd_opts1(self.descs.add(idx)) };
        if status & DESC_OWN != 0 { return RxEvent::Empty; }
        // Read nothing else of the descriptor/buffer before seeing OWN clear.
        fence(Ordering::Acquire);
        if status & RX_RES != 0 { return RxEvent::Error { idx, status }; }
        if status & (DESC_FIRST | DESC_LAST) != (DESC_FIRST | DESC_LAST) {
            return RxEvent::Fragment { idx, status };
        }
        let raw = (status & RX_LEN_MASK) as usize;
        if raw < 4 { return RxEvent::Error { idx, status }; }
        RxEvent::Frame { idx, len: (raw - 4).min(self.buf_size as usize) }
    }

    /// Give descriptor `idx` (the one `next` returned) back to the NIC and
    /// advance.
    pub fn release(&mut self, idx: usize) {
        debug_assert_eq!(idx, self.cur);
        unsafe {
            let d = self.descs.add(idx);
            wr_opts2(d, 0);
            fence(Ordering::Release);
            wr_opts1(d, DESC_OWN | self.eor(idx) | self.buf_size);
        }
        self.cur = (self.cur + 1) % self.n;
    }
}

/// TX ring bookkeeping. `cur` and `dirty` are free-running counters: slots
/// `dirty..cur` are posted and not yet reclaimed.
pub struct TxRing {
    pub descs: *mut Desc,
    pub n: usize,
    pub cur: u64,
    pub dirty: u64,
}

unsafe impl Send for TxRing {}

impl TxRing {
    /// # Safety
    /// `descs` must be valid for `n` descriptors.
    pub unsafe fn init(descs: *mut Desc, n: usize, buf_phys: &dyn Fn(usize) -> u64) -> Self {
        for i in 0..n {
            let d = descs.add(i);
            wr_addr(d, buf_phys(i));
            wr_opts2(d, 0);
            wr_opts1(d, if i == n - 1 { DESC_RING_END } else { 0 });
        }
        TxRing { descs, n, cur: 0, dirty: 0 }
    }

    pub fn in_flight(&self) -> usize { (self.cur - self.dirty) as usize }

    /// The slot the next frame goes into, or None when the ring is full (one
    /// slot is always kept free).
    pub fn slot(&self) -> Option<usize> {
        if self.in_flight() >= self.n - 1 { None } else { Some((self.cur % self.n as u64) as usize) }
    }

    /// Hand the frame already copied into slot `idx`'s buffer (`len` bytes)
    /// to the NIC. The caller rings the doorbell.
    pub fn post(&mut self, idx: usize, len: usize) {
        debug_assert_eq!(Some(idx), self.slot());
        let eor = if idx == self.n - 1 { DESC_RING_END } else { 0 };
        unsafe {
            let d = self.descs.add(idx);
            wr_opts2(d, 0);
            fence(Ordering::Release);
            wr_opts1(d, DESC_OWN | eor | DESC_FIRST | DESC_LAST | (len as u32 & RX_LEN_MASK));
        }
        fence(Ordering::Release);
        self.cur += 1;
    }

    /// Reclaim the slots the NIC has finished with. Returns how many.
    pub fn reclaim(&mut self) -> usize {
        let mut k = 0;
        while self.dirty != self.cur {
            let idx = (self.dirty % self.n as u64) as usize;
            let st = unsafe { rd_opts1(self.descs.add(idx)) };
            if st & DESC_OWN != 0 { break; }
            self.dirty += 1;
            k += 1;
        }
        if k > 0 { fence(Ordering::Acquire); }
        k
    }

    /// Forget everything in flight (after a chip reset): clear OWN on every
    /// descriptor and empty the ring.
    pub fn reset(&mut self) {
        unsafe {
            for i in 0..self.n {
                let d = self.descs.add(i);
                wr_opts2(d, 0);
                wr_opts1(d, if i == self.n - 1 { DESC_RING_END } else { 0 });
            }
        }
        self.cur = 0;
        self.dirty = 0;
    }
}

// ── interrupt status/mask protocol ──────────────────────────────────────────
//
// The chip raises its interrupt while `IntrStatus & IntrMask` is non-zero,
// and an MSI is a message sent on the rising edge of that condition. Two
// consequences decide the whole protocol:
//
// * An enabled status bit that is still set when the handler returns keeps
//   the condition true, so no later event makes a new edge: the interrupt is
//   dead until something clears that bit. The handler therefore masks
//   everything first (IntrMask = 0, the condition drops whatever arrives
//   next) and acks exactly the bits it read (write-1-to-clear; a bit that
//   arrived after the read stays latched for the next look).
// * Re-enabling the mask while an enabled bit is pending makes a rising
//   edge, but the poll side does not depend on that: after unmasking it
//   re-reads IntrStatus and, if enabled work is pending, masks again, acks it
//   and polls once more (Linux's NAPI does the same with its re-check after
//   napi_complete_done). Bits outside the mask (TxOK, TxDescUnavail, and on
//   8168 parts RxDescUnavail / RxFIFOOver) are acked there too, so nothing
//   stays latched for minutes.

/// Interrupt status (W1C) and mask registers, as the protocol needs them.
pub trait IrqRegs {
    fn status(&mut self) -> u16;
    fn set_mask(&mut self, mask: u16);
    fn ack(&mut self, bits: u16);
}

/// The interrupt handler's register work. `Some(status)`: the interrupt was
/// ours; it is now masked (the caller must wake the poller, which re-enables
/// it) and exactly `status` was acked. `None`: nothing pending (or the device
/// is gone: all-ones), nothing touched.
pub fn irq_service<R: IrqRegs + ?Sized>(r: &mut R) -> Option<u16> {
    let st = r.status();
    if st == 0 || st == 0xffff { return None; }
    r.set_mask(0);
    r.ack(st);
    Some(st)
}

/// What `irq_rearm` found after unmasking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Rearm {
    /// Armed. `stale` = bits outside the mask that were latched and are now
    /// acked (0 when none).
    Armed { stale: u16 },
    /// Enabled events were pending at the unmask: the interrupt is masked
    /// again, those bits (all of `status`) are acked, poll once more.
    Pending { status: u16 },
}

/// End of a poll pass: enable `mask`, then re-read IntrStatus (the read also
/// flushes the posted mask write) and deal with what is latched.
pub fn irq_rearm<R: IrqRegs + ?Sized>(r: &mut R, mask: u16) -> Rearm {
    if mask != 0 { r.set_mask(mask); }
    let st = r.status();
    if st == 0 || st == 0xffff { return Rearm::Armed { stale: 0 }; }
    if st & mask != 0 {
        r.set_mask(0);
        r.ack(st);
        return Rearm::Pending { status: st };
    }
    r.ack(st);
    Rearm::Armed { stale: st }
}

// ── PHY firmware ("rtl_nic/*.fw") ───────────────────────────────────────────
//
// A small bytecode of 32-bit little-endian actions: opcode in bits 31:28,
// register in 27:16, data in 15:0. The file is either a bare action list or
// starts with a header {magic: u32 = 0, version: [u8; 32], start: u32,
// len: u32 (in actions), checksum: u8} whose bytes sum to 0 over the whole
// file.

pub const FW_PHY_READ: u32 = 0x0;
pub const FW_PHY_DATA_OR: u32 = 0x1;
pub const FW_PHY_DATA_AND: u32 = 0x2;
pub const FW_PHY_BJMPN: u32 = 0x3;
pub const FW_PHY_MDIO_CHG: u32 = 0x4;
pub const FW_PHY_CLEAR_READCOUNT: u32 = 0x7;
pub const FW_PHY_WRITE: u32 = 0x8;
pub const FW_PHY_READCOUNT_EQ_SKIP: u32 = 0x9;
pub const FW_PHY_COMP_EQ_SKIPN: u32 = 0xa;
pub const FW_PHY_COMP_NEQ_SKIPN: u32 = 0xb;
pub const FW_PHY_WRITE_PREVIOUS: u32 = 0xc;
pub const FW_PHY_SKIPN: u32 = 0xd;
pub const FW_PHY_DELAY_MS: u32 = 0xe;

/// A validated firmware program.
pub struct FwProgram {
    pub version: [u8; 32],
    pub version_len: usize,
    pub code: alloc::vec::Vec<u32>,
}

impl FwProgram {
    pub fn version_str(&self) -> &str {
        core::str::from_utf8(&self.version[..self.version_len]).unwrap_or("?")
    }
}

fn le32(b: &[u8], off: usize) -> Option<u32> {
    b.get(off..off + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Parse and validate a firmware file. `fallback_name` names a headerless file.
pub fn fw_parse(data: &[u8], fallback_name: &str) -> Result<FwProgram, &'static str> {
    const HDR: usize = 4 + 32 + 4 + 4 + 1;
    if data.len() < 4 { return Err("file too short"); }
    let mut version = [0u8; 32];
    let version_len;
    let code_bytes: &[u8];
    if le32(data, 0) == Some(0) {
        if data.len() < HDR { return Err("header truncated"); }
        let sum = data.iter().fold(0u8, |a, &b| a.wrapping_add(b));
        if sum != 0 { return Err("checksum mismatch"); }
        let start = le32(data, 36).unwrap() as usize;
        let len = le32(data, 40).unwrap() as usize;
        if start > data.len() { return Err("start beyond end of file"); }
        if len > (data.len() - start) / 4 { return Err("length beyond end of file"); }
        let v = &data[4..36];
        version_len = v.iter().position(|&b| b == 0).unwrap_or(32);
        version[..version_len].copy_from_slice(&v[..version_len]);
        code_bytes = &data[start..start + len * 4];
    } else {
        if data.len() % 4 != 0 { return Err("size not a multiple of 4"); }
        let n = fallback_name.len().min(32);
        version[..n].copy_from_slice(&fallback_name.as_bytes()[..n]);
        version_len = n;
        code_bytes = data;
    }
    let code: alloc::vec::Vec<u32> = code_bytes.chunks_exact(4)
        .map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).collect();
    fw_validate(&code)?;
    Ok(FwProgram { version, version_len, code })
}

/// Reject unknown opcodes and jumps out of the program.
pub fn fw_validate(code: &[u32]) -> Result<(), &'static str> {
    let size = code.len();
    for (index, &action) in code.iter().enumerate() {
        let val = action & 0xffff;
        let regno = ((action >> 16) & 0x0fff) as usize;
        match action >> 28 {
            FW_PHY_READ | FW_PHY_DATA_OR | FW_PHY_DATA_AND | FW_PHY_CLEAR_READCOUNT
            | FW_PHY_WRITE | FW_PHY_WRITE_PREVIOUS | FW_PHY_DELAY_MS => {}
            FW_PHY_MDIO_CHG => if val > 1 { return Err("bad MDIO_CHG"); },
            FW_PHY_BJMPN => if regno > index { return Err("backward jump out of range"); },
            FW_PHY_READCOUNT_EQ_SKIP => if index + 2 >= size { return Err("skip out of range"); },
            FW_PHY_COMP_EQ_SKIPN | FW_PHY_COMP_NEQ_SKIPN | FW_PHY_SKIPN =>
                if index + 1 + regno >= size { return Err("skip out of range"); },
            _ => return Err("unknown opcode"),
        }
    }
    Ok(())
}

/// What a firmware program drives: the PHY's MDIO registers, or (after
/// MDIO_CHG 1) the MAC's MCU registers.
pub trait FwTarget {
    fn phy_write(&mut self, reg: u32, val: u32);
    fn phy_read(&mut self, reg: u32) -> u32;
    fn mcu_write(&mut self, reg: u32, val: u32);
    fn mcu_read(&mut self, reg: u32) -> u32;
    fn delay_ms(&mut self, ms: u32);
}

/// Run a validated program. `max_steps` bounds a looping program (BJMPN can
/// loop until a register changes); returns false if the bound was hit.
pub fn fw_run(code: &[u32], t: &mut dyn FwTarget, max_steps: usize) -> bool {
    let mut predata: u32 = 0;
    let mut count: u32 = 0;
    let mut mcu = false;
    let mut index: usize = 0;
    let mut steps = 0usize;
    while index < code.len() {
        steps += 1;
        if steps > max_steps { return false; }
        let action = code[index];
        let data = action & 0xffff;
        let regno = (action >> 16) & 0x0fff;
        match action >> 28 {
            FW_PHY_READ => {
                predata = if mcu { t.mcu_read(regno) } else { t.phy_read(regno) };
                count += 1;
            }
            FW_PHY_DATA_OR => predata |= data,
            FW_PHY_DATA_AND => predata &= data,
            FW_PHY_BJMPN => {
                // index -= regno + 1, then the loop's +1: lands regno back.
                index -= regno as usize;
                continue;
            }
            FW_PHY_MDIO_CHG => mcu = data != 0,
            FW_PHY_CLEAR_READCOUNT => count = 0,
            FW_PHY_WRITE => if mcu { t.mcu_write(regno, data) } else { t.phy_write(regno, data) },
            FW_PHY_READCOUNT_EQ_SKIP => if count == data { index += 1; },
            FW_PHY_COMP_EQ_SKIPN => if predata == data { index += regno as usize; },
            FW_PHY_COMP_NEQ_SKIPN => if predata != data { index += regno as usize; },
            FW_PHY_WRITE_PREVIOUS => if mcu { t.mcu_write(regno, predata) } else { t.phy_write(regno, predata) },
            FW_PHY_SKIPN => index += regno as usize,
            FW_PHY_DELAY_MS => t.delay_ms(data),
            _ => {}
        }
        index += 1;
    }
    true
}

// ── link status ─────────────────────────────────────────────────────────────

/// PHYstatus register (0x6c) bits.
pub const PHYS_TBI: u8 = 0x80;
pub const PHYS_TXFLOW: u8 = 0x40;
pub const PHYS_RXFLOW: u8 = 0x20;
pub const PHYS_1000F: u8 = 0x10;
pub const PHYS_100: u8 = 0x08;
pub const PHYS_10: u8 = 0x04;
pub const PHYS_LINK: u8 = 0x02;
pub const PHYS_FULL: u8 = 0x01;

/// (link up, Mbit/s, full duplex) from a PHYstatus value.
pub fn decode_phystatus(v: u8) -> (bool, u32, bool) {
    let up = v & PHYS_LINK != 0;
    let speed = if v & PHYS_1000F != 0 { 1000 } else if v & PHYS_100 != 0 { 100 } else if v & PHYS_10 != 0 { 10 } else { 0 };
    let full = v & PHYS_FULL != 0 || v & PHYS_1000F != 0;
    (up, speed, full)
}

/// A MAC address the NIC can use: not all-zero, not multicast/broadcast.
pub fn mac_valid(m: &[u8; 6]) -> bool {
    m.iter().any(|&b| b != 0) && m[0] & 1 == 0
}
