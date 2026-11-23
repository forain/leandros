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

/// The pinned BCM4345C0.hcd (Alpine linux-firmware-brcm, same bytes as RPi-Distro bluez-firmware cdf61dc6), if cached.
fn real_hcd() -> Option<Vec<u8>> {
    let dir = std::env::var("BTFW_CACHE").unwrap_or_else(|_| format!("{}/.cache/leandros/alpine-fw/aarch64/current/lib/firmware/brcm", std::env::var("HOME").unwrap_or_default()));
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
    /// Like the real BCM4345C0: after answering Reset the controller
    /// re-initialises and drops every command until the host has waited.
    reboot_window: bool,
    rebooting: bool,
    /// A rate mismatch puts garbage on the wire instead of silence.
    garble: bool,
    /// Real Pi 5 suspect: with the UART's auto RTS the host's RTS stays
    /// deasserted once an answer reaches the 8-byte RX trigger, so the
    /// controller sends 8 bytes and holds the rest (and everything after).
    auto_rts_stall: bool,
    soft: bool,
    held: VecDeque<u8>,
    /// Real Pi 5 (main dd54cdcd): the BCM43xx samples CTS_N (= host RTS) when
    /// BT_ON rises; asserted there it answers HCI Reset and nothing after.
    rts_strap: bool,
    rts_held: bool,
    powered: bool,
    strapped: bool,
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
        let mut b = vec![h4::H4_EVT, code, p.len() as u8];
        b.extend_from_slice(p);
        for x in b {
            let stalled = self.auto_rts_stall && !self.soft && (!self.held.is_empty() || self.rx.len() >= 8);
            if stalled { self.held.push_back(x) } else { self.rx.push_back(x) }
        }
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
        if self.rebooting { return; }
        if self.strapped && op != h4::OP_RESET { return; }
        if self.noisy { self.evt(0xff, &[0x12, 0x34]); }
        if self.mute == Some(op) { return; }
        if let Some((o, s)) = self.fail_status { if o == op { return self.cc(op, &[s]); } }
        match op {
            h4::OP_RESET => {
                if self.ignore_resets > 0 { self.ignore_resets -= 1; return; }
                self.cc(op, &[0]);
                self.rebooting = self.reboot_window;
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
                else if self.garble { self.rx.extend([0xf0, 0x0f, 0x00, 0xff]); }
            }
        }
        true
    }
    fn recv(&mut self, _timeout_us: u32) -> Option<u8> { self.rx.pop_front() }
    fn set_baud(&mut self, baud: u32) -> bool { self.host_baud = baud; true }
    fn set_flow(&mut self, f: bringup::Flow) -> bool {
        self.soft = f == bringup::Flow::Soft;
        if self.soft { let h: Vec<u8> = self.held.drain(..).collect(); self.rx.extend(h); }
        true
    }
    fn lines(&mut self) -> bringup::Lines {
        bringup::Lines { mcr: if self.soft { 0x03 } else { 0x23 }, msr: 0x10, rts_pad: Some(self.rts_held || !self.held.is_empty()), cts_pad: Some(false), dcts: 0 }
    }
    fn sleep_ms(&mut self, ms: u32) {
        if ms >= bringup::RESET_SETTLE_MS { self.rebooting = false; }
        self.sleeps.push(ms);
    }
    fn set_power(&mut self, on: bool) {
        if on && !self.powered {
            // Power-on: ROM at 115200, unpatched, RTS sampled now.
            self.ctrl_baud = INIT_BAUD;
            self.patched = false;
            self.minidriver = false;
            self.rebooting = false;
            self.rx.clear();
            self.strapped = self.rts_strap && !self.rts_held;
        }
        self.powered = on;
    }
    fn hold_rts_deasserted(&mut self, hold: bool) { self.rts_held = hold; }
}

/// The pre-fix power-up: BT_ON toggled while the UART keeps RTS asserted.
struct NoRtsHold<'a>(&'a mut Fake);

impl Transport for NoRtsHold<'_> {
    fn send(&mut self, b: &[u8]) -> bool { self.0.send(b) }
    fn recv(&mut self, t: u32) -> Option<u8> { self.0.recv(t) }
    fn set_baud(&mut self, b: u32) -> bool { self.0.set_baud(b) }
    fn sleep_ms(&mut self, ms: u32) { self.0.sleep_ms(ms) }
    fn set_flow(&mut self, f: bringup::Flow) -> bool { self.0.set_flow(f) }
    fn lines(&mut self) -> bringup::Lines { self.0.lines() }
    fn set_power(&mut self, on: bool) { self.0.set_power(on) }
}

