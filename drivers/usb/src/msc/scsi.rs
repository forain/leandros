//! SCSI transparent command set on top of Bulk-Only Transport.

use super::bot::{Bot, BotErr, Completion, Data, CSW_FAILED};
use super::transport::{BulkTransport, UsbErr};

/// Largest payload of a single READ/WRITE command.
/// 120 KiB (240 sectors of 512 B): Linux usb-storage's default `max_sectors`
/// for USB 2.0 mass storage, which every stick it supports accepts. Larger
/// commands amortise the per-command BOT overhead (CBW + CSW + device
/// latency) on sequential reads.
pub const MAX_CHUNK_BYTES: usize = 120 * 1024;
/// A command's whole error ladder (retries + reset recovery) gives up after
/// this long; with 5 s per-transfer deadlines a dead device used to keep one
/// request "in flight" for minutes.
pub const LADDER_BUDGET_NS: u64 = 30_000_000_000;
/// How long to wait for a unit to become ready.
pub const READY_TIMEOUT_MS: u32 = 10_000;
const READY_POLL_MS: u32 = 250;

const SK_NO_SENSE: u8 = 0x00;
const SK_RECOVERED_ERROR: u8 = 0x01;
const SK_NOT_READY: u8 = 0x02;
const SK_ILLEGAL_REQUEST: u8 = 0x05;
const SK_UNIT_ATTENTION: u8 = 0x06;
const SK_DATA_PROTECT: u8 = 0x07;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MscErr {
    Transport(UsbErr),
    /// Bad CSW or phase error persisted through reset recovery.
    Protocol,
    /// CHECK CONDITION with the decoded sense triple.
    CheckCondition { key: u8, asc: u8, ascq: u8 },
    /// Fewer bytes than requested were transferred.
    ShortTransfer,
    /// No LUN became ready.
    NoDevice,
    /// Sector size other than 512 or 4096.
    UnsupportedSectorSize(u32),
    /// Unit did not become ready within the timeout.
    NotReady,
    /// Buffer length not a multiple of the sector size, or LBA out of range.
    InvalidArg,
    WriteProtected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Sense {
    key: u8,
    asc: u8,
    ascq: u8,
}

impl From<Sense> for MscErr {
    fn from(s: Sense) -> Self {
        MscErr::CheckCondition { key: s.key, asc: s.asc, ascq: s.ascq }
    }
}

pub struct ScsiDisk<T: BulkTransport> {
    bot: Bot<T>,
    delay_ms: fn(u32),
    lun: u8,
    sector_size: u32,
    sectors: u64,
    read_only: bool,
    /// Set once the device answered SYNCHRONIZE CACHE with ILLEGAL REQUEST,
    /// NO SENSE or RECOVERED ERROR; the command is not issued again.
    sync_cache_unsupported: bool,
}

impl<T: BulkTransport> ScsiDisk<T> {
    /// `delay_ms` blocks the caller for at least the given milliseconds.
    pub fn new(transport: T, delay_ms: fn(u32)) -> Self {
        ScsiDisk { bot: Bot::new(transport), delay_ms, lun: 0, sector_size: 0, sectors: 0, read_only: false, sync_cache_unsupported: false }
    }

    pub fn sector_size(&self) -> u32 {
        self.sector_size
    }
    pub fn capacity_sectors(&self) -> u64 {
        self.sectors
    }
    pub fn read_only(&self) -> bool {
        self.read_only
    }
    /// True once SYNCHRONIZE CACHE has been found unusable on this device.
    pub fn sync_cache_unsupported(&self) -> bool {
        self.sync_cache_unsupported
    }
    pub fn lun(&self) -> u8 {
        self.lun
    }
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.bot.transport
    }

    // ---- command plumbing -------------------------------------------------

    fn map_bot(e: BotErr) -> MscErr {
        match e {
            BotErr::Transport(u) => MscErr::Transport(u),
            BotErr::BadCsw | BotErr::Phase => MscErr::Protocol,
        }
    }

    fn request_sense(&mut self) -> Result<Sense, MscErr> {
        let mut buf = [0u8; 18];
        let cdb = [0x03, 0, 0, 0, 18, 0];
        let c = self.bot.execute(self.lun, &cdb, Data::In(&mut buf)).map_err(Self::map_bot)?;
        if c.status != 0 || c.transferred < 3 {
            return Ok(Sense { key: 0, asc: 0, ascq: 0 });
        }
        let (asc, ascq) = if c.transferred >= 14 { (buf[12], buf[13]) } else { (0, 0) };
        Ok(Sense { key: buf[2] & 0x0F, asc, ascq })
    }

    /// One attempt. CHECK CONDITION is resolved to `Err(CheckCondition)` via
    /// REQUEST SENSE.
    fn once(&mut self, cdb: &[u8], mut data: Data<'_>) -> Result<Completion, MscErr> {
        let c = self.bot.execute(self.lun, cdb, data.reborrow()).map_err(Self::map_bot)?;
        if c.status == CSW_FAILED {
            let s = self.request_sense()?;
            return Err(s.into());
        }
        Ok(c)
    }

    fn retryable(e: &MscErr) -> bool {
        match e {
            MscErr::Transport(UsbErr::Disconnected) => false,
            MscErr::CheckCondition { key, .. } => *key != SK_ILLEGAL_REQUEST && *key != SK_DATA_PROTECT,
            MscErr::InvalidArg | MscErr::WriteProtected | MscErr::UnsupportedSectorSize(_) => false,
            _ => true,
        }
    }

    /// Error ladder: 3 attempts, then reset recovery, then 3 more.
    /// With `exact`, a data stage shorter than the buffer counts as failure.
    fn run(&mut self, cdb: &[u8], mut data: Data<'_>, exact: bool) -> Result<Completion, MscErr> {
        let want = match &data {
            Data::None => 0,
            Data::In(b) => b.len(),
            Data::Out(b) => b.len(),
        };
        // Lane loginhang: every failed attempt, the reset recovery and the
        // final outcome are logged (they used to be silent until the whole
        // ladder — up to 6 attempts of 3 transfers with a 5 s deadline each,
        // plus resets — had run out, i.e. minutes of a "hung" disk with nothing
        // on screen), and the ladder gives up once `LADDER_BUDGET_NS` has
        // passed, so a dead device turns into EIO in bounded time.
        let t0 = crate::platform::now_ns();
        let ms = |t: u64| crate::platform::now_ns().saturating_sub(t) / 1_000_000;
        let mut last = MscErr::Protocol;
        let mut attempt = 0u32;
        for round in 0..2 {
            for _ in 0..3 {
                attempt += 1;
                match self.once(cdb, data.reborrow()) {
                    Ok(c) if exact && (c.transferred != want || c.residue != 0) => {
                        last = MscErr::ShortTransfer;
                    }
                    Ok(c) => {
                        if attempt > 1 {
                            crate::usb_log!("[USB-MSC] lun {} cmd {:#04x}: succeeded on attempt {} after {} ms\n",
                                            self.lun, cdb[0], attempt, ms(t0));
                        }
                        return Ok(c);
                    }
                    Err(e) => {
                        if !Self::retryable(&e) {
                            return Err(e);
                        }
                        last = e;
                    }
                }
                crate::usb_log!("[USB-MSC] lun {} cmd {:#04x} lba-cdb {:02x?}: attempt {} failed: {:?} ({} ms into the command)\n",
                                self.lun, cdb[0], &cdb[1..cdb.len().min(10)], attempt, last, ms(t0));
                if crate::platform::now_ns().saturating_sub(t0) >= LADDER_BUDGET_NS {
                    crate::usb_log!("[USB-MSC] lun {} cmd {:#04x}: giving up after {} ms: {:?}\n",
                                    self.lun, cdb[0], ms(t0), last);
                    return Err(last);
                }
            }
            if round == 0 {
                crate::usb_log!("[USB-MSC] lun {}: Bulk-Only reset recovery after 3 failed attempts ({} ms)\n",
                                self.lun, ms(t0));
                if let Err(e) = self.bot.reset_recovery() {
                    crate::usb_log!("[USB-MSC] lun {}: reset recovery failed: {:?}; giving up after {} ms\n",
                                    self.lun, e, ms(t0));
                    return Err(last);
                }
            }
        }
        crate::usb_log!("[USB-MSC] lun {} cmd {:#04x}: failed after {} attempts, {} ms: {:?}\n",
                        self.lun, cdb[0], attempt, ms(t0), last);
        Err(last)
    }

    // ---- initialisation ---------------------------------------------------

    /// Probe LUNs and bring the first ready one online.
    pub fn init(&mut self) -> Result<(), MscErr> {
        let max_lun = self.bot.max_lun();
        let mut last = MscErr::NoDevice;
        for lun in 0..=max_lun {
            self.lun = lun;
            match self.init_lun() {
                Ok(()) => return Ok(()),
                Err(e @ MscErr::Transport(UsbErr::Disconnected)) => return Err(e),
                Err(e @ MscErr::UnsupportedSectorSize(_)) => last = e,
                Err(_) => {}
            }
        }
        Err(last)
    }

    fn init_lun(&mut self) -> Result<(), MscErr> {
        // INQUIRY
        let mut inq = [0u8; 36];
        let c = self.run(&[0x12, 0, 0, 0, 36, 0], Data::In(&mut inq), false)?;
        if c.transferred < 1 {
            return Err(MscErr::ShortTransfer);
        }
        // Qualifier != 0: no device on this LUN. Type: direct access / RBC.
        if inq[0] >> 5 != 0 || !matches!(inq[0] & 0x1F, 0x00 | 0x0E) {
            return Err(MscErr::NoDevice);
        }

        self.wait_ready()?;
        self.read_capacity()?;
        self.read_only = self.probe_write_protect();
        Ok(())
    }

    fn wait_ready(&mut self) -> Result<(), MscErr> {
        let mut waited = 0u32;
        let mut xfer_fail = 0u32;
        loop {
            match self.once(&[0u8; 6], Data::None) {
                Ok(_) => return Ok(()),
                Err(MscErr::CheckCondition { key, asc, .. })
                    if key == SK_UNIT_ATTENTION || (key == SK_NOT_READY && asc == 0x04) =>
                {
                    // 06/28, 06/29, 02/04/xx: retry after a delay.
                }
                Err(e @ MscErr::CheckCondition { .. }) => return Err(e),
                Err(e @ MscErr::Transport(UsbErr::Disconnected)) => return Err(e),
                Err(e) => {
                    xfer_fail += 1;
                    if xfer_fail >= 3 {
                        return Err(e);
                    }
                    let _ = self.bot.reset_recovery();
                }
            }
            if waited >= READY_TIMEOUT_MS {
                return Err(MscErr::NotReady);
            }
            (self.delay_ms)(READY_POLL_MS);
            waited += READY_POLL_MS;
        }
    }

    fn read_capacity(&mut self) -> Result<(), MscErr> {
        let mut r = [0u8; 8];
        self.run(&[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0], Data::In(&mut r), true)?;
        let last = u32::from_be_bytes([r[0], r[1], r[2], r[3]]);
        let mut bs = u32::from_be_bytes([r[4], r[5], r[6], r[7]]);
        let mut sectors = last as u64 + 1;
        if last == 0xFFFF_FFFF {
            let mut r16 = [0u8; 32];
            let mut cdb = [0u8; 16];
            cdb[0] = 0x9E;
            cdb[1] = 0x10;
            cdb[10..14].copy_from_slice(&32u32.to_be_bytes());
            let c = self.run(&cdb, Data::In(&mut r16), false)?;
            if c.transferred < 12 {
                return Err(MscErr::ShortTransfer);
            }
            let l = u64::from_be_bytes([r16[0], r16[1], r16[2], r16[3], r16[4], r16[5], r16[6], r16[7]]);
            bs = u32::from_be_bytes([r16[8], r16[9], r16[10], r16[11]]);
            sectors = l.checked_add(1).ok_or(MscErr::InvalidArg)?;
        }
        if bs != 512 && bs != 4096 {
            return Err(MscErr::UnsupportedSectorSize(bs));
        }
        self.sector_size = bs;
        self.sectors = sectors;
        Ok(())
    }

    fn probe_write_protect(&mut self) -> bool {
        let mut r = [0u8; 4];
        // MODE SENSE(6), all pages, allocation length 4. Single attempt:
        // any failure means "assume writable".
        match self.once(&[0x1A, 0, 0x3F, 0, 4, 0], Data::In(&mut r)) {
            Ok(c) if c.transferred >= 3 => r[2] & 0x80 != 0,
            Ok(_) => false,
            Err(MscErr::Transport(_)) | Err(MscErr::Protocol) => {
                // The failure may have left the pipes unusable; BOT already
                // recovered in the paths that need it.
                false
            }
            Err(_) => false,
        }
    }

    // ---- I/O --------------------------------------------------------------

    fn rw_cdb(write: bool, lba: u64, count: u32) -> ([u8; 16], usize) {
        let mut cdb = [0u8; 16];
        if lba + count as u64 > 0xFFFF_FFFF {
            cdb[0] = if write { 0x8A } else { 0x88 };
            cdb[2..10].copy_from_slice(&lba.to_be_bytes());
            cdb[10..14].copy_from_slice(&count.to_be_bytes());
            (cdb, 16)
        } else {
            cdb[0] = if write { 0x2A } else { 0x28 };
            cdb[2..6].copy_from_slice(&(lba as u32).to_be_bytes());
            cdb[7..9].copy_from_slice(&(count as u16).to_be_bytes());
            (cdb, 10)
        }
    }

    fn check_range(&self, lba: u64, len: usize) -> Result<u64, MscErr> {
        if self.sector_size == 0 || len % self.sector_size as usize != 0 {
            return Err(MscErr::InvalidArg);
        }
        let count = (len / self.sector_size as usize) as u64;
        match lba.checked_add(count) {
            Some(end) if end <= self.sectors => Ok(count),
            _ => Err(MscErr::InvalidArg),
        }
    }

    pub fn read_sectors(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), MscErr> {
        let total = self.check_range(lba, buf.len())?;
        let per = (MAX_CHUNK_BYTES / self.sector_size as usize) as u64;
        let ss = self.sector_size as usize;
        let mut done = 0u64;
        while done < total {
            let n = per.min(total - done);
            let (cdb, len) = Self::rw_cdb(false, lba + done, n as u32);
            let off = done as usize * ss;
            let chunk = &mut buf[off..off + n as usize * ss];
            self.run(&cdb[..len], Data::In(chunk), true)?;
            done += n;
        }
        Ok(())
    }

    pub fn write_sectors(&mut self, lba: u64, buf: &[u8]) -> Result<(), MscErr> {
        if self.read_only {
            return Err(MscErr::WriteProtected);
        }
        let total = self.check_range(lba, buf.len())?;
        let per = (MAX_CHUNK_BYTES / self.sector_size as usize) as u64;
        let ss = self.sector_size as usize;
        let mut done = 0u64;
        while done < total {
            let n = per.min(total - done);
            let (cdb, len) = Self::rw_cdb(true, lba + done, n as u32);
            let off = done as usize * ss;
            let chunk = &buf[off..off + n as usize * ss];
            self.run(&cdb[..len], Data::Out(chunk), true)?;
            done += n;
        }
        Ok(())
    }

    /// SYNCHRONIZE CACHE(10). Many USB sticks answer it with ILLEGAL REQUEST,
    /// or with CHECK CONDITION carrying NO SENSE / RECOVERED ERROR (the
    /// command completed but the bridge still flags it); none of these is a
    /// data-loss condition, so they count as success and the command is not
    /// issued again on this device.
    pub fn flush(&mut self) -> Result<(), MscErr> {
        if self.sync_cache_unsupported {
            return Ok(());
        }
        let saved = self.bot.timeout_ms;
        self.bot.timeout_ms = 30_000;
        let mut cdb = [0u8; 10];
        cdb[0] = 0x35;
        let r = self.run(&cdb, Data::None, false);
        self.bot.timeout_ms = saved;
        match r {
            Ok(_) => Ok(()),
            Err(MscErr::CheckCondition { key: SK_ILLEGAL_REQUEST | SK_NO_SENSE | SK_RECOVERED_ERROR, .. }) => {
                self.sync_cache_unsupported = true;
                Ok(())
            }
            Err(e) => Err(e),
        }
    }
}
