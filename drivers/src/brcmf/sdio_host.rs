//! SDIO host for the BCM2712's second SDHCI controller (`sdio2`, wired to the
//! CYW43455 Wi-Fi chip on the Raspberry Pi 5), implementing [`SdioBus`].
//!
//! ## Where every hardware number comes from (decoded `bcm2712-rpi-5-b.dtb`)
//!
//! ```text
//! sdio2: mmc@1100000 { compatible = "brcm,bcm2712-sdhci";
//!     reg = <0x10 0x1100000 0x00 0x260   0x10 0x1100400 0x00 0x200>;  // "host", "cfg"
//!     interrupts = <0x00 0x112 0x04>;   // GIC SPI 0x112, level — unused, this host polls
//!     clocks = <&clk_emmc2>;            // fixed-clock 0xbebc200 = 200 MHz
//!     bus-width = <4>; non-removable; vmmc-supply = <&wl_on_reg>;
//!     pinctrl-0 = <&sdio2_30_pins>;     // gpio30 clk (no pull), 31 cmd, 32..35 dat (pull-up), "sd2"
//!     wifi@1 { compatible = "brcm,bcm4329-fmac"; } };
//! wl_on_reg { compatible = "regulator-fixed"; gpio = <&gio 28 GPIO_ACTIVE_HIGH>;
//!     startup-delay-us = <150000>; enable-active-high; pinctrl-0 = <&wl_on_pins>; } // gpio28 "gpio"
//! gio: gpio@7d508500 { compatible = "brcm,brcmstb-gpio"; bank widths 32, 4 }  // soc ranges: +0x10_0000_0000
//! pinctrl: pinctrl@7d504100 { compatible = "brcm,bcm2712c0-pinctrl"; }
//! ```
//!
//! The host-side "cfg" window programming (force card presence, SD pin select)
//! follows Linux `sdhci-brcmstb.c` (`sdhci_brcmstb_cfginit_2712`,
//! `sdhci_bcm2712_set_clock`); the pin-mux and pad tables follow Linux
//! `pinctrl-bcm2712.c` (c0 and d0 tables differ; the stepping comes from the
//! chip id, `crate::bcm2712_id`, with the DT as fallback; an unknown stepping leaves the
//! mux untouched).
//!
//! ## Why this does not share `sdhci.rs`
//! The register protocol is the same SDHCI 3.0, but the root card's driver is a
//! memory-card state machine with private, spin-count based waits, and it must
//! not regress. This file carries its own small SDHCI core, generic over
//! [`HostIo`], so the CMD52/CMD53 logic runs unchanged against a mock register
//! file in `drivers/sdio-tests`. `sdhci.rs` is not modified.

use super::bus::{BusError, SdioBus, FUNC_BACKPLANE, FUNC_BUS};

// ── Register-level access the core needs ─────────────────────────────────────

/// Register file plus a microsecond clock: the only things the SDIO core
/// touches, so tests can substitute a simulated controller.
pub trait HostIo {
    fn r8(&mut self, off: usize) -> u8;
    fn r16(&mut self, off: usize) -> u16;
    fn r32(&mut self, off: usize) -> u32;
    fn w8(&mut self, off: usize, v: u8);
    fn w16(&mut self, off: usize, v: u16);
    fn w32(&mut self, off: usize, v: u32);
    fn now_us(&mut self) -> u64;
    fn delay_us(&mut self, us: u32);
}

// ── SDHCI 3.0 registers ─────────────────────────────────────────────────────
pub const REG_BLOCK_SIZE: usize = 0x04;
pub const REG_BLOCK_COUNT: usize = 0x06;
pub const REG_ARGUMENT1: usize = 0x08;
pub const REG_TRANSFER_MODE: usize = 0x0C;
pub const REG_COMMAND: usize = 0x0E;
pub const REG_RESPONSE0: usize = 0x10;
pub const REG_BUFFER_DATA: usize = 0x20;
pub const REG_PRESENT_STATE: usize = 0x24;
pub const REG_HOST_CONTROL1: usize = 0x28;
pub const REG_POWER_CONTROL: usize = 0x29;
pub const REG_CLOCK_CONTROL: usize = 0x2C;
pub const REG_TIMEOUT_CONTROL: usize = 0x2E;
pub const REG_SOFTWARE_RESET: usize = 0x2F;
pub const REG_NORMAL_INT_STATUS: usize = 0x30;
pub const REG_ERROR_INT_STATUS: usize = 0x32;
pub const REG_NORMAL_INT_ENABLE: usize = 0x34;
pub const REG_ERROR_INT_ENABLE: usize = 0x36;
pub const REG_NORMAL_SIG_ENABLE: usize = 0x38;
pub const REG_ERROR_SIG_ENABLE: usize = 0x3A;
pub const REG_CAPABILITIES: usize = 0x40;
pub const REG_HOST_VERSION: usize = 0xFE;

pub const PSTATE_CMD_INHIBIT: u32 = 1 << 0;
pub const PSTATE_DAT_INHIBIT: u32 = 1 << 1;
/// DAT[1] line level. Low while the bus is idle = the card is asserting its interrupt.
pub const PSTATE_DAT1_LEVEL: u32 = 1 << 21;

pub const NI_CMD_COMPLETE: u16 = 1 << 0;
pub const NI_XFER_COMPLETE: u16 = 1 << 1;
pub const NI_BUF_WRITE_READY: u16 = 1 << 4;
pub const NI_BUF_READ_READY: u16 = 1 << 5;
pub const NI_CARD_INT: u16 = 1 << 8;
pub const NI_ERROR: u16 = 1 << 15;

pub const EI_CMD_TIMEOUT: u16 = 1 << 0;
pub const EI_CMD_CRC: u16 = 1 << 1;
pub const EI_DATA_TIMEOUT: u16 = 1 << 4;
pub const EI_DATA_CRC: u16 = 1 << 5;

pub const SWRST_ALL: u8 = 1 << 0;
pub const SWRST_CMD: u8 = 1 << 1;
pub const SWRST_DAT: u8 = 1 << 2;

pub const HC1_DATA_WIDTH_4: u8 = 1 << 1;
pub const HC1_HIGH_SPEED: u8 = 1 << 2;

