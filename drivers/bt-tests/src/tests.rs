use std::collections::VecDeque;

use crate::bringup::{self, ErrorKind, Options, Transport, INIT_BAUD};
use crate::h4::{self, BdAddr, Event, H4Parser};
use crate::hcd::{self, HcdError};

// ── H4 framing ───────────────────────────────────────────────────────────────

#[test]
fn encode_reset_and_vendor_cmd() {
    let mut v = Vec::new();
    h4::encode_cmd(h4::OP_RESET, &[], &mut v);
    assert_eq!(v, [0x01, 0x03, 0x0c, 0x00]);
    v.clear();
    h4::encode_cmd(h4::OP_BCM_UPDATE_BAUD, &h4::bcm_baud_params(3_000_000), &mut v);
    assert_eq!(v, [0x01, 0x18, 0xfc, 0x06, 0, 0, 0xc0, 0xc6, 0x2d, 0x00]);
}

fn feed_all(p: &mut H4Parser, bytes: &[u8]) -> Vec<h4::Packet> {
    bytes.iter().filter_map(|&b| p.feed(b)).collect()
}

#[test]
fn parser_reassembles_events_acl_and_resyncs() {
    let mut p = H4Parser::new();
    // Noise, a Command Complete for Reset, an ACL packet, noise, a zero-length event.
    let mut s = vec![0x00, 0xee, 0x77];
    s.extend_from_slice(&[0x04, 0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00]);
    s.extend_from_slice(&[0x02, 0x01, 0x20, 0x03, 0x00, 0xaa, 0xbb, 0xcc]);
    s.extend_from_slice(&[0x99]);
    s.extend_from_slice(&[0x04, 0x10, 0x00]);
    let pk = feed_all(&mut p, &s);
    assert_eq!(pk.len(), 3);
    assert_eq!(p.garbage, 4);
    assert_eq!(pk[0].kind, h4::H4_EVT);
    assert_eq!(h4::parse_event(&pk[0].data), Some(Event::CommandComplete { ncmd: 1, opcode: h4::OP_RESET, ret: &[0] }));
    assert_eq!(pk[1].kind, h4::H4_ACL);
    assert_eq!(&pk[1].data[4..], &[0xaa, 0xbb, 0xcc]);
    assert_eq!(pk[2].data, [0x10, 0x00]);
}

#[test]
fn parser_handles_split_and_back_to_back_packets() {
    let mut p = H4Parser::new();
    let s = [0x04, 0x0f, 0x04, 0x02, 0x01, 0x2e, 0xfc, 0x04, 0x0f, 0x04, 0x00, 0x01, 0x4c, 0xfc];
    let pk = feed_all(&mut p, &s[..5]);
    assert!(pk.is_empty());
    let pk = feed_all(&mut p, &s[5..]);
    assert_eq!(pk.len(), 2);
    assert_eq!(h4::parse_event(&pk[0].data), Some(Event::CommandStatus { status: 2, ncmd: 1, opcode: 0xfc2e }));
    assert_eq!(h4::parse_event(&pk[1].data), Some(Event::CommandStatus { status: 0, ncmd: 1, opcode: 0xfc4c }));
}

#[test]
fn parse_event_rejects_malformed() {
    assert_eq!(h4::parse_event(&[0x0e, 0x02, 0x01, 0x03]), None); // CC too short
    assert_eq!(h4::parse_event(&[0x0e, 0x05, 0x01]), None); // length mismatch
    assert!(matches!(h4::parse_event(&[0xff, 0x01, 0x22]), Some(Event::Other { code: 0xff, .. })));
}

#[test]
fn version_and_bdaddr_parsers() {
    let v = h4::parse_local_version(&[0, 9, 0x00, 0x01, 9, 0x0f, 0x00, 0x19, 0x61]).unwrap();
    assert_eq!((v.hci_ver, v.hci_rev, v.lmp_ver, v.manufacturer, v.lmp_subver), (9, 0x100, 9, 15, 0x6119));
    assert_eq!(h4::parse_local_version(&[0x0c, 9, 0, 1, 9, 15, 0, 0x19, 0x61]), None);
    let a = h4::parse_bdaddr(&[0, 0xac, 0x1f, 0x00, 0xc0, 0x45, 0x43]).unwrap();
    assert_eq!(a, BdAddr::BCM4345C0_PLACEHOLDER);
    assert_eq!(a.to_string(), "43:45:c0:00:1f:ac");
    assert!(a.is_placeholder());
    assert!(!BdAddr([1, 2, 3, 0x3a, 0x83, 0xd8]).is_placeholder());
}

