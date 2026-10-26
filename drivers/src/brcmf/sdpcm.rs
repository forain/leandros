//! SDPCM framing over SDIO function 2 (brcmfmac `sdio.c`): hardware tag,
//! software header, sequence numbers, channels, tx credit window, F2 reads
//! driven by the frame tag, and the SDIO core's interrupt status / host
//! mailbox. Glommed superframes are not requested and are dropped if seen.

use alloc::vec::Vec;

use super::bus::{BusError, SdioBus, FUNC_BACKPLANE, FUNC_BUS, FUNC_WLAN};
use super::chip::{self, Chip, ChipError, SB_ACCESS_2_4B_FLAG, SB_OFT_ADDR_MASK, SI_ENUM_BASE};

pub const HWHDR_LEN: usize = 4;
pub const SWHDR_LEN: usize = 8;
pub const HDRLEN: usize = HWHDR_LEN + SWHDR_LEN;

pub const CHAN_CONTROL: u8 = 0;
pub const CHAN_EVENT: u8 = 1;
pub const CHAN_DATA: u8 = 2;
pub const CHAN_GLOM: u8 = 3;

/// First F2 read of a frame whose length is not known yet (BRCMF_FIRSTREAD).
pub const FIRSTREAD: usize = 64;
/// Largest frame accepted on the event/data channels (MAX_RX_DATASZ).
pub const MAX_RX_DATASZ: usize = 2048;
/// Control frames may be longer (large iovar results).
pub const MAX_CTL_FRAME: usize = 8192 + 64;

// intstatus bits (SDIO device core).
pub const I_HMB_FC_STATE: u32 = 1 << 4;
pub const I_HMB_FC_CHANGE: u32 = 1 << 5;
pub const I_HMB_FRAME_IND: u32 = 1 << 6;
pub const I_HMB_HOST_INT: u32 = 1 << 7;
pub const I_CHIPACTIVE: u32 = 1 << 29;

// tosbmailbox / tohostmailboxdata.
pub const SMB_INT_ACK: u32 = 1 << 1;
pub const HMB_DATA_NAKHANDLED: u32 = 0x0001;
pub const HMB_DATA_DEVREADY: u32 = 0x0002;
pub const HMB_DATA_FC: u32 = 0x0004;
pub const HMB_DATA_FWREADY: u32 = 0x0008;
pub const HMB_DATA_FWHALT: u32 = 0x0010;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SdpcmError {
    Bus(BusError),
    MissingCore,
    /// len ^ ~len check failed.
    BadTag { len: u16, check: u16 },
    BadLen(u16),
    BadOffset { len: u16, doff: u8 },
    TooLong,
    /// No tx credit (firmware window closed).
    NoCredit,
}

impl From<BusError> for SdpcmError {
    fn from(e: BusError) -> Self {
        SdpcmError::Bus(e)
    }
}

impl From<ChipError> for SdpcmError {
    fn from(e: ChipError) -> Self {
        match e {
            ChipError::Bus(b) => SdpcmError::Bus(b),
            _ => SdpcmError::MissingCore,
        }
    }
}

/// Decoded hardware tag + software header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    pub len: u16,
    pub seq: u8,
    pub chan: u8,
    pub nextlen: u8,
    pub doff: u8,
    pub fcmask: u8,
    pub window: u8,
}

/// Writes the 12-byte SDPCM header for a frame of `len` bytes (header included).
pub fn pack_header(hdr: &mut [u8], len: u16, seq: u8, chan: u8, doff: u8) {
    hdr[0..2].copy_from_slice(&len.to_le_bytes());
    hdr[2..4].copy_from_slice(&(!len).to_le_bytes());
    let sw = seq as u32 | ((chan as u32 & 0xF) << 8) | ((doff as u32) << 24);
    hdr[4..8].copy_from_slice(&sw.to_le_bytes());
    hdr[8..12].copy_from_slice(&0u32.to_le_bytes());
}

/// Parses the hardware tag only: `Ok(None)` for the all-zero "no frame" tag.
pub fn parse_tag(b: &[u8]) -> Result<Option<u16>, SdpcmError> {
    let len = u16::from_le_bytes([b[0], b[1]]);
    let check = u16::from_le_bytes([b[2], b[3]]);
    if len == 0 && check == 0 {
        return Ok(None);
    }
    if len ^ check != 0xFFFF {
        return Err(SdpcmError::BadTag { len, check });
    }
    if (len as usize) < HDRLEN {
        return Err(SdpcmError::BadLen(len));
    }
    Ok(Some(len))
}

