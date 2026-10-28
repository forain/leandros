//! btusb reassembly and the Intel (btintel) bootloader set-up against a
//! scripted controller (lane lapbt).
use std::collections::VecDeque;

use crate::btintel::{self, Error, Link};
use crate::usb_reasm::{Reasm, H4_ACL, H4_EVENT};

// ── reassembly ───────────────────────────────────────────────────────────────

#[test]
fn event_split_over_interrupt_packets() {
    let mut ev = vec![0x0e, 200];
    ev.extend((0..200).map(|i| i as u8));
    let mut r = Reasm::new(H4_EVENT);
    let mut out = Vec::new();
    for c in ev.chunks(64) { r.push(c, &mut out); }
    assert_eq!(out.len(), 1);
    assert_eq!(out[0][0], H4_EVENT);
    assert_eq!(&out[0][1..], &ev[..]);
    assert_eq!(r.pending(), 0);
}

#[test]
fn two_events_in_one_chunk_and_a_partial_header() {
    let mut r = Reasm::new(H4_EVENT);
    let mut out = Vec::new();
    r.push(&[0x0e, 1, 0xAA, 0x0f, 4, 0, 1, 2], &mut out);
    assert_eq!(out.len(), 1);
    r.push(&[3, 0x05], &mut out);
    assert_eq!(out, vec![vec![4, 0x0e, 1, 0xAA], vec![4, 0x0f, 4, 0, 1, 2, 3]]);
    assert_eq!(r.pending(), 1);
}

#[test]
fn acl_split_and_oversize_resync() {
    let mut acl = vec![0x01, 0x20, 0x2c, 0x01];
    acl.extend(std::iter::repeat(7u8).take(300));
    let mut r = Reasm::new(H4_ACL);
    let mut out = Vec::new();
    for c in acl.chunks(64) { r.push(c, &mut out); }
    assert_eq!(out.len(), 1);
    assert_eq!(out[0].len(), 305);
    r.push(&[0x01, 0x20, 0xff, 0xff, 1, 2], &mut out);
    assert_eq!(r.pending(), 0);
    assert_eq!(r.resyncs, 6);
}

// ── btintel ──────────────────────────────────────────────────────────────────

fn sfi(cmds: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut f = vec![0u8; 644];
    f[0] = 6;
    for (i, b) in f.iter_mut().enumerate().skip(4) { *b = i as u8; }
    for (op, p) in cmds {
        f.extend_from_slice(&op.to_le_bytes());
        f.push(p.len() as u8);
        f.extend_from_slice(p);
    }
    f
}

/// A firmware whose commands need the 4-byte alignment rule: 3+1, 3+5, 3+250 (big), boot.
fn sample_fw() -> Vec<u8> {
    sfi(&[
        (0xfc8e, vec![1]),
        (0xfc8e, vec![2; 5]),
        (0xfc8e, vec![3; 249]),
        (0xfc8e, vec![4; 250]),
        (0xfc0e, vec![0x00, 0x08, 0x04, 0x00, 0, 0, 0, 0]),
    ])
}

#[test]
fn sfi_fragments_align_on_command_boundaries() {
    let fw = sample_fw();
    let (frags, boot) = btintel::sfi_fragments(&fw).unwrap();
    assert_eq!(boot, 0x40800);
    assert_eq!(frags[0], (0, &fw[0..128]));
    assert_eq!(frags[1], (3, &fw[128..384]));
    assert_eq!(frags[2], (2, &fw[388..644]));
    let body: Vec<u8> = frags[3..].iter().flat_map(|f| { assert_eq!(f.0, 1); assert_eq!(f.1.len() % 4, 0); f.1.to_vec() }).collect();
    assert_eq!(&body[..], &fw[644..]);
    // 4+8 = 12 first (3+1 alone is 4: its own fragment), then 3+249 = 252, then 253+11 = 264 (> 252: split by secure_send).
    assert_eq!(frags[3..].iter().map(|f| f.1.len()).collect::<Vec<_>>(), vec![4, 8, 252, 264]);
}

#[test]
fn sfi_rejects_truncated_and_foreign_images() {
    let mut fw = sample_fw();
    fw.truncate(fw.len() - 2);
    assert!(btintel::sfi_fragments(&fw).is_err());
    let mut fw = sample_fw();
    fw[0] = 3;
    assert!(btintel::sfi_fragments(&fw).is_err());
}

#[test]
fn real_ibt_12_16_walks_cleanly_if_cached() {
    let dir = format!("{}/.cache/leandros/intel-bt", std::env::var("HOME").unwrap_or_default());
    let Ok(fw) = std::fs::read(format!("{}/ibt-12-16.sfi", dir)) else { return };
    let (frags, boot) = btintel::sfi_fragments(&fw).unwrap();
    assert_eq!(boot, 0x40800);
    assert_eq!(frags.len(), 3 + 2356);
    let ddc = std::fs::read(format!("{}/ibt-12-16.ddc", dir)).unwrap();
    let recs = btintel::ddc_records(&ddc).unwrap();
    assert_eq!(recs.iter().map(|r| r.len()).sum::<usize>(), ddc.len());
}

/// Scripted 8265: bootloader until Intel Reset, then operational.
struct Fake {
    t: u64,
    oper: bool,
    events: VecDeque<Vec<u8>>,
    fw_len: usize,
    got: usize,
    frags: usize,
    reject: bool,
    ddc: Vec<Vec<u8>>,
    reset_param: Vec<u8>,
    bulk_cmds_after_boot: usize,
    became_operational: bool,
}