// ── .hcd records ─────────────────────────────────────────────────────────────

fn rec(op: u16, data: &[u8]) -> Vec<u8> {
    let mut v = op.to_le_bytes().to_vec();
    v.push(data.len() as u8);
    v.extend_from_slice(data);
    v
}

fn synthetic_hcd() -> Vec<u8> {
    let mut v = Vec::new();
    for i in 0..5u8 { v.extend(rec(0xfc4c, &[i; 40])); }
    v.extend(rec(0xfc4e, &[0xff, 0xff, 0xff, 0xff]));
    v
}

#[test]
fn hcd_validate_synthetic_and_errors() {
    let s = hcd::validate(&synthetic_hcd()).unwrap();
    assert_eq!((s.records, s.write_ram, s.payload_bytes), (6, 5, 204));
    let mut t = synthetic_hcd();
    t.pop();
    assert_eq!(hcd::validate(&t), Err(HcdError::Truncated(5 * 43)));
    assert_eq!(hcd::validate(&[0x4c, 0xfc]), Err(HcdError::Truncated(0)));
    assert_eq!(hcd::validate(&[]), Err(HcdError::Empty));
    assert_eq!(hcd::validate(&rec(0xfc4c, &[1])), Err(HcdError::NoLaunch));
    let mut after = synthetic_hcd();
    after.extend(rec(0xfc4c, &[1]));
    assert_eq!(hcd::validate(&after), Err(HcdError::NoLaunch));
    assert_eq!(hcd::validate(&rec(0x0c03, &[])), Err(HcdError::BadOpcode(0, 0x0c03)));
}

/// The pinned BCM4345C0.hcd (bluez-firmware cdf61dc6), if cached.
fn real_hcd() -> Option<Vec<u8>> {
    let dir = std::env::var("BTFW_CACHE").unwrap_or_else(|_| format!("{}/.cache/leandros/bluez-firmware", std::env::var("HOME").unwrap_or_default()));
    std::fs::read(format!("{}/BCM4345C0.hcd", dir)).ok()
}

#[test]
fn hcd_real_file_parses() {
    let Some(img) = real_hcd() else { eprintln!("BCM4345C0.hcd not cached; skipped"); return };
    assert_eq!(img.len(), 63_806, "pinned file size");
    let s = hcd::validate(&img).unwrap();
    assert_eq!(s.records, s.write_ram + 1, "all Write RAM plus one Launch RAM");
    let recs: Vec<_> = hcd::records(&img).map(|r| r.unwrap()).collect();
    assert_eq!((recs[0].1, recs[0].2.len()), (0xfc4c, 0x46));
    assert_eq!(recs.last().unwrap().1, 0xfc4e);
    // Every record's parameters fit one HCI command and the walk covers the file.
    assert_eq!(recs.iter().map(|r| 3 + r.2.len()).sum::<usize>(), img.len());
    eprintln!("BCM4345C0.hcd: {} records, {} payload bytes", s.records, s.payload_bytes);
}

// ── Scripted controller ──────────────────────────────────────────────────────

#[derive(Default)]
struct Fake {
    rx: VecDeque<u8>,
    tx: H4Parser,
    host_baud: u32,
    ctrl_baud: u32,
    patched: bool,
    minidriver: bool,
    bdaddr: [u8; 6],
    /// Opcodes in the order received (the btmon-style transcript).
    log: Vec<u16>,
    /// Opcode that never gets an answer / gets this status.
    mute: Option<u16>,
    fail_status: Option<(u16, u8)>,
    /// First N Resets are ignored (controller still booting).
    ignore_resets: u32,
    noisy: bool,
    sleeps: Vec<u32>,
}

