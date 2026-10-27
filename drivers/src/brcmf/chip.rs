//! Chip layer: SDIO backplane access, EROM core discovery, AI core reset,
//! RAM geometry, clocks and KSO for the CYW43455 (chip 0x4345 rev 6).
//!
//! Follows brcmfmac's `chip.c` and the bus-core half of `sdio.c`. Every
//! access goes through the frozen [`SdioBus`] seam, so the whole layer runs
//! against a scripted mock on the host.

use super::bus::{BusError, SdioBus, FUNC_BACKPLANE, FUNC_BUS};

// ---- CCCR (function 0) -------------------------------------------------------
pub const CCCR_IOEN: u32 = 0x02;
pub const CCCR_IORDY: u32 = 0x03;
pub const CCCR_IENX: u32 = 0x04;
pub const CCCR_ABORT: u32 = 0x06;
/// Broadcom vendor CCCR: card capabilities (CMD14 support).
pub const CCCR_BRCM_CARDCAP: u32 = 0xF0;
pub const CARDCAP_CMD14_SUPPORT: u8 = 0x02;
pub const CARDCAP_CMD14_EXT: u8 = 0x04;
/// Broadcom vendor CCCR: card control; WLANRESET makes an SDIO reset reset the backplane.
pub const CCCR_BRCM_CARDCTRL: u32 = 0xF1;
pub const CARDCTRL_WLANRESET: u8 = 0x02;

// ---- Function 1 registers ----------------------------------------------------
pub const SBSDIO_WATERMARK: u32 = 0x10008;
pub const SBSDIO_DEVICE_CTL: u32 = 0x10009;
pub const SBSDIO_FUNC1_SBADDRLOW: u32 = 0x1000A;
pub const SBSDIO_FUNC1_FRAMECTRL: u32 = 0x1000D;
pub const SBSDIO_FUNC1_CHIPCLKCSR: u32 = 0x1000E;
pub const SBSDIO_FUNC1_SDIOPULLUP: u32 = 0x1000F;
pub const SBSDIO_FUNC1_RFRAMEBCLO: u32 = 0x1001B;
pub const SBSDIO_FUNC1_RFRAMEBCHI: u32 = 0x1001C;
pub const SBSDIO_FUNC1_WAKEUPCTRL: u32 = 0x1001E;
pub const SBSDIO_FUNC1_SLEEPCSR: u32 = 0x1001F;

pub const SFC_RF_TERM: u8 = 0x01;
pub const WCTRL_HTWAIT: u8 = 1 << 1;
pub const SLEEPCSR_KSO: u8 = 0x01;
pub const SLEEPCSR_DEVON: u8 = 0x02;

// CHIPCLKCSR bits.
pub const SBSDIO_FORCE_ALP: u8 = 0x01;
pub const SBSDIO_FORCE_HT: u8 = 0x02;
pub const SBSDIO_FORCE_ILP: u8 = 0x04;
pub const SBSDIO_ALP_AVAIL_REQ: u8 = 0x08;
pub const SBSDIO_HT_AVAIL_REQ: u8 = 0x10;
pub const SBSDIO_FORCE_HW_CLKREQ_OFF: u8 = 0x20;
pub const SBSDIO_ALP_AVAIL: u8 = 0x40;
pub const SBSDIO_HT_AVAIL: u8 = 0x80;
pub const SBSDIO_AVBITS: u8 = SBSDIO_ALP_AVAIL | SBSDIO_HT_AVAIL;

/// Backplane window: F1 CMD53 offsets 0..0x7FFF, 0x8000 = 32-bit access flag.
pub const SB_OFT_ADDR_MASK: u32 = 0x7FFF;
pub const SB_ACCESS_2_4B_FLAG: u32 = 0x8000;
pub const SB_WINDOW_MASK: u32 = !SB_OFT_ADDR_MASK;
pub const SB_WINDOW_SIZE: u32 = 0x8000;

