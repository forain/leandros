//! Firmware bring-up for the CYW43455: code download into TCM, nvram
//! normalisation and placement, ARM CR4 release, F2 ready, and the CLM blob.
//!
//! The firmware bytes come in as caller-supplied slices. In the kernel they
//! are loaded with `crate::firmware::request_firmware(path, max)` from the
//! real root, the same way iwlwifi gets its `.ucode` (see `iwlwifi.rs`); the
//! default paths are the constants below (the Raspberry Pi OS / linux-firmware
//! names, so a stock `/lib/firmware/brcm` tree works unchanged).

use alloc::vec::Vec;

use super::bus::{BusError, SdioBus, FUNC_BACKPLANE, FUNC_BUS, FUNC_WLAN};
use super::chip::{self, Chip, ChipError};
use super::fwil::{FwError, Fwil};

/// Names as staged on the Pi 5 image (Infineon/Raspberry Pi firmware package).
pub const FW_PATH: &str = "/lib/firmware/brcm/cyw43455-sdio.bin";
pub const NVRAM_PATH: &str = "/lib/firmware/brcm/brcmfmac43455-sdio.raspberrypi,5-model-b.txt";
/// Generic fallback name for the nvram file.
pub const NVRAM_FALLBACK_PATH: &str = "/lib/firmware/brcm/brcmfmac43455-sdio.txt";
pub const CLM_PATH: &str = "/lib/firmware/brcm/cyw43455-sdio.clm_blob";

/// F2 block size brcmfmac uses for the 4345 family.
pub const F2_BLOCKSIZE: u16 = 512;
/// SDPCM protocol version announced through tosbmailboxdata.
pub const SDPCM_PROT_VERSION: u32 = 4;
pub const SMB_DATA_VERSION_SHIFT: u32 = 16;
/// HOSTINTMASK = I_HMB_SW_MASK | I_CHIPACTIVE.
pub const HOSTINTMASK: u32 = 0x0000_00F0 | (1 << 29);
/// How long to wait for the started firmware to raise F2 IORDY.
pub const F2_READY_TIMEOUT_US: u32 = 3_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FwLoadError {
    Chip(ChipError),
    EmptyImage,
    /// Code + nvram do not fit in TCM.
    TooBig,
    /// Read-back of the downloaded image differs.
    Verify,
    /// Firmware did not raise F2 IORDY.
    F2Timeout,
}

impl From<ChipError> for FwLoadError {
    fn from(e: ChipError) -> Self {
        FwLoadError::Chip(e)
    }
}

impl From<BusError> for FwLoadError {
    fn from(e: BusError) -> Self {
        FwLoadError::Chip(ChipError::Bus(e))
    }
}

impl FwLoadError {
    pub fn as_str(&self) -> &'static str {
        match self {
            FwLoadError::Chip(c) => c.as_str(),
            FwLoadError::EmptyImage => "empty firmware image",
            FwLoadError::TooBig => "image does not fit in RAM",
            FwLoadError::Verify => "RAM read-back mismatch",
            FwLoadError::F2Timeout => "F2 not ready after firmware start",
        }
    }
}

fn nvram_key_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_'
}

fn nvram_char(c: u8) -> bool {
    c != b'#' && (0x20..0x7F).contains(&c)
}

/// brcmf_fw_nvram_strip: turns the nvram text file into the blob the
/// firmware expects. Comments, blank lines, CRs and invalid lines go; each
/// `key=value` becomes a NUL-terminated entry; `boardrev=0xff` is added when
/// missing; the list ends with an extra NUL, is zero-padded to 4 bytes and is
/// followed by the length token `(~words << 16) | words` (little endian).
pub fn nvram_normalize(text: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + 32);
    let mut boardrev = false;
    for raw in text.split(|&c| c == b'\n') {
        let line = match raw.iter().position(|&c| !matches!(c, b' ' | b'\t' | b'\r')) {
            Some(p) => &raw[p..],
            None => continue,
        };
        if !nvram_key_start(line[0]) {
            continue;
        }
        let Some(eq) = line.iter().position(|&c| c == b'=') else { continue };
        let key = &line[..eq];
        if !key.iter().all(|&c| c.is_ascii_alphanumeric() || c == b'_' || c == b'/') {
            continue;
        }
        let rest = &line[eq + 1..];
        let vlen = rest.iter().position(|&c| !nvram_char(c)).unwrap_or(rest.len());
        let mut val = &rest[..vlen];
        while let [v @ .., b' '] = val {
            val = v;
        }
        if key == b"boardrev" {
            boardrev = true;
        }
        out.extend_from_slice(key);
        out.push(b'=');
        out.extend_from_slice(val);
        out.push(0);
    }
    if !boardrev {
        out.extend_from_slice(b"boardrev=0xff\0");
    }
    // Terminating NUL, then pad to a 4-byte boundary.
    let len = (out.len() + 1 + 3) & !3;
    out.resize(len, 0);
    let words = (len / 4) as u32;
    let token = (!words << 16) | (words & 0xFFFF);
    out.extend_from_slice(&token.to_le_bytes());
    out
}