impl Fake {
    fn new() -> Self {
        Fake { host_baud: INIT_BAUD, ctrl_baud: INIT_BAUD, bdaddr: BdAddr::BCM4345C0_PLACEHOLDER.0, ..Default::default() }
    }
    fn evt(&mut self, code: u8, p: &[u8]) {
        if self.host_baud != self.ctrl_baud {
            self.rx.extend([0xf0, 0x0f, 0x00, 0xff]); // what a rate mismatch looks like
            return;
        }
        self.rx.push_back(h4::H4_EVT);
        self.rx.push_back(code);
        self.rx.push_back(p.len() as u8);
        self.rx.extend(p);
    }
    fn cc(&mut self, op: u16, ret: &[u8]) {
        let mut p = vec![1];
        p.extend_from_slice(&op.to_le_bytes());
        p.extend_from_slice(ret);
        self.evt(0x0e, &p);
    }
    fn command(&mut self, data: &[u8]) {
        let op = u16::from_le_bytes([data[0], data[1]]);
        let params = &data[3..];
        assert_eq!(params.len(), data[2] as usize);
        self.log.push(op);
        if self.noisy { self.evt(0xff, &[0x12, 0x34]); }
        if self.mute == Some(op) { return; }
        if let Some((o, s)) = self.fail_status { if o == op { return self.cc(op, &[s]); } }
        match op {
            h4::OP_RESET => {
                if self.ignore_resets > 0 { self.ignore_resets -= 1; return; }
                self.cc(op, &[0]);
            }
            h4::OP_READ_LOCAL_VERSION => {
                let sub: u16 = if self.patched { 0x6119 } else { 0x2105 };
                let mut r = vec![0, 9, 0x00, 0x01, 9, 0x0f, 0x00];
                r.extend_from_slice(&sub.to_le_bytes());
                self.cc(op, &r);
            }
            h4::OP_READ_BD_ADDR => { let mut r = vec![0]; r.extend_from_slice(&self.bdaddr.clone()); self.cc(op, &r); }
            h4::OP_BCM_DOWNLOAD_MINIDRV => { self.minidriver = true; self.cc(op, &[0]); }
            h4::OP_BCM_WRITE_RAM => { assert!(self.minidriver); self.cc(op, &[0]); }
            h4::OP_BCM_LAUNCH_RAM => { assert!(self.minidriver); self.cc(op, &[0]); self.patched = true; self.minidriver = false; }
            h4::OP_BCM_UPDATE_BAUD => {
                self.cc(op, &[0]);
                self.ctrl_baud = u32::from_le_bytes([params[2], params[3], params[4], params[5]]);
            }
            h4::OP_BCM_WRITE_BD_ADDR => { self.bdaddr.copy_from_slice(params); self.cc(op, &[0]); }
            _ => self.cc(op, &[0x01]), // Unknown HCI Command
        }
    }
}

impl Transport for Fake {
    fn send(&mut self, bytes: &[u8]) -> bool {
        for &b in bytes {
            if let Some(p) = self.tx.feed(b) {
                assert_eq!(p.kind, h4::H4_CMD);
                if self.host_baud == self.ctrl_baud { self.command(&p.data); }
            }
        }
        true
    }
    fn recv(&mut self, _timeout_us: u32) -> Option<u8> { self.rx.pop_front() }
    fn set_baud(&mut self, baud: u32) -> bool { self.host_baud = baud; true }
    fn sleep_ms(&mut self, ms: u32) { self.sleeps.push(ms); }
}

const DT_ADDR: BdAddr = BdAddr([0x11, 0x22, 0x33, 0x3a, 0xdd, 0xd8]);

fn expected_transcript(records: &[u16]) -> Vec<u16> {
    let mut t = vec![0x0c03, 0x1001, 0xfc2e];
    t.extend_from_slice(records);
    t.extend([0x0c03, 0xfc18, 0x1001, 0x1009, 0xfc01, 0x1009]);
    t
}

#[test]
fn bring_up_patch_baud_bdaddr_transcript() {
    let img = real_hcd().unwrap_or_else(synthetic_hcd);
    let ops: Vec<u16> = hcd::records(&img).map(|r| r.unwrap().1).collect();
    let mut f = Fake::new();
    f.noisy = true;
    let o = Options { hcd: Some(&img), oper_baud: Some(3_000_000), bdaddr: Some(DT_ADDR), cmd_timeout_us: 0 };
    let r = bringup::bring_up(&mut f, &o).unwrap();
    assert_eq!(f.log, expected_transcript(&ops));
    assert_eq!(f.sleeps, [50, 250, 10]);
    assert_eq!((r.rom.lmp_subver, r.ver.lmp_subver), (0x2105, 0x6119));
    assert_eq!(r.baud, 3_000_000);
    assert_eq!(f.ctrl_baud, 3_000_000);
    assert_eq!(r.bdaddr, DT_ADDR);
    assert!(r.bdaddr_set);
    assert_eq!(r.patch.unwrap().records, ops.len());
    assert_eq!(r.stray_events as usize, f.log.len(), "one vendor event per command, all skipped");
    assert_eq!(r.garbage, 0);
}

