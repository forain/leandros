use crate::msc::bot::{Bot, BotErr, Data, CSW_SIG};
use crate::msc::scsi::{MscErr, ScsiDisk};
use crate::msc::transport::{BulkTransport, UsbErr};
use std::collections::HashMap;

#[derive(Default)]
struct Faults {
    stall_data: bool,
    stall_csw: bool,
    bad_sig: bool,
    tag_mismatch: bool,
    phase_error: bool,
    short_read: bool,
    /// Number of upcoming TURs answered with UNIT ATTENTION 06/29/00.
    ua_turs: u32,
    /// Number of upcoming TURs answered with NOT READY 02/04/01.
    not_ready_turs: u32,
    /// Next READ/WRITE answers CHECK CONDITION (03/11/00 medium error).
    check_once: bool,
    /// Every TUR is NOT READY forever.
    never_ready: bool,
    max_lun_stall: bool,
    /// SYNCHRONIZE CACHE answers CHECK CONDITION with this sense key.
    sync_key: Option<u8>,
}

enum Stage {
    Idle,
    DataIn(Vec<u8>),
    DataOut { need: usize, lba: u64 },
    Csw { residue: u32, status: u8 },
}

struct Mock {
    ss: usize,
    sectors: u64,
    store: HashMap<u64, Vec<u8>>,
    wp: bool,
    max_lun: u8,
    /// LUN 0 has no medium when `max_lun > 0`.
    lun0_empty: bool,
    cap10_saturates: bool,
    f: Faults,
    stage: Stage,
    sense: [u8; 3],
    tag: u32,
    lun: u8,
    // observations
    resets: u32,
    clears_in: u32,
    clears_out: u32,
    halted_in: bool,
    halted_out: bool,
    cmds: Vec<u8>,
    max_rw_bytes: usize,
    /// Host will attempt a data stage the device refuses (failed command).
    skip_data: Option<bool>,
}

impl Mock {
    fn new(ss: usize, sectors: u64) -> Self {
        Mock {
            ss, sectors, store: HashMap::new(), wp: false, max_lun: 0, lun0_empty: false,
            cap10_saturates: false, f: Faults::default(), stage: Stage::Idle, sense: [0; 3],
            tag: 0, lun: 0, resets: 0, clears_in: 0, clears_out: 0, halted_in: false,
            halted_out: false, cmds: vec![], max_rw_bytes: 0, skip_data: None,
        }
    }

    fn pattern(lba: u64, ss: usize) -> Vec<u8> {
        (0..ss).map(|i| (lba as usize).wrapping_mul(31).wrapping_add(i) as u8).collect()
    }

    fn sector(&self, lba: u64) -> Vec<u8> {
        self.store.get(&lba).cloned().unwrap_or_else(|| Self::pattern(lba, self.ss))
    }

    fn check(&mut self, key: u8, asc: u8, ascq: u8) -> Stage {
        self.sense = [key, asc, ascq];
        Stage::Csw { residue: 0, status: 1 }
    }