/// Length in bytes of a normalised blob's entries (padded), from its token.
pub fn nvram_token_words(blob: &[u8]) -> Option<u32> {
    let t = u32::from_le_bytes(blob.get(blob.len().checked_sub(4)?..)?.try_into().ok()?);
    let words = t & 0xFFFF;
    (t >> 16 == (!words & 0xFFFF)).then_some(words)
}

#[derive(Debug, Clone, Copy, Default)]
pub struct DownloadReport {
    pub code_len: u32,
    pub nvram_len: u32,
    pub nvram_addr: u32,
    pub rstvec: u32,
    pub sr_enabled: bool,
}

/// Bytes read back after the download to catch a broken data path cheaply.
const VERIFY_SAMPLE: usize = 256;

fn verify(chip: &mut Chip, bus: &mut dyn SdioBus, addr: u32, data: &[u8]) -> Result<(), FwLoadError> {
    let mut buf = [0u8; VERIFY_SAMPLE];
    let n = core::cmp::min(VERIFY_SAMPLE, data.len());
    for (off, src) in [(0usize, &data[..n]), (data.len() - n, &data[data.len() - n..])] {
        chip.bp.ram_read(bus, addr + off as u32, &mut buf[..n])?;
        if &buf[..n] != src {
            return Err(FwLoadError::Verify);
        }
    }
    Ok(())
}

/// Bring-up knobs. `Default` is what brcmfmac does for a 43455 on SDIO; the
/// Pi's runtime `brcmfctl retry k=v ...` overrides them one by one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Knobs {
    /// Clock wait bound for the download clock (µs).
    pub clk_us: u32,
    /// Request HT (not just ALP) for the download. brcmfmac downloads with
    /// `alp_only = true`: before the firmware runs, HT may never come up.
    pub dl_ht: bool,
    /// Delay between releasing the CR4 and asking for HT (µs).
    pub post_us: u32,
    /// HT wait after the firmware started (µs).
    pub ht_us: u32,
    /// F2 IORDY wait (µs).
    pub f2_us: u32,
    /// Save/restore: None = re-probe PMU chipcontrol 3 after the firmware
    /// started (brcmfmac evaluates brcmf_chip_sr_capable there), Some = forced.
    pub sr: Option<bool>,
    /// KSO wake before the download when SR is enabled.
    pub kso: bool,
    /// F2 watermark (SBSDIO_WATERMARK); brcmfmac's default is 8.
    pub wm: u8,
    /// Set SBSDIO_DEVCTL_F2WM_ENAB in SBSDIO_DEVICE_CTL.
    pub f2wm: bool,
    /// SBSDIO_FUNC1_MESBUSYCTRL value; 0 = not written.
    pub mesbusy: u8,
    /// Request FORCE_HT while F2 comes up (brcmfmac does).
    pub force_ht: bool,
    /// Skip the CLM blob (boot layer).
    pub noclm: bool,
}

impl Default for Knobs {
    fn default() -> Self {
        Knobs {
            clk_us: chip::PMU_MAX_TRANSITION_US,
            dl_ht: false,
            post_us: 0,
            ht_us: chip::PMU_MAX_TRANSITION_US,
            f2_us: F2_READY_TIMEOUT_US,
            sr: None,
            kso: true,
            wm: DEFAULT_F2_WATERMARK,
            f2wm: false,
            mesbusy: 0,
            force_ht: true,
            noclm: false,
        }
    }
}

pub const DEFAULT_F2_WATERMARK: u8 = 8;

fn parse_num(v: &str) -> Option<u32> {
    let v = v.trim();
    if let Some(h) = v.strip_prefix("0x") {
        u32::from_str_radix(h, 16).ok()
    } else {
        v.parse().ok()
    }
}

