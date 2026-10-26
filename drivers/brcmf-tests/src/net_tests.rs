//! cfg/event against a mock `Fwil` that records every ioctl/iovar and
//! replays canned firmware events.

use std::cell::Cell;
use std::collections::{HashMap, VecDeque};

use crate::brcmf::cfg::*;
use crate::brcmf::event::{self, ev, status, EventMask};
use crate::brcmf::fwil::{FwError, Fwil, RxSink};
use crate::wifi_frame;

thread_local! { static NOW: Cell<u64> = Cell::new(0); }
fn now() -> u64 { NOW.with(|n| n.get()) }
fn advance(ms: u64) { NOW.with(|n| n.set(n.get() + ms)) }
fn env() -> Env { Env { now_ms: now, sleep_ms: advance, log: |_| {} } }

#[derive(Clone, Debug, PartialEq)]
enum Call {
    CmdGet(u32),
    CmdSet(u32, Vec<u8>),
    IovarGet(String),
    IovarSet(String, Vec<u8>),
    Tx(Vec<u8>),
}

enum Frame { Event(Vec<u8>), Data(Vec<u8>) }

#[derive(Default)]
struct Mock {
    calls: Vec<Call>,
    pending: VecDeque<Frame>,
    /// Batches of frames queued when the named iovar (or "cmd:N") is set.
    react: HashMap<String, VecDeque<Vec<Frame>>>,
    refuse: Vec<String>,
    rssi: i32,
}

impl Mock {
    fn on(&mut self, key: &str, batch: Vec<Frame>) { self.react.entry(key.into()).or_default().push_back(batch); }
    fn fire(&mut self, key: &str) {
        if let Some(b) = self.react.get_mut(key).and_then(|q| q.pop_front()) { self.pending.extend(b); }
    }
    fn iovars_set(&self) -> Vec<&str> {
        self.calls.iter().filter_map(|c| if let Call::IovarSet(n, _) = c { Some(n.as_str()) } else { None }).collect()
    }
    fn iovar(&self, name: &str) -> Option<&Vec<u8>> {
        self.calls.iter().rev().find_map(|c| match c { Call::IovarSet(n, d) if n == name => Some(d), _ => None })
    }
    fn cmd(&self, c: u32) -> Option<&Vec<u8>> {
        self.calls.iter().rev().find_map(|x| match x { Call::CmdSet(n, d) if *n == c => Some(d), _ => None })
    }
}

impl Fwil for Mock {
    fn cmd_get(&mut self, cmd: u32, buf: &mut [u8]) -> Result<usize, FwError> {
        self.calls.push(Call::CmdGet(cmd));
        match cmd {
            cmd::GET_RSSI => { buf[..4].copy_from_slice(&self.rssi.to_le_bytes()); Ok(4) }
            cmd::GET_RATE => { buf[..4].copy_from_slice(&866u32.to_le_bytes()); Ok(4) }
            _ => Ok(0),
        }
    }
    fn cmd_set(&mut self, cmd: u32, buf: &[u8]) -> Result<(), FwError> {
        self.calls.push(Call::CmdSet(cmd, buf.to_vec()));
        if self.refuse.iter().any(|r| *r == format!("cmd:{}", cmd)) { return Err(FwError::Status(-23)); }
        self.fire(&format!("cmd:{}", cmd));
        Ok(())
    }
    fn iovar_get(&mut self, name: &str, buf: &mut [u8]) -> Result<usize, FwError> {
        self.calls.push(Call::IovarGet(name.into()));
        if name == "cur_etheraddr" { buf[..6].copy_from_slice(&[0xb8, 0x27, 0xeb, 1, 2, 3]); return Ok(6); }
        Err(FwError::Status(-23))
    }
    fn iovar_set(&mut self, name: &str, buf: &[u8]) -> Result<(), FwError> {
        self.calls.push(Call::IovarSet(name.into(), buf.to_vec()));
        if self.refuse.iter().any(|r| r == name) { return Err(FwError::Status(-23)); }
        self.fire(name);
        Ok(())
    }
    fn tx_data(&mut self, frame: &[u8]) -> Result<(), FwError> { self.calls.push(Call::Tx(frame.to_vec())); Ok(()) }
    fn poll(&mut self, sink: &mut dyn RxSink, _budget_us: u32) -> Result<(), FwError> {
        advance(1);
        // Drains everything received, as the real BCDC/SDPCM poll does.
        while let Some(fr) = self.pending.pop_front() {
            match fr {
                Frame::Event(f) => sink.on_event(&f),
                Frame::Data(f) => sink.on_data(&f),
            }
        }
        Ok(())
    }
}