pub const CLK_INTERNAL_EN: u16 = 1 << 0;
pub const CLK_STABLE: u16 = 1 << 1;
pub const CLK_SD_EN: u16 = 1 << 2;

pub const RESP_NONE: u16 = 0b00;
pub const RESP_48: u16 = 0b10;
pub const CMD_CRC_CHECK: u16 = 1 << 3;
pub const CMD_IDX_CHECK: u16 = 1 << 4;
pub const CMD_DATA_PRESENT: u16 = 1 << 5;

pub const TM_BLOCK_COUNT_EN: u16 = 1 << 1;
pub const TM_DIR_READ: u16 = 1 << 4;
pub const TM_MULTI_BLOCK: u16 = 1 << 5;

// ── SDIO protocol constants ─────────────────────────────────────────────────
pub const CCCR_SDIO_REV: u32 = 0x00;
pub const CCCR_IOEX: u32 = 0x02;
pub const CCCR_IORX: u32 = 0x03;
pub const CCCR_IENX: u32 = 0x04;
pub const CCCR_BUS_IF: u32 = 0x07;
pub const CCCR_CAPS: u32 = 0x08;
pub const CCCR_CIS_PTR: u32 = 0x09;
pub const CCCR_SPEED: u32 = 0x13;
/// Card capability: Supports Multi-Block (CMD53 block mode).
pub const CAPS_SMB: u8 = 1 << 1;
/// Card capability: Low-Speed card (then 4-bit needs 4BLS).
pub const CAPS_LSC: u8 = 1 << 6;
pub const CAPS_4BLS: u8 = 1 << 7;
pub const SPEED_SHS: u8 = 1 << 0;
pub const SPEED_EHS: u8 = 1 << 1;
/// Bus Interface Control: 4-bit width, card-detect pull-up disconnected (Linux `sdio_disable_cd`).
pub const BUS_IF_4BIT: u8 = 0x02;
pub const BUS_IF_CD_DISABLE: u8 = 0x80;
/// R5 response flag bits that mean the command failed.
pub const R5_ERR_MASK: u8 = 0xCB; // COM_CRC | ILLEGAL_CMD | ERROR | FUNC_NUM | OUT_OF_RANGE
pub const R5_COM_CRC: u8 = 0x80;

/// Most blocks one CMD53 can carry (9-bit count; 0 would mean "infinite").
pub const MAX_BLOCKS_PER_CMD53: usize = 511;
/// Byte mode carries at most 512 bytes (count 0 encodes 512).
pub const MAX_BYTE_MODE: usize = 512;

/// Broadcom F1 backplane window (brcmfmac `sdio.h`).
pub const SBSDIO_FUNC1_SBADDRLOW: u32 = 0x1000A;
pub const SBSDIO_FUNC1_SBADDRMID: u32 = 0x1000B;
pub const SBSDIO_FUNC1_SBADDRHIGH: u32 = 0x1000C;
pub const SBSDIO_FUNC1_CHIPCLKCSR: u32 = 0x1000E;
pub const SBSDIO_SB_ACCESS_2_4B_FLAG: u32 = 0x8000;
pub const SBSDIO_SB_OFT_ADDR_MASK: u32 = 0x7FFF;
pub const SI_ENUM_BASE: u32 = 0x1800_0000;
pub const CLKCSR_ALP_AVAIL_REQ: u8 = 0x08;
pub const CLKCSR_FORCE_HW_CLKREQ_OFF: u8 = 0x20;
pub const CLKCSR_FORCE_ALP: u8 = 0x01;
pub const CLKCSR_ALP_AVAIL: u8 = 0x40;

/// Command timeout for a single command or one data phase.
const CMD_TIMEOUT_US: u64 = 50_000;
/// Fallback base clock (dts `clk_emmc2`) when CAPABILITIES[15:8] reads 0.
pub const BASE_CLOCK_FALLBACK_KHZ: u32 = 200_000;

/// CMD52 argument: R/W, function, RAW (unused), 17-bit register address, data.
pub fn cmd52_arg(write: bool, func: u8, addr: u32, data: u8) -> u32 {
    ((write as u32) << 31) | (((func & 7) as u32) << 28) | ((addr & 0x1_FFFF) << 9) | data as u32
}

/// CMD53 argument. `count` is blocks in block mode, bytes in byte mode (512 encodes as 0).
pub fn cmd53_arg(write: bool, func: u8, addr: u32, block: bool, incr: bool, count: usize) -> u32 {
    ((write as u32) << 31)
        | (((func & 7) as u32) << 28)
        | ((block as u32) << 27)
        | ((incr as u32) << 26)
        | ((addr & 0x1_FFFF) << 9)
        | (count as u32 & 0x1FF)
}

/// One CMD53 the splitter decided on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Chunk {
    /// Card address of this chunk.
    pub addr: u32,
    /// Offset into the caller's buffer.
    pub off: usize,
    /// True = block mode (`count` blocks of `blksz`), false = byte mode (`count` bytes).
    pub block: bool,
    pub count: usize,
}

impl Chunk {
    pub fn bytes(&self, blksz: usize) -> usize { if self.block { self.count * blksz } else { self.count } }
}

/// Split a transfer of `len` bytes into CMD53s, the way Linux's
/// `sdio_io_rw_ext_helper` does: whole blocks in block mode (≤ 511 per command)
/// when the card supports multi-block and the transfer is longer than the
/// biggest byte-mode transfer, then the remainder in byte mode, never more
/// than `min(blksz, 512)` bytes per byte-mode command.
///
/// Calls `f` for each chunk in order; stops at the first error.
pub fn split_cmd53<E>(
    addr: u32, incr: bool, len: usize, blksz: usize, multi_block: bool,
    f: impl FnMut(Chunk) -> Result<(), E>,
) -> Result<(), E> {
    split_cmd53_capped(addr, incr, len, blksz, multi_block, 0, f)
}