impl Knobs {
    /// Applies one `key=value`. Durations take `us`; `*_ms` keys take ms.
    /// Returns false for an unknown key or a bad value.
    pub fn set(&mut self, key: &str, val: &str) -> bool {
        let Some(n) = parse_num(val) else {
            return key == "sr" && val == "auto" && {
                self.sr = None;
                true
            };
        };
        let ms = n.saturating_mul(1000);
        match key {
            "clk_us" => self.clk_us = n,
            "clk_ms" => self.clk_us = ms,
            "dl_ht" => self.dl_ht = n != 0,
            "post_us" => self.post_us = n,
            "post_ms" => self.post_us = ms,
            "ht_us" => self.ht_us = n,
            "ht_ms" => self.ht_us = ms,
            "f2_ms" => self.f2_us = ms,
            "sr" => self.sr = Some(n != 0),
            "kso" => self.kso = n != 0,
            "wm" => self.wm = n as u8,
            "f2wm" => self.f2wm = n != 0,
            "mesbusy" => self.mesbusy = n as u8,
            "force_ht" => self.force_ht = n != 0,
            "noclm" => self.noclm = n != 0,
            _ => return false,
        }
        true
    }
}

/// A failed bring-up: what went wrong and in which step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DlFail {
    pub err: FwLoadError,
    pub stage: &'static str,
}

/// brcmf_sdio_download_firmware + brcmf_sdio_firmware_callback with the
/// default (brcmfmac) knobs.
///
/// `nvram_text` is the raw `.txt` file; it is normalised here.
pub fn download(bus: &mut dyn SdioBus, chip: &mut Chip, code: &[u8], nvram_text: &[u8]) -> Result<DownloadReport, FwLoadError> {
    download_with(bus, chip, code, nvram_text, &Knobs::default()).map_err(|f| f.err)
}

/// The brcmfmac order for a CR4 chip on SDIO:
/// 1. clock for the download: ALP only (`alp_only = true`), unless `dl_ht`;
/// 2. ARM CR4 halted + D11 in reset, code at the RAM base, nvram (with its
///    `~len|len` token as the last word) at the top of RAM, both read back;
/// 3. reset vector at backplane 0, CR4 released; the clock request dropped
///    (CLK_SDONLY);
/// 4. firmware_callback: HT request with the 1 s PMU bound, SR re-evaluated,
///    FORCE_HT, tosbmailboxdata = SDPCM v4, F2 on and IORDY, hostintmask,
///    watermark, then sr_init (or restore the saved clock), card IRQ on.
pub fn download_with(
    bus: &mut dyn SdioBus,
    chip: &mut Chip,
    code: &[u8],
    nvram_text: &[u8],
    k: &Knobs,
) -> Result<DownloadReport, DlFail> {
    let mut stage = "image";
    let r = download_inner(bus, chip, code, nvram_text, k, &mut stage);
    r.map_err(|err| DlFail { err, stage })
}

fn download_inner(
    bus: &mut dyn SdioBus,
    chip: &mut Chip,
    code: &[u8],
    nvram_text: &[u8],
    k: &Knobs,
    stage: &mut &'static str,
) -> Result<DownloadReport, FwLoadError> {
    if code.len() < 4 {
        return Err(FwLoadError::EmptyImage);
    }
    let nvram = nvram_normalize(nvram_text);
    if code.len() + nvram.len() > chip.ram_size as usize {
        return Err(FwLoadError::TooBig);
    }
    let rstvec = u32::from_le_bytes([code[0], code[1], code[2], code[3]]);

    if chip.sr_capable && k.kso {
        *stage = "kso";
        chip.kso_control(bus, true)?;
    }
    *stage = if k.dl_ht { "dl-clk-ht" } else { "dl-clk-alp" };
    chip.clk_request_t(bus, k.dl_ht, k.clk_us)?;
    *stage = "passive";
    chip.set_passive(bus)?;

    *stage = "code";
    chip.bp.ram_write(bus, chip.ram_base, code)?;
    verify(chip, bus, chip.ram_base, code)?;

    *stage = "nvram";
    let nvram_addr = chip.ram_base + chip.ram_size - nvram.len() as u32;
    chip.bp.ram_write(bus, nvram_addr, &nvram)?;
    verify(chip, bus, nvram_addr, &nvram)?;

    *stage = "active";
    chip.set_active(bus, rstvec)?;
    // CLK_SDONLY: drop the download clock request.
    chip.clk_release(bus)?;
    let mut rep = DownloadReport {
        code_len: code.len() as u32,
        nvram_len: nvram.len() as u32,
        nvram_addr,
        rstvec,
        sr_enabled: false,
    };
    if k.post_us != 0 {
        bus.delay_us(k.post_us);
    }
    *stage = "ht";
    chip.clk_request_t(bus, true, k.ht_us)?;
    *stage = "sr-probe";
    chip.sr_capable = match k.sr {
        Some(v) => v,
        None => chip.sr_capable_probe(bus)?,
    };
    *stage = "f2";
    start_f2(bus, chip, k)?;
    rep.sr_enabled = chip.sr_capable;
    Ok(rep)
}

