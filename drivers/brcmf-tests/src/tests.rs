use crate::brcmf::bcdc::{self, Brcmf};
use crate::brcmf::bus::SdioBus;
use crate::brcmf::chip::*;
use crate::brcmf::fw::{self, nvram_normalize, nvram_token_words, FwLoadError};
use crate::brcmf::fwil::{FwError, Fwil, RxSink};
use crate::brcmf::sdpcm::{self, Sdpcm, SdpcmError};
use crate::mock::*;

const NVRAM: &[u8] = b"# CYW43455 test nvram\r\n\
NVRAMRev=$Rev$\r\n\
  sromrev=11   \n\
\n\
boardflags=0x00480201 # trailing comment\n\
=novalue\n\
9bad=1\n\
noequals\n\
macaddr=00:90:4c:c5:12:38\n";

fn fake_code(len: usize) -> Vec<u8> {
    let mut v: Vec<u8> = (0..len).map(|i| (i * 7 + 3) as u8).collect();
    v[..4].copy_from_slice(&0xB8000_u32.to_le_bytes()); // reset vector
    v
}

fn attached() -> (MockChip, Chip) {
    let mut m = MockChip::new();
    let c = Chip::attach(&mut m).expect("attach");
    (m, c)
}

fn booted() -> Brcmf<MockChip> {
    let (mut m, mut c) = attached();
    fw::download(&mut m, &mut c, &fake_code(40_000), NVRAM).expect("download");
    Brcmf::new(m, c)
}

#[derive(Default)]
struct Sink {
    events: Vec<Vec<u8>>,
    data: Vec<Vec<u8>>,
}
impl RxSink for Sink {
    fn on_event(&mut self, f: &[u8]) {
        self.events.push(f.to_vec());
    }
    fn on_data(&mut self, f: &[u8]) {
        self.data.push(f.to_vec());
    }
}

fn eth(ethertype: u16, body: &[u8]) -> Vec<u8> {
    let mut f = vec![0xFF; 6];
    f.extend_from_slice(&[0x00, 0x90, 0x4c, 1, 2, 3]);
    f.extend_from_slice(&ethertype.to_be_bytes());
    f.extend_from_slice(body);
    f
}

#[test]
fn erom_scan_finds_cores() {
    let (_m, c) = attached();
    assert_eq!((c.chip_id, c.chip_rev), (0x4345, 6));
    let get = |id| c.core(id).unwrap_or_else(|| panic!("core {id:#x} missing"));
    assert_eq!(get(CORE_CHIPCOMMON), Core { id: CORE_CHIPCOMMON, rev: 43, base: CC_BASE, wrap: CC_BASE + WRAP_OFF });
    assert_eq!(get(CORE_80211).wrap, D11_BASE + WRAP_OFF, "D11 wrapper is a master wrapper");
    let cr4 = get(CORE_ARM_CR4);
    assert_eq!((cr4.base, cr4.wrap, cr4.rev), (CR4_BASE, CR4_BASE + WRAP_OFF, 7), "16K region and 64-bit desc skipped");
    assert_eq!(get(CORE_SDIO_DEV).base, SDIO_BASE);
    assert_eq!(get(CORE_INTERNAL_MEM).base, SOCRAM_BASE);
    assert!(c.core(0x367).is_none(), "wrapperless core skipped");
    assert_eq!(c.core_count(), 5);
}

#[test]
fn ram_geometry_43455() {
    let (_m, c) = attached();
    assert_eq!(c.ram_base, 0x198000);
    assert_eq!(c.ram_size, 0xC8000);
    assert!(c.sr_capable);
}

#[test]
fn attach_side_effects() {
    let (m, _c) = attached();
    assert_eq!(m.f0[&CCCR_IOEN] & 0x06, 0x02, "F1 on, F2 off");
    assert_eq!(m.f0[&CCCR_BRCM_CARDCTRL] & CARDCTRL_WLANRESET, CARDCTRL_WLANRESET);
    assert_eq!(m.regs[&(CC_BASE + CC_PMUCONTROL)] & PMU_CTL_RES_RELOAD, PMU_CTL_RES_RELOAD);
    assert_eq!(m.f1[&SBSDIO_FUNC1_CHIPCLKCSR], 0, "clock request dropped");
}