    fn on_cbw(&mut self, cbw: &[u8]) {
        assert_eq!(cbw.len(), 31);
        assert_eq!(&cbw[0..4], b"USBC");
        self.tag = u32::from_le_bytes(cbw[4..8].try_into().unwrap());
        let dlen = u32::from_le_bytes(cbw[8..12].try_into().unwrap()) as usize;
        let flags = cbw[12];
        self.lun = cbw[13];
        self.skip_data = None;
        let cbl = cbw[14] as usize;
        assert!((1..=16).contains(&cbl));
        let cdb = &cbw[15..15 + cbl];
        let op = cdb[0];
        self.cmds.push(op);
        let lba10 = || u32::from_be_bytes(cdb[2..6].try_into().unwrap()) as u64;
        let lba16 = || u64::from_be_bytes(cdb[2..10].try_into().unwrap());

        self.stage = match op {
            0x00 => {
                if self.f.ua_turs > 0 {
                    self.f.ua_turs -= 1;
                    self.check(6, 0x29, 0)
                } else if self.f.not_ready_turs > 0 || self.f.never_ready {
                    self.f.not_ready_turs = self.f.not_ready_turs.saturating_sub(1);
                    self.check(2, 0x04, 0x01)
                } else if self.max_lun > 0 && self.lun == 0 && self.lun0_empty {
                    self.check(2, 0x3A, 0)
                } else {
                    Stage::Csw { residue: 0, status: 0 }
                }
            }
            0x03 => {
                let mut s = vec![0u8; 18];
                s[0] = 0x70;
                s[2] = self.sense[0];
                s[7] = 10;
                s[12] = self.sense[1];
                s[13] = self.sense[2];
                self.sense = [0; 3];
                s.truncate(dlen);
                Stage::DataIn(s)
            }
            0x12 => {
                let mut d = vec![0u8; 36];
                d[0] = 0;
                d[8..16].copy_from_slice(b"MOCK    ");
                d.truncate(dlen);
                Stage::DataIn(d)
            }
            0x25 => {
                let mut d = vec![0u8; 8];
                let last = if self.cap10_saturates { 0xFFFF_FFFF } else { (self.sectors - 1) as u32 };
                d[0..4].copy_from_slice(&last.to_be_bytes());
                d[4..8].copy_from_slice(&(self.ss as u32).to_be_bytes());
                Stage::DataIn(d)
            }
            0x9E => {
                assert_eq!(cdb[1], 0x10);
                let mut d = vec![0u8; 32];
                d[0..8].copy_from_slice(&(self.sectors - 1).to_be_bytes());
                d[8..12].copy_from_slice(&(self.ss as u32).to_be_bytes());
                d.truncate(dlen);
                Stage::DataIn(d)
            }
            0x1A => {
                let d = vec![3, 0, if self.wp { 0x80 } else { 0 }, 0];
                Stage::DataIn(d)
            }
            0x28 | 0x88 => {
                assert_eq!(flags, 0x80);
                let (lba, n) = if op == 0x28 {
                    (lba10(), u16::from_be_bytes(cdb[7..9].try_into().unwrap()) as u64)
                } else {
                    (lba16(), u32::from_be_bytes(cdb[10..14].try_into().unwrap()) as u64)
                };
                assert_eq!(dlen as u64, n * self.ss as u64);
                self.max_rw_bytes = self.max_rw_bytes.max(dlen);
                if self.f.check_once {
                    self.f.check_once = false;
                    self.check(3, 0x11, 0)
                } else {
                    let mut d = Vec::new();
                    for i in 0..n {
                        d.extend(self.sector(lba + i));
                    }
                    Stage::DataIn(d)
                }
            }
            0x2A | 0x8A => {
                assert_eq!(flags, 0);
                let (lba, n) = if op == 0x2A {
                    (lba10(), u16::from_be_bytes(cdb[7..9].try_into().unwrap()) as u64)
                } else {
                    (lba16(), u32::from_be_bytes(cdb[10..14].try_into().unwrap()) as u64)
                };
                assert_eq!(dlen as u64, n * self.ss as u64);
                self.max_rw_bytes = self.max_rw_bytes.max(dlen);
                if self.f.check_once {
                    self.f.check_once = false;
                    self.check(3, 0x0C, 0)
                } else {
                    Stage::DataOut { need: dlen, lba }
                }
            }
            0x35 => match self.f.sync_key {
                Some(k) => self.check(k, 0, 0),
                None => self.check(5, 0x20, 0), // not supported: ILLEGAL REQUEST
            },
            _ => self.check(5, 0x20, 0),
        };
        if dlen > 0 && matches!(self.stage, Stage::Csw { status: 1, .. }) {
            self.skip_data = Some(flags & 0x80 != 0);
        }
    }
}

impl BulkTransport for Mock {
    fn bulk_out(&mut self, data: &[u8], _t: u32) -> Result<usize, UsbErr> {
        match std::mem::replace(&mut self.stage, Stage::Idle) {
            Stage::Idle => {
                self.on_cbw(data);
                Ok(data.len())
            }
            Stage::DataOut { need, lba } => {
                assert_eq!(data.len(), need);
                if self.f.stall_data {
                    self.f.stall_data = false;
                    self.halted_out = true;
                    self.sense = [3, 0x0C, 0];
                    self.stage = Stage::Csw { residue: need as u32, status: 1 };
                    return Err(UsbErr::Stall);
                }
                for (i, c) in data.chunks(self.ss).enumerate() {
                    self.store.insert(lba + i as u64, c.to_vec());
                }
                self.stage = Stage::Csw { residue: 0, status: 0 };
                Ok(data.len())
            }
            Stage::Csw { residue, status } if self.skip_data == Some(false) => {
                self.skip_data = None;
                self.halted_out = true;
                self.stage = Stage::Csw { residue, status };
                Err(UsbErr::Stall)
            }
            other => {
                self.stage = other;
                Err(UsbErr::Timeout)
            }
        }
    }