const AP: [u8; 6] = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x01];
const AP2: [u8; 6] = [0x02, 0xaa, 0xbb, 0xcc, 0xdd, 0x02];
const SSID: &[u8] = b"homenet";

fn evf(t: u32, st: u32, reason: u32, flags: u16, data: &[u8]) -> Frame {
    Frame::Event(event::build(t, st, reason, flags, AP, data))
}

fn rsn_ies(ssid: &[u8], ch: u8) -> Vec<u8> {
    let mut v = vec![0, ssid.len() as u8];
    v.extend_from_slice(ssid);
    v.extend_from_slice(&[1, 8, 0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24]);
    v.extend_from_slice(&[3, 1, ch]);
    v.extend_from_slice(&[48, 20, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 2, 0, 0]);
    v
}

fn bss(bssid: [u8; 6], ssid: &[u8], ch: u8, rssi: i8) -> BssEntry {
    BssEntry { bssid, ssid: ssid.to_vec(), channel: ch, chanspec: chanspec20(ch), rssi, capability: 0x0411,
               beacon_period: 100, ies: rsn_ies(ssid, ch) }
}

fn escan_frames(sync: u16, list: &[BssEntry]) -> Vec<Frame> {
    let mut v: Vec<Frame> = list.iter().map(|b| evf(ev::ESCAN_RESULT, status::PARTIAL, 0, 0, &build_escan(sync, &[b.clone()]))).collect();
    v.push(evf(ev::ESCAN_RESULT, status::SUCCESS, 0, 0, &build_escan(sync, &[])));
    v
}

fn join_ok() -> Vec<Frame> {
    vec![
        evf(ev::ASSOC, status::SUCCESS, 0, 0, &[]),
        evf(ev::LINK, status::SUCCESS, 0, event::FLAG_LINK, &[]),
        evf(ev::PSK_SUP, status::FWSUP_WAIT_M3, 0, 0, &[]),
        evf(ev::PSK_SUP, status::FWSUP_COMPLETED, 0, 0, &[]),
        evf(ev::SET_SSID, status::SUCCESS, 0, 0, &[]),
    ]
}

fn psk_req() -> JoinReq {
    JoinReq { ssid: SSID.to_vec(), bssid: Some(AP), channel: 36, key: Key::from_secret(b"correct horse battery") }
}

#[derive(Default)]
struct Steps { seen: Vec<Step>, abort: bool }
impl JoinObserver for Steps {
    fn aborted(&self) -> bool { self.abort }
    fn step(&mut self, s: Step) { self.seen.push(s); }
}

fn up() -> Brcmf<Mock> {
    let mut d = Brcmf::new(Mock { rssi: -48, ..Default::default() }, env());
    d.init(*b"PT").expect("init");
    d.fw.calls.clear();
    d
}

// ── event.rs ───────────────────────────────────────────────────────────────

#[test]
fn event_roundtrip_big_endian() {
    let f = event::build(ev::DEAUTH_IND, 0, 15, 0, AP, &[1, 2, 3]);
    assert_eq!(f.len(), event::EVENT_HDR_LEN + 3);
    assert_eq!(&f[12..14], &[0x88, 0x6c]);
    // event_type is big-endian at 24 + 4.
    assert_eq!(&f[28..32], &[0, 0, 0, 6]);
    let e = event::parse(&f).unwrap();
    assert_eq!((e.event_type, e.status, e.reason, e.addr, e.ifidx), (ev::DEAUTH_IND, 0, 15, AP, 0));
    assert_eq!(e.data, &[1, 2, 3]);
}

#[test]
fn event_rejects_foreign_and_clamps_datalen() {
    let mut f = event::build(ev::LINK, 0, 0, 1, AP, &[9; 8]);
    let mut short = f.clone();
    short.truncate(event::EVENT_HDR_LEN + 4);
    assert_eq!(event::parse(&short).unwrap().data.len(), 4);
    assert!(event::parse(&f[..40]).is_none());
    f[12] = 0x08; // IPv4 ethertype
    assert!(event::parse(&f).is_none());
    let mut g = event::build(ev::LINK, 0, 0, 1, AP, &[]);
    g[19] = 0x99; // OUI
    assert!(event::parse(&g).is_none());
}

