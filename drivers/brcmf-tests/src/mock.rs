//! A scripted CYW43455 behind the SdioBus seam: CCCR, F1 registers, a
//! backplane with an EROM table, AI wrappers, the CR4 TCM bank registers,
//! the SDIO core's interrupt/mailbox registers, TCM as a byte array, and a
//! fake firmware that starts when the CR4 is released and answers BCDC
//! control requests over F2.

use std::collections::{HashMap, VecDeque};

use crate::brcmf::bcdc;
use crate::brcmf::bus::{BusError, SdioBus};
use crate::brcmf::chip::*;
use crate::brcmf::sdpcm;

pub const CC_BASE: u32 = 0x1800_0000;
pub const D11_BASE: u32 = 0x1800_1000;
pub const CR4_BASE: u32 = 0x1800_2000;
pub const SDIO_BASE: u32 = 0x1800_4000;
pub const SOCRAM_BASE: u32 = 0x1800_5000;
pub const WRAP_OFF: u32 = 0x0010_0000;
pub const EROM_BASE: u32 = 0x180F_C000;
pub const RAM_SIZE: usize = 0xC8000;
pub const VERSION: &str = "wl0: Oct 26 2026 00:00:00 version 7.45.241 (mock) FWID 01-00000000";

fn comp1(id: u16) -> u32 {
    // designer 0x4BF (Broadcom), part number, class, VALID|COMPONENT
    (0x4BF << 20) | ((id as u32) << 8) | 0x1
}
fn comp2(rev: u8, nmw: u32, nsw: u32) -> u32 {
    ((rev as u32) << 24) | (nsw << 19) | (nmw << 14) | 0x1
}
fn addr_desc(base: u32, stype: u32, size: u32) -> u32 {
    base | (stype << 6) | (size << 4) | 0x5
}
const MPORT: u32 = 0x3;

/// EROM like a real 4345: CC, D11, CR4 (with a master port and a 64-bit
/// address descriptor), a size-descriptor region, SDIO, SOCRAM, PMU.
pub fn erom_table() -> Vec<u32> {
    let mut t = vec![
        comp1(CORE_CHIPCOMMON), comp2(43, 0, 1),
        addr_desc(CC_BASE, 0, 0), addr_desc(CC_BASE + WRAP_OFF, 2, 0),
        comp1(CORE_80211), comp2(42, 1, 1), MPORT,
        addr_desc(D11_BASE, 0, 1), addr_desc(D11_BASE + WRAP_OFF, 3, 0),
        comp1(CORE_ARM_CR4), comp2(7, 1, 0), MPORT | 0x10,
        // a 16K region with an explicit size descriptor, then a 64-bit address
        addr_desc(0x1900_0000, 0, 3), 0x0001_0000 | 0x0,
        addr_desc(CR4_BASE, 0, 0) | 0x8, 0x0000_0000,
        addr_desc(CR4_BASE + WRAP_OFF, 3, 0),
        0x0, // empty
        comp1(CORE_SDIO_DEV), comp2(21, 0, 1),
        addr_desc(SDIO_BASE, 0, 0), addr_desc(SDIO_BASE + WRAP_OFF, 2, 0),
        comp1(CORE_INTERNAL_MEM), comp2(23, 0, 1),
        addr_desc(SOCRAM_BASE, 0, 0), addr_desc(SOCRAM_BASE + WRAP_OFF, 2, 0),
        // PMU without wrappers: kept, no address descriptors follow
        comp1(CORE_PMU), comp2(28, 0, 0),
        // a core without wrappers that is skipped
        comp1(0x367), comp2(1, 0, 0),
    ];
    t.push(0xF); // EOT
    t
}

#[derive(Default)]
pub struct Log {
    pub sbaddr_writes: u32,
    pub ctl_reqs: Vec<(u32, String)>,
    pub clm_flags: Vec<u16>,
    pub data_tx: Vec<Vec<u8>>,
    pub host_tx_seq: Vec<u8>,
}

pub struct MockChip {
    pub f0: HashMap<u32, u8>,
    pub f1: HashMap<u32, u8>,
    pub regs: HashMap<u32, u32>,
    pub ram: Vec<u8>,
    pub erom: Vec<u32>,
    pub bankidx: u32,
    pub fw_running: bool,
    pub fw_nvram_ok: bool,
    pub now_us: u64,
    /// Frames the firmware queued for the host (whole SDPCM frames).
    pub rxq: VecDeque<Vec<u8>>,
    rx_cursor: usize,
    fw_seq: u8,
    pub window_adv: u8,
    pub log: Log,
    /// sr engine bit in PMU chipcontrol 3
    pub sr: bool,
    /// Corrupt the hw tag of the next queued frame.
    pub corrupt_next: bool,
    /// Firmware swallows control requests.
    pub mute: bool,
}