/// ChipCommon is always at the enumeration base.
pub const SI_ENUM_BASE: u32 = 0x1800_0000;
pub const CC_CHIPID: u32 = 0x00;
pub const CC_EROMPTR: u32 = 0xFC;
pub const CC_PMUCONTROL: u32 = 0x600;
pub const CC_CHIPCONTROL_ADDR: u32 = 0x650;
pub const CC_CHIPCONTROL_DATA: u32 = 0x654;
pub const PMU_CTL_RES_RELOAD: u32 = 2 << 13;
pub const CC_PMUSTATUS: u32 = 0x608;
pub const CC_RES_STATE: u32 = 0x60C;
pub const CC_RES_PENDING: u32 = 0x610;
pub const CC_MIN_RES_MASK: u32 = 0x618;
pub const CC_MAX_RES_MASK: u32 = 0x61C;
pub const SBSDIO_FUNC1_MESBUSYCTRL: u32 = 0x1001D;
pub const SBSDIO_DEVCTL_F2WM_ENAB: u8 = 0x10;

// BCMA core ids.
pub const CORE_CHIPCOMMON: u16 = 0x800;
pub const CORE_INTERNAL_MEM: u16 = 0x80E;
pub const CORE_80211: u16 = 0x812;
pub const CORE_PMU: u16 = 0x827;
pub const CORE_SDIO_DEV: u16 = 0x829;
pub const CORE_ARM_CM3: u16 = 0x82A;
pub const CORE_ARM_CR4: u16 = 0x83E;
pub const CORE_GCI: u16 = 0x840;
pub const CORE_SYS_MEM: u16 = 0x849;

// AI wrapper registers / bits.
pub const BCMA_IOCTL: u32 = 0x0408;
pub const BCMA_IOST: u32 = 0x0500;
pub const BCMA_RESET_CTL: u32 = 0x0800;
pub const BCMA_RESET_ST: u32 = 0x0804;
pub const BCMA_IOCTL_CLK: u32 = 0x0001;
pub const BCMA_IOCTL_FGC: u32 = 0x0002;
pub const BCMA_RESET_CTL_RESET: u32 = 0x0001;
pub const ARMCR4_IOCTL_CPUHALT: u32 = 0x0020;
pub const D11_IOCTL_PHYCLOCKEN: u32 = 0x0004;
pub const D11_IOCTL_PHYRESET: u32 = 0x0008;

// ARM CR4 core registers (TCM banks).
pub const ARMCR4_CAP: u32 = 0x04;
pub const ARMCR4_BANKIDX: u32 = 0x40;
pub const ARMCR4_BANKINFO: u32 = 0x44;

// SDIO device core registers (struct sdpcmd_regs).
pub const SD_INTSTATUS: u32 = 0x20;
pub const SD_HOSTINTMASK: u32 = 0x24;
pub const SD_INTMASK: u32 = 0x28;
pub const SD_TOSBMAILBOX: u32 = 0x40;
pub const SD_TOHOSTMAILBOX: u32 = 0x44;
pub const SD_TOSBMAILBOXDATA: u32 = 0x48;
pub const SD_TOHOSTMAILBOXDATA: u32 = 0x4C;

pub const CHIP_4345: u16 = 0x4345;
/// TCM base of the 4345 family (brcmf_chip_tcm_rambase).
pub const RAMBASE_4345: u32 = 0x198000;

/// PMU clock transitions: brcmfmac's PMU_MAX_TRANSITION_DLY is 1 s (sdio.c);
/// the HT PLL of a freshly started 43455 takes far longer than 15 ms.
pub const PMU_MAX_TRANSITION_US: u32 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChipError {
    Bus(BusError),
    /// CHIPCLKCSR did not read back what was written (no card / wrong device).
    ClkCsr { wrote: u8, read: u8 },
    AlpTimeout,
    HtTimeout,
    /// Chip id register read back something that is not an AI chip.
    NotAi(u32),
    UnsupportedChip(u16),
    /// EROM table malformed or a required core missing.
    Erom,
    MissingCore(u16),
    BadRam,
    KsoTimeout,
}

impl From<BusError> for ChipError {
    fn from(e: BusError) -> Self {
        ChipError::Bus(e)
    }
}