#[test]
fn if_event_parses() {
    let i = event::parse_if(&[1, event::if_action::ADD, 0, 1, 0]).unwrap();
    assert_eq!((i.ifidx, i.action, i.bsscfgidx), (1, event::if_action::ADD, 1));
    assert!(event::parse_if(&[1, 2]).is_none());
}

#[test]
fn event_mask_bits() {
    let m = EventMask::station();
    assert_eq!(m.bytes().len(), 16);
    for t in [ev::SET_SSID, ev::LINK, ev::DEAUTH, ev::DEAUTH_IND, ev::DISASSOC, ev::DISASSOC_IND, ev::PSK_SUP, ev::ESCAN_RESULT, ev::IF] {
        assert!(m.has(t), "{}", event::name(t));
    }
    assert_eq!(m.bytes()[0] & 1, 1);          // SET_SSID = bit 0
    assert_eq!(m.bytes()[69 / 8] & (1 << 5), 1 << 5); // ESCAN_RESULT = 69
    assert!(!m.has(15));
    assert!(!EventMask::none().set(200).has(200));
    assert!(!m.clear(ev::LINK).has(ev::LINK));
}

// ── encoders ───────────────────────────────────────────────────────────────

#[test]
fn escan_params_layout() {
    let p = escan_params(7, ESCAN_ACTION_START, &[]);
    assert_eq!(p.len(), 72);
    assert_eq!(&p[0..8], &[1, 0, 0, 0, 1, 0, 7, 0]);
    assert_eq!(&p[8 + 36..8 + 42], &[0xff; 6]);
    assert_eq!(p[8 + 42], 2);
    assert_eq!(&p[8 + 60..8 + 64], &[0, 0, 0, 0]);
    let d = escan_params(8, ESCAN_ACTION_START, &[b"hidden"]);
    assert_eq!(d.len(), 72 + 2 * 36);
    assert_eq!(u32::from_le_bytes(d[68..72].try_into().unwrap()), 2 << 16);
    assert_eq!(&d[72..76], &[0, 0, 0, 0]);            // wildcard first
    assert_eq!(&d[108..112], &[6, 0, 0, 0]);
    assert_eq!(&d[112..118], b"hidden");
}

#[test]
fn join_params_layout() {
    let p = ext_join_params(SSID, Some(AP), 36);
    assert_eq!(p.len(), 72);
    assert_eq!(&p[0..4], &[7, 0, 0, 0]);
    assert_eq!(&p[4..11], SSID);
    assert_eq!(p[36], 0xff);
    assert_eq!(i32::from_le_bytes(p[44..48].try_into().unwrap()), 320);
    assert_eq!(&p[56..62], &AP);
    assert_eq!(u32::from_le_bytes(p[64..68].try_into().unwrap()), 1);
    assert_eq!(u16::from_le_bytes(p[68..70].try_into().unwrap()), 0xd024); // 5 GHz | BW20 | ch 36
    assert_eq!(chanspec20(6), 0x1006);
    let j = join_params(SSID, None, 0);
    assert_eq!(j.len(), 52);
    assert_eq!(&j[36..42], &[0xff; 6]);
}

#[test]
fn pmk_struct_passphrase_and_hex() {
    let p = wsec_pmk(&Key::from_secret(b"password1"));
    assert_eq!(p.len(), 70);
    assert_eq!(&p[0..4], &[9, 0, 1, 0]);
    assert_eq!(&p[4..13], b"password1");
    let hex = "00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF";
    let k = Key::from_secret(hex.as_bytes());
    assert!(matches!(k, Key::Pmk(_)));
    let p = wsec_pmk(&k);
    assert_eq!(&p[0..4], &[64, 0, 0, 0]);
    assert_eq!(&p[4..68], hex.to_ascii_lowercase().as_bytes());
    assert_eq!(format!("{:?}", Key::from_secret(b"secretpw")), "Passphrase(<redacted>)");
    assert!(matches!(Key::from_secret(b""), Key::Open));
}

#[test]
fn country_selection() {
    assert_eq!(&country_from_cmdline(""), b"PT");
    assert_eq!(&country_from_cmdline("root=x wifi.country=de"), b"DE");
    assert_eq!(&country_from_cmdline("wifi.country=DE brcmf.country=us"), b"US");
    assert_eq!(&country_from_cmdline("brcmf.country=USA"), b"PT");
    let c = country_params(*b"PT");
    assert_eq!(&c, &[b'P', b'T', 0, 0, 0xff, 0xff, 0xff, 0xff, b'P', b'T', 0, 0]);
}