/// `split_cmd53` with at most `cap` bytes per CMD53 (0 = no cap; a cap below
/// one block turns block mode off).
pub fn split_cmd53_capped<E>(
    addr: u32, incr: bool, len: usize, blksz: usize, multi_block: bool, cap: usize,
    mut f: impl FnMut(Chunk) -> Result<(), E>,
) -> Result<(), E> {
    let mut max_byte = if blksz == 0 { MAX_BYTE_MODE } else { blksz.min(MAX_BYTE_MODE) };
    let mut max_blocks = MAX_BLOCKS_PER_CMD53;
    let mut multi_block = multi_block;
    if cap != 0 {
        max_byte = max_byte.min(cap);
        if blksz == 0 || cap < blksz { multi_block = false; } else { max_blocks = max_blocks.min(cap / blksz); }
    }
    let mut off = 0usize;
    let mut left = len;
    let at = |off: usize| if incr { addr.wrapping_add(off as u32) } else { addr };
    if multi_block && blksz > 0 && left > max_byte {
        while left >= blksz {
            let blocks = (left / blksz).min(max_blocks);
            f(Chunk { addr: at(off), off, block: true, count: blocks })?;
            off += blocks * blksz;
            left -= blocks * blksz;
        }
    }
    while left > 0 {
        let n = left.min(max_byte);
        f(Chunk { addr: at(off), off, block: false, count: n })?;
        off += n;
        left -= n;
    }
    Ok(())
}

/// Encode SDCLK Frequency Select (SDHCI 3.0 10-bit divider or 2.0 power of two),
/// the fastest setting not above `target_khz`. Same arithmetic as `sdhci.rs`.
pub fn clock_divider(base_khz: u32, target_khz: u32, v3: bool) -> (u16, u32) {
    if base_khz == 0 || target_khz == 0 {
        return (0xFF << 8, 0);
    }
    if v3 {
        let n = if base_khz <= target_khz { 0 } else { ((base_khz + 2 * target_khz - 1) / (2 * target_khz)).min(1023) };
        let actual = if n == 0 { base_khz } else { base_khz / (2 * n) };
        let n = n as u16;
        ((n & 0xFF) << 8 | ((n >> 8) & 0x03) << 6, actual)
    } else {
        let mut d: u32 = 1;
        while d < 256 && base_khz / d > target_khz { d <<= 1; }
        (((d as u16) >> 1) << 8, base_khz / d)
    }
}

/// What card init found, for the boot line and for brcmf.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CardInfo {
    pub ocr: u32,
    pub nfuncs: u8,
    pub rca: u16,
    pub cccr_rev: u8,
    pub caps: u8,
    pub manf: u16,
    pub card: u16,
    pub clk_khz: u32,
    pub width4: bool,
    pub high_speed: bool,
}

/// Which init step failed, with the bus error behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitError {
    pub stage: &'static str,
    pub err: BusError,
}

fn fail<T>(stage: &'static str, err: BusError) -> Result<T, InitError> { Err(InitError { stage, err }) }

pub struct SdioHost<H: HostIo> {
    pub io: H,
    pub rca: u16,
    /// Per-function block size as programmed (0 = never set).
    pub blksz: [u16; 8],
    pub multi_block: bool,
    base_khz: u32,
    v3: bool,
    /// Absolute `now_us` after which every wait gives up (boot probe budget).
    pub deadline_us: Option<u64>,
    /// `(NORMAL_INT_STATUS << 16) | ERROR_INT_STATUS` at the last failure.
    pub last_fail: u32,
    pub card_irq_enabled: bool,
    /// Runtime bring-up knobs (`brcmfctl retry sdclk= hs= blk= blksz= chunk=`).
    pub tune: SdioTune,
}

/// Bus knobs the brcmf retry path can vary without a new kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SdioTune {
    /// SD clock target in kHz (capped at 25 MHz without high speed).
    pub sdclk_khz: u32,
    /// Try SDIO high-speed (EHS + host HS bit).
    pub hs: bool,
    /// Block-mode CMD53 (false: byte mode only).
    pub blk: bool,
    /// F1 block size override (0: whatever the driver asks, 64).
    pub f1_blksz: u16,
    /// Max bytes per CMD53 (0: no cap).
    pub chunk: u32,
}

impl Default for SdioTune {
    fn default() -> Self { SdioTune { sdclk_khz: 50_000, hs: true, blk: true, f1_blksz: 0, chunk: 0 } }
}

impl SdioTune {
    /// One `k=v` retry knob; false if the key is not a bus knob or the value is bad.
    pub fn set(&mut self, key: &str, val: &str) -> bool {
        let Ok(n) = val.parse::<u32>() else { return false };
        match key {
            "sdclk" if (400..=50_000).contains(&n) => self.sdclk_khz = n,
            "hs" => self.hs = n != 0,
            "blk" => self.blk = n != 0,
            "blksz" if n == 0 || (4..=2048).contains(&n) && n % 4 == 0 => self.f1_blksz = n as u16,
            "chunk" if n == 0 || (4..=0x8000).contains(&n) => self.chunk = n,
            _ => return false,
        }
        true
    }
}

impl<H: HostIo> SdioHost<H> {
    pub fn new(io: H) -> Self {
        SdioHost { io, rca: 0, blksz: [0; 8], multi_block: false, base_khz: 0, v3: true,
                   deadline_us: None, last_fail: 0, card_irq_enabled: false,
                   tune: SdioTune::default() }
    }

    fn expired(&mut self, start: u64, timeout_us: u64) -> bool {
        let now = self.io.now_us();
        now.saturating_sub(start) > timeout_us || self.deadline_us.map_or(false, |d| now > d)
    }

    /// Bounded wait for a register condition.
    fn wait_until(&mut self, timeout_us: u64, mut cond: impl FnMut(&mut H) -> bool) -> bool {
        let start = self.io.now_us();
        loop {
            if cond(&mut self.io) { return true; }
            if self.expired(start, timeout_us) { return false; }
            core::hint::spin_loop();
        }
    }

    /// SDHCI §3.10 error recovery: reset CMD and DAT circuits, clear status.
    pub fn reset_cmd_dat(&mut self) {
        self.io.w8(REG_SOFTWARE_RESET, SWRST_CMD | SWRST_DAT);
        // Not subject to the probe deadline: recovery must always run to completion or 10 ms.
        let start = self.io.now_us();
        while self.io.r8(REG_SOFTWARE_RESET) & (SWRST_CMD | SWRST_DAT) != 0 {
            if self.io.now_us().saturating_sub(start) > 10_000 { break; }
        }
        self.io.w16(REG_NORMAL_INT_STATUS, 0xFFFF);
        self.io.w16(REG_ERROR_INT_STATUS, 0xFFFF);
    }