/// The F2 half of brcmf_sdio_firmware_callback.
fn start_f2(bus: &mut dyn SdioBus, chip: &mut Chip, k: &Knobs) -> Result<(), FwLoadError> {
    let sd = chip.sdio_core()?;
    let saveclk = bus.read8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_CHIPCLKCSR)?;
    if k.force_ht {
        bus.write8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_CHIPCLKCSR, saveclk | chip::SBSDIO_FORCE_HT)?;
    }
    chip.write32(bus, sd.base + chip::SD_TOSBMAILBOXDATA, SDPCM_PROT_VERSION << SMB_DATA_VERSION_SHIFT)?;
    bus.set_block_size(FUNC_WLAN, F2_BLOCKSIZE)?;
    if !chip::enable_func(bus, FUNC_WLAN, k.f2_us)? {
        let _ = chip::disable_func(bus, FUNC_WLAN);
        bus.write8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_CHIPCLKCSR, saveclk)?;
        return Err(FwLoadError::F2Timeout);
    }
    chip.write32(bus, sd.base + chip::SD_HOSTINTMASK, HOSTINTMASK)?;
    bus.write8(FUNC_BACKPLANE, chip::SBSDIO_WATERMARK, k.wm)?;
    if k.f2wm {
        let d = bus.read8(FUNC_BACKPLANE, chip::SBSDIO_DEVICE_CTL)?;
        bus.write8(FUNC_BACKPLANE, chip::SBSDIO_DEVICE_CTL, d | chip::SBSDIO_DEVCTL_F2WM_ENAB)?;
    }
    if k.mesbusy != 0 {
        bus.write8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_MESBUSYCTRL, k.mesbusy)?;
    }
    if chip.sr_capable {
        chip.sr_init(bus)?;
    } else {
        bus.write8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_CHIPCLKCSR, saveclk)?;
    }
    // Card interrupt: master enable + F1 + F2 (what sdio_claim_irq does).
    let ien = bus.read8(FUNC_BUS, chip::CCCR_IENX)?;
    bus.write8(FUNC_BUS, chip::CCCR_IENX, ien | 0x07)?;
    Ok(())
}

// ---- Failure diagnostics ---------------------------------------------------------

/// sdpcm_shared flags: protocol version in the low byte.
pub const SDPCM_SHARED_VERSION_MASK: u32 = 0xFF;
pub const SDPCM_SHARED_ASSERT: u32 = 0x0200;
pub const SDPCM_SHARED_TRAP: u32 = 0x0400;
/// Console ring bytes read at most.
pub const CONSOLE_MAX: u32 = 8192;

/// What the chip looks like after a failed bring-up.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Diag {
    pub line: alloc::string::String,
    /// Last console lines of a running firmware (oldest first).
    pub console: Vec<alloc::string::String>,
}

fn rd(chip: &mut Chip, bus: &mut dyn SdioBus, a: u32) -> Option<u32> {
    chip.read32(bus, a).ok()
}

fn hx(v: Option<u32>) -> alloc::string::String {
    match v {
        Some(v) => alloc::format!("{:08x}", v),
        None => alloc::string::String::from("err"),
    }
}

/// The end-of-RAM word: the nvram token before the firmware runs, the
/// sdpcm_shared address once it does (brcmf_sdio_readshared).
pub fn shared_addr(chip: &mut Chip, bus: &mut dyn SdioBus) -> Option<u32> {
    let mut w = [0u8; 4];
    chip.bp.ram_read(bus, chip.ram_base + chip.ram_size - 4, &mut w).ok()?;
    let a = u32::from_le_bytes(w);
    let token = a != 0 && (!a >> 16) & 0xFFFF == a & 0xFFFF;
    let in_ram = a >= chip.ram_base && a < chip.ram_base + chip.ram_size;
    if a == 0 || token || !in_ram {
        None
    } else {
        Some(a)
    }
}