/// Real Pi 5 (D0, main dd54cdcd, kexec): Reset answered twice, Read Local
/// Version never, with RTS and CTS both asserted (flow control ruled out).
/// Linux hci_bcm (`brcm,bcm43438-bt` => drive_rts_on_open) holds RTS
/// deasserted across the BT_ON edge; without that the controller is strapped.
#[test]
fn rts_asserted_at_power_on_answers_only_reset() {
    let mut f = Fake::new();
    f.rts_strap = true;
    let pw = bringup::power_cycle(&mut NoRtsHold(&mut f), 50);
    assert_eq!(pw.rts_pad_at_on, Some(false));
    let e = bringup::bring_up(&mut NoRtsHold(&mut f), &Options { cmd_timeout_us: 5_000, ..Options::default() }).unwrap_err();
    assert_eq!((e.stage, e.kind), ("version", ErrorKind::NoAnswer(0x1001)));
    assert_eq!((e.diag.rx_len, e.diag.reset_tries, e.diag.version_tries), (0, 2, 2));
    assert_eq!(f.log, [0x0c03, 0x1001, 0x0c03, 0x1001]);
}

#[test]
fn power_cycle_holds_rts_deasserted_across_bt_on_edge() {
    let img = real_hcd().unwrap_or_else(synthetic_hcd);
    let ops: Vec<u16> = hcd::records(&img).map(|r| r.unwrap().1).collect();
    let mut f = Fake::new();
    f.rts_strap = true;
    // State a previous kernel left behind (kexec): patched, at 3 Mbaud.
    f.patched = true;
    f.ctrl_baud = 3_000_000;
    f.powered = true;
    let pw = bringup::power_cycle(&mut f, 50);
    assert_eq!(pw, bringup::PowerDiag { off_ms: 50, rts_pad_at_on: Some(true) });
    assert!(!f.rts_held && !f.strapped && f.powered);
    assert_eq!(f.sleeps, [50, bringup::POWER_ON_SETTLE_MS]);
    let o = Options { hcd: Some(&img), oper_baud: Some(3_000_000), bdaddr: Some(DT_ADDR), cmd_timeout_us: 0 };
    let r = bringup::bring_up(&mut f, &o).unwrap();
    assert_eq!(f.log, expected_transcript(&ops));
    assert_eq!((r.rom.lmp_subver, r.ver.lmp_subver, r.baud, r.flow), (0x2105, 0x6119, 3_000_000, bringup::Flow::Auto));
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
    assert_eq!(f.sleeps, [100, 50, 250, 100, 10]);
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
    assert_eq!((e.diag.baud, e.diag.rx_len, e.diag.rx_hex().as_str()), (3_000_000, 0, "-"));
}

/// Real Pi 5 (main c6f5fc63): Reset answered, the Read Local Version sent
/// right after it got nothing. The controller is still re-initialising; the
/// 100 ms btbcm_reset() settle must come before the next command, after the
/// patched-firmware Reset too.
#[test]
fn reset_settle_before_next_command() {
    let img = synthetic_hcd();
    let mut f = Fake::new();
    f.reboot_window = true;
    let o = Options { hcd: Some(&img), oper_baud: Some(3_000_000), bdaddr: None, cmd_timeout_us: 5_000 };
    let r = bringup::bring_up(&mut f, &o).unwrap();
    assert_eq!((r.reset_tries, r.version_tries), (1, 1));
    assert_eq!(f.log.iter().filter(|&&op| op == 0x1001).count(), 2, "no lost version command");
    assert_eq!(r.ver.lmp_subver, 0x6119);
}

/// Controller still at another rate (e.g. left at 3 Mbaud): the failure line
/// must show the host rate and the raw garbage, not just NoAnswer.
#[test]
fn rate_mismatch_failure_reports_baud_and_raw_bytes() {
    let mut f = Fake::new();
    f.ctrl_baud = 3_000_000;
    f.garble = true;
    let e = bringup::bring_up(&mut f, &Options { cmd_timeout_us: 5_000, ..Options::default() }).unwrap_err();
    assert_eq!((e.stage, e.kind), ("reset", ErrorKind::NoAnswer(0x0c03)));
    assert_eq!((e.diag.baud, e.diag.rx_len, e.diag.reset_tries), (INIT_BAUD, 4, 2));
    assert_eq!(e.diag.rx_hex(), "f0 0f 00 ff");
}

/// Answer for Reset, then silence: the stage is `version`, after one retry,
/// with an empty receive window.
#[test]
fn silent_after_reset_is_version_no_answer_with_empty_rx() {
    let mut f = Fake::new();
    f.mute = Some(h4::OP_READ_LOCAL_VERSION);
    let e = bringup::bring_up(&mut f, &Options { cmd_timeout_us: 5_000, ..Options::default() }).unwrap_err();
    assert_eq!((e.stage, e.kind), ("version", ErrorKind::NoAnswer(0x1001)));
    assert_eq!((e.diag.baud, e.diag.rx_len, e.diag.reset_tries, e.diag.version_tries), (INIT_BAUD, 0, 2, 2));
    assert_eq!(f.log, [0x0c03, 0x1001, 0x0c03, 0x1001]);
    assert_eq!(f.sleeps, [100, 100]);
    assert_eq!((e.diag.flow, e.diag.reset_tries), (bringup::Flow::Soft, 2));
    let (w, l) = e.diag.v1.expect("first window kept");
    assert_eq!((w.hex().as_str(), l.rts_pad), ("-", Some(false)));
}