#[test]
fn attach_rejects_other_chip_and_dead_card() {
    let mut m = MockChip::new();
    m.regs.insert(CC_BASE, 0x4330 | (1 << 28));
    assert_eq!(Chip::attach(&mut m).unwrap_err(), ChipError::UnsupportedChip(0x4330));

    // A card whose F1 registers read back zero: CHIPCLKCSR mismatch, no hang.
    struct Dead;
    impl SdioBus for Dead {
        fn read8(&mut self, _: u8, _: u32) -> Result<u8, crate::brcmf::bus::BusError> { Ok(0) }
        fn write8(&mut self, _: u8, _: u32, _: u8) -> Result<(), crate::brcmf::bus::BusError> { Ok(()) }
        fn read_bytes(&mut self, _: u8, _: u32, _: bool, b: &mut [u8]) -> Result<(), crate::brcmf::bus::BusError> { b.fill(0); Ok(()) }
        fn write_bytes(&mut self, _: u8, _: u32, _: bool, _: &[u8]) -> Result<(), crate::brcmf::bus::BusError> { Ok(()) }
        fn set_block_size(&mut self, _: u8, _: u16) -> Result<(), crate::brcmf::bus::BusError> { Ok(()) }
        fn wait_card_irq(&mut self, _: u32) -> bool { false }
        fn delay_us(&mut self, _: u32) {}
    }
    assert!(Chip::attach(&mut Dead).is_err());
}

#[test]
fn window_is_cached() {
    let (mut m, mut c) = attached();
    c.bp.invalidate();
    let before = m.log.sbaddr_writes;
    for off in [0x20, 0x24, 0x28] {
        c.read32(&mut m, SDIO_BASE + off).unwrap();
    }
    assert_eq!(m.log.sbaddr_writes - before, 3, "one window programming = 3 byte writes");
    c.read32(&mut m, EROM_BASE).unwrap();
    assert_eq!(m.log.sbaddr_writes - before, 6);
}

#[test]
fn ram_write_splits_at_window() {
    let (mut m, mut c) = attached();
    let data: Vec<u8> = (0..0x9000u32).map(|i| (i ^ (i >> 8)) as u8).collect();
    let addr = RAMBASE_4345 + 0x7F00; // straddles 0x1A0000
    c.bp.ram_write(&mut m, addr, &data).unwrap();
    assert_eq!(m.ram_at(addr, data.len()), &data[..]);
    let mut back = vec![0u8; data.len()];
    c.bp.ram_read(&mut m, addr, &mut back).unwrap();
    assert_eq!(back, data);
}

#[test]
fn core_reset_and_disable() {
    let (mut m, mut c) = attached();
    let d11 = c.core(CORE_80211).unwrap();
    assert!(!c.is_core_up(&mut m, d11).unwrap());
    c.core_reset(&mut m, d11, 0, 0, D11_IOCTL_PHYCLOCKEN).unwrap();
    assert!(c.is_core_up(&mut m, d11).unwrap());
    assert_eq!(m.regs[&(d11.wrap + BCMA_IOCTL)], D11_IOCTL_PHYCLOCKEN | BCMA_IOCTL_CLK);
    c.core_disable(&mut m, d11, 0, 0).unwrap();
    assert!(!c.is_core_up(&mut m, d11).unwrap());
    assert_eq!(m.regs[&(d11.wrap + BCMA_RESET_CTL)], BCMA_RESET_CTL_RESET);
}

#[test]
fn clocks_and_kso() {
    let (mut m, mut c) = attached();
    let v = c.clk_request(&mut m, true).unwrap();
    assert_eq!(v & SBSDIO_AVBITS, SBSDIO_AVBITS);
    c.kso_control(&mut m, true).unwrap();
    assert_eq!(m.f1[&SBSDIO_FUNC1_SLEEPCSR], SLEEPCSR_KSO | SLEEPCSR_DEVON);
    c.kso_control(&mut m, false).unwrap();
    // HT never comes: bounded timeout, not a hang.
    struct NoHt(MockChip);
    impl SdioBus for NoHt {
        fn read8(&mut self, f: u8, a: u32) -> Result<u8, crate::brcmf::bus::BusError> {
            let v = self.0.read8(f, a)?;
            Ok(if f == 1 && a == SBSDIO_FUNC1_CHIPCLKCSR { v & !SBSDIO_HT_AVAIL } else { v })
        }
        fn write8(&mut self, f: u8, a: u32, v: u8) -> Result<(), crate::brcmf::bus::BusError> { self.0.write8(f, a, v) }
        fn read_bytes(&mut self, f: u8, a: u32, i: bool, b: &mut [u8]) -> Result<(), crate::brcmf::bus::BusError> { self.0.read_bytes(f, a, i, b) }
        fn write_bytes(&mut self, f: u8, a: u32, i: bool, b: &[u8]) -> Result<(), crate::brcmf::bus::BusError> { self.0.write_bytes(f, a, i, b) }
        fn set_block_size(&mut self, f: u8, s: u16) -> Result<(), crate::brcmf::bus::BusError> { self.0.set_block_size(f, s) }
        fn wait_card_irq(&mut self, t: u32) -> bool { self.0.wait_card_irq(t) }
        fn delay_us(&mut self, u: u32) { self.0.delay_us(u) }
    }
    let mut n = NoHt(m);
    assert_eq!(c.clk_request(&mut n, true).unwrap_err(), ChipError::HtTimeout);
    assert!(n.0.now_us < 100_000);
}