/// One-line chip state plus the firmware console tail (`max_lines`).
pub fn diag(bus: &mut dyn SdioBus, chip: &mut Chip, max_lines: usize) -> Diag {
    use core::fmt::Write;
    let mut d = Diag::default();
    let clk = bus.read8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_CHIPCLKCSR).ok();
    let slp = bus.read8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_SLEEPCSR).ok();
    let ior = bus.read8(FUNC_BUS, chip::CCCR_IORDY).ok();
    // Backplane reads need at least ALP.
    if clk.map_or(true, |c| c & chip::SBSDIO_ALP_AVAIL == 0) {
        let _ = bus.write8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_CHIPCLKCSR, chip::SBSDIO_ALP_AVAIL_REQ);
        bus.delay_us(2000);
    }
    let pmu = chip.pmu_base();
    let b8 = |v: Option<u8>| v.map_or(alloc::string::String::from("err"), |v| alloc::format!("{:02x}", v));
    let _ = write!(d.line, "clkcsr={} sleepcsr={} iordy={}", b8(clk), b8(slp), b8(ior));
    let _ = write!(
        d.line,
        " pmuctl={} pmust={} res={} pend={} min={} max={}",
        hx(rd(chip, bus, pmu + chip::CC_PMUCONTROL)),
        hx(rd(chip, bus, pmu + chip::CC_PMUSTATUS)),
        hx(rd(chip, bus, pmu + chip::CC_RES_STATE)),
        hx(rd(chip, bus, pmu + chip::CC_RES_PENDING)),
        hx(rd(chip, bus, pmu + chip::CC_MIN_RES_MASK)),
        hx(rd(chip, bus, pmu + chip::CC_MAX_RES_MASK)),
    );
    if let Some(cr4) = chip.core(chip::CORE_ARM_CR4) {
        let io = rd(chip, bus, cr4.wrap + chip::BCMA_IOCTL);
        let halt = io.map_or("?", |v| if v & chip::ARMCR4_IOCTL_CPUHALT != 0 { "halt" } else { "run" });
        let _ = write!(
            d.line,
            " cr4 ioctl={} rst={} iost={} {}",
            hx(io),
            hx(rd(chip, bus, cr4.wrap + chip::BCMA_RESET_CTL)),
            hx(rd(chip, bus, cr4.wrap + chip::BCMA_IOST)),
            halt
        );
    }
    if let Some(sd) = chip.core(chip::CORE_SDIO_DEV) {
        let _ = write!(
            d.line,
            " sd int={} mb={}",
            hx(rd(chip, bus, sd.base + chip::SD_INTSTATUS)),
            hx(rd(chip, bus, sd.base + chip::SD_TOHOSTMAILBOXDATA))
        );
    }
    let mut w = [0u8; 4];
    let last = chip.bp.ram_read(bus, chip.ram_base + chip.ram_size - 4, &mut w).ok().map(|_| u32::from_le_bytes(w));
    let _ = write!(d.line, " ramtop={}", hx(last));
    let Some(sh) = shared_addr(chip, bus) else {
        d.line.push_str(" fw=not-started");
        return d;
    };
    // struct sdpcm_shared_le: flags, trap_addr, assert_exp_addr,
    // assert_file_addr, assert_line, console_addr, msgtrace_addr, ...
    let mut s = [0u8; 28];
    if chip.bp.ram_read(bus, sh, &mut s).is_err() {
        d.line.push_str(" shared=err");
        return d;
    }
    let f = |i: usize| u32::from_le_bytes([s[i * 4], s[i * 4 + 1], s[i * 4 + 2], s[i * 4 + 3]]);
    let (flags, trap, aline, con) = (f(0), f(1), f(4), f(5));
    let _ = write!(d.line, " shared@{:x} ver={} flags={:x}", sh, flags & SDPCM_SHARED_VERSION_MASK, flags);
    if flags & SDPCM_SHARED_TRAP != 0 {
        let _ = write!(d.line, " TRAP@{:x}", trap);
    }
    if flags & SDPCM_SHARED_ASSERT != 0 {
        let _ = write!(d.line, " ASSERT line {}", aline);
    }
    // struct rte_console: vcons_in, vcons_out, then rte_log_le {buf, buf_size, idx}.
    let mut lg = [0u8; 12];
    if con == 0 || chip.bp.ram_read(bus, con + 8, &mut lg).is_err() {
        let _ = write!(d.line, " con={:x}", con);
        return d;
    }
    let g = |i: usize| u32::from_le_bytes([lg[i * 4], lg[i * 4 + 1], lg[i * 4 + 2], lg[i * 4 + 3]]);
    let (buf, size, idx) = (g(0), g(1), g(2));
    let _ = write!(d.line, " con={:x} buf={:x} size={} idx={}", con, buf, size, idx);
    if size == 0 || size > 0x10000 || idx >= size || buf < chip.ram_base || buf + size > chip.ram_base + chip.ram_size {
        return d;
    }
    // Read the newest CONSOLE_MAX bytes of the ring in order.
    let n = core::cmp::min(size, CONSOLE_MAX);
    let start = (idx + size - n) % size;
    let mut ring = alloc::vec![0u8; n as usize];
    let first = core::cmp::min(n, size - start);
    if chip.bp.ram_read(bus, buf + start, &mut ring[..first as usize]).is_err()
        || (first < n && chip.bp.ram_read(bus, buf, &mut ring[first as usize..]).is_err())
    {
        d.line.push_str(" ring=err");
        return d;
    }
    d.console = console_tail(&ring, max_lines);
    d
}