/// Real Pi 5 (main 910d71e7): Reset answered (7 bytes, below the 8-byte RX
/// trigger), both Read Local Version attempts got nothing. With auto RTS
/// stuck deasserted at the trigger the controller holds the 15-byte answer;
/// the bring-up must fall back to driver-driven RTS/CTS and finish there
/// (patch, 3 Mbaud, BD_ADDR), reporting the stalled first window.
#[test]
fn auto_rts_stall_falls_back_to_soft_flow() {
    let img = synthetic_hcd();
    let mut f = Fake::new();
    f.auto_rts_stall = true;
    f.reboot_window = true;
    let o = Options { hcd: Some(&img), oper_baud: Some(3_000_000), bdaddr: Some(DT_ADDR), cmd_timeout_us: 5_000 };
    let r = bringup::bring_up(&mut f, &o).unwrap();
    assert_eq!(r.flow, bringup::Flow::Soft);
    assert_eq!((r.reset_tries, r.version_tries), (2, 2));
    let (w, l) = r.v1.expect("stalled window reported");
    assert_eq!(w.hex(), "04 0e 0c 01 01 10 00 09");
    assert_eq!((l.rts_pad, l.mcr), (Some(true), 0x23));
    assert_eq!(r.ver.lmp_subver, 0x6119);
    assert_eq!((r.baud, r.bdaddr, r.bdaddr_set), (3_000_000, DT_ADDR, true));
    assert_eq!(&f.log[..4], [0x0c03, 0x1001, 0x0c03, 0x1001]);
}

/// A controller that answers in auto mode never sees the fallback.
#[test]
fn healthy_controller_stays_in_auto_flow() {
    let mut f = Fake::new();
    let r = bringup::bring_up(&mut f, &Options { cmd_timeout_us: 5_000, ..Options::default() }).unwrap();
    assert_eq!((r.flow, r.v1, r.reset_tries, r.version_tries), (bringup::Flow::Auto, None, 1, 1));
    assert!(!f.soft);
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

// ── bdaddr selection (bt.bdaddr=) ────────────────────────────────────────────

#[test]
fn bdaddr_from_otp_mac_is_lsb_first() {
    let a = bringup::bdaddr_from_mac(&[0xd8, 0x3a, 0xdd, 0x12, 0x34, 0x58]);
    assert_eq!(a.0, [0x58, 0x34, 0x12, 0xdd, 0x3a, 0xd8], "WRITE_BD_ADDR payload is LSB first");
    assert_eq!(format!("{}", a), "d8:3a:dd:12:34:58");
}

#[test]
fn bdaddr_knob_selection() {
    let dt = Some(BdAddr([1, 2, 3, 4, 5, 6]));
    let otp = Some(bringup::bdaddr_from_mac(&[0xd8, 0x3a, 0xdd, 0, 0, 2]));
    assert_eq!(bringup::pick_bdaddr(None, None, otp), (otp, "set-from-otp"));
    assert_eq!(bringup::pick_bdaddr(Some("auto"), dt, otp), (dt, "set-from-dt"));
    assert_eq!(bringup::pick_bdaddr(Some("auto"), Some(BdAddr([0; 6])), otp), (otp, "set-from-otp"));
    assert_eq!(bringup::pick_bdaddr(Some("dt"), None, otp), (None, "set-from-dt"));
    assert_eq!(bringup::pick_bdaddr(Some("otp"), dt, otp), (otp, "set-from-otp"));
    assert_eq!(bringup::pick_bdaddr(Some("keep"), dt, otp), (None, "placeholder"));
    assert_eq!(bringup::pick_bdaddr(None, None, None), (None, "set-from-otp"));
    let (a, l) = bringup::pick_bdaddr(Some("aa:bb:cc:dd:ee:ff"), dt, otp);
    assert_eq!((a.unwrap().0, l), ([0xff, 0xee, 0xdd, 0xcc, 0xbb, 0xaa], "set-from-cmdline"));
    assert_eq!(bringup::pick_bdaddr(Some("zz:bb"), dt, otp).1, "set-from-dt");
    assert!(bringup::parse_bdaddr_text("aa:bb:cc:dd:ee").is_none());
    assert!(bringup::parse_bdaddr_text("00:00:00:00:00:00").is_none());
}
