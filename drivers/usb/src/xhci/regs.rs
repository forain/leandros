//! xHCI MMIO register map (xHCI 1.2 §5) and HVF-safe accessors.
//!
//! Every access is a single 32-bit load or store. 64-bit registers are
//! written low dword first, then high (the order QEMU's model and the spec's
//! 32-bit-access rule both expect; HVF on the Mac also aborts on some 64-bit
//! and all writeback-form MMIO accesses). On aarch64 the access is a plain
//! `ldr`/`str` from inline asm so LLVM can never pick a base-writeback
//! addressing form (ISV=0 → `hvf_handle_exception: Assertion isv failed`).

// ── Capability registers (offset from MMIO base) ─────────────────────────────
pub const CAP_CAPLENGTH:  usize = 0x00; // [7:0] CAPLENGTH, [31:16] HCIVERSION
pub const CAP_HCSPARAMS1: usize = 0x04;
pub const CAP_HCSPARAMS2: usize = 0x08;
pub const CAP_HCCPARAMS1: usize = 0x10;
pub const CAP_DBOFF:      usize = 0x14;
pub const CAP_RTSOFF:     usize = 0x18;

pub const HCCPARAMS1_AC64: u32 = 1 << 0;
pub const HCCPARAMS1_CSZ:  u32 = 1 << 2;
pub const HCCPARAMS1_PPC:  u32 = 1 << 3;

// ── Operational registers (offset from MMIO base + CAPLENGTH) ────────────────
pub const OP_USBCMD:   usize = 0x00;
pub const OP_USBSTS:   usize = 0x04;
pub const OP_PAGESIZE: usize = 0x08;
pub const OP_DNCTRL:   usize = 0x14;
pub const OP_CRCR:     usize = 0x18;
pub const OP_DCBAAP:   usize = 0x30;
pub const OP_CONFIG:   usize = 0x38;
pub const OP_PORTSC:   usize = 0x400; // port n at +0x10*(n-1)

pub const CMD_RUN:   u32 = 1 << 0;
pub const CMD_HCRST: u32 = 1 << 1;
pub const CMD_INTE:  u32 = 1 << 2;
pub const CMD_HSEE:  u32 = 1 << 3;

pub const STS_HCH:  u32 = 1 << 0;
pub const STS_HSE:  u32 = 1 << 2;
pub const STS_EINT: u32 = 1 << 3;
pub const STS_PCD:  u32 = 1 << 4;
pub const STS_CNR:  u32 = 1 << 11;
pub const STS_HCE:  u32 = 1 << 12;

pub const CRCR_RCS: u32 = 1 << 0;
pub const CRCR_CS:  u32 = 1 << 1;
pub const CRCR_CA:  u32 = 1 << 2;
pub const CRCR_CRR: u32 = 1 << 3;

// ── PORTSC ───────────────────────────────────────────────────────────────────
pub const PORTSC_CCS: u32 = 1 << 0;
pub const PORTSC_PED: u32 = 1 << 1;  // RW1CS: writing 1 DISABLES the port
pub const PORTSC_OCA: u32 = 1 << 3;
pub const PORTSC_PR:  u32 = 1 << 4;
pub const PORTSC_PLS_SHIFT: u32 = 5;
pub const PORTSC_PLS: u32 = 0xF << 5;
pub const PORTSC_PP:  u32 = 1 << 9;
pub const PORTSC_SPEED_SHIFT: u32 = 10;
pub const PORTSC_PIC: u32 = 3 << 14;
pub const PORTSC_LWS: u32 = 1 << 16;
pub const PORTSC_CSC: u32 = 1 << 17;
pub const PORTSC_PEC: u32 = 1 << 18;
pub const PORTSC_WRC: u32 = 1 << 19;
pub const PORTSC_OCC: u32 = 1 << 20;
pub const PORTSC_PRC: u32 = 1 << 21;
pub const PORTSC_PLC: u32 = 1 << 22;
pub const PORTSC_CEC: u32 = 1 << 23;
pub const PORTSC_WCE: u32 = 1 << 25;
pub const PORTSC_WDE: u32 = 1 << 26;
pub const PORTSC_WOE: u32 = 1 << 27;
pub const PORTSC_WPR: u32 = 1 << 31;

/// Bits that must be written back unchanged on any PORTSC write. Notably
/// NOT PED (RW1C — writing it back disables the port), NOT PR/WPR (would
/// start a reset), NOT LWS (would commit the stale PLS value as a link
/// state transition), and none of the RW1C change bits.
pub const PORTSC_PRESERVE: u32 = PORTSC_PP | PORTSC_PIC | PORTSC_WCE | PORTSC_WDE | PORTSC_WOE;
/// All RW1C change bits.
pub const PORTSC_CHANGE: u32 =
    PORTSC_CSC | PORTSC_PEC | PORTSC_WRC | PORTSC_OCC | PORTSC_PRC | PORTSC_PLC | PORTSC_CEC;