impl ChipError {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChipError::Bus(BusError::Timeout) => "bus timeout",
            ChipError::Bus(BusError::Crc) => "bus crc",
            ChipError::Bus(BusError::NoCard) => "no card",
            ChipError::Bus(BusError::Io) => "bus io",
            ChipError::ClkCsr { .. } => "chipclkcsr readback mismatch",
            ChipError::AlpTimeout => "ALP clock timeout",
            ChipError::HtTimeout => "HT clock timeout",
            ChipError::NotAi(_) => "not an AI chip",
            ChipError::UnsupportedChip(_) => "unsupported chip",
            ChipError::Erom => "EROM scan failed",
            ChipError::MissingCore(_) => "required core missing",
            ChipError::BadRam => "TCM size invalid",
            ChipError::KsoTimeout => "KSO wake timeout",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Core {
    pub id: u16,
    pub rev: u8,
    pub base: u32,
    pub wrap: u32,
}

pub const MAX_CORES: usize = 16;

/// Backplane accessor with the F1 window cache.
#[derive(Debug, Default)]
pub struct Backplane {
    window: Option<u32>,
    /// Number of SBADDR window programmings (for tests / stats).
    pub window_sets: u32,
}

impl Backplane {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the cached window (after a bus reset or an error).
    pub fn invalidate(&mut self) {
        self.window = None;
    }

    /// Points the F1 window at the 32 KiB region holding `addr`; no bus traffic if cached.
    pub fn set_window(&mut self, bus: &mut dyn SdioBus, addr: u32) -> Result<(), BusError> {
        let w = addr & SB_WINDOW_MASK;
        if self.window == Some(w) {
            return Ok(());
        }
        self.window = None;
        for i in 0..3u32 {
            let byte = (w >> (8 + 8 * i)) as u8;
            bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_SBADDRLOW + i, byte)?;
        }
        self.window = Some(w);
        self.window_sets += 1;
        Ok(())
    }

    pub fn read32(&mut self, bus: &mut dyn SdioBus, addr: u32) -> Result<u32, BusError> {
        let r = self.read32_inner(bus, addr);
        if r.is_err() {
            self.invalidate();
        }
        r
    }