    fn map_err(eis: u16) -> BusError {
        if eis & (EI_CMD_TIMEOUT | EI_DATA_TIMEOUT) != 0 { BusError::Timeout }
        else if eis & (EI_CMD_CRC | EI_DATA_CRC) != 0 { BusError::Crc }
        else { BusError::Io }
    }

    /// Wait until all of `bits` are set in NORMAL_INT_STATUS; clears them.
    fn wait_int(&mut self, bits: u16) -> Result<(), BusError> {
        let start = self.io.now_us();
        loop {
            let st = self.io.r16(REG_NORMAL_INT_STATUS);
            if st & NI_ERROR != 0 {
                let eis = self.io.r16(REG_ERROR_INT_STATUS);
                self.last_fail = ((st as u32) << 16) | eis as u32;
                self.reset_cmd_dat();
                return Err(Self::map_err(eis));
            }
            if st & bits == bits {
                self.io.w16(REG_NORMAL_INT_STATUS, bits);
                return Ok(());
            }
            if self.expired(start, CMD_TIMEOUT_US) {
                let eis = self.io.r16(REG_ERROR_INT_STATUS);
                self.last_fail = ((st as u32) << 16) | eis as u32;
                self.reset_cmd_dat();
                return Err(BusError::Timeout);
            }
            core::hint::spin_loop();
        }
    }

    /// Issue a command; returns RESPONSE0. `flags` = response type | CRC/IDX/DATA bits.
    pub fn command(&mut self, idx: u8, arg: u32, flags: u16) -> Result<u32, BusError> {
        let mask = if flags & CMD_DATA_PRESENT != 0 { PSTATE_CMD_INHIBIT | PSTATE_DAT_INHIBIT } else { PSTATE_CMD_INHIBIT };
        if !self.wait_until(CMD_TIMEOUT_US, |io| io.r32(REG_PRESENT_STATE) & mask == 0) {
            self.reset_cmd_dat();
            if self.io.r32(REG_PRESENT_STATE) & mask != 0 { return Err(BusError::Timeout); }
        }
        // Stale completion bits would satisfy the next wait falsely (see sdhci.rs).
        self.io.w16(REG_NORMAL_INT_STATUS, !NI_CARD_INT);
        self.io.w16(REG_ERROR_INT_STATUS, 0xFFFF);
        self.io.w32(REG_ARGUMENT1, arg);
        self.io.w16(REG_COMMAND, ((idx as u16) << 8) | flags);
        self.wait_int(NI_CMD_COMPLETE)?;
        Ok(self.io.r32(REG_RESPONSE0))
    }

    fn check_r5(&mut self, resp: u32) -> Result<u8, BusError> {
        let flags = (resp >> 8) as u8;
        if flags & R5_ERR_MASK != 0 {
            self.last_fail = resp;
            return Err(if flags & R5_COM_CRC != 0 { BusError::Crc } else { BusError::Io });
        }
        Ok(resp as u8)
    }

    pub fn cmd52(&mut self, write: bool, func: u8, addr: u32, data: u8) -> Result<u8, BusError> {
        let r = self.command(52, cmd52_arg(write, func, addr, data), RESP_48 | CMD_CRC_CHECK | CMD_IDX_CHECK)?;
        self.check_r5(r)
    }

    /// One CMD53 by PIO. `buf.len()` must equal the chunk's byte length.
    fn cmd53_pio(&mut self, write: bool, func: u8, c: Chunk, incr: bool, blksz: usize, buf: *mut u8, len: usize)
        -> Result<(), BusError>
    {
        let (bsz, bcnt) = if c.block { (blksz, c.count) } else { (c.count, 1) };
        // [14:12] SDMA boundary 512 KiB (unused by PIO), [11:0] transfer block size.
        self.io.w16(REG_BLOCK_SIZE, (7 << 12) | (bsz as u16 & 0xFFF));
        self.io.w16(REG_BLOCK_COUNT, bcnt as u16);
        let mut tm = TM_BLOCK_COUNT_EN;
        if !write { tm |= TM_DIR_READ; }
        if bcnt > 1 { tm |= TM_MULTI_BLOCK; }
        self.io.w16(REG_TRANSFER_MODE, tm);
        let count = if c.block { c.count } else { c.count & 0x1FF };
        let r = self.command(53, cmd53_arg(write, func, c.addr, c.block, incr, count),
                             RESP_48 | CMD_CRC_CHECK | CMD_IDX_CHECK | CMD_DATA_PRESENT)?;
        if let Err(e) = self.check_r5(r) { self.reset_cmd_dat(); return Err(e); }
        let ready = if write { NI_BUF_WRITE_READY } else { NI_BUF_READ_READY };
        let mut done = 0usize;
        for _ in 0..bcnt {
            self.wait_int(ready)?;
            let end = (done + bsz).min(len);
            while done < end {
                let n = (end - done).min(4);
                if write {
                    let mut w = [0u8; 4];
                    for i in 0..n { w[i] = unsafe { *buf.add(done + i) }; }
                    self.io.w32(REG_BUFFER_DATA, u32::from_le_bytes(w));
                } else {
                    let w = self.io.r32(REG_BUFFER_DATA).to_le_bytes();
                    for i in 0..n { unsafe { *buf.add(done + i) = w[i]; } }
                }
                done += n;
            }
        }
        self.wait_int(NI_XFER_COMPLETE)
    }

    fn rw_ext(&mut self, write: bool, func: u8, addr: u32, incr: bool, buf: *mut u8, len: usize)
        -> Result<(), BusError>
    {
        if func > 7 { return Err(BusError::Io); }
        let blksz = self.blksz[func as usize] as usize;
        let mb = self.multi_block && self.tune.blk;
        let cap = self.tune.chunk as usize;
        split_cmd53_capped(addr, incr, len, blksz, mb, cap, |c| {
            let n = c.bytes(blksz);
            self.cmd53_pio(write, func, c, incr, blksz, unsafe { buf.add(c.off) }, n)
        })
    }