/// Parses tag + software header of a complete frame (`b.len() >= HDRLEN`).
pub fn parse_header(b: &[u8]) -> Result<Option<Header>, SdpcmError> {
    let Some(len) = parse_tag(b)? else { return Ok(None) };
    let sw0 = u32::from_le_bytes([b[4], b[5], b[6], b[7]]);
    let sw1 = u32::from_le_bytes([b[8], b[9], b[10], b[11]]);
    let h = Header {
        len,
        seq: sw0 as u8,
        chan: ((sw0 >> 8) & 0xF) as u8,
        nextlen: (sw0 >> 16) as u8,
        doff: (sw0 >> 24) as u8,
        fcmask: sw1 as u8,
        window: (sw1 >> 8) as u8,
    };
    if (h.doff as usize) < HDRLEN || h.doff as u16 > len {
        return Err(SdpcmError::BadOffset { len, doff: h.doff });
    }
    Ok(Some(h))
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Stats {
    pub tx_frames: u32,
    pub rx_frames: u32,
    pub rx_badhdr: u32,
    pub rx_badseq: u32,
    pub rx_glom_dropped: u32,
    pub rxfail: u32,
    pub hostmail: u32,
    pub window_errors: u32,
}

/// One received frame, as a range of the caller's buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RxFrame {
    pub chan: u8,
    pub seq: u8,
    /// Payload starts here (after the SDPCM headers).
    pub doff: usize,
    pub len: usize,
}

#[derive(Debug)]
pub struct Sdpcm {
    pub tx_seq: u8,
    pub tx_max: u8,
    pub rx_seq: u8,
    pub blocksize: usize,
    /// Per-fifo flow-control mask from the last rx header.
    pub flowcontrol: u8,
    /// Flow-control state bit from intstatus (I_HMB_FC_STATE).
    pub fcstate: bool,
    pub last_hmb_data: u32,
    pub fw_halted: bool,
    /// Protocol version the firmware announced with DEVREADY/FWREADY.
    pub fw_sdpcm_ver: Option<u8>,
    /// FRAME_IND seen and no empty read since.
    pub rx_pending: bool,
    pub stats: Stats,
    txbuf: Vec<u8>,
}

impl Default for Sdpcm {
    fn default() -> Self {
        Self::new(super::fw::F2_BLOCKSIZE as usize)
    }
}

impl Sdpcm {
    pub fn new(blocksize: usize) -> Self {
        // One credit until the firmware's first header opens the window.
        Sdpcm {
            tx_seq: 0,
            tx_max: 1,
            rx_seq: 0,
            blocksize,
            flowcontrol: 0,
            fcstate: false,
            last_hmb_data: 0,
            fw_halted: false,
            fw_sdpcm_ver: None,
            rx_pending: false,
            stats: Stats::default(),
            txbuf: Vec::new(),
        }
    }

    /// brcmf data_ok(): the firmware window still has room.
    pub fn tx_credit(&self) -> bool {
        let d = self.tx_max.wrapping_sub(self.tx_seq);
        d != 0 && d & 0x80 == 0
    }

    /// F2 transfer length: 4-byte aligned, block-rounded above one block.
    pub fn padded_len(&self, len: usize) -> usize {
        let l = (len + 3) & !3;
        if self.blocksize != 0 && l > self.blocksize {
            l.div_ceil(self.blocksize) * self.blocksize
        } else {
            l
        }
    }

    fn f2_addr(chip: &mut Chip, bus: &mut dyn SdioBus) -> Result<u32, BusError> {
        chip.bp.set_window(bus, SI_ENUM_BASE)?;
        Ok((SI_ENUM_BASE & SB_OFT_ADDR_MASK) | SB_ACCESS_2_4B_FLAG)
    }