/// Splits console ring bytes into printable lines and keeps the last `max`.
pub fn console_tail(ring: &[u8], max: usize) -> Vec<alloc::string::String> {
    let mut out: Vec<alloc::string::String> = Vec::new();
    for l in ring.split(|&c| c == b'\n' || c == 0) {
        let t: alloc::string::String = l
            .iter()
            .filter(|&&c| c != b'\r')
            .map(|&c| if (0x20..0x7F).contains(&c) { c as char } else { '.' })
            .collect();
        let t = t.trim();
        if !t.is_empty() && t.bytes().any(|c| c != b'.') {
            out.push(alloc::string::String::from(t));
        }
    }
    let skip = out.len().saturating_sub(max);
    out.split_off(skip)
}

// ---- CLM blob -------------------------------------------------------------------

pub const CLM_CHUNK: usize = 1400;
const DL_BEGIN: u16 = 0x0002;
const DL_END: u16 = 0x0004;
const DL_TYPE_CLM: u16 = 2;
const DLOAD_HANDLER_VER: u16 = 1;
const DLOAD_FLAG_VER_SHIFT: u16 = 12;
/// sizeof(struct brcmf_dload_data_le) without the data array.
pub const DLOAD_HDR: usize = 12;

/// brcmf_c_process_clm_blob: pushes the CLM blob through the "clmload"
/// iovar in 1400-byte chunks and checks "clmload_status". Returns the number
/// of chunks sent.
pub fn clm_load(fwil: &mut dyn Fwil, clm: &[u8]) -> Result<usize, FwError> {
    if clm.is_empty() {
        return Ok(0);
    }
    let mut buf = Vec::with_capacity(DLOAD_HDR + CLM_CHUNK);
    let mut flag = DL_BEGIN | (DLOAD_HANDLER_VER << DLOAD_FLAG_VER_SHIFT);
    let mut chunks = 0;
    let mut it = clm.chunks(CLM_CHUNK).peekable();
    while let Some(chunk) = it.next() {
        if it.peek().is_none() {
            flag |= DL_END;
        }
        buf.clear();
        buf.extend_from_slice(&flag.to_le_bytes());
        buf.extend_from_slice(&DL_TYPE_CLM.to_le_bytes());
        buf.extend_from_slice(&(chunk.len() as u32).to_le_bytes());
        buf.extend_from_slice(&0u32.to_le_bytes()); // crc: unused by the firmware
        buf.extend_from_slice(chunk);
        fwil.iovar_set("clmload", &buf)?;
        flag &= !DL_BEGIN;
        chunks += 1;
    }
    let mut st = [0u8; 4];
    fwil.iovar_get("clmload_status", &mut st)?;
    let st = i32::from_le_bytes(st);
    if st != 0 {
        return Err(FwError::Status(st));
    }
    Ok(chunks)
}