#[test]
fn bring_up_rom_only_keeps_rate_and_controller_address() {
    let mut f = Fake::new();
    f.bdaddr = [6, 5, 4, 3, 2, 1];
    let o = Options { hcd: None, oper_baud: Some(INIT_BAUD), bdaddr: Some(DT_ADDR), cmd_timeout_us: 0 };
    let r = bringup::bring_up(&mut f, &o).unwrap();
    assert_eq!(f.log, [0x0c03, 0x1001, 0x1001, 0x1009]);
    assert_eq!((r.baud, r.bdaddr_set, r.patch), (INIT_BAUD, false, None));
    assert_eq!(r.bdaddr.to_string(), "01:02:03:04:05:06");
}

#[test]
fn bring_up_retries_one_lost_reset() {
    let mut f = Fake::new();
    f.ignore_resets = 1;
    let o = Options { cmd_timeout_us: 5_000, ..Options::default() };
    assert!(bringup::bring_up(&mut f, &o).is_ok());
    assert_eq!(&f.log[..3], &[0x0c03, 0x0c03, 0x1001]);
}

#[test]
fn silent_controller_is_no_answer_at_reset() {
    let mut f = Fake::new();
    f.mute = Some(h4::OP_RESET);
    let e = bringup::bring_up(&mut f, &Options { cmd_timeout_us: 5_000, ..Options::default() }).unwrap_err();
    assert_eq!((e.stage, e.kind), ("reset", ErrorKind::NoAnswer(0x0c03)));
    assert_eq!(f.log, [0x0c03, 0x0c03]);
}

#[test]
fn patch_record_failure_stops_the_download() {
    let img = synthetic_hcd();
    let mut f = Fake::new();
    f.fail_status = Some((0xfc4c, 0x12));
    let o = Options { hcd: Some(&img), ..Options::default() };
    let e = bringup::bring_up(&mut f, &o).unwrap_err();
    assert_eq!((e.stage, e.kind), ("patch", ErrorKind::Status(0xfc4c, 0x12)));
    assert_eq!(f.log, [0x0c03, 0x1001, 0xfc2e, 0xfc4c]);
}

#[test]
fn bad_image_never_touches_the_controller() {
    let mut f = Fake::new();
    let img = rec(0xfc4c, &[1, 2, 3]);
    let e = bringup::bring_up(&mut f, &Options { hcd: Some(&img), ..Options::default() }).unwrap_err();
    assert_eq!((e.stage, e.kind), ("hcd", ErrorKind::Hcd(HcdError::NoLaunch)));
    assert!(f.log.is_empty());
}

/// A controller that ignores the rate change: the host switches, the
/// controller does not, and the next command is a bounded NoAnswer.
#[test]
fn baud_mismatch_is_bounded_no_answer() {
    struct Stubborn(Fake);
    impl Transport for Stubborn {
        fn send(&mut self, b: &[u8]) -> bool { self.0.send(b) }
        fn recv(&mut self, t: u32) -> Option<u8> { self.0.recv(t) }
        fn set_baud(&mut self, baud: u32) -> bool { self.0.host_baud = baud; self.0.ctrl_baud = INIT_BAUD; true }
        fn sleep_ms(&mut self, ms: u32) { self.0.sleep_ms(ms) }
    }
    let mut s = Stubborn(Fake::new());
    let o = Options { oper_baud: Some(3_000_000), cmd_timeout_us: 5_000, ..Options::default() };
    let e = bringup::bring_up(&mut s, &o).unwrap_err();
    assert_eq!((e.stage, e.kind), ("version-final", ErrorKind::NoAnswer(0x1001)));
}

// ── Device tree ──────────────────────────────────────────────────────────────