// ── Runtime registers: interrupter n at RTSOFF + 0x20 + 0x20*n ───────────────
pub const IR_IMAN:   usize = 0x00;
pub const IR_IMOD:   usize = 0x04;
pub const IR_ERSTSZ: usize = 0x08;
pub const IR_ERSTBA: usize = 0x10;
pub const IR_ERDP:   usize = 0x18;

pub const IMAN_IP: u32 = 1 << 0;
pub const ERDP_EHB: u32 = 1 << 3;

// ── Extended capabilities ────────────────────────────────────────────────────
pub const XECP_LEGACY: u8 = 1;
pub const XECP_PROTOCOL: u8 = 2;
pub const USBLEGSUP_BIOS_OWNED: u32 = 1 << 16;
pub const USBLEGSUP_OS_OWNED: u32 = 1 << 24;
/// USBLEGCTLSTS: keep only the RsvdP bits when disabling SMIs (Linux
/// `XHCI_LEGACY_DISABLE_SMI`), and write 1 to the three RW1C SMI status bits.
pub const USBLEGCTLSTS_KEEP: u32 = (0x7 << 1) | (0xff << 5) | (0x7 << 17);
pub const USBLEGCTLSTS_SMI_EVENTS: u32 = 0x7 << 29;

#[inline(always)]
pub unsafe fn mmio_r32(addr: usize) -> u32 {
    #[cfg(target_arch = "aarch64")]
    {
        let v: u32;
        core::arch::asm!("ldr {v:w}, [{p}]", v = out(reg) v, p = in(reg) addr,
                         options(nostack, readonly, preserves_flags));
        v
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        (addr as *const u32).read_volatile()
    }
}

#[inline(always)]
pub unsafe fn mmio_w32(addr: usize, v: u32) {
    #[cfg(target_arch = "aarch64")]
    {
        core::arch::asm!("str {v:w}, [{p}]", v = in(reg) v, p = in(reg) addr,
                         options(nostack, preserves_flags));
    }
    #[cfg(not(target_arch = "aarch64"))]
    {
        (addr as *mut u32).write_volatile(v)
    }
}

/// Resolved register-space bases of one controller.
#[derive(Clone, Copy)]
pub struct Regs {
    pub base: usize,
    pub len: usize,
    pub op: usize,
    pub rt: usize,
    pub db: usize,
}

impl Regs {
    /// # Safety
    /// `base..base+len` must be the controller's mapped (device-memory) BAR.
    pub unsafe fn new(base: usize, len: usize) -> Option<Self> {
        let cap = mmio_r32(base + CAP_CAPLENGTH);
        if cap == 0xFFFF_FFFF { return None; }
        let caplen = (cap & 0xFF) as usize;
        let db = (mmio_r32(base + CAP_DBOFF) & !0x3) as usize;
        let rt = (mmio_r32(base + CAP_RTSOFF) & !0x1F) as usize;
        if caplen < 0x20 || caplen + 0x400 > len || db >= len || rt + 0x40 > len {
            return None;
        }
        Some(Regs { base, len, op: base + caplen, rt: base + rt, db: base + db })
    }

    #[inline] pub fn cap(&self, off: usize) -> u32 { unsafe { mmio_r32(self.base + off) } }
    #[inline] pub fn op(&self, off: usize) -> u32 { unsafe { mmio_r32(self.op + off) } }
    #[inline] pub fn set_op(&self, off: usize, v: u32) { unsafe { mmio_w32(self.op + off, v) } }
    #[inline]
    pub fn set_op64(&self, off: usize, v: u64) {
        self.set_op(off, v as u32);
        self.set_op(off + 4, (v >> 32) as u32);
    }
    #[inline] pub fn ir(&self, off: usize) -> u32 { unsafe { mmio_r32(self.rt + 0x20 + off) } }
    #[inline] pub fn set_ir(&self, off: usize, v: u32) { unsafe { mmio_w32(self.rt + 0x20 + off, v) } }
    #[inline]
    pub fn set_ir64(&self, off: usize, v: u64) {
        self.set_ir(off, v as u32);
        self.set_ir(off + 4, (v >> 32) as u32);
    }
    #[inline]
    pub fn portsc(&self, port: u8) -> u32 { self.op(OP_PORTSC + (port as usize - 1) * 0x10) }
    #[inline]
    pub fn set_portsc(&self, port: u8, v: u32) { self.set_op(OP_PORTSC + (port as usize - 1) * 0x10, v) }
    /// Ring doorbell `slot` (0 = command ring) with `target` (DCI, or 0).
    #[inline]
    pub fn doorbell(&self, slot: u8, target: u32) {
        crate::dma::dma_wmb();
        unsafe { mmio_w32(self.db + slot as usize * 4, target) }
    }
    /// Raw dword at `off` from the MMIO base (extended capabilities).
    #[inline]
    pub fn raw(&self, off: usize) -> Option<u32> {
        if off + 4 > self.len { return None; }
        Some(unsafe { mmio_r32(self.base + off) })
    }
    #[inline]
    pub fn set_raw(&self, off: usize, v: u32) {
        if off + 4 <= self.len { unsafe { mmio_w32(self.base + off, v) } }
    }
}