impl MockChip {
    pub fn new() -> Self {
        let mut m = MockChip {
            f0: HashMap::new(),
            f1: HashMap::new(),
            regs: HashMap::new(),
            ram: vec![0; RAM_SIZE],
            erom: erom_table(),
            bankidx: 0,
            fw_running: false,
            fw_nvram_ok: false,
            now_us: 0,
            rxq: VecDeque::new(),
            rx_cursor: 0,
            fw_seq: 0,
            window_adv: 8,
            log: Log::default(),
            sr: true,
            corrupt_next: false,
            mute: false,
        };
        m.regs.insert(CC_BASE + CC_CHIPID, 0x4345 | (6 << 16) | (1 << 28));
        m.regs.insert(CC_BASE + CC_EROMPTR, EROM_BASE);
        m.regs.insert(CR4_BASE + ARMCR4_CAP, 0x11); // one A bank, one B bank
        // CR4 starts in reset, the D11 too.
        m.regs.insert(CR4_BASE + WRAP_OFF + BCMA_RESET_CTL, 1);
        m.regs.insert(D11_BASE + WRAP_OFF + BCMA_RESET_CTL, 1);
        m
    }

    pub fn window(&self) -> u32 {
        let b = |a| *self.f1.get(&a).unwrap_or(&0) as u32;
        (b(SBSDIO_FUNC1_SBADDRLOW) << 8) | (b(SBSDIO_FUNC1_SBADDRLOW + 1) << 16) | (b(SBSDIO_FUNC1_SBADDRLOW + 2) << 24)
    }

    fn in_ram(a: u32) -> bool {
        (RAMBASE_4345..RAMBASE_4345 + RAM_SIZE as u32).contains(&a)
    }

    pub fn ram_at(&self, a: u32, n: usize) -> &[u8] {
        let o = (a - RAMBASE_4345) as usize;
        &self.ram[o..o + n]
    }

    fn bp_read32(&mut self, a: u32) -> u32 {
        if (EROM_BASE..EROM_BASE + 4 * self.erom.len() as u32).contains(&a) {
            return self.erom[((a - EROM_BASE) / 4) as usize];
        }
        if a == CR4_BASE + ARMCR4_BANKINFO {
            return if self.bankidx == 0 { 0x3F } else { 0x23 }; // 512K + 288K = 800K
        }
        if a == CC_BASE + CC_CHIPCONTROL_DATA {
            return if self.sr { 1 << 2 } else { 0 };
        }
        *self.regs.get(&a).unwrap_or(&0)
    }

    fn bp_write32(&mut self, a: u32, v: u32) {
        if a == SDIO_BASE + SD_INTSTATUS {
            let cur = self.bp_read32(a);
            self.regs.insert(a, cur & !v);
            return;
        }
        if a == CR4_BASE + ARMCR4_BANKIDX {
            self.bankidx = v;
        }
        self.regs.insert(a, v);
        if a == CR4_BASE + WRAP_OFF + BCMA_IOCTL || a == CR4_BASE + WRAP_OFF + BCMA_RESET_CTL {
            self.check_cr4();
        }
    }

    fn check_cr4(&mut self) {
        let ioctl = self.bp_read32(CR4_BASE + WRAP_OFF + BCMA_IOCTL);
        let rst = self.bp_read32(CR4_BASE + WRAP_OFF + BCMA_RESET_CTL);
        let running = rst == 0 && ioctl & ARMCR4_IOCTL_CPUHALT == 0 && ioctl & BCMA_IOCTL_CLK != 0;
        if running && !self.fw_running {
            // "Boot": the image must start with the reset vector we got at 0,
            // and the nvram token must sit in the last word of RAM.
            let rstvec = self.bp_read32(0);
            let img = u32::from_le_bytes(self.ram[0..4].try_into().unwrap());
            let tok = u32::from_le_bytes(self.ram[RAM_SIZE - 4..].try_into().unwrap());
            let words = tok & 0xFFFF;
            self.fw_nvram_ok = tok >> 16 == (!words & 0xFFFF) && words > 0;
            if rstvec == img && self.fw_nvram_ok {
                self.fw_running = true;
                // Firmware announces itself through the host mailbox.
                self.regs.insert(SDIO_BASE + SD_TOHOSTMAILBOXDATA, (4 << 16) | sdpcm::HMB_DATA_FWREADY);
                self.raise(sdpcm::I_HMB_HOST_INT);
            }
        }
    }

    fn raise(&mut self, bits: u32) {
        let v = self.bp_read32(SDIO_BASE + SD_INTSTATUS) | bits;
        self.regs.insert(SDIO_BASE + SD_INTSTATUS, v);
    }