#[test]
fn nvram_normalisation() {
    let blob = nvram_normalize(NVRAM);
    let words = nvram_token_words(&blob).unwrap() as usize;
    assert_eq!(words * 4 + 4, blob.len());
    assert_eq!(blob.len() % 4, 0);
    let body = &blob[..words * 4];
    let entries: Vec<&[u8]> = body.split(|&c| c == 0).filter(|e| !e.is_empty()).collect();
    let want: Vec<&[u8]> = vec![
        b"NVRAMRev=$Rev$",
        b"sromrev=11",
        b"boardflags=0x00480201",
        b"macaddr=00:90:4c:c5:12:38",
        b"boardrev=0xff",
    ];
    assert_eq!(entries, want);
    // Entries end with a double NUL before padding.
    let used: usize = want.iter().map(|e| e.len() + 1).sum();
    assert_eq!(body[used], 0);
    // boardrev present: no default added.
    let b2 = nvram_normalize(b"boardrev=0x1202\n");
    assert!(!b2.windows(13).any(|w| w == b"boardrev=0xff"));
    let tok = u32::from_le_bytes(b2[b2.len() - 4..].try_into().unwrap());
    assert_eq!(tok, (!(5u32) << 16) | 5, "17 bytes + NUL -> 20 -> 5 words");
}

#[test]
fn download_places_code_nvram_and_starts_fw() {
    let (mut m, mut c) = attached();
    let code = fake_code(70_000); // > 2 windows
    let rep = fw::download(&mut m, &mut c, &code, NVRAM).unwrap();
    assert!(m.fw_running && m.fw_nvram_ok);
    assert_eq!(m.ram_at(RAMBASE_4345, code.len()), &code[..]);
    let nv = nvram_normalize(NVRAM);
    assert_eq!(rep.nvram_addr, RAMBASE_4345 + 0xC8000 - nv.len() as u32);
    assert_eq!(m.ram_at(rep.nvram_addr, nv.len()), &nv[..]);
    assert_eq!(rep.rstvec, 0xB8000);
    assert!(rep.sr_enabled);
    assert_eq!(m.f0[&CCCR_IOEN] & 0x06, 0x06);
    assert_eq!(m.f0[&CCCR_IENX] & 0x07, 0x07);
    assert_eq!(m.regs[&(SDIO_BASE + SD_HOSTINTMASK)], fw::HOSTINTMASK);
    assert_eq!(m.regs[&(SDIO_BASE + SD_TOSBMAILBOXDATA)], 4 << 16);
    assert_eq!(m.f1[&SBSDIO_WATERMARK], 8);
    // D11 held with PHY clock, CR4 running.
    let cr4 = c.core(CORE_ARM_CR4).unwrap();
    assert_eq!(m.regs[&(cr4.wrap + BCMA_IOCTL)], BCMA_IOCTL_CLK);
}

