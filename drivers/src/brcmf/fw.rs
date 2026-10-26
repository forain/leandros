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

/// brcmf_sdio_download_firmware + brcmf_sdio_firmware_callback: holds the
/// ARM in reset, writes code at the RAM base and the normalised nvram at the
/// top of RAM, releases the CR4, enables F2 and waits for it to come ready,
/// then arms the host interrupt mask and the card interrupt.
///
/// `nvram_text` is the raw `.txt` file; it is normalised here.
pub fn download(bus: &mut dyn SdioBus, chip: &mut Chip, code: &[u8], nvram_text: &[u8]) -> Result<DownloadReport, FwLoadError> {
    if code.len() < 4 {
        return Err(FwLoadError::EmptyImage);
    }
    let nvram = nvram_normalize(nvram_text);
    if code.len() + nvram.len() > chip.ram_size as usize {
        return Err(FwLoadError::TooBig);
    }
    let rstvec = u32::from_le_bytes([code[0], code[1], code[2], code[3]]);

    if chip.sr_capable {
        chip.kso_control(bus, true)?;
    }
    chip.clk_request(bus, true)?;
    chip.set_passive(bus)?;

    chip.bp.ram_write(bus, chip.ram_base, code)?;
    verify(chip, bus, chip.ram_base, code)?;

    let nvram_addr = chip.ram_base + chip.ram_size - nvram.len() as u32;
    chip.bp.ram_write(bus, nvram_addr, &nvram)?;
    verify(chip, bus, nvram_addr, &nvram)?;

    chip.set_active(bus, rstvec)?;
    let mut rep = DownloadReport {
        code_len: code.len() as u32,
        nvram_len: nvram.len() as u32,
        nvram_addr,
        rstvec,
        sr_enabled: false,
    };
    start_f2(bus, chip)?;
    if chip.sr_capable {
        chip.sr_init(bus)?;
        rep.sr_enabled = true;
    }
    Ok(rep)
}

/// The F2 half of brcmf_sdio_firmware_callback.
fn start_f2(bus: &mut dyn SdioBus, chip: &mut Chip) -> Result<(), FwLoadError> {
    let sd = chip.sdio_core()?;
    let saveclk = bus.read8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_CHIPCLKCSR)?;
    bus.write8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_CHIPCLKCSR, saveclk | chip::SBSDIO_FORCE_HT)?;
    chip.write32(bus, sd.base + chip::SD_TOSBMAILBOXDATA, SDPCM_PROT_VERSION << SMB_DATA_VERSION_SHIFT)?;
    bus.set_block_size(FUNC_WLAN, F2_BLOCKSIZE)?;
    if !chip::enable_func(bus, FUNC_WLAN, F2_READY_TIMEOUT_US)? {
        bus.write8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_CHIPCLKCSR, saveclk)?;
        return Err(FwLoadError::F2Timeout);
    }
    chip.write32(bus, sd.base + chip::SD_HOSTINTMASK, HOSTINTMASK)?;
    bus.write8(FUNC_BACKPLANE, chip::SBSDIO_WATERMARK, 8)?;
    bus.write8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_CHIPCLKCSR, saveclk)?;
    // Card interrupt: master enable + F1 + F2 (what sdio_claim_irq does).
    let ien = bus.read8(FUNC_BUS, chip::CCCR_IENX)?;
    bus.write8(FUNC_BUS, chip::CCCR_IENX, ien | 0x07)?;
    Ok(())
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