// ── init ───────────────────────────────────────────────────────────────────

#[test]
fn init_sequence() {
    let mut d = Brcmf::new(Mock::default(), env());
    assert_eq!(d.init(*b"PT").unwrap(), [0xb8, 0x27, 0xeb, 1, 2, 3]);
    let c = &d.fw.calls;
    assert_eq!(c[0], Call::IovarGet("cur_etheraddr".into()));
    assert_eq!(c[1], Call::IovarSet("country".into(), country_params(*b"PT").to_vec()));
    assert_eq!(c[2], Call::IovarSet("mpc".into(), vec![0, 0, 0, 0]));
    assert_eq!(c[3], Call::IovarSet("bus:txglom".into(), vec![0, 0, 0, 0]));
    assert_eq!(c[4], Call::IovarSet("event_msgs".into(), EventMask::station().bytes().to_vec()));
    assert_eq!(c[5], Call::CmdSet(cmd::UP, vec![1, 0, 0, 0]));
    assert!(d.radio_on());
}

#[test]
fn init_tolerates_optional_refusals() {
    let mut m = Mock::default();
    m.refuse = vec!["country".into(), "bus:txglom".into(), "mpc".into()];
    let mut d = Brcmf::new(m, env());
    assert!(d.init(*b"PT").is_ok());
    let mut m = Mock::default();
    m.refuse = vec!["event_msgs".into()];
    let mut d = Brcmf::new(m, env());
    assert_eq!(d.init(*b"PT"), Err(FwError::Status(-23)));
}

// ── scan ───────────────────────────────────────────────────────────────────

#[test]
fn scan_accumulates_and_dedups() {
    let mut d = up();
    let hidden = BssEntry { ssid: vec![], ..bss(AP2, b"", 6, -40) };
    let frames = escan_frames(1, &[bss(AP, SSID, 36, -70), bss(AP2, b"other", 6, -60), bss(AP, SSID, 36, -50), hidden]);
    d.fw.on("escan", frames);
    let r = d.scan(&[], 1000, &mut NoObserver).unwrap();
    assert_eq!(r.len(), 2);
    // strongest first; AP2 kept its name across the hidden sighting.
    assert_eq!((r[0].bssid, r[0].rssi), (AP2, -40));
    assert_eq!(r[0].ssid, b"other");
    assert_eq!((r[1].bssid, r[1].rssi, r[1].channel), (AP, -50, 36));
    // The entry parses as a beacon with RSN (WPA2-PSK/CCMP).
    let b = wifi_frame::parse_beacon(&r[1].beacon_frame(), r[1].channel).unwrap();
    assert_eq!((b.bssid, b.ssid.as_slice(), b.channel), (AP, SSID, 36));
    assert!(b.rsn.is_some());
    assert_eq!(d.fw.cmd(cmd::SET_PASSIVE_SCAN), Some(&vec![0, 0, 0, 0]));
}

#[test]
fn scan_ignores_stale_sync_id() {
    let mut d = up();
    let mut f = escan_frames(99, &[bss(AP, SSID, 1, -50)]);
    f.insert(0, evf(ev::ESCAN_RESULT, status::PARTIAL, 0, 0, &build_escan(1, &[bss(AP2, b"x", 11, -30)])));
    d.fw.on("escan", f);
    let r = d.scan(&[], 1000, &mut NoObserver).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].bssid, AP2);
}

#[test]
fn scan_timeout_aborts() {
    let mut d = up();
    assert_eq!(d.scan(&[b"x"], 50, &mut NoObserver), Err(FwError::Timeout));
    let p = d.fw.iovar("escan").unwrap();
    assert_eq!(u16::from_le_bytes([p[4], p[5]]), ESCAN_ACTION_ABORT);
}

// ── the whole life cycle ───────────────────────────────────────────────────