    /// Queues a frame from the firmware on `chan`.
    pub fn fw_send(&mut self, chan: u8, payload: &[u8]) {
        let len = sdpcm::HDRLEN + payload.len();
        let mut f = vec![0u8; len];
        sdpcm::pack_header(&mut f, len as u16, self.fw_seq, chan, sdpcm::HDRLEN as u8);
        let last = *self.log.host_tx_seq.last().unwrap_or(&0);
        f[9] = last.wrapping_add(self.window_adv); // window
        f[12..].copy_from_slice(payload);
        if self.corrupt_next {
            f[2] ^= 0x10;
            self.corrupt_next = false;
        }
        self.fw_seq = self.fw_seq.wrapping_add(1);
        self.rxq.push_back(f);
        self.raise(sdpcm::I_HMB_FRAME_IND);
    }

    /// Queues an Ethernet frame on the event or data channel with a BCDC header.
    pub fn fw_send_eth(&mut self, chan: u8, eth: &[u8], doff_words: u8) {
        let mut p = vec![0x20, 0, 0, doff_words];
        p.extend(std::iter::repeat(0xEE).take(doff_words as usize * 4));
        p.extend_from_slice(eth);
        self.fw_send(chan, &p);
    }

    fn fw_rx(&mut self, frame: &[u8]) {
        let len = u16::from_le_bytes([frame[0], frame[1]]) as usize;
        let chk = u16::from_le_bytes([frame[2], frame[3]]);
        assert_eq!(len as u16 ^ chk, 0xFFFF, "host sent a bad tag");
        assert!(frame.len() % 4 == 0, "F2 write not 4-byte padded");
        let seq = frame[4];
        let chan = frame[5] & 0xF;
        let doff = frame[7] as usize;
        self.log.host_tx_seq.push(seq);
        let p = &frame[doff..len];
        match chan {
            sdpcm::CHAN_CONTROL => self.fw_dcmd(p),
            sdpcm::CHAN_DATA => {
                assert_eq!(p[0], 0x20);
                self.log.data_tx.push(p[4 + p[3] as usize * 4..].to_vec());
            }
            _ => panic!("host tx on channel {chan}"),
        }
    }

    fn fw_dcmd(&mut self, p: &[u8]) {
        let u = |o: usize| u32::from_le_bytes(p[o..o + 4].try_into().unwrap());
        let (cmd, len, flags) = (u(0), u(4) as usize, u(8));
        assert_eq!(p.len(), bcdc::DCMD_HDR + len);
        let body = &p[bcdc::DCMD_HDR..];
        let name_end = body.iter().position(|&c| c == 0).unwrap_or(0);
        let name = String::from_utf8_lossy(&body[..name_end]).into_owned();
        self.log.ctl_reqs.push((cmd, name.clone()));
        if self.mute {
            return;
        }
        let mut out = vec![0u8; len];
        let mut status = 0i32;
        match (cmd, name.as_str()) {
            (bcdc::WLC_GET_VAR, "ver") => {
                let n = VERSION.len().min(len);
                out[..n].copy_from_slice(&VERSION.as_bytes()[..n]);
            }
            (bcdc::WLC_GET_VAR, "clmload_status") => out[..4].copy_from_slice(&0u32.to_le_bytes()),
            (bcdc::WLC_SET_VAR, "clmload") => {
                let d = &body[name_end + 1..];
                let flag = u16::from_le_bytes([d[0], d[1]]);
                let clen = u32::from_le_bytes(d[4..8].try_into().unwrap()) as usize;
                assert_eq!(u16::from_le_bytes([d[2], d[3]]), 2, "dload_type CLM");
                assert_eq!(clen, d.len() - 12);
                self.log.clm_flags.push(flag);
            }
            (1, _) => out[..4].copy_from_slice(&1u32.to_le_bytes()), // WLC_GET_VERSION
            _ => status = -23, // BCME_UNSUPPORTED
        }
        let mut r = vec![0u8; bcdc::DCMD_HDR];
        let rflags = flags | if status != 0 { bcdc::BCDC_DCMD_ERROR } else { 0 };
        bcdc::pack_dcmd(&mut r, cmd, len as u32, rflags, status);
        r.extend_from_slice(&out);
        self.fw_send(sdpcm::CHAN_CONTROL, &r);
    }
}

impl SdioBus for MockChip {
    fn read8(&mut self, func: u8, addr: u32) -> Result<u8, BusError> {
        Ok(match func {
            0 if addr == CCCR_IORDY => {
                let en = *self.f0.get(&CCCR_IOEN).unwrap_or(&0);
                (en & 0x02) | if self.fw_running { en & 0x04 } else { 0 }
            }
            0 => *self.f0.get(&addr).unwrap_or(&0),
            1 => *self.f1.get(&addr).unwrap_or(&0),
            _ => return Err(BusError::Io),
        })
    }