/// Minimal FDT: root node with one child carrying `local-bd-address`.
fn fdt(addr: [u8; 6]) -> Vec<u8> {
    let strings = b"compatible\0local-bd-address\0".to_vec();
    let mut st = Vec::new();
    let be = |v: &mut Vec<u8>, x: u32| v.extend_from_slice(&x.to_be_bytes());
    be(&mut st, 1); st.extend_from_slice(&[0, 0, 0, 0]); // root ""
    be(&mut st, 1); st.extend_from_slice(b"bluetooth\0\0\0");
    be(&mut st, 3); be(&mut st, 4); be(&mut st, 0); st.extend_from_slice(b"abc\0");
    be(&mut st, 3); be(&mut st, 6); be(&mut st, 11); st.extend_from_slice(&addr); st.extend_from_slice(&[0, 0]);
    be(&mut st, 2); be(&mut st, 2); be(&mut st, 9);
    let off_st = 40u32;
    let off_strs = off_st + st.len() as u32;
    let mut h = Vec::new();
    for x in [0xd00d_feedu32, off_strs + strings.len() as u32, off_st, off_strs, 0, 17, 16, 0, strings.len() as u32, st.len() as u32] {
        be(&mut h, x);
    }
    h.extend(st);
    h.extend(strings);
    h
}

#[test]
fn fdt_local_bd_address_found_and_zero_ignored() {
    assert_eq!(bringup::fdt_local_bd_address(&fdt(DT_ADDR.0)), Some(DT_ADDR));
    assert_eq!(bringup::fdt_local_bd_address(&fdt([0; 6])), None);
    assert_eq!(bringup::fdt_local_bd_address(&[0u8; 64]), None);
}

// ── /dev/hci0 queues + pump (lane pibt-hci0) ────────────────────────────────

mod hci0 {
    use std::cell::RefCell;
    use std::collections::VecDeque;

    use crate::bringup::Transport;
    use crate::h4::H4Parser;
    use crate::hcidev_core::*;

    struct Cell(RefCell<HciQueues>);
    impl QueueLock for Cell {
        fn with<R>(&self, f: impl FnOnce(&mut HciQueues) -> R) -> R { f(&mut self.0.borrow_mut()) }
    }
    fn opened() -> Cell { let c = Cell(RefCell::new(HciQueues::new())); c.0.borrow_mut().open(); c }

    /// Fake H4 controller: answers every command with Command Complete
    /// (status 0) for its opcode; `alive = false` refuses bytes.
    struct FakeCtl { rx: VecDeque<u8>, sent: Vec<Vec<u8>>, alive: bool }
    impl FakeCtl { fn new() -> Self { Self { rx: VecDeque::new(), sent: Vec::new(), alive: true } } }
    impl Transport for FakeCtl {
        fn send(&mut self, b: &[u8]) -> bool {
            if !self.alive { return false; }
            self.sent.push(b.to_vec());
            if b[0] == 0x01 {
                self.rx.extend([0x04, 0x0e, 0x04, 0x01, b[1], b[2], 0x00]);
            }
            true
        }
        fn recv(&mut self, _t: u32) -> Option<u8> { self.rx.pop_front() }
        fn set_baud(&mut self, _b: u32) -> bool { true }
        fn sleep_ms(&mut self, _ms: u32) {}
    }

    const RESET: [u8; 4] = [0x01, 0x03, 0x0c, 0x00];
    const RESET_CC: [u8; 7] = [0x04, 0x0e, 0x04, 0x01, 0x03, 0x0c, 0x00];

    #[test]
    fn reset_loopback_through_node_with_poll_readiness() {
        let q = opened();
        let (mut ctl, mut p) = (FakeCtl::new(), H4Parser::new());
        assert_eq!(q.with(|q| q.poll_mask()), POLLOUT, "idle: writable, nothing to read");
        assert_eq!(q.with(|q| q.read(4096)), Err(EAGAIN));
        assert_eq!(q.with(|q| q.write(&RESET)), Ok(4));
        let out = pump_once(&q, &mut ctl, &mut p, 1000, 4096);
        assert_eq!((out.sent, out.received, out.changed()), (1, 1, true));
        assert_eq!(ctl.sent, vec![RESET.to_vec()]);
        assert_eq!(q.with(|q| q.poll_mask()), POLLIN | POLLOUT);
        assert_eq!(q.with(|q| q.read(4096)), Ok(RESET_CC.to_vec()), "one whole packet, type byte first");
        assert_eq!(q.with(|q| q.poll_mask()), POLLOUT);
        assert!(!pump_once(&q, &mut ctl, &mut p, 1000, 4096).changed());
    }

    #[test]
    fn read_too_small_is_emsgsize_and_keeps_packet() {
        let q = opened();
        q.with(|q| q.push_rx(RESET_CC.to_vec()));
        assert_eq!(q.with(|q| q.read(6)), Err(EMSGSIZE));
        assert_eq!(q.with(|q| q.read(7)), Ok(RESET_CC.to_vec()));
    }