    /// Program SDCLK and Host Control 1 (clock off around the change, SDHCI §3.2.3).
    pub fn set_bus(&mut self, target_khz: u32, width4: bool, hs: bool) -> u32 {
        let (sel, actual) = clock_divider(self.base_khz, target_khz, self.v3);
        let cc = self.io.r16(REG_CLOCK_CONTROL);
        self.io.w16(REG_CLOCK_CONTROL, cc & !CLK_SD_EN);
        let hc = self.io.r8(REG_HOST_CONTROL1) & !(HC1_DATA_WIDTH_4 | HC1_HIGH_SPEED);
        let hc = hc | if width4 { HC1_DATA_WIDTH_4 } else { 0 } | if hs { HC1_HIGH_SPEED } else { 0 };
        self.io.w8(REG_HOST_CONTROL1, hc);
        self.io.w16(REG_CLOCK_CONTROL, sel | CLK_INTERNAL_EN);
        let start = self.io.now_us();
        while self.io.r16(REG_CLOCK_CONTROL) & CLK_STABLE == 0 {
            if self.io.now_us().saturating_sub(start) > 20_000 { break; }
        }
        let cc = self.io.r16(REG_CLOCK_CONTROL);
        self.io.w16(REG_CLOCK_CONTROL, cc | CLK_SD_EN);
        self.io.delay_us(100);
        actual
    }

    /// Controller reset, interrupt status enables, 3.3 V bus power, identification clock.
    pub fn host_init(&mut self) -> Result<(), InitError> {
        let ver = self.io.r16(REG_HOST_VERSION);
        if ver == 0xFFFF { return fail("hostver", BusError::NoCard); }
        self.v3 = (ver & 0xFF) >= 2;
        self.io.w8(REG_SOFTWARE_RESET, SWRST_ALL);
        if !self.wait_until(20_000, |io| io.r8(REG_SOFTWARE_RESET) & SWRST_ALL == 0) {
            return fail("reset", BusError::Timeout);
        }
        let caps = self.io.r32(REG_CAPABILITIES);
        let mhz = if self.v3 { (caps >> 8) & 0xFF } else { (caps >> 8) & 0x3F };
        self.base_khz = if mhz == 0 { BASE_CLOCK_FALLBACK_KHZ } else { mhz * 1000 };
        // Status enables: everything except card interrupt (armed by wait_card_irq).
        self.io.w16(REG_NORMAL_INT_ENABLE, 0xFFFF & !NI_CARD_INT);
        self.io.w16(REG_ERROR_INT_ENABLE, 0xFFFF);
        // Polled host: no signal (GIC) enables.
        self.io.w16(REG_NORMAL_SIG_ENABLE, 0);
        self.io.w16(REG_ERROR_SIG_ENABLE, 0);
        self.io.w8(REG_TIMEOUT_CONTROL, 0x0E);
        // 3.3 V select first, then bus power on.
        self.io.w8(REG_POWER_CONTROL, 0x0E);
        self.io.w8(REG_POWER_CONTROL, 0x0F);
        self.io.delay_us(1_000);
        self.set_bus(400, false, false);
        // ≥ 74 identification clocks (185 µs at 400 kHz) before CMD0; be generous.
        self.io.delay_us(1_000);
        Ok(())
    }

    /// Card identification and configuration: CMD0, CMD5, CMD3, CMD7, CCCR,
    /// CIS manufacturer ID, 4-bit bus, high speed, F1 enabled with 64-byte blocks.
    pub fn init_card(&mut self, want_hs: bool) -> Result<CardInfo, InitError> {
        let mut ci = CardInfo::default();
        let _ = self.command(0, 0, RESP_NONE); // GO_IDLE; SDIO-only cards ignore it
        self.io.delay_us(1_000);
        // CMD5 inquiry (arg 0): R4 has no valid CRC/index.
        let r = match self.command(5, 0, RESP_48) { Ok(r) => r, Err(e) => return fail("cmd5", e) };
        ci.ocr = r & 0x00FF_FFFF;
        ci.nfuncs = ((r >> 28) & 7) as u8;
        // Pick the 3.2–3.4 V window (the bus is 3.3 V); any 2.7–3.6 V bit as fallback.
        let mut want = ci.ocr & 0x0030_0000;
        if want == 0 { want = ci.ocr & 0x00FF_8000; }
        if want == 0 { return fail("ocr", BusError::Io); }
        let start = self.io.now_us();
        loop {
            match self.command(5, want, RESP_48) {
                Ok(r) if r & (1 << 31) != 0 => { ci.ocr = r & 0x00FF_FFFF; break; }
                Ok(_) => {}
                Err(e) => return fail("cmd5", e),
            }
            if self.expired(start, 1_000_000) { return fail("cmd5-busy", BusError::Timeout); }
            self.io.delay_us(10_000);
        }
        let r = match self.command(3, 0, RESP_48 | CMD_CRC_CHECK | CMD_IDX_CHECK) { Ok(r) => r, Err(e) => return fail("cmd3", e) };
        self.rca = (r >> 16) as u16;
        ci.rca = self.rca;
        if let Err(e) = self.command(7, (self.rca as u32) << 16, RESP_48 | CMD_CRC_CHECK | CMD_IDX_CHECK) {
            return fail("cmd7", e);
        }
        let rd = |s: &mut Self, a: u32, st: &'static str| s.cmd52(false, FUNC_BUS, a, 0).map_err(|e| InitError { stage: st, err: e });
        ci.cccr_rev = rd(self, CCCR_SDIO_REV, "cccr")?;
        ci.caps = rd(self, CCCR_CAPS, "cccr")?;
        self.multi_block = ci.caps & CAPS_SMB != 0;
        let cis = rd(self, CCCR_CIS_PTR, "cis")? as u32
            | (rd(self, CCCR_CIS_PTR + 1, "cis")? as u32) << 8
            | (rd(self, CCCR_CIS_PTR + 2, "cis")? as u32) << 16;
        if let Some((m, c)) = self.find_manfid(cis) { ci.manf = m; ci.card = c; }
        // 4-bit bus unless this is a low-speed card without 4BLS.
        if ci.caps & CAPS_LSC == 0 || ci.caps & CAPS_4BLS != 0 {
            let v = rd(self, CCCR_BUS_IF, "busif")?;
            if self.cmd52(true, FUNC_BUS, CCCR_BUS_IF, (v & !0x03) | BUS_IF_4BIT | BUS_IF_CD_DISABLE).is_ok() {
                ci.width4 = true;
            }
        }
        let mut hs = false;
        if want_hs {
            if let Ok(sp) = self.cmd52(false, FUNC_BUS, CCCR_SPEED, 0) {
                if sp & SPEED_SHS != 0 {
                    hs = self.cmd52(true, FUNC_BUS, CCCR_SPEED, sp | SPEED_EHS).map_or(false, |r| r & SPEED_EHS != 0);
                }
            }
        }
        let want_khz = self.tune.sdclk_khz.min(if hs { 50_000 } else { 25_000 });
        ci.clk_khz = self.set_bus(want_khz, ci.width4, hs);
        // Prove the faster bus; retreat to 25 MHz default speed if it does not hold.
        if self.cmd52(false, FUNC_BUS, CCCR_SDIO_REV, 0) != Ok(ci.cccr_rev) {
            self.reset_cmd_dat();
            ci.clk_khz = self.set_bus(want_khz.min(25_000), ci.width4, false);
            if hs { let _ = self.cmd52(true, FUNC_BUS, CCCR_SPEED, 0); }
            hs = false;
            if self.cmd52(false, FUNC_BUS, CCCR_SDIO_REV, 0) != Ok(ci.cccr_rev) {
                return fail("busverify", BusError::Io);
            }
        }
        ci.high_speed = hs;
        self.enable_func(FUNC_BACKPLANE).map_err(|e| InitError { stage: "f1-enable", err: e })?;
        SdioBus::set_block_size(self, FUNC_BACKPLANE, 64).map_err(|e| InitError { stage: "f1-blksz", err: e })?;
        Ok(ci)
    }