    fn bulk_in(&mut self, buf: &mut [u8], _t: u32) -> Result<usize, UsbErr> {
        match std::mem::replace(&mut self.stage, Stage::Idle) {
            Stage::DataIn(mut v) => {
                if self.halted_in {
                    self.stage = Stage::DataIn(v);
                    return Err(UsbErr::Stall);
                }
                if self.f.stall_data && v.len() > 64 {
                    self.f.stall_data = false;
                    self.halted_in = true;
                    self.sense = [3, 0x11, 0];
                    self.stage = Stage::Csw { residue: v.len() as u32, status: 1 };
                    return Err(UsbErr::Stall);
                }
                let mut residue = 0;
                if self.f.short_read && v.len() > 64 {
                    self.f.short_read = false;
                    let keep = v.len() / 2;
                    residue = (v.len() - keep) as u32;
                    v.truncate(keep);
                }
                let n = v.len().min(buf.len());
                buf[..n].copy_from_slice(&v[..n]);
                self.stage = Stage::Csw { residue, status: 0 };
                Ok(n)
            }
            Stage::Csw { residue, status } => {
                if self.skip_data == Some(true) {
                    self.skip_data = None;
                    self.halted_in = true;
                }
                if self.halted_in {
                    self.stage = Stage::Csw { residue, status };
                    return Err(UsbErr::Stall);
                }
                if self.f.stall_csw {
                    self.f.stall_csw = false;
                    self.halted_in = true;
                    self.stage = Stage::Csw { residue, status };
                    return Err(UsbErr::Stall);
                }
                let mut sig = CSW_SIG;
                let mut tag = self.tag;
                let mut status = status;
                if self.f.bad_sig {
                    self.f.bad_sig = false;
                    sig = 0xDEAD_BEEF;
                }
                if self.f.tag_mismatch {
                    self.f.tag_mismatch = false;
                    tag ^= 0x100;
                }
                if self.f.phase_error {
                    self.f.phase_error = false;
                    status = 2;
                }
                buf[0..4].copy_from_slice(&sig.to_le_bytes());
                buf[4..8].copy_from_slice(&tag.to_le_bytes());
                buf[8..12].copy_from_slice(&residue.to_le_bytes());
                buf[12] = status;
                Ok(13)
            }
            other => {
                self.stage = other;
                Err(UsbErr::Timeout)
            }
        }
    }

    fn class_control_out(&mut self, request: u8, value: u16, index: u16, data: &[u8]) -> Result<(), UsbErr> {
        assert_eq!((request, value, index, data.len()), (0xFF, 0, 5, 0));
        self.resets += 1;
        self.stage = Stage::Idle;
        Ok(())
    }

    fn class_control_in(&mut self, request: u8, _v: u16, index: u16, buf: &mut [u8]) -> Result<usize, UsbErr> {
        assert_eq!((request, index), (0xFE, 5));
        if self.f.max_lun_stall {
            return Err(UsbErr::Stall);
        }
        buf[0] = self.max_lun;
        Ok(1)
    }

    fn clear_halt_in(&mut self) -> Result<(), UsbErr> {
        self.clears_in += 1;
        self.halted_in = false;
        Ok(())
    }
    fn clear_halt_out(&mut self) -> Result<(), UsbErr> {
        self.clears_out += 1;
        self.halted_out = false;
        Ok(())
    }
    fn interface_number(&self) -> u8 {
        5
    }
}

fn no_delay(_ms: u32) {}

use std::cell::Cell;
thread_local! { static DELAYED: Cell<u32> = Cell::new(0); }
fn count_delay(ms: u32) {
    DELAYED.with(|d| d.set(d.get() + ms));
}
fn delayed() -> u32 {
    DELAYED.with(|d| d.get())
}

fn disk(m: Mock) -> ScsiDisk<Mock> {
    ScsiDisk::new(m, no_delay)
}

fn ready(m: Mock) -> ScsiDisk<Mock> {
    let mut d = disk(m);
    d.init().expect("init");
    d
}

fn expect(ss: usize, lba: u64, buf: &[u8]) {
    for (i, c) in buf.chunks(ss).enumerate() {
        assert_eq!(c, &Mock::pattern(lba + i as u64, ss)[..], "sector {}", lba + i as u64);
    }
}