    #[test]
    fn two_packets_are_two_reads() {
        let q = opened();
        let (mut ctl, mut p) = (FakeCtl::new(), H4Parser::new());
        q.with(|q| { q.write(&RESET).unwrap(); q.write(&[0x01, 0x01, 0x10, 0x00]).unwrap(); });
        assert_eq!(pump_once(&q, &mut ctl, &mut p, 1000, 4096).received, 2);
        assert_eq!(q.with(|q| q.read(4096)), Ok(RESET_CC.to_vec()));
        assert_eq!(q.with(|q| q.read(4096)).unwrap()[4..6], [0x01, 0x10]);
        assert_eq!(q.with(|q| q.read(4096)), Err(EAGAIN));
    }

    #[test]
    fn write_validation() {
        assert_eq!(validate_h4_write(&RESET), Ok(()));
        assert_eq!(validate_h4_write(&[0x01, 0x03, 0x0c, 0x01, 0xaa]), Ok(()));
        assert_eq!(validate_h4_write(&[0x01, 0x03, 0x0c, 0x01]), Err(EINVAL), "short params");
        assert_eq!(validate_h4_write(&[0x01, 0x03, 0x0c, 0x00, 0x00]), Err(EINVAL), "trailing byte (two packets)");
        assert_eq!(validate_h4_write(&[0x02, 0x01, 0x20, 0x02, 0x00, 1, 2]), Ok(()));
        assert_eq!(validate_h4_write(&[0x02, 0x01, 0x20, 0x02]), Err(EINVAL));
        assert_eq!(validate_h4_write(&[0x03, 0x01, 0x00, 0x01, 9]), Ok(()));
        assert_eq!(validate_h4_write(&[0x04, 0x0e, 0x00]), Err(EINVAL), "events are not host->controller");
        assert_eq!(validate_h4_write(&[]), Err(EINVAL));
    }

    #[test]
    fn tx_full_is_eagain_and_clears_pollout() {
        let q = opened();
        for _ in 0..TX_CAP { assert_eq!(q.with(|q| q.write(&RESET)), Ok(4)); }
        assert_eq!(q.with(|q| q.write(&RESET)), Err(EAGAIN));
        assert_eq!(q.with(|q| q.poll_mask()) & POLLOUT, 0);
        pump_once(&q, &mut FakeCtl::new(), &mut H4Parser::new(), 0, 0);
        assert_ne!(q.with(|q| q.poll_mask()) & POLLOUT, 0);
    }

    #[test]
    fn rx_overflow_drops_newest_and_counts() {
        let q = opened();
        for i in 0..RX_CAP + 3 { q.with(|q| q.push_rx(vec![0x04, 0xff, 0x01, i as u8])); }
        q.with(|q| { assert_eq!(q.rx_len(), RX_CAP); assert_eq!(q.rx_drops, 3); });
        assert_eq!(q.with(|q| q.read(4096)).unwrap()[3], 0, "oldest kept");
    }

    #[test]
    fn closed_node_discards_and_reopen_is_clean() {
        let q = Cell(RefCell::new(HciQueues::new()));
        assert!(!q.with(|q| q.push_rx(RESET_CC.to_vec())));
        q.with(|q| { q.open(); q.push_rx(RESET_CC.to_vec()); q.write(&RESET).unwrap(); q.close(); q.open(); });
        assert_eq!(q.with(|q| q.read(4096)), Err(EAGAIN));
        assert!(q.with(|q| q.pop_tx()).is_none());
    }

    #[test]
    fn dead_controller_is_hup_err_then_eio() {
        let q = opened();
        q.with(|q| { q.push_rx(RESET_CC.to_vec()); q.write(&RESET).unwrap(); });
        let mut ctl = FakeCtl::new(); ctl.alive = false;
        assert!(pump_once(&q, &mut ctl, &mut H4Parser::new(), 0, 16).died);
        assert_eq!(q.with(|q| q.poll_mask()), POLLIN | POLLHUP | POLLERR);
        assert_eq!(q.with(|q| q.write(&RESET)), Err(EIO));
        assert_eq!(q.with(|q| q.read(4096)), Ok(RESET_CC.to_vec()), "queued packets still drain");
        assert_eq!(q.with(|q| q.read(4096)), Err(EIO));
    }
}