    /// Set IOEx for `func` and wait (≤ 100 ms) for IORx.
    pub fn enable_func(&mut self, func: u8) -> Result<(), BusError> {
        let bit = 1u8 << func;
        let v = self.cmd52(false, FUNC_BUS, CCCR_IOEX, 0)?;
        self.cmd52(true, FUNC_BUS, CCCR_IOEX, v | bit)?;
        let start = self.io.now_us();
        loop {
            if self.cmd52(false, FUNC_BUS, CCCR_IORX, 0)? & bit != 0 { return Ok(()); }
            if self.expired(start, 100_000) { return Err(BusError::Timeout); }
            self.io.delay_us(1_000);
        }
    }

    /// Walk the common CIS for CISTPL_MANFID (0x20). Bounded to 512 bytes.
    pub fn find_manfid(&mut self, cis: u32) -> Option<(u16, u16)> {
        if cis == 0 { return None; }
        let mut p = cis;
        let end = cis + 512;
        while p < end {
            let code = self.cmd52(false, FUNC_BUS, p, 0).ok()?;
            if code == 0xFF { return None; }
            if code == 0x00 { p += 1; continue; }
            let link = self.cmd52(false, FUNC_BUS, p + 1, 0).ok()?;
            if code == 0x20 && link >= 4 {
                let mut b = [0u8; 4];
                for i in 0..4 { b[i] = self.cmd52(false, FUNC_BUS, p + 2 + i as u32, 0).ok()?; }
                return Some((u16::from_le_bytes([b[0], b[1]]), u16::from_le_bytes([b[2], b[3]])));
            }
            if link == 0xFF { return None; }
            p += 2 + link as u32;
        }
        None
    }
}

impl<H: HostIo> SdioBus for SdioHost<H> {
    fn read8(&mut self, func: u8, addr: u32) -> Result<u8, BusError> { self.cmd52(false, func, addr, 0) }
    fn write8(&mut self, func: u8, addr: u32, val: u8) -> Result<(), BusError> {
        self.cmd52(true, func, addr, val).map(|_| ())
    }
    fn read_bytes(&mut self, func: u8, addr: u32, incr: bool, buf: &mut [u8]) -> Result<(), BusError> {
        self.rw_ext(false, func, addr, incr, buf.as_mut_ptr(), buf.len())
    }
    fn write_bytes(&mut self, func: u8, addr: u32, incr: bool, buf: &[u8]) -> Result<(), BusError> {
        // Writes only read from the pointer.
        self.rw_ext(true, func, addr, incr, buf.as_ptr() as *mut u8, buf.len())
    }
    fn set_block_size(&mut self, func: u8, size: u16) -> Result<(), BusError> {
        if func > 7 || size == 0 || size > 2048 { return Err(BusError::Io); }
        let size = if func == FUNC_BACKPLANE && self.tune.f1_blksz != 0 { self.tune.f1_blksz } else { size };
        let fbr = (func as u32) * 0x100; // F0 → CCCR 0x10/0x11, Fn → FBR n 0x10/0x11
        self.cmd52(true, FUNC_BUS, fbr + 0x10, size as u8)?;
        self.cmd52(true, FUNC_BUS, fbr + 0x11, (size >> 8) as u8)?;
        self.blksz[func as usize] = size;
        Ok(())
    }
    fn wait_card_irq(&mut self, timeout_us: u32) -> bool {
        // The Card Interrupt status bit is not W1C: it follows DAT1 while its
        // status enable is set. Arm it, poll it (and the DAT1 level while the
        // bus is idle), and disarm on a hit so the next wait sees a fresh edge
        // after brcmf has serviced the chip.
        let en = self.io.r16(REG_NORMAL_INT_ENABLE);
        self.io.w16(REG_NORMAL_INT_ENABLE, en | NI_CARD_INT);
        self.card_irq_enabled = true;
        let start = self.io.now_us();
        let hit = loop {
            if self.io.r16(REG_NORMAL_INT_STATUS) & NI_CARD_INT != 0 { break true; }
            let ps = self.io.r32(REG_PRESENT_STATE);
            if ps & (PSTATE_CMD_INHIBIT | PSTATE_DAT_INHIBIT) == 0 && ps & PSTATE_DAT1_LEVEL == 0 { break true; }
            if self.io.now_us().saturating_sub(start) >= timeout_us as u64 { break false; }
            core::hint::spin_loop();
        };
        let en = self.io.r16(REG_NORMAL_INT_ENABLE);
        self.io.w16(REG_NORMAL_INT_ENABLE, en & !NI_CARD_INT);
        self.card_irq_enabled = false;
        hit
    }
    fn delay_us(&mut self, us: u32) { self.io.delay_us(us) }
}