#[test]
fn scan_join_data_deauth_reconnect() {
    let mut d = up();
    d.fw.on("escan", escan_frames(1, &[bss(AP, SSID, 36, -45)]));
    let hits = d.scan(&[], 1000, &mut NoObserver).unwrap();
    assert_eq!(hits.len(), 1);

    // Join through the firmware supplicant.
    d.fw.on("join", join_ok());
    let mut obs = Steps::default();
    d.join(psk_req(), &mut obs, 5000).expect("join");
    assert_eq!(obs.seen, vec![Step::Associating, Step::Handshake]);
    assert_eq!(d.link(), Link::Up);
    assert_eq!(d.bssid(), Some(AP));
    let f = &d.fw;
    assert_eq!(f.cmd(cmd::SET_INFRA), Some(&vec![1, 0, 0, 0]));
    assert_eq!(f.cmd(cmd::SET_AUTH), Some(&vec![0, 0, 0, 0]));
    assert_eq!(f.cmd(cmd::SET_WSEC), Some(&vec![4, 0, 0, 0]));
    assert_eq!(f.cmd(cmd::SET_WPA_AUTH), Some(&vec![0x80, 0, 0, 0]));
    assert_eq!(f.iovar("sup_wpa"), Some(&vec![1, 0, 0, 0]));
    let pmk = f.cmd(cmd::SET_WSEC_PMK).unwrap();
    assert_eq!(&pmk[0..4], &[21, 0, 1, 0]);
    assert_eq!(f.iovar("join"), Some(&ext_join_params(SSID, Some(AP), 36)));
    // Order: security first, then join.
    let names = f.iovars_set();
    let pos = |n: &str| names.iter().position(|x| *x == n).unwrap();
    assert!(pos("sup_wpa") < pos("join"));
    assert_eq!(d.rssi(), Some(-48));
    assert_eq!(d.rate_kbps(), 433_000);

    // Data in.
    let frame_in = vec![0x11u8; 98];
    d.fw.pending.push_back(Frame::Data(frame_in.clone()));
    assert_eq!(d.poll_link(1000).unwrap(), None);
    assert_eq!(d.take_rx(), Some(frame_in));
    assert_eq!(d.take_rx(), None);
    // Data out.
    let frame_out = vec![0x22u8; 60];
    d.tx(&frame_out).unwrap();
    assert_eq!(d.fw.calls.last(), Some(&Call::Tx(frame_out)));

    // The AP deauthenticates us.
    d.fw.pending.push_back(evf(ev::DEAUTH_IND, 0, 7, 0, &[]));
    assert_eq!(d.poll_link(1000).unwrap(), Some(Down::Deauth { reason: 7, by_ap: true }));
    assert_eq!(d.link(), Link::Down);
    assert!(d.tx(&[0; 60]).is_err());
    assert_eq!(d.poll_link(1000).unwrap(), None);

    // Reconnect to the same network.
    d.fw.on("join", join_ok());
    d.reconnect(&mut NoObserver, 5000).expect("reconnect");
    assert_eq!(d.link(), Link::Up);
    assert_eq!(d.stats.joins, 2);
    assert_eq!(d.stats.link_downs, 1);

    // Our own disconnect: DISASSOC with reason 3 and the BSSID.
    d.disconnect().unwrap();
    assert_eq!(d.fw.cmd(cmd::DISASSOC), Some(&scb_val(3, AP).to_vec()));
    assert_eq!(d.link(), Link::Down);
    assert_eq!(d.rssi(), None);
}

#[test]
fn link_loss_and_supplicant_failure_while_up() {
    let mut d = up();
    d.fw.on("join", join_ok());
    d.join(psk_req(), &mut NoObserver, 5000).unwrap();
    d.fw.pending.push_back(evf(ev::LINK, 0, 2, 0, &[]));
    assert_eq!(d.poll_link(0).unwrap(), Some(Down::LinkLost { reason: 2 }));
    d.fw.on("join", join_ok());
    d.reconnect(&mut NoObserver, 5000).unwrap();
    d.fw.pending.push_back(evf(ev::PSK_SUP, status::FWSUP_TIMEOUT, 0, 0, &[]));
    assert_eq!(d.poll_link(0).unwrap(), Some(Down::SupFailed { status: status::FWSUP_TIMEOUT, reason: 0 }));
}

#[test]
fn open_network_join() {
    let mut d = up();
    d.fw.on("join", vec![evf(ev::LINK, 0, 0, event::FLAG_LINK, &[]), evf(ev::SET_SSID, 0, 0, 0, &[])]);
    let req = JoinReq { ssid: b"cafe".to_vec(), bssid: None, channel: 0, key: Key::Open };
    let mut obs = Steps::default();
    d.join(req, &mut obs, 5000).unwrap();
    assert_eq!(obs.seen, vec![Step::Associating]);
    assert_eq!(d.fw.cmd(cmd::SET_WSEC), Some(&vec![0, 0, 0, 0]));
    assert!(d.fw.cmd(cmd::SET_WSEC_PMK).is_none());
}