    fn read32_inner(&mut self, bus: &mut dyn SdioBus, addr: u32) -> Result<u32, BusError> {
        self.set_window(bus, addr)?;
        let mut b = [0u8; 4];
        bus.read_bytes(FUNC_BACKPLANE, (addr & SB_OFT_ADDR_MASK) | SB_ACCESS_2_4B_FLAG, true, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }

    pub fn write32(&mut self, bus: &mut dyn SdioBus, addr: u32, val: u32) -> Result<(), BusError> {
        let r = self.set_window(bus, addr).and_then(|_| {
            bus.write_bytes(
                FUNC_BACKPLANE,
                (addr & SB_OFT_ADDR_MASK) | SB_ACCESS_2_4B_FLAG,
                true,
                &val.to_le_bytes(),
            )
        });
        if r.is_err() {
            self.invalidate();
        }
        r
    }

    /// Bulk RAM write, split at window boundaries (brcmf_sdiod_ramrw).
    pub fn ram_write(&mut self, bus: &mut dyn SdioBus, mut addr: u32, mut data: &[u8]) -> Result<(), BusError> {
        while !data.is_empty() {
            let off = addr & SB_OFT_ADDR_MASK;
            let n = core::cmp::min((SB_WINDOW_SIZE - off) as usize, data.len());
            self.set_window(bus, addr)?;
            if let Err(e) = bus.write_bytes(FUNC_BACKPLANE, off | SB_ACCESS_2_4B_FLAG, true, &data[..n]) {
                self.invalidate();
                return Err(e);
            }
            data = &data[n..];
            addr = addr.wrapping_add(n as u32);
        }
        Ok(())
    }

    /// Bulk RAM read, split at window boundaries.
    pub fn ram_read(&mut self, bus: &mut dyn SdioBus, mut addr: u32, mut out: &mut [u8]) -> Result<(), BusError> {
        while !out.is_empty() {
            let off = addr & SB_OFT_ADDR_MASK;
            let n = core::cmp::min((SB_WINDOW_SIZE - off) as usize, out.len());
            self.set_window(bus, addr)?;
            let (head, tail) = core::mem::take(&mut out).split_at_mut(n);
            if let Err(e) = bus.read_bytes(FUNC_BACKPLANE, off | SB_ACCESS_2_4B_FLAG, true, head) {
                self.invalidate();
                return Err(e);
            }
            out = tail;
            addr = addr.wrapping_add(n as u32);
        }
        Ok(())
    }
}

/// What the probe learned about the chip.
#[derive(Debug)]
pub struct Chip {
    pub bp: Backplane,
    pub chip_id: u16,
    pub chip_rev: u8,
    pub cores: [Option<Core>; MAX_CORES],
    pub ram_base: u32,
    pub ram_size: u32,
    /// Save/restore engine enabled in PMU chipcontrol 3 (decides KSO handling).
    pub sr_capable: bool,
}

/// Polls `f` every `step_us` until it returns true or `total_us` elapses.
pub(crate) fn spin_until(
    bus: &mut dyn SdioBus,
    total_us: u32,
    step_us: u32,
    mut f: impl FnMut(&mut dyn SdioBus) -> Result<bool, BusError>,
) -> Result<bool, BusError> {
    let mut waited = 0u32;
    loop {
        if f(bus)? {
            return Ok(true);
        }
        if waited >= total_us {
            return Ok(false);
        }
        bus.delay_us(step_us);
        waited = waited.saturating_add(step_us);
    }
}

// ---- EROM descriptor decoding -------------------------------------------------
const DESC_TYPE_MSK: u32 = 0xF;
const DESC_EMPTY: u32 = 0x0;
const DESC_VALID: u32 = 0x1;
const DESC_COMPONENT: u32 = 0x1;
const DESC_MASTER_PORT: u32 = 0x3;
const DESC_ADDRESS: u32 = 0x5;
const DESC_ADDRSIZE_GT32: u32 = 0x8;
const DESC_EOT: u32 = 0xF;
const COMP_PARTNUM: u32 = 0x000F_FF00;
const COMP_REVISION_S: u32 = 24;
const COMP_NUM_SWRAP: u32 = 0x00F8_0000;
const COMP_NUM_MWRAP: u32 = 0x0007_C000;
const SLAVE_ADDR_BASE: u32 = 0xFFFF_F000;
const SLAVE_TYPE_S: u32 = 6;
const SLAVE_TYPE_SLAVE: u32 = 0;
const SLAVE_TYPE_SWRAP: u32 = 2;
const SLAVE_TYPE_MWRAP: u32 = 3;
const SLAVE_SIZE_TYPE_S: u32 = 4;
const SLAVE_SIZE_4K: u32 = 0;
const SLAVE_SIZE_8K: u32 = 1;
const SLAVE_SIZE_DESC: u32 = 3;
/// Upper bound on EROM words, so a garbage table cannot loop forever.
const EROM_MAX_WORDS: u32 = 1024;

struct Erom<'a> {
    bp: &'a mut Backplane,
    addr: u32,
    end: u32,
}

impl Erom<'_> {
    /// Reads one descriptor; address descriptors (incl. 64-bit) report DESC_ADDRESS.
    fn next(&mut self, bus: &mut dyn SdioBus) -> Result<(u32, u32), ChipError> {
        if self.addr >= self.end {
            return Err(ChipError::Erom);
        }
        let val = self.bp.read32(bus, self.addr)?;
        self.addr += 4;
        let mut ty = val & DESC_TYPE_MSK;
        if ty & !DESC_ADDRSIZE_GT32 == DESC_ADDRESS {
            ty = DESC_ADDRESS;
        }
        Ok((val, ty))
    }

    /// brcmf_chip_dmp_get_regaddr: register base + wrapper base of the current component.
    fn regaddr(&mut self, bus: &mut dyn SdioBus) -> Result<Option<(u32, u32)>, ChipError> {
        let (val, ty) = self.next(bus)?;
        let wraptype = if ty == DESC_MASTER_PORT {
            let _ = val;
            SLAVE_TYPE_MWRAP
        } else if ty == DESC_ADDRESS {
            self.addr -= 4;
            SLAVE_TYPE_SWRAP
        } else {
            self.addr -= 4;
            return Ok(None);
        };
        let (mut regbase, mut wrapbase) = (0u32, 0u32);
        loop {
            let (val, ty) = loop {
                let (v, t) = self.next(bus)?;
                if t == DESC_EOT {
                    self.addr -= 4;
                    return Ok(None);
                }
                if t == DESC_ADDRESS || t == DESC_COMPONENT {
                    break (v, t);
                }
            };
            if ty == DESC_COMPONENT {
                self.addr -= 4;
                return Ok(Some((regbase, wrapbase)));
            }
            if val & DESC_ADDRSIZE_GT32 != 0 {
                self.next(bus)?;
            }
            let sztype = (val >> SLAVE_SIZE_TYPE_S) & 3;
            if sztype == SLAVE_SIZE_DESC {
                let (sz, _) = self.next(bus)?;
                if sz & DESC_ADDRSIZE_GT32 != 0 {
                    self.next(bus)?;
                }
            }
            if sztype != SLAVE_SIZE_4K && sztype != SLAVE_SIZE_8K {
                continue;
            }
            let stype = (val >> SLAVE_TYPE_S) & 3;
            if regbase == 0 && stype == SLAVE_TYPE_SLAVE {
                regbase = val & SLAVE_ADDR_BASE;
            }
            if wrapbase == 0 && stype == wraptype {
                wrapbase = val & SLAVE_ADDR_BASE;
            }
            if regbase != 0 && wrapbase != 0 {
                return Ok(Some((regbase, wrapbase)));
            }
        }
    }
}