    fn write8(&mut self, func: u8, addr: u32, val: u8) -> Result<(), BusError> {
        match func {
            0 => {
                self.f0.insert(addr, val);
            }
            1 => {
                let v = match addr {
                    SBSDIO_FUNC1_CHIPCLKCSR => {
                        let mut v = val & !SBSDIO_AVBITS;
                        if val & (SBSDIO_ALP_AVAIL_REQ | SBSDIO_FORCE_ALP | SBSDIO_HT_AVAIL_REQ | SBSDIO_FORCE_HT) != 0 {
                            v |= SBSDIO_ALP_AVAIL;
                        }
                        if val & (SBSDIO_HT_AVAIL_REQ | SBSDIO_FORCE_HT) != 0 {
                            v |= SBSDIO_HT_AVAIL;
                        }
                        v
                    }
                    SBSDIO_FUNC1_FRAMECTRL => {
                        // RF_TERM: the rest of the current rx frame is discarded.
                        if val & SFC_RF_TERM != 0 && self.rx_cursor > 0 {
                            self.rxq.pop_front();
                            self.rx_cursor = 0;
                        }
                        0
                    }
                    SBSDIO_FUNC1_SLEEPCSR => {
                        if val & SLEEPCSR_KSO != 0 { SLEEPCSR_KSO | SLEEPCSR_DEVON } else { 0 }
                    }
                    a if (SBSDIO_FUNC1_SBADDRLOW..SBSDIO_FUNC1_SBADDRLOW + 3).contains(&a) => {
                        self.log.sbaddr_writes += 1;
                        val
                    }
                    _ => val,
                };
                self.f1.insert(addr, v);
            }
            _ => return Err(BusError::Io),
        }
        Ok(())
    }

    fn read_bytes(&mut self, func: u8, addr: u32, incr: bool, buf: &mut [u8]) -> Result<(), BusError> {
        match func {
            1 => {
                assert!(incr && addr & SB_ACCESS_2_4B_FLAG != 0);
                let a = self.window() | (addr & SB_OFT_ADDR_MASK);
                assert!((addr & SB_OFT_ADDR_MASK) as usize + buf.len() <= 0x8000, "crossed window");
                if Self::in_ram(a) {
                    let o = (a - RAMBASE_4345) as usize;
                    buf.copy_from_slice(&self.ram[o..o + buf.len()]);
                } else {
                    assert_eq!(buf.len(), 4, "register access must be 32-bit");
                    buf.copy_from_slice(&self.bp_read32(a).to_le_bytes());
                }
            }
            2 => {
                assert!(!incr && self.fw_running);
                assert_eq!(self.window(), CC_BASE & SB_WINDOW_MASK);
                buf.fill(0);
                if let Some(f) = self.rxq.front() {
                    let n = buf.len().min(f.len().saturating_sub(self.rx_cursor));
                    buf[..n].copy_from_slice(&f[self.rx_cursor..self.rx_cursor + n]);
                    self.rx_cursor += buf.len();
                    if self.rx_cursor >= f.len() {
                        self.rxq.pop_front();
                        self.rx_cursor = 0;
                    }
                }
            }
            _ => return Err(BusError::Io),
        }
        Ok(())
    }

    fn write_bytes(&mut self, func: u8, addr: u32, incr: bool, buf: &[u8]) -> Result<(), BusError> {
        match func {
            1 => {
                assert!(incr && addr & SB_ACCESS_2_4B_FLAG != 0);
                assert!((addr & SB_OFT_ADDR_MASK) as usize + buf.len() <= 0x8000, "crossed window");
                let a = self.window() | (addr & SB_OFT_ADDR_MASK);
                if Self::in_ram(a) {
                    let o = (a - RAMBASE_4345) as usize;
                    self.ram[o..o + buf.len()].copy_from_slice(buf);
                } else {
                    assert_eq!(buf.len(), 4, "register access must be 32-bit");
                    self.bp_write32(a, u32::from_le_bytes(buf.try_into().unwrap()));
                }
            }
            2 => {
                assert!(!incr && self.fw_running);
                assert_eq!(self.window(), CC_BASE & SB_WINDOW_MASK);
                self.fw_rx(buf);
            }
            _ => return Err(BusError::Io),
        }
        Ok(())
    }

    fn set_block_size(&mut self, func: u8, size: u16) -> Result<(), BusError> {
        self.f0.insert(0x110 + 0x100 * (func as u32 - 1), size as u8);
        Ok(())
    }

    fn wait_card_irq(&mut self, timeout_us: u32) -> bool {
        let st = self.bp_read32(SDIO_BASE + SD_INTSTATUS) & self.bp_read32(SDIO_BASE + SD_HOSTINTMASK);
        if st == 0 {
            self.now_us += timeout_us as u64;
        }
        st != 0
    }

    fn delay_us(&mut self, us: u32) {
        self.now_us += us as u64;
    }
}