#[test]
fn wrong_key_is_a_handshake_failure() {
    let mut d = up();
    d.fw.on("join", vec![
        evf(ev::LINK, 0, 0, event::FLAG_LINK, &[]),
        evf(ev::PSK_SUP, status::FWSUP_WAIT_M3, 0, 0, &[]),
        evf(ev::DEAUTH_IND, 0, 15, 0, &[]),
    ]);
    let e = d.join(psk_req(), &mut NoObserver, 5000).unwrap_err();
    assert_eq!(e, JoinFail::Handshake { msg2: true, reason: Some(15) });
    assert_eq!(d.link(), Link::Down);
    assert_eq!(d.fw.cmd(cmd::DISASSOC), Some(&scb_val(3, [0; 6]).to_vec()));
    // A later LINK-down for the failed attempt is not reported as a drop.
    d.fw.pending.push_back(evf(ev::LINK, 0, 0, 0, &[]));
    assert_eq!(d.poll_link(0).unwrap(), None);
}

#[test]
fn supplicant_timeout_after_m2() {
    let mut d = up();
    d.fw.on("join", vec![
        evf(ev::LINK, 0, 0, event::FLAG_LINK, &[]),
        evf(ev::PSK_SUP, status::FWSUP_TIMEOUT, 0, 0, &[]),
    ]);
    let e = d.join(psk_req(), &mut NoObserver, 5000).unwrap_err();
    assert_eq!(e, JoinFail::Handshake { msg2: true, reason: None });
}

#[test]
fn no_network_and_assoc_refusal() {
    let mut d = up();
    d.fw.on("join", vec![evf(ev::SET_SSID, status::NO_NETWORKS, 0, 0, &[])]);
    assert_eq!(d.join(psk_req(), &mut NoObserver, 5000), Err(JoinFail::NoNetwork));
    d.fw.on("join", vec![evf(ev::SET_SSID, status::FAIL, 17, 0, &[])]);
    assert_eq!(d.join(psk_req(), &mut NoObserver, 5000), Err(JoinFail::Assoc { status: status::FAIL, reason: 17 }));
    assert_eq!(d.stats.join_fails, 2);
}

#[test]
fn join_iovar_refused_falls_back_to_set_ssid() {
    let mut d = up();
    d.fw.refuse = vec!["join".into()];
    d.fw.on(&format!("cmd:{}", cmd::SET_SSID), join_ok());
    d.join(psk_req(), &mut NoObserver, 5000).unwrap();
    assert_eq!(d.fw.cmd(cmd::SET_SSID), Some(&join_params(SSID, Some(AP), 36)));
}

#[test]
fn join_timeout_and_abort() {
    let mut d = up();
    let t0 = now();
    assert_eq!(d.join(psk_req(), &mut NoObserver, 300), Err(JoinFail::Timeout));
    assert!(now() - t0 >= 300 && now() - t0 < 1000);
    let mut obs = Steps { abort: true, ..Default::default() };
    assert_eq!(d.join(psk_req(), &mut obs, 5000), Err(JoinFail::Aborted));
    // Associated but the handshake never finished: timed out after message 2.
    d.fw.on("join", vec![evf(ev::LINK, 0, 0, event::FLAG_LINK, &[])]);
    assert_eq!(d.join(psk_req(), &mut NoObserver, 300), Err(JoinFail::Handshake { msg2: true, reason: None }));
}

#[test]
fn rx_queue_is_bounded_and_foreign_ifs_ignored() {
    let mut rx = Rx::new();
    for i in 0..(RX_QUEUE_MAX + 5) { rx.on_data(&[i as u8; 20]); }
    assert_eq!(rx.rx.len(), RX_QUEUE_MAX);
    assert_eq!(rx.rx_dropped, 5);
    let mut f = event::build(ev::LINK, 0, 0, event::FLAG_LINK, AP, &[]);
    f[24 + 46] = 1; // ifidx 1 (a P2P interface)
    rx.on_event(&f);
    assert!(!rx.link_up);
    rx.on_event(&event::build(ev::IF, 0, 0, 0, AP, &[1, 1, 0, 1, 0]));
    assert_eq!(rx.if_events, 1);
    rx.on_event(&[0u8; 10]);
    assert_eq!(rx.bad_events, 1);
}