impl Chip {
    pub fn core(&self, id: u16) -> Option<Core> {
        self.cores.iter().flatten().find(|c| c.id == id).copied()
    }

    fn need(&self, id: u16) -> Result<Core, ChipError> {
        self.core(id).ok_or(ChipError::MissingCore(id))
    }

    pub fn core_count(&self) -> usize {
        self.cores.iter().flatten().count()
    }

    pub fn sdio_core(&self) -> Result<Core, ChipError> {
        self.need(CORE_SDIO_DEV)
    }

    /// PMU registers live in ChipCommon unless the chip has a separate PMU core.
    pub fn pmu_base(&self) -> u32 {
        self.core(CORE_PMU).map(|c| c.base).unwrap_or(SI_ENUM_BASE)
    }

    pub fn read32(&mut self, bus: &mut dyn SdioBus, addr: u32) -> Result<u32, BusError> {
        self.bp.read32(bus, addr)
    }

    pub fn write32(&mut self, bus: &mut dyn SdioBus, addr: u32, val: u32) -> Result<(), BusError> {
        self.bp.write32(bus, addr, val)
    }

    /// The probe half of brcmf_sdio_probe_attach + brcmf_chip_attach:
    /// enable F1, get ALP, identify the chip, scan the EROM, size the RAM,
    /// arm WLANRESET and PMU resource reload, and drop the clock request again.
    pub fn attach(bus: &mut dyn SdioBus) -> Result<Chip, ChipError> {
        enable_func(bus, 1, 100_000)?;
        bus.set_block_size(FUNC_BACKPLANE, 64)?;

        // Ask for ALP only, then force it (brcmfmac does the same dance).
        let clkset = SBSDIO_FORCE_HW_CLKREQ_OFF | SBSDIO_ALP_AVAIL_REQ;
        bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR, clkset)?;
        let rd = bus.read8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR)?;
        if rd & !SBSDIO_AVBITS != clkset {
            return Err(ChipError::ClkCsr { wrote: clkset, read: rd });
        }
        let ok = spin_until(bus, PMU_MAX_TRANSITION_US, 100, |b| {
            Ok(b.read8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR)? & SBSDIO_ALP_AVAIL != 0)
        })?;
        if !ok {
            return Err(ChipError::AlpTimeout);
        }
        bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR, SBSDIO_FORCE_HW_CLKREQ_OFF | SBSDIO_FORCE_ALP)?;
        bus.delay_us(65);
        bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_SDIOPULLUP, 0)?;

        let mut chip = Chip {
            bp: Backplane::new(),
            chip_id: 0,
            chip_rev: 0,
            cores: [None; MAX_CORES],
            ram_base: 0,
            ram_size: 0,
            sr_capable: false,
        };
        let id = chip.read32(bus, SI_ENUM_BASE + CC_CHIPID)?;
        chip.chip_id = id as u16;
        chip.chip_rev = ((id >> 16) & 0xF) as u8;
        if (id >> 28) & 0xF != 1 {
            return Err(ChipError::NotAi(id));
        }
        if chip.chip_id != CHIP_4345 {
            return Err(ChipError::UnsupportedChip(chip.chip_id));
        }
        chip.erom_scan(bus)?;
        for need in [CORE_CHIPCOMMON, CORE_ARM_CR4, CORE_SDIO_DEV, CORE_80211] {
            chip.need(need)?;
        }
        chip.ram_size = chip.cr4_tcm_size(bus)?;
        chip.ram_base = RAMBASE_4345;
        if chip.ram_size == 0 || chip.ram_size > 0x0040_0000 {
            return Err(ChipError::BadRam);
        }
        chip.sr_capable = chip.sr_capable_probe(bus)?;

        // An SDIO card reset resets the WLAN backplane.
        let cc = bus.read8(FUNC_BUS, CCCR_BRCM_CARDCTRL)?;
        bus.write8(FUNC_BUS, CCCR_BRCM_CARDCTRL, cc | CARDCTRL_WLANRESET)?;
        // A backplane reset reloads the PMU resources.
        let pmuctl = chip.pmu_base() + CC_PMUCONTROL;
        let v = chip.read32(bus, pmuctl)?;
        chip.write32(bus, pmuctl, v | PMU_CTL_RES_RELOAD)?;

        // Clear intermediate F2 frame state, drop the forced clock.
        disable_func(bus, 2)?;
        bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR, 0)?;
        Ok(chip)
    }

    /// brcmf_chip_dmp_erom_scan.
    pub fn erom_scan(&mut self, bus: &mut dyn SdioBus) -> Result<(), ChipError> {
        let start = self.read32(bus, SI_ENUM_BASE + CC_EROMPTR)?;
        let mut erom = Erom { bp: &mut self.bp, addr: start, end: start.wrapping_add(EROM_MAX_WORDS * 4) };
        let mut n = 0usize;
        let mut cores = [None; MAX_CORES];
        loop {
            let (val, ty) = erom.next(bus)?;
            if ty == DESC_EOT {
                break;
            }
            if val & DESC_VALID == 0 || ty == DESC_EMPTY || ty != DESC_COMPONENT {
                continue;
            }
            let id = ((val & COMP_PARTNUM) >> 8) as u16;
            let (val2, _) = erom.next(bus)?;
            if val2 & DESC_TYPE_MSK != DESC_COMPONENT {
                return Err(ChipError::Erom);
            }
            let nmw = (val2 & COMP_NUM_MWRAP) >> 14;
            let nsw = (val2 & COMP_NUM_SWRAP) >> 19;
            let rev = (val2 >> COMP_REVISION_S) as u8;
            if nmw + nsw == 0 && id != CORE_PMU && id != CORE_GCI {
                continue;
            }
            let Some((base, wrap)) = erom.regaddr(bus)? else { continue };
            if n < MAX_CORES {
                cores[n] = Some(Core { id, rev, base, wrap });
                n += 1;
            }
        }
        self.cores = cores;
        if n == 0 {
            return Err(ChipError::Erom);
        }
        Ok(())
    }

    /// brcmf_chip_tcm_ramsize: sum of the CR4's A+B TCM banks.
    pub fn cr4_tcm_size(&mut self, bus: &mut dyn SdioBus) -> Result<u32, ChipError> {
        let cr4 = self.need(CORE_ARM_CR4)?;
        let cap = self.read32(bus, cr4.base + ARMCR4_CAP)?;
        let banks = (cap & 0xF) + ((cap >> 4) & 0xF);
        let mut size = 0u32;
        for i in 0..banks {
            self.write32(bus, cr4.base + ARMCR4_BANKIDX, i)?;
            let info = self.read32(bus, cr4.base + ARMCR4_BANKINFO)?;
            size += ((info & 0x7F) + 1) * 8192;
        }
        Ok(size)
    }

    /// brcmf_chip_sr_capable for the 4345: PMU chipcontrol 3, bit 2.
    pub fn sr_capable_probe(&mut self, bus: &mut dyn SdioBus) -> Result<bool, ChipError> {
        let pmu = self.pmu_base();
        self.write32(bus, pmu + CC_CHIPCONTROL_ADDR, 3)?;
        let v = self.read32(bus, pmu + CC_CHIPCONTROL_DATA)?;
        Ok(v & (1 << 2) != 0)
    }

    pub fn is_core_up(&mut self, bus: &mut dyn SdioBus, core: Core) -> Result<bool, BusError> {
        let ioctl = self.read32(bus, core.wrap + BCMA_IOCTL)?;
        let rst = self.read32(bus, core.wrap + BCMA_RESET_CTL)?;
        Ok(ioctl & (BCMA_IOCTL_FGC | BCMA_IOCTL_CLK) == BCMA_IOCTL_CLK && rst & BCMA_RESET_CTL_RESET == 0)
    }

    /// brcmf_chip_ai_coredisable.
    pub fn core_disable(&mut self, bus: &mut dyn SdioBus, core: Core, prereset: u32, reset: u32) -> Result<(), BusError> {
        let w = core.wrap;
        if self.read32(bus, w + BCMA_RESET_CTL)? & BCMA_RESET_CTL_RESET == 0 {
            self.write32(bus, w + BCMA_IOCTL, prereset | BCMA_IOCTL_FGC | BCMA_IOCTL_CLK)?;
            self.read32(bus, w + BCMA_IOCTL)?;
            self.write32(bus, w + BCMA_RESET_CTL, BCMA_RESET_CTL_RESET)?;
            bus.delay_us(10);
            let bp = &mut self.bp;
            spin_until(bus, 300, 10, |b| Ok(bp.read32(b, w + BCMA_RESET_CTL)? == BCMA_RESET_CTL_RESET))?;
        }
        self.write32(bus, w + BCMA_IOCTL, reset | BCMA_IOCTL_FGC | BCMA_IOCTL_CLK)?;
        self.read32(bus, w + BCMA_IOCTL)?;
        Ok(())
    }

    /// brcmf_chip_ai_resetcore.
    pub fn core_reset(
        &mut self,
        bus: &mut dyn SdioBus,
        core: Core,
        prereset: u32,
        reset: u32,
        postreset: u32,
    ) -> Result<(), BusError> {
        self.core_disable(bus, core, prereset, reset)?;
        let w = core.wrap;
        let mut count = 0;
        while self.read32(bus, w + BCMA_RESET_CTL)? & BCMA_RESET_CTL_RESET != 0 {
            self.write32(bus, w + BCMA_RESET_CTL, 0)?;
            count += 1;
            if count > 50 {
                break;
            }
            bus.delay_us(40);
        }
        self.write32(bus, w + BCMA_IOCTL, postreset | BCMA_IOCTL_CLK)?;
        self.read32(bus, w + BCMA_IOCTL)?;
        Ok(())
    }

    /// brcmf_chip_cr4_set_passive: halt the ARM and hold the D11 PHY in reset
    /// so firmware can be written to TCM.
    pub fn set_passive(&mut self, bus: &mut dyn SdioBus) -> Result<(), ChipError> {
        let cr4 = self.need(CORE_ARM_CR4)?;
        let v = self.read32(bus, cr4.wrap + BCMA_IOCTL)? & ARMCR4_IOCTL_CPUHALT;
        self.core_reset(bus, cr4, v, ARMCR4_IOCTL_CPUHALT, ARMCR4_IOCTL_CPUHALT)?;
        let d11 = self.need(CORE_80211)?;
        self.core_reset(
            bus,
            d11,
            D11_IOCTL_PHYRESET | D11_IOCTL_PHYCLOCKEN,
            D11_IOCTL_PHYCLOCKEN,
            D11_IOCTL_PHYCLOCKEN,
        )?;
        Ok(())
    }

    /// brcmf_chip_cr4_set_active + brcmf_sdio_buscore_activate: clear the SDIO
    /// core's interrupts, write the reset vector to backplane address 0 and
    /// release the ARM CR4.
    pub fn set_active(&mut self, bus: &mut dyn SdioBus, rstvec: u32) -> Result<(), ChipError> {
        let sd = self.sdio_core()?;
        self.write32(bus, sd.base + SD_INTSTATUS, 0xFFFF_FFFF)?;
        self.bp.ram_write(bus, 0, &rstvec.to_le_bytes())?;
        let cr4 = self.need(CORE_ARM_CR4)?;
        self.core_reset(bus, cr4, ARMCR4_IOCTL_CPUHALT, 0, 0)?;
        Ok(())
    }

    /// brcmf_sdio_htclk(on): request HT (or ALP only) and poll for it.
    pub fn clk_request(&mut self, bus: &mut dyn SdioBus, ht: bool) -> Result<u8, ChipError> {
        self.clk_request_t(bus, ht, PMU_MAX_TRANSITION_US)
    }

    /// `clk_request` with an explicit wait bound.
    pub fn clk_request_t(&mut self, bus: &mut dyn SdioBus, ht: bool, timeout_us: u32) -> Result<u8, ChipError> {
        let req = if ht { SBSDIO_HT_AVAIL_REQ } else { SBSDIO_ALP_AVAIL_REQ };
        bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR, req)?;
        let want = if ht { SBSDIO_HT_AVAIL | SBSDIO_ALP_AVAIL } else { SBSDIO_ALP_AVAIL };
        let mut last = 0u8;
        let ok = spin_until(bus, timeout_us, 500, |b| {
            last = b.read8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR)?;
            Ok(last & want == want)
        })?;
        if !ok {
            return Err(if ht { ChipError::HtTimeout } else { ChipError::AlpTimeout });
        }
        Ok(last)
    }

    /// Drop the clock request (lets the chip fall back to ILP/sleep).
    pub fn clk_release(&mut self, bus: &mut dyn SdioBus) -> Result<(), BusError> {
        bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR, 0)
    }

    /// brcmf_sdio_kso_control: set or clear Keep-SDIO-On. The first write may
    /// fail while the chip sleeps (it wakes the always-on domain), so errors on
    /// it are ignored and the bit is re-written until it reads back.
    pub fn kso_control(&mut self, bus: &mut dyn SdioBus, on: bool) -> Result<(), ChipError> {
        let wr = if on { SLEEPCSR_KSO } else { 0 };
        let _ = bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_SLEEPCSR, wr);
        let (cmp, mask) = if on {
            bus.delay_us(2000);
            (SLEEPCSR_KSO | SLEEPCSR_DEVON, SLEEPCSR_KSO | SLEEPCSR_DEVON)
        } else {
            (0, SLEEPCSR_KSO)
        };
        let mut errs = 0;
        for _ in 0..(PMU_MAX_TRANSITION_US / 50) {
            match bus.read8(FUNC_BACKPLANE, SBSDIO_FUNC1_SLEEPCSR) {
                Ok(v) if v & mask == cmp => return Ok(()),
                Ok(_) => errs = 0,
                Err(e) => {
                    errs += 1;
                    if errs > 5 {
                        return Err(ChipError::Bus(e));
                    }
                }
            }
            bus.delay_us(50);
            let _ = bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_SLEEPCSR, wr);
        }
        Err(ChipError::KsoTimeout)
    }

    /// brcmf_sdio_sr_init: HT-wait on wake, CMD14 support, keep HT forced.
    pub fn sr_init(&mut self, bus: &mut dyn SdioBus) -> Result<(), BusError> {
        let v = bus.read8(FUNC_BACKPLANE, SBSDIO_FUNC1_WAKEUPCTRL)?;
        bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_WAKEUPCTRL, v | WCTRL_HTWAIT)?;
        bus.write8(FUNC_BUS, CCCR_BRCM_CARDCAP, CARDCAP_CMD14_SUPPORT | CARDCAP_CMD14_EXT)?;
        bus.write8(FUNC_BACKPLANE, SBSDIO_FUNC1_CHIPCLKCSR, SBSDIO_FORCE_HT)
    }
}

/// Sets the function's IOEN bit and polls IORDY.
pub fn enable_func(bus: &mut dyn SdioBus, func: u8, timeout_us: u32) -> Result<bool, BusError> {
    let bit = 1u8 << func;
    let en = bus.read8(FUNC_BUS, CCCR_IOEN)?;
    if en & bit == 0 {
        bus.write8(FUNC_BUS, CCCR_IOEN, en | bit)?;
    }
    spin_until(bus, timeout_us, 1000, |b| Ok(b.read8(FUNC_BUS, CCCR_IORDY)? & bit != 0))
}

pub fn disable_func(bus: &mut dyn SdioBus, func: u8) -> Result<(), BusError> {
    let bit = 1u8 << func;
    let en = bus.read8(FUNC_BUS, CCCR_IOEN)?;
    if en & bit != 0 {
        bus.write8(FUNC_BUS, CCCR_IOEN, en & !bit)?;
    }
    Ok(())
}