#[test]
fn init_512_and_4096() {
    for ss in [512usize, 4096] {
        let d = ready(Mock::new(ss, 2048));
        assert_eq!(d.sector_size() as usize, ss);
        assert_eq!(d.capacity_sectors(), 2048);
        assert!(!d.read_only());
    }
}

#[test]
fn reject_sector_size_2048() {
    let mut d = disk(Mock::new(2048, 100));
    assert_eq!(d.init(), Err(MscErr::UnsupportedSectorSize(2048)));
}

#[test]
fn roundtrip_512_and_4096() {
    for ss in [512usize, 4096] {
        let mut d = ready(Mock::new(ss, 1024));
        let mut buf = vec![0u8; ss * 4];
        d.read_sectors(10, &mut buf).unwrap();
        expect(ss, 10, &buf);
        let data: Vec<u8> = (0..ss * 3).map(|i| (i * 7 + 1) as u8).collect();
        d.write_sectors(100, &data).unwrap();
        let mut back = vec![0u8; ss * 3];
        d.read_sectors(100, &mut back).unwrap();
        assert_eq!(back, data);
        d.flush().unwrap(); // device answers ILLEGAL REQUEST: ignored
    }
}

#[test]
fn capacity16_when_cap10_saturates() {
    let sectors = 0x1_0000_0010u64;
    let mut m = Mock::new(512, sectors);
    m.cap10_saturates = true;
    let mut d = ready(m);
    assert_eq!(d.capacity_sectors(), sectors);
    assert!(d.transport_mut().cmds.contains(&0x9E));
}

#[test]
fn rw16_beyond_32bit_lba() {
    let sectors = 0x1_0000_1000u64;
    let mut m = Mock::new(512, sectors);
    m.cap10_saturates = true;
    let mut d = ready(m);
    let lba = 0xFFFF_FFF0u64; // lba + count crosses 2^32
    let mut buf = vec![0u8; 512 * 32];
    d.read_sectors(lba, &mut buf).unwrap();
    expect(512, lba, &buf);
    assert!(d.transport_mut().cmds.contains(&0x88));
    let w: Vec<u8> = vec![0xA5; 512 * 2];
    d.write_sectors(0x1_0000_0000, &w).unwrap();
    assert!(d.transport_mut().cmds.contains(&0x8A));
    let mut b2 = vec![0u8; 1024];
    d.read_sectors(0x1_0000_0000, &mut b2).unwrap();
    assert_eq!(b2, w);
}

#[test]
fn small_lba_uses_10_byte_cdbs() {
    let mut d = ready(Mock::new(512, 100));
    let mut b = vec![0u8; 512];
    d.read_sectors(1, &mut b).unwrap();
    assert!(d.transport_mut().cmds.contains(&0x28));
    assert!(!d.transport_mut().cmds.contains(&0x88));
}

#[test]
fn chunking_over_64k() {
    for ss in [512usize, 4096] {
        let sectors = 1024u64;
        let mut d = ready(Mock::new(ss, sectors));
        let n = 300 * 1024 / ss;
        let mut buf = vec![0u8; n * ss];
        d.read_sectors(3, &mut buf).unwrap();
        expect(ss, 3, &buf);
        assert_eq!(d.transport_mut().max_rw_bytes, crate::msc::scsi::MAX_CHUNK_BYTES);
        let reads = d.transport_mut().cmds.iter().filter(|&&c| c == 0x28).count();
        assert_eq!(reads, 3); // 300 KiB / 120 KiB
        let w: Vec<u8> = (0..n * ss).map(|i| (i ^ (i >> 8)) as u8).collect();
        d.write_sectors(5, &w).unwrap();
        let mut back = vec![0u8; n * ss];
        d.read_sectors(5, &mut back).unwrap();
        assert_eq!(back, w);
    }
}

#[test]
fn write_protect_detected() {
    let mut m = Mock::new(512, 64);
    m.wp = true;
    let mut d = ready(m);
    assert!(d.read_only());
    assert_eq!(d.write_sectors(0, &[0u8; 512]), Err(MscErr::WriteProtected));
    let mut b = [0u8; 512];
    d.read_sectors(0, &mut b).unwrap();
}