impl Fake {
    fn new(fw_len: usize, oper: bool) -> Fake {
        Fake { t: 0, oper, events: VecDeque::new(), fw_len, got: 0, frags: 0, reject: false, ddc: vec![],
               reset_param: vec![], bulk_cmds_after_boot: 0, became_operational: false }
    }
    fn cc(&mut self, op: u16, ret: &[u8]) {
        let mut e = vec![0x0e, (3 + ret.len()) as u8, 1];
        e.extend_from_slice(&op.to_le_bytes());
        e.extend_from_slice(ret);
        self.events.push_back(e);
    }
}

impl Link for Fake {
    fn send_cmd(&mut self, pkt: &[u8], bulk: bool) -> bool {
        let op = u16::from_le_bytes([pkt[0], pkt[1]]);
        assert_eq!(pkt[2] as usize, pkt.len() - 3);
        let p = &pkt[3..];
        assert_eq!(bulk, op == btintel::OP_SECURE_SEND, "only secure send goes on bulk");
        match op {
            btintel::OP_READ_VERSION => {
                let fwv = if self.oper { 0x23 } else { 0x06 };
                self.cc(op, &[0, 0x37, 0x0c, 0x00, fwv, 0x10, 0x11, 0x2e, 0x15, 0]);
            }
            btintel::OP_READ_BOOT_PARAMS => {
                assert!(!self.oper);
                let mut r = vec![0u8; 23];
                r[4] = 16;
                r[12..18].copy_from_slice(&[0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f]);
                self.cc(op, &r);
            }
            btintel::OP_SECURE_SEND => {
                if self.oper { self.bulk_cmds_after_boot += 1; }
                assert!(p.len() <= 253);
                self.got += p.len() - 1;
                self.frags += 1;
                self.cc(op, &[0]);
                if self.got == self.fw_len - 4 {
                    let r = if self.reject { 1 } else { 0 };
                    self.events.push_back(vec![0xff, 5, 0x06, r, 0x09, 0xfc, 0]);
                }
            }
            btintel::OP_RESET => {
                self.reset_param = p.to_vec();
                self.oper = true;
                self.events.push_back(vec![0xff, 1, 0x02]);
            }
            btintel::OP_WRITE_DDC => { assert!(self.oper); self.ddc.push(p.to_vec()); self.cc(op, &[0]); }
            btintel::OP_EVENT_MASK => self.cc(op, &[0]),
            _ => panic!("unexpected command {:#06x}", op),
        }
        true
    }
    fn event(&mut self, timeout_ms: u32) -> Option<Vec<u8>> {
        match self.events.pop_front() {
            Some(e) => { self.t += 1; Some(e) }
            None => { self.t += timeout_ms as u64; None }
        }
    }
    fn now_ms(&mut self) -> u64 { self.t }
    fn operational(&mut self) { self.became_operational = true; }
}

fn loader(fw: Vec<u8>) -> impl FnMut(&str) -> Option<Vec<u8>> {
    move |n: &str| match n {
        "ibt-12-16.sfi" => Some(fw.clone()),
        "ibt-12-16.ddc" => Some(vec![3, 0x28, 0x01, 0x18, 4, 0x42, 0x01, 0x45, 0x80, 4, 0x29, 0x01, 0x01, 0x00]),
        _ => None,
    }
}

#[test]
fn bootloader_download_boot_and_ddc() {
    let fw = sample_fw();
    let mut f = Fake::new(fw.len(), false);
    let r = btintel::setup(&mut f, &mut loader(fw.clone())).unwrap();
    assert_eq!(r.fw, "ibt-12-16.sfi");
    assert_eq!(r.boot_addr, 0x40800);
    assert_eq!(f.got, fw.len() - 4);
    // 128 (1) + 256 (2) + 256 (2) + 4 + 8 + 252 + 264 (2) = 10 commands
    assert_eq!(f.frags, 10);
    assert_eq!(f.reset_param, vec![0, 1, 0, 1, 0x00, 0x08, 0x04, 0x00]);
    assert!(f.became_operational);
    assert_eq!(f.bulk_cmds_after_boot, 0);
    assert_eq!(f.ddc, vec![vec![3, 0x28, 0x01, 0x18], vec![4, 0x42, 0x01, 0x45, 0x80], vec![4, 0x29, 0x01, 0x01, 0x00]]);
    assert_eq!(r.ddc_records, 3);
    assert_eq!(r.oper.fw_variant, 0x23);
}

#[test]
fn already_operational_skips_download() {
    let mut f = Fake::new(0, true);
    let r = btintel::setup(&mut f, &mut loader(sample_fw())).unwrap();
    assert!(r.already_operational);
    assert_eq!(f.frags, 0);
    assert_eq!(f.ddc.len(), 3);
}

#[test]
fn rejected_firmware_is_an_error() {
    let fw = sample_fw();
    let mut f = Fake::new(fw.len(), false);
    f.reject = true;
    match btintel::setup(&mut f, &mut loader(fw)) {
        Err(Error::Firmware(m)) => assert!(m.contains("rejected"), "{}", m),
        r => panic!("{:?}", r.map(|_| ())),
    }
    assert!(f.reset_param.is_empty());
}

#[test]
fn missing_firmware_is_an_error() {
    let mut f = Fake::new(1, false);
    let r = btintel::setup(&mut f, &mut |_| None);
    assert!(matches!(r, Err(Error::Firmware(ref m)) if m.contains("missing")));
}