    /// Sends one frame on `chan`. The caller checks [`Self::tx_credit`].
    pub fn send(&mut self, bus: &mut dyn SdioBus, chip: &mut Chip, chan: u8, payload: &[u8]) -> Result<(), SdpcmError> {
        if !self.tx_credit() {
            return Err(SdpcmError::NoCredit);
        }
        let len = HDRLEN + payload.len();
        if len > MAX_CTL_FRAME {
            return Err(SdpcmError::TooLong);
        }
        let total = self.padded_len(len);
        self.txbuf.clear();
        self.txbuf.resize(total, 0);
        pack_header(&mut self.txbuf[..HDRLEN], len as u16, self.tx_seq, chan, HDRLEN as u8);
        self.txbuf[HDRLEN..len].copy_from_slice(payload);
        let addr = Self::f2_addr(chip, bus)?;
        if let Err(e) = bus.write_bytes(FUNC_WLAN, addr, false, &self.txbuf) {
            // brcmf_sdio_txfail: abort F2 and terminate the write frame.
            let _ = bus.write8(FUNC_BUS, chip::CCCR_ABORT, FUNC_WLAN);
            let _ = bus.write8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_FRAMECTRL, 0x02);
            chip.bp.invalidate();
            return Err(e.into());
        }
        self.tx_seq = self.tx_seq.wrapping_add(1);
        self.stats.tx_frames += 1;
        Ok(())
    }

    /// Reads, acks and acts on the SDIO core's interrupt status (masked by
    /// HOSTINTMASK). Returns the bits that were set.
    pub fn service_irq(&mut self, bus: &mut dyn SdioBus, chip: &mut Chip) -> Result<u32, SdpcmError> {
        let sd = chip.sdio_core()?;
        let mut st = chip.read32(bus, sd.base + chip::SD_INTSTATUS)? & super::fw::HOSTINTMASK;
        if st == 0 {
            return Ok(0);
        }
        chip.write32(bus, sd.base + chip::SD_INTSTATUS, st)?;
        if st & I_HMB_FC_CHANGE != 0 {
            let now = chip.read32(bus, sd.base + chip::SD_INTSTATUS)?;
            self.fcstate = now & (I_HMB_FC_STATE | I_HMB_FC_CHANGE) != 0;
        }
        if st & I_HMB_HOST_INT != 0 && self.hostmail(bus, chip)? {
            st |= I_HMB_FRAME_IND;
        }
        if st & I_HMB_FRAME_IND != 0 {
            self.rx_pending = true;
        }
        Ok(st)
    }

    /// brcmf_sdio_hostmail. Returns true if rx should be retried (NAK handled).
    fn hostmail(&mut self, bus: &mut dyn SdioBus, chip: &mut Chip) -> Result<bool, SdpcmError> {
        let sd = chip.sdio_core()?;
        let d = chip.read32(bus, sd.base + chip::SD_TOHOSTMAILBOXDATA)?;
        chip.write32(bus, sd.base + chip::SD_TOSBMAILBOX, SMB_INT_ACK)?;
        self.stats.hostmail += 1;
        self.last_hmb_data = d;
        if d & HMB_DATA_FWHALT != 0 {
            self.fw_halted = true;
        }
        if d & (HMB_DATA_DEVREADY | HMB_DATA_FWREADY) != 0 {
            self.fw_sdpcm_ver = Some((d >> 16) as u8);
        }
        if d & HMB_DATA_FC != 0 {
            self.flowcontrol = (d >> 24) as u8;
        }
        Ok(d & HMB_DATA_NAKHANDLED != 0)
    }

    /// brcmf_sdio_rxfail: abort the F2 read and terminate the frame.
    fn rxfail(&mut self, bus: &mut dyn SdioBus) {
        self.stats.rxfail += 1;
        let _ = bus.write8(FUNC_BUS, chip::CCCR_ABORT, FUNC_WLAN);
        let _ = bus.write8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_FRAMECTRL, chip::SFC_RF_TERM);
        for _ in 0..10 {
            let hi = bus.read8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_RFRAMEBCHI).unwrap_or(0);
            let lo = bus.read8(FUNC_BACKPLANE, chip::SBSDIO_FUNC1_RFRAMEBCLO).unwrap_or(0);
            if hi == 0 && lo == 0 {
                break;
            }
            bus.delay_us(1);
        }
    }

    /// Reads one frame from F2 into `buf`. `Ok(None)` when the firmware has
    /// nothing queued (zero tag); that also clears `rx_pending`.
    pub fn read_frame(&mut self, bus: &mut dyn SdioBus, chip: &mut Chip, buf: &mut Vec<u8>) -> Result<Option<RxFrame>, SdpcmError> {
        let addr = Self::f2_addr(chip, bus)?;
        buf.clear();
        buf.resize(FIRSTREAD, 0);
        if let Err(e) = bus.read_bytes(FUNC_WLAN, addr, false, buf) {
            self.rxfail(bus);
            chip.bp.invalidate();
            return Err(e.into());
        }
        let len = match parse_tag(buf) {
            Ok(None) => {
                self.rx_pending = false;
                return Ok(None);
            }
            Ok(Some(l)) => l as usize,
            Err(e) => {
                self.stats.rx_badhdr += 1;
                self.rxfail(bus);
                return Err(e);
            }
        };
        if len > MAX_CTL_FRAME {
            self.rxfail(bus);
            return Err(SdpcmError::TooLong);
        }
        if len > FIRSTREAD {
            let rest = self.padded_len(len - FIRSTREAD);
            buf.resize(FIRSTREAD + rest, 0);
            if let Err(e) = bus.read_bytes(FUNC_WLAN, addr, false, &mut buf[FIRSTREAD..]) {
                self.rxfail(bus);
                chip.bp.invalidate();
                return Err(e.into());
            }
        }
        buf.truncate(len);
        let h = match parse_header(buf) {
            Ok(Some(h)) => h,
            Ok(None) => unreachable!(),
            Err(e) => {
                self.stats.rx_badhdr += 1;
                self.rxfail(bus);
                return Err(e);
            }
        };
        if h.chan != CHAN_CONTROL && len > MAX_RX_DATASZ {
            return Err(SdpcmError::TooLong);
        }
        if h.seq != self.rx_seq {
            self.stats.rx_badseq += 1;
        }
        self.rx_seq = h.seq.wrapping_add(1);
        self.flowcontrol = h.fcmask;
        let mut window = h.window;
        if window.wrapping_sub(self.tx_seq) > 0x40 {
            self.stats.window_errors += 1;
            window = self.tx_seq.wrapping_add(2);
        }
        self.tx_max = window;
        self.stats.rx_frames += 1;
        Ok(Some(RxFrame { chan: h.chan, seq: h.seq, doff: h.doff as usize, len }))
    }
}