#[test]
fn bad_args_rejected() {
    let mut d = ready(Mock::new(512, 8));
    let mut b = [0u8; 100];
    assert_eq!(d.read_sectors(0, &mut b), Err(MscErr::InvalidArg));
    let mut b = [0u8; 1024];
    assert_eq!(d.read_sectors(7, &mut b), Err(MscErr::InvalidArg));
}

#[test]
fn fault_unit_attention_on_first_tur() {
    let mut m = Mock::new(512, 64);
    m.f.ua_turs = 1;
    m.f.not_ready_turs = 2;
    let mut d = ScsiDisk::new(m, count_delay);
    d.init().unwrap();
    assert!(delayed() >= 3 * 250);
    assert_eq!(d.capacity_sectors(), 64);
}

#[test]
fn fault_never_ready_gives_up_after_10s() {
    let mut m = Mock::new(512, 64);
    m.f.never_ready = true;
    let mut d = ScsiDisk::new(m, count_delay);
    assert!(d.init().is_err());
    let w = delayed();
    assert!((10_000..=11_000).contains(&w), "waited {w}");
}

#[test]
fn fault_data_phase_stall_in() {
    let mut d = ready(Mock::new(512, 64));
    d.transport_mut().f.stall_data = true;
    let mut b = vec![0u8; 512 * 2];
    d.read_sectors(4, &mut b).unwrap(); // recovered by retry
    expect(512, 4, &b);
    assert_eq!(d.transport_mut().clears_in, 1);
    assert_eq!(d.transport_mut().resets, 0);
}

#[test]
fn fault_data_phase_stall_out() {
    let mut d = ready(Mock::new(512, 64));
    d.transport_mut().f.stall_data = true;
    let w = vec![0x42u8; 512];
    d.write_sectors(9, &w).unwrap();
    assert_eq!(d.transport_mut().clears_out, 1);
    let mut b = vec![0u8; 512];
    d.read_sectors(9, &mut b).unwrap();
    assert_eq!(b, w);
}

#[test]
fn fault_csw_stall_retried_once_without_reset() {
    let mut d = ready(Mock::new(512, 64));
    d.transport_mut().f.stall_csw = true;
    let mut b = vec![0u8; 512];
    d.read_sectors(0, &mut b).unwrap();
    expect(512, 0, &b);
    assert_eq!(d.transport_mut().clears_in, 1);
    assert_eq!(d.transport_mut().resets, 0);
}

#[test]
fn fault_bad_csw_signature_triggers_reset_recovery() {
    let mut d = ready(Mock::new(512, 64));
    d.transport_mut().f.bad_sig = true;
    let mut b = vec![0u8; 512];
    d.read_sectors(2, &mut b).unwrap();
    expect(512, 2, &b);
    let m = d.transport_mut();
    assert_eq!(m.resets, 1);
    assert!(m.clears_in >= 1 && m.clears_out >= 1);
}

#[test]
fn fault_tag_mismatch_triggers_reset_recovery() {
    let mut d = ready(Mock::new(512, 64));
    d.transport_mut().f.tag_mismatch = true;
    let mut b = vec![0u8; 512];
    d.read_sectors(2, &mut b).unwrap();
    assert_eq!(d.transport_mut().resets, 1);
}

#[test]
fn fault_phase_error_triggers_reset_recovery() {
    let mut d = ready(Mock::new(512, 64));
    d.transport_mut().f.phase_error = true;
    let mut b = vec![0u8; 512];
    d.read_sectors(2, &mut b).unwrap();
    expect(512, 2, &b);
    assert_eq!(d.transport_mut().resets, 1);
}

#[test]
fn fault_short_read_with_residue_is_retried() {
    let mut d = ready(Mock::new(512, 64));
    d.transport_mut().f.short_read = true;
    let mut b = vec![0u8; 512 * 4];
    d.read_sectors(8, &mut b).unwrap();
    expect(512, 8, &b);
    let reads = d.transport_mut().cmds.iter().filter(|&&c| c == 0x28).count();
    assert_eq!(reads, 2);
}

#[test]
fn fault_check_condition_then_success() {
    let mut d = ready(Mock::new(512, 64));
    d.transport_mut().f.check_once = true;
    let mut b = vec![0u8; 512];
    d.read_sectors(1, &mut b).unwrap();
    expect(512, 1, &b);
    // READ, REQUEST SENSE, READ
    let cmds = &d.transport_mut().cmds;
    let tail: Vec<u8> = cmds[cmds.len() - 3..].to_vec();
    assert_eq!(tail, vec![0x28, 0x03, 0x28]);
}