/// Read the chip-common ID word at backplane 0x18000000 the brcmfmac way:
/// request the ALP clock, point the F1 SBADDR window at the core, read 4 bytes
/// at F1 0x8000 (2/4-byte access flag), then drop the clock request again.
/// Returns `(chipid_word, alp_ok)`.
pub fn read_chip_id<B: SdioBus>(bus: &mut B) -> Result<(u32, bool), BusError> {
    bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR, CLKCSR_FORCE_HW_CLKREQ_OFF | CLKCSR_ALP_AVAIL_REQ)?;
    let mut alp = false;
    for _ in 0..50 {
        if bus.read8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR)? & CLKCSR_ALP_AVAIL != 0 { alp = true; break; }
        bus.delay_us(1_000);
    }
    if alp {
        bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR, CLKCSR_FORCE_HW_CLKREQ_OFF | CLKCSR_FORCE_ALP)?;
    }
    let a = SI_ENUM_BASE;
    bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_SBADDRLOW, ((a >> 8) & 0x80) as u8)?;
    bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_SBADDRMID, (a >> 16) as u8)?;
    bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_SBADDRHIGH, (a >> 24) as u8)?;
    let mut w = [0u8; 4];
    let r = bus.read_bytes(FUNC_BACKPLANE, (a & SBSDIO_SB_OFT_ADDR_MASK) | SBSDIO_SB_ACCESS_2_4B_FLAG, true, &mut w);
    let _ = bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR, 0);
    r?;
    Ok((u32::from_le_bytes(w), alp))
}

// ── Raspberry Pi 5 hardware: power path, pin mux, MMIO, boot probe ───────────
#[cfg(feature = "rpi5")]
pub use hw::{boot_probe, probe, probe_with, take_host, Mmio, PROBE_STATE};

#[cfg(feature = "rpi5")]
mod hw {
    extern crate alloc;
    use super::*;
    use core::sync::atomic::{AtomicU32, Ordering};
    use leandros_lib::mmio::{MmioRd, MmioWr};
    use spin::Mutex;

    pub const SDIO2_HOST_PHYS: u64 = 0x10_0110_0000;
    pub const SDIO2_CFG_PHYS: u64 = 0x10_0110_0400;
    /// wl_on_reg `startup-delay-us`.
    pub const WL_ON_STARTUP_US: u32 = 150_000;
    /// Hard probe budget, all waits included.
    pub const PROBE_BUDGET_US: u64 = 2_000_000;

    const SDIO_CFG_CTRL: usize = 0x00;
    const SDIO_CFG_SD_PIN_SEL: usize = 0x44;

    /// 0 = not run, 1 = ok (chip id 0x4345 seen), 2 = card up but chip id mismatch/unreadable,
    /// 3 = failed before card init finished, 4 = MMIO mapping failed.
    pub static PROBE_STATE: AtomicU32 = AtomicU32::new(0);

    pub struct Mmio { host: usize }

    impl HostIo for Mmio {
        fn r8(&mut self, o: usize) -> u8 { unsafe { ((self.host + o) as *const u8).mmio_read() } }
        fn r16(&mut self, o: usize) -> u16 { unsafe { ((self.host + o) as *const u16).mmio_read() } }
        fn r32(&mut self, o: usize) -> u32 { unsafe { ((self.host + o) as *const u32).mmio_read() } }
        fn w8(&mut self, o: usize, v: u8) { unsafe { ((self.host + o) as *mut u8).mmio_write(v) } }
        fn w16(&mut self, o: usize, v: u16) { unsafe { ((self.host + o) as *mut u16).mmio_write(v) } }
        fn w32(&mut self, o: usize, v: u32) { unsafe { ((self.host + o) as *mut u32).mmio_write(v) } }
        fn now_us(&mut self) -> u64 { sched::monotonic_ns() / 1_000 }
        fn delay_us(&mut self, us: u32) { sched::kthread::udelay(us as u64) }
    }

    unsafe impl Send for Mmio {}

    static HOST: Mutex<Option<SdioHost<Mmio>>> = Mutex::new(None);

    /// Hand the initialised sdio2 host to brcmf (once). `None` if the probe failed.
    pub fn take_host() -> Option<SdioHost<Mmio>> { HOST.lock().take() }

    extern "C" { fn serial_print_str_raw(s: *const u8, len: usize); }
    fn log(msg: &str) {
        let mut s = alloc::string::String::with_capacity(msg.len() + 8);
        s.push_str("[sdio] ");
        s.push_str(msg);
        s.push('\n');
        unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
    }
    macro_rules! slog { ($($a:tt)*) => { log(&alloc::format!($($a)*)) }; }

    unsafe fn rd32(a: usize) -> u32 { (a as *const u32).mmio_read() }
    unsafe fn wr32(a: usize, v: u32) { (a as *mut u32).mmio_write(v) }

    use crate::bcm2712_id::{self, Stepping};

    use crate::bcm2712_gpio::{self as gpio, Block, Chip, GROUP_SDIO2_WL_ON, WL_ON};

    /// Mux gpio28 to GPIO and gpio30..35 to sd2 with the dts pulls, through the
    /// shared BCM2712 pinctrl driver. Returns a short description for the log.
    /// Writes only registers whose value changes.
    fn pinmux(st: Stepping) -> alloc::string::String {
        if bcm2712_id::pin_rows(st).is_none() {
            return alloc::string::String::from("pinmux=skip(unknown stepping)");
        }
        let mut before = 0u32; // packed fsel nibbles of pins 30..35 as found
        for (i, p) in (30..=35).enumerate() {
            before |= (gpio::pin_get_fsel(Block::Main, p).unwrap_or(0) & 0xF) << (i as u32 * 4);
        }
        match gpio::pin_apply(Block::Main, &GROUP_SDIO2_WL_ON) {
            Ok(w) => alloc::format!("pinmux={:?} sd2-fsel-before={:06x} writes={}", st, before, w),
            Err(e) => alloc::format!("pinmux=fail({:?})", e),
        }
    }