#[test]
fn download_rejects_bad_images() {
    let (mut m, mut c) = attached();
    assert_eq!(fw::download(&mut m, &mut c, &[1, 2], NVRAM).unwrap_err(), FwLoadError::EmptyImage);
    assert_eq!(fw::download(&mut m, &mut c, &fake_code(0xC8000), NVRAM).unwrap_err(), FwLoadError::TooBig);
    // Firmware that never starts (wrong reset vector at 0 vs image): F2 times out, bounded.
    let (mut m, mut c) = attached();
    m.fw_nvram_ok = false;
    let code = fake_code(4096);
    struct Corrupt(MockChip);
    impl SdioBus for Corrupt {
        fn read8(&mut self, f: u8, a: u32) -> Result<u8, crate::brcmf::bus::BusError> { self.0.read8(f, a) }
        fn write8(&mut self, f: u8, a: u32, v: u8) -> Result<(), crate::brcmf::bus::BusError> { self.0.write8(f, a, v) }
        fn read_bytes(&mut self, f: u8, a: u32, i: bool, b: &mut [u8]) -> Result<(), crate::brcmf::bus::BusError> { self.0.read_bytes(f, a, i, b) }
        fn write_bytes(&mut self, f: u8, a: u32, i: bool, b: &[u8]) -> Result<(), crate::brcmf::bus::BusError> {
            // Drop the reset-vector write at backplane 0.
            if f == 1 && b.len() == 4 && (a & 0x7FFF) == 0 && self.0.window() == 0 {
                return Ok(());
            }
            self.0.write_bytes(f, a, i, b)
        }
        fn set_block_size(&mut self, f: u8, s: u16) -> Result<(), crate::brcmf::bus::BusError> { self.0.set_block_size(f, s) }
        fn wait_card_irq(&mut self, t: u32) -> bool { self.0.wait_card_irq(t) }
        fn delay_us(&mut self, u: u32) { self.0.delay_us(u) }
    }
    let mut cm = Corrupt(m);
    assert_eq!(fw::download(&mut cm, &mut c, &code, NVRAM).unwrap_err(), FwLoadError::F2Timeout);
    assert!(cm.0.now_us <= 3_100_000);
}

#[test]
fn sdpcm_header_roundtrip() {
    let mut h = [0u8; 12];
    sdpcm::pack_header(&mut h, 100, 7, sdpcm::CHAN_EVENT, 12);
    h[8] = 0x03; // fcmask
    h[9] = 0x2A; // window
    let p = sdpcm::parse_header(&h).unwrap().unwrap();
    assert_eq!((p.len, p.seq, p.chan, p.doff, p.fcmask, p.window), (100, 7, 1, 12, 3, 0x2A));
    assert_eq!(sdpcm::parse_header(&[0u8; 12]).unwrap(), None);
    h[2] ^= 1;
    assert!(matches!(sdpcm::parse_header(&h), Err(SdpcmError::BadTag { .. })));
    sdpcm::pack_header(&mut h, 100, 7, 0, 8);
    assert!(matches!(sdpcm::parse_header(&h), Err(SdpcmError::BadOffset { .. })));
}

#[test]
fn sdpcm_credit_and_padding() {
    let mut s = Sdpcm::new(512);
    assert!(s.tx_credit());
    s.tx_seq = 1;
    assert!(!s.tx_credit());
    s.tx_max = 9;
    assert!(s.tx_credit());
    s.tx_seq = 255;
    s.tx_max = 3; // wraps
    assert!(s.tx_credit());
    assert_eq!(s.padded_len(13), 16);
    assert_eq!(s.padded_len(512), 512);
    assert_eq!(s.padded_len(513), 1024);
}

/// The end-to-end path: attach -> download -> BCDC "ver" iovar over SDPCM.
#[test]
fn download_then_ver_iovar() {
    let mut d = booted();
    let mut buf = [0u8; 256];
    let n = d.iovar_get("ver", &mut buf).expect("ver");
    assert_eq!(n, 256);
    let s = std::str::from_utf8(&buf[..buf.iter().position(|&c| c == 0).unwrap()]).unwrap();
    assert_eq!(s, VERSION);
    assert_eq!(d.sdpcm.fw_sdpcm_ver, Some(4), "FWREADY mailbox handled");
    assert_eq!(d.bus.log.ctl_reqs, vec![(bcdc::WLC_GET_VAR, "ver".to_string())]);
    // Window opened by the response header.
    assert_eq!(d.sdpcm.tx_max, 8);
    // A second request uses the next id and sequence.
    d.iovar_get("ver", &mut buf).unwrap();
    assert_eq!(d.bus.log.host_tx_seq, vec![0, 1]);
    assert_eq!(d.sdpcm.stats.rx_badseq, 0);
}

#[test]
fn fw_error_status_and_cmd() {
    let mut d = booted();
    let mut b = [0u8; 4];
    assert_eq!(d.iovar_get("nosuchvar", &mut b).unwrap_err(), FwError::Status(-23));
    assert_eq!(d.cmd_get(1, &mut b).unwrap(), 4);
    assert_eq!(u32::from_le_bytes(b), 1);
    assert_eq!(d.cmd_set(99, &[0; 4]).unwrap_err(), FwError::Status(-23));
    assert_eq!(d.iovar_set("x", &[0; 1600]).unwrap_err(), FwError::TooLong);
}