#[test]
fn bot_level_phase_error_reports_and_recovers() {
    let mut m = Mock::new(512, 64);
    m.f.phase_error = true;
    let mut bot = Bot::new(m);
    let r = bot.execute(0, &[0u8; 6], Data::None);
    assert_eq!(r, Err(BotErr::Phase));
    assert_eq!(bot.transport.resets, 1);
    // Next command works and tags keep incrementing.
    assert!(bot.execute(0, &[0u8; 6], Data::None).is_ok());
}

#[test]
fn bot_level_residue_reported() {
    let mut m = Mock::new(512, 64);
    m.f.short_read = true;
    let mut bot = Bot::new(m);
    let mut cdb = [0u8; 10];
    cdb[0] = 0x28;
    cdb[8] = 4;
    let mut buf = vec![0u8; 2048];
    let c = bot.execute(0, &cdb, Data::In(&mut buf)).unwrap();
    assert_eq!(c.transferred, 1024);
    assert_eq!(c.residue, 1024);
}

#[test]
fn max_lun_stall_means_zero() {
    let mut m = Mock::new(512, 64);
    m.f.max_lun_stall = true;
    m.max_lun = 3;
    let mut bot = Bot::new(m);
    assert_eq!(bot.max_lun(), 0);
    bot.transport.f.max_lun_stall = false;
    assert_eq!(bot.max_lun(), 3);
}

#[test]
fn picks_first_ready_lun() {
    let mut m = Mock::new(512, 64);
    m.max_lun = 2;
    m.lun0_empty = true;
    let mut d = ready(m);
    assert_eq!(d.lun(), 1);
    let mut b = vec![0u8; 512];
    d.read_sectors(0, &mut b).unwrap();
}

#[test]
fn cbw_tags_increment() {
    struct Spy(Vec<u32>);
    impl BulkTransport for Spy {
        fn bulk_out(&mut self, d: &[u8], _: u32) -> Result<usize, UsbErr> {
            self.0.push(u32::from_le_bytes(d[4..8].try_into().unwrap()));
            Ok(d.len())
        }
        fn bulk_in(&mut self, b: &mut [u8], _: u32) -> Result<usize, UsbErr> {
            let tag = *self.0.last().unwrap();
            b[0..4].copy_from_slice(&CSW_SIG.to_le_bytes());
            b[4..8].copy_from_slice(&tag.to_le_bytes());
            b[8..13].fill(0);
            Ok(13)
        }
        fn class_control_out(&mut self, _: u8, _: u16, _: u16, _: &[u8]) -> Result<(), UsbErr> { Ok(()) }
        fn class_control_in(&mut self, _: u8, _: u16, _: u16, _: &mut [u8]) -> Result<usize, UsbErr> { Ok(0) }
        fn clear_halt_in(&mut self) -> Result<(), UsbErr> { Ok(()) }
        fn clear_halt_out(&mut self) -> Result<(), UsbErr> { Ok(()) }
        fn interface_number(&self) -> u8 { 0 }
    }
    let mut bot = Bot::new(Spy(vec![]));
    for _ in 0..3 {
        bot.execute(0, &[0u8; 6], Data::None).unwrap();
    }
    assert_eq!(bot.transport.0[2], bot.transport.0[0] + 2);
}

#[test]
fn flush_no_sense_and_recovered_are_success_and_cached() {
    for key in [0u8, 1, 5] {
        let mut d = ready(Mock::new(512, 64));
        d.transport_mut().f.sync_key = Some(key);
        d.flush().unwrap();
        assert!(d.sync_cache_unsupported(), "key {key}");
        let n = d.transport_mut().cmds.iter().filter(|&&c| c == 0x35).count();
        assert!(n >= 1);
        d.flush().unwrap();
        d.flush().unwrap();
        let m = d.transport_mut().cmds.iter().filter(|&&c| c == 0x35).count();
        assert_eq!(n, m, "SYNCHRONIZE CACHE re-issued after being found unsupported (key {key})");
    }
}

#[test]
fn flush_real_error_still_fails() {
    let mut d = ready(Mock::new(512, 64));
    d.transport_mut().f.sync_key = Some(3); // MEDIUM ERROR
    assert!(d.flush().is_err());
    assert!(!d.sync_cache_unsupported());
}