    fn wl_reg_on(on: bool) { let _ = gpio::gpio_set_output(Chip::Gio, WL_ON, on); }

    fn map(phys: u64, len: usize) -> Option<usize> { mm::kva::ioremap(phys, len, mm::kva::CacheMode::Uncached) }

    /// Boot probe: power the CYW43455, bring sdio2 up as an SDIO host, print the
    /// card identity and the chip id. Inert on failure (one line, WL_REG_ON
    /// dropped again, host left in reset); never waits more than ~2 s.
    /// Mappings made once (the probe re-runs from `brcmfctl retry`).
    static MAPS: Mutex<Option<(usize, usize)>> = Mutex::new(None);

    pub fn boot_probe() { probe(20_000, WL_ON_STARTUP_US as u32) }

    pub fn probe(off_us: u32, on_us: u32) { probe_with(off_us, on_us, SdioTune::default()) }

    /// The whole probe: host quiesce, WL_REG_ON low for `off_us`, high and
    /// `on_us` settle, SDIO init, chip id, host parked. Re-runnable: a parked
    /// host from an earlier run is dropped first.
    pub fn probe_with(off_us: u32, on_us: u32, tune: SdioTune) {
        let t0 = sched::monotonic_ns() / 1_000;
        drop(HOST.lock().take());
        let cached = *MAPS.lock();
        let maps = cached.or_else(|| match (map(SDIO2_HOST_PHYS, 0x100), map(SDIO2_CFG_PHYS, 0x100)) {
            (Some(h), Some(c)) => Some((h, c)),
            _ => None,
        });
        // WL_REG_ON and the pin mux go through the shared BCM2712 GPIO driver.
        let Some((host, cfg)) = maps.filter(|_| gpio::gpio_is_output(Chip::Gio, WL_ON).is_ok()) else {
            PROBE_STATE.store(4, Ordering::Relaxed); log("FAIL stage=map"); return;
        };
        *MAPS.lock() = Some((host, cfg));
        let st = bcm2712_id::stepping().stepping;
        let mux = pinmux(st);
        // Force card presence (non-removable) and select SD pin timing.
        unsafe {
            let v = rd32(cfg + SDIO_CFG_CTRL);
            wr32(cfg + SDIO_CFG_CTRL, (v & !(1 << 30)) | (1 << 31));
            let v = rd32(cfg + SDIO_CFG_SD_PIN_SEL);
            wr32(cfg + SDIO_CFG_SD_PIN_SEL, (v & !3) | 2);
        }
        // The CYW43455 keeps running across kexec, and so may this host. Always
        // quiesce the host (full reset, SD clock and bus power off), then pulse
        // WL_REG_ON low (≥ 10 ms) → high and wait the regulator's startup delay,
        // even when the chip already looks powered. host_init resets again.
        {
            let mut q = Mmio { host };
            q.w16(REG_NORMAL_SIG_ENABLE, 0);
            q.w16(REG_ERROR_SIG_ENABLE, 0);
            q.w8(REG_SOFTWARE_RESET, SWRST_ALL);
            sched::kthread::udelay(1_000);
            q.w16(REG_CLOCK_CONTROL, 0);
            q.w8(REG_POWER_CONTROL, 0);
        }
        wl_reg_on(false);
        sched::kthread::udelay(off_us.max(1_000) as u64);
        wl_reg_on(true);
        sched::kthread::udelay(on_us as u64);

        let mut h = SdioHost::new(Mmio { host });
        h.deadline_us = Some(t0 + PROBE_BUDGET_US);
        h.tune = tune;
        let res = h.host_init().and_then(|_| h.init_card(tune.hs));
        let ci = match res {
            Ok(ci) => ci,
            Err(e) => {
                let ps = h.io.r32(REG_PRESENT_STATE);
                slog!("FAIL stage={} err={:?} nis/eis={:08x} pstate={:08x} {} t={}ms",
                      e.stage, e.err, h.last_fail, ps, mux, (sched::monotonic_ns() / 1_000 - t0) / 1_000);
                h.io.w8(REG_POWER_CONTROL, 0);
                h.io.w8(REG_SOFTWARE_RESET, SWRST_ALL);
                wl_reg_on(false);
                PROBE_STATE.store(3, Ordering::Relaxed);
                return;
            }
        };
        slog!("sdio2 base={:#x} ocr={:#08x} rca={:#06x} cccr={:#04x} caps={:#04x} funcs={} manf={:#06x}:{:#06x} bus={}bit {}kHz{} {}",
              SDIO2_HOST_PHYS, ci.ocr, ci.rca, ci.cccr_rev, ci.caps, ci.nfuncs, ci.manf, ci.card,
              if ci.width4 { 4 } else { 1 }, ci.clk_khz, if ci.high_speed { " hs" } else { "" }, mux);
        if tune != SdioTune::default() {
            slog!("tune sdclk={} hs={} blk={} blksz={} chunk={}", tune.sdclk_khz, tune.hs as u8, tune.blk as u8, tune.f1_blksz, tune.chunk);
        }
        let t = (sched::monotonic_ns() / 1_000 - t0) / 1_000;
        match read_chip_id(&mut h) {
            Ok((id, alp)) if id & 0xFFFF == 0x4345 => {
                slog!("chipid={:#010x} chip=0x4345 rev={} alp={} ok t={}ms", id, (id >> 16) & 0xF, alp, t);
                PROBE_STATE.store(1, Ordering::Relaxed);
            }
            Ok((id, alp)) => {
                slog!("FAIL stage=chipid chipid={:#010x} alp={} (want low16 0x4345) t={}ms", id, alp, t);
                PROBE_STATE.store(2, Ordering::Relaxed);
            }
            Err(e) => {
                slog!("FAIL stage=chipid err={:?} nis/eis={:08x} t={}ms", e, h.last_fail, t);
                PROBE_STATE.store(2, Ordering::Relaxed);
            }
        }
        // The card stays up for brcmf; drop the boot-only deadline.
        h.deadline_us = None;
        *HOST.lock() = Some(h);
    }
}