#[test]
fn clm_blob_chunks() {
    let mut d = booted();
    let clm: Vec<u8> = (0..3000u32).map(|i| i as u8).collect();
    assert_eq!(fw::clm_load(&mut d, &clm).unwrap(), 3);
    let v = 1 << 12;
    assert_eq!(d.bus.log.clm_flags, vec![v | 0x2, v, v | 0x4]);
    let mut d = booted();
    assert_eq!(fw::clm_load(&mut d, &[7; 10]).unwrap(), 1);
    assert_eq!(d.bus.log.clm_flags, vec![(1 << 12) | 0x6]);
}

#[test]
fn events_and_data_routed() {
    let mut d = booted();
    let ev = eth(0x886c, &[0x80, 0x01, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    let big = eth(0x0800, &vec![0x5A; 1500]); // longer than FIRSTREAD, block-rounded rest
    d.bus.fw_send_eth(sdpcm::CHAN_EVENT, &ev, 0);
    d.bus.fw_send_eth(sdpcm::CHAN_DATA, &big, 2); // with 8 bytes of fw signals
    d.bus.fw_send(sdpcm::CHAN_GLOM, &[0; 8]);
    let mut sink = Sink::default();
    d.poll(&mut sink, 1000).unwrap();
    assert_eq!(sink.events, vec![ev]);
    assert_eq!(sink.data, vec![big]);
    assert_eq!(d.sdpcm.stats.rx_glom_dropped, 1);
    assert!(!d.sdpcm.rx_pending);
    // Nothing queued: poll returns immediately.
    let mut sink = Sink::default();
    d.poll(&mut sink, 1000).unwrap();
    assert!(sink.events.is_empty() && sink.data.is_empty());
}

#[test]
fn events_during_ioctl_are_kept() {
    let mut d = booted();
    let ev = eth(0x886c, &[1; 20]);
    d.bus.fw_send_eth(sdpcm::CHAN_EVENT, &ev, 0);
    let mut buf = [0u8; 64];
    d.iovar_get("ver", &mut buf).unwrap();
    assert_eq!(d.pending_frames(), 1);
    let mut sink = Sink::default();
    d.poll(&mut sink, 0).unwrap();
    assert_eq!(sink.events, vec![ev]);
}

#[test]
fn tx_data_framing() {
    let mut d = booted();
    let mut b = [0u8; 4];
    d.iovar_get("ver", &mut b).unwrap(); // opens the window
    let f = eth(0x0800, &[9; 60]);
    d.tx_data(&f).unwrap();
    assert_eq!(d.bus.log.data_tx, vec![f]);
}

#[test]
fn tx_blocks_without_credit() {
    let mut d = booted();
    d.sdpcm.tx_seq = 1; // window (tx_max = 1) exhausted, no frame will open it
    let t0 = d.bus.now_us;
    assert_eq!(d.tx_data(&eth(0x0800, &[0; 46])).unwrap_err(), FwError::Timeout);
    assert!(d.bus.now_us - t0 <= 20_000);
}

#[test]
fn bad_frame_is_dropped_and_rx_recovers() {
    let mut d = booted();
    d.bus.corrupt_next = true;
    d.bus.fw_send_eth(sdpcm::CHAN_DATA, &eth(0x0800, &[1; 40]), 0);
    let good = eth(0x0800, &[2; 40]);
    d.bus.fw_send_eth(sdpcm::CHAN_DATA, &good, 0);
    let mut sink = Sink::default();
    d.poll(&mut sink, 1000).unwrap();
    assert_eq!(sink.data, vec![good]);
    assert_eq!(d.sdpcm.stats.rx_badhdr, 1);
    assert_eq!(d.bus.f0[&CCCR_ABORT], 2, "F2 aborted");
}

#[test]
fn ioctl_times_out_when_fw_silent() {
    let mut d = booted();
    d.ctl_timeout_us = 50_000;
    d.bus.mute = true;
    let t0 = d.bus.now_us;
    let mut b = [0u8; 4];
    assert_eq!(d.iovar_get("ver", &mut b).unwrap_err(), FwError::Timeout);
    let dt = d.bus.now_us - t0;
    assert!((50_000..60_000).contains(&dt), "waited {dt} us");
}
