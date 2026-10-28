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
fn env() -> Env { Env { now_ms: now, sleep_ms: advance, log: |_| {}, random: |b| b.fill(7) } }

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
    /// Frames the "air" answers a transmitted data frame with (the AP side of the 4-way handshake).
    on_tx: Option<Box<dyn FnMut(&[u8]) -> Vec<Frame>>>,
    /// Canned answers for iovar gets ("assoc_info", "assoc_req_ies", ...).
    gets: HashMap<String, Vec<u8>>,
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
        // The real firmware answers BCME_BUFTOOSHORT (-14) to a too small assoc_info buffer.
        if name == "assoc_info" && buf.len() < 32 { return Err(FwError::Status(-14)); }
        if let Some(v) = self.gets.get(name) { let n = v.len().min(buf.len()); buf[..n].copy_from_slice(&v[..n]); return Ok(n); }
        Err(FwError::Status(-23))
    }
    fn iovar_set(&mut self, name: &str, buf: &[u8]) -> Result<(), FwError> {
        self.calls.push(Call::IovarSet(name.into(), buf.to_vec()));
        if self.refuse.iter().any(|r| r == name) { return Err(FwError::Status(-23)); }
        self.fire(name);
        Ok(())
    }
    fn tx_data(&mut self, frame: &[u8]) -> Result<(), FwError> {
        self.calls.push(Call::Tx(frame.to_vec()));
        if let Some(f) = self.on_tx.as_mut() { let v = f(frame); self.pending.extend(v); }
        Ok(())
    }
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
fn pmk_struct_forms() {
    let k = Key::from_secret(b"password1");
    let p = wsec_pmk(&k, SSID, PmkForm::Passphrase).unwrap();
    assert_eq!(p.len(), 70);
    assert_eq!(&p[0..4], &[9, 0, 1, 0]);
    assert_eq!(&p[4..13], b"password1");
    let psk = crate::wifi_crypto::wpa_psk(b"password1", SSID);
    // Raw = Linux brcmf_set_pmk: 32 bytes, flags 0.
    let p = wsec_pmk(&k, SSID, PmkForm::Raw).unwrap();
    assert_eq!(&p[0..4], &[32, 0, 0, 0]);
    assert_eq!(&p[4..36], &psk);
    let hex = "00112233445566778899aabbccddeeff00112233445566778899AABBCCDDEEFF";
    let k = Key::from_secret(hex.as_bytes());
    assert!(matches!(k, Key::Pmk(_)));
    // Hex = Linux 4.13: 64 hex digits with WSEC_PASSPHRASE.
    let p = wsec_pmk(&k, SSID, PmkForm::Hex).unwrap();
    assert_eq!(&p[0..4], &[64, 0, 1, 0]);
    assert_eq!(&p[4..68], hex.to_ascii_lowercase().as_bytes());
    assert!(wsec_pmk(&k, SSID, PmkForm::Passphrase).is_none());
    assert_eq!(format!("{:?}", Key::from_secret(b"secretpw")), "Passphrase(<redacted>)");
    assert!(matches!(Key::from_secret(b""), Key::Open));
}

#[test]
fn wsec_key_layout_matches_brcmf_wsec_key_le() {
    let tk = [0x11u8; 16];
    let p = wsec_key(0, &tk, Some(AP));
    assert_eq!(p.len(), 164);
    assert_eq!(&p[0..8], &[0, 0, 0, 0, 16, 0, 0, 0]);
    assert_eq!(&p[8..24], &tk);
    assert_eq!(&p[112..120], &[4, 0, 0, 0, 0, 0, 0, 0]); // AES_CCM, pairwise: no flags
    assert_eq!(&p[156..162], &AP);
    let g = wsec_key(1, &[0x22; 16], None);
    assert_eq!(&g[0..4], &[1, 0, 0, 0]);
    assert_eq!(&g[116..120], &[2, 0, 0, 0]); // BRCMF_PRIMARY_KEY
    assert_eq!(&g[156..162], &[0; 6]);
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
    let mut c = d.fw.calls.clone();
    assert_eq!(c.remove(1), Call::IovarGet("cap".into()));
    let c = &c;
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
    // Default PMK form = Linux brcmf_set_pmk: the 32-byte PSK, flags 0.
    assert_eq!(&pmk[0..4], &[32, 0, 0, 0]);
    assert_eq!(&pmk[4..36], &crate::wifi_crypto::wpa_psk(b"correct horse battery", SSID));
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

// ── pibrcmf-up: MAC source and ctl parsing ─────────────────────────────────

#[test]
fn mac_from_cmdline_prefers_brcmf_mac_then_board_eth_plus_one() {
    let pi = "reboot=w smsc95xx.macaddr=2C:CF:67:D8:CA:1A vc_mem.mem_base=0x3fc00000 root=LABEL=leandros-root";
    assert_eq!(mac_from_cmdline(pi), Some(([0x2c, 0xcf, 0x67, 0xd8, 0xca, 0x1b], "cmdline smsc95xx.macaddr+1")));
    let both = "smsc95xx.macaddr=2C:CF:67:D8:CA:1A brcmf.mac=02:11:22:33:44:55";
    assert_eq!(mac_from_cmdline(both).map(|m| m.1), Some("cmdline brcmf.mac"));
    // NIC part wraps within 24 bits; OUI untouched.
    assert_eq!(wifi_from_eth(&[0xd8, 0x3a, 0xdd, 0xff, 0xff, 0xff]), [0xd8, 0x3a, 0xdd, 0, 0, 0]);
    assert_eq!(mac_from_cmdline("root=x"), None);
    assert_eq!(parse_mac("01:00:5e:00:00:01"), None, "multicast is not a station MAC");
    assert_eq!(parse_mac("00:00:00:00:00:00"), None);
    assert_eq!(parse_mac("2c:cf:67:d8:ca"), None);
}

#[test]
fn ctl_commands_parse_in_every_state() {
    assert_eq!(parse_ctl("seq=3 status"), CtlCmd::Status);
    assert_eq!(parse_ctl(""), CtlCmd::Status);
    assert_eq!(parse_ctl("seq=4 scan"), CtlCmd::Scan);
    assert_eq!(parse_ctl("seq=5 join"), CtlCmd::Join { ssid: None, knobs: vec![] });
    assert_eq!(parse_ctl("seq=6 join ssid=My Home Net"), CtlCmd::Join { ssid: Some("My Home Net"), knobs: vec![] });
    assert_eq!(parse_ctl("seq=6 join method=setssid supwpa=0 ssid=a b=c"),
               CtlCmd::Join { ssid: Some("a b=c"), knobs: vec!["method=setssid", "supwpa=0"] });
    let mut o = JoinOpts::default();
    assert_eq!(o.apply(&["method=setssid", "supwpa=0", "pmk=hex", "sec=iovar", "mfp=1", "wpaie=0", "bogus=1"]), vec!["bogus=1"]);
    assert_eq!(o.describe(), "method=setssid supwpa=0 pmk=hex sec=iovar mfp=1 wpaie=0 akm=auto mfpc=auto mfpr=auto msg2ie=auto");
    o.apply(&["reset"]);
    assert_eq!(o, JoinOpts::default());
    assert_eq!(parse_ctl("seq=7 diag lines=8"), CtlCmd::Diag(8));
    assert_eq!(parse_ctl("diag"), CtlCmd::Diag(32));
    assert_eq!(parse_ctl("seq=8 retry sdclk=25000 hs=0"), CtlCmd::Retry(vec!["sdclk=25000", "hs=0"]));
    assert_eq!(parse_ctl("seq=9 peek addr=0x18000000"), CtlCmd::Other("peek"));
}

// ── join variants (pibrcmf-join: Pi 5 firmware 7.45.265 answered -23) ───────

const STA: [u8; 6] = [0xb8, 0x27, 0xeb, 1, 2, 3];

fn eth(dst: [u8; 6], src: [u8; 6], pdu: &[u8]) -> Vec<u8> {
    let mut f = dst.to_vec();
    f.extend_from_slice(&src);
    f.extend_from_slice(&[0x88, 0x8e]);
    f.extend_from_slice(pdu);
    f
}

/// A minimal WPA2-PSK authenticator for the mock "air": M1 on join, M3 on M2.
fn ap_handshake(d: &mut Brcmf<Mock>, pass: &[u8], gtk: [u8; 16]) -> std::rc::Rc<std::cell::RefCell<Option<[u8; 16]>>> {
    use crate::wifi_crypto as wc;
    use wc::key_info as ki;
    let pmk = wc::wpa_psk(pass, SSID);
    let anonce = [0x5au8; 32];
    let m1 = wc::build_eapol_key(2, 2 | ki::TYPE_PAIRWISE | ki::ACK, 16, &1u64.to_be_bytes(), &anonce, &[]);
    d.fw.on("join", vec![
        evf(ev::ASSOC, status::SUCCESS, 0, 0, &[]),
        evf(ev::LINK, status::SUCCESS, 0, event::FLAG_LINK, &[]),
        Frame::Data(eth(STA, AP, &m1)),
    ]);
    let tk_out = std::rc::Rc::new(std::cell::RefCell::new(None));
    let tk2 = tk_out.clone();
    d.fw.on_tx = Some(Box::new(move |f: &[u8]| {
        if f.len() < 14 + 99 || f[12..14] != [0x88, 0x8e] { return vec![]; }
        assert_eq!(&f[0..6], &AP);
        let pdu = &f[14..];
        let info = u16::from_be_bytes([pdu[5], pdu[6]]);
        if info & ki::SECURE != 0 { return vec![]; } // M4
        let mut snonce = [0u8; 32];
        snonce.copy_from_slice(&pdu[17..49]);
        let ptk = wc::derive_ptk(&pmk, &AP, &STA, &anonce, &snonce);
        assert!(wc::eapol_mic_ok(&ptk.kck, pdu), "M2 MIC");
        *tk2.borrow_mut() = Some(ptk.tk);
        let mut kd = wc::rsn_ie_psk_ccmp().to_vec();
        kd.extend_from_slice(&[0xdd, 22, 0x00, 0x0f, 0xac, 0x01, 0x01, 0x00]);
        kd.extend_from_slice(&gtk);
        while kd.len() % 8 != 0 || kd.len() < 16 { kd.push(if kd.len() == 46 { 0xdd } else { 0 }); }
        let wrapped = wc::aes_key_wrap(&ptk.kek, &kd);
        let mut m3 = wc::build_eapol_key(2, 2 | ki::TYPE_PAIRWISE | ki::INSTALL | ki::ACK | ki::MIC | ki::SECURE | ki::ENCRYPTED_KEY_DATA,
                                         16, &2u64.to_be_bytes(), &anonce, &wrapped);
        wc::eapol_set_mic(&ptk.kck, &mut m3);
        vec![Frame::Data(eth(STA, AP, &m3))]
    }));
    tk_out
}

fn wsec_keys(m: &Mock) -> Vec<Vec<u8>> {
    m.calls.iter().filter_map(|c| match c { Call::IovarSet(n, d) if n == "wsec_key" => Some(d.clone()), _ => None }).collect()
}

#[test]
fn sup_wpa_unsupported_falls_back_to_host_handshake() {
    let mut d = up();
    d.fw.refuse = vec!["sup_wpa".into()]; // firmware without idsup: BCME_UNSUPPORTED (-23)
    let gtk = [0x33u8; 16];
    let tk = ap_handshake(&mut d, b"correct horse battery", gtk);
    let r = d.join(psk_req(), &mut Steps::default(), 5_000);
    assert_eq!(r, Ok(()), "fail_cmd {}", d.fail_cmd);
    assert_eq!(d.sup_used, "host");
    assert!(d.fail_cmd.contains("sup_wpa=1 -> Status(-23)"));
    // No firmware PMK in host mode; our RSN IE went to the firmware first.
    assert!(d.fw.cmd(cmd::SET_WSEC_PMK).is_none());
    assert_eq!(d.fw.iovar("wpaie").map(|v| v[0]), Some(0x30));
    let keys = wsec_keys(&d.fw);
    assert_eq!(keys.len(), 2, "pairwise + group");
    assert_eq!(keys[0], wsec_key(0, &tk.borrow().unwrap(), Some(AP)).to_vec());
    assert_eq!(keys[1], wsec_key(1, &gtk, None).to_vec());
    let tx = d.fw.calls.iter().filter(|c| matches!(c, Call::Tx(_))).count();
    assert_eq!(tx, 2, "M2 + M4");
    assert_eq!((d.stats.eapol_rx, d.stats.eapol_tx), (2, 2));
    // EAPOL never reaches the net stack.
    assert!(d.rx.rx.iter().all(|f| f[12..14] != [0x88, 0x8e]));
}

#[test]
fn forced_firmware_supplicant_reports_the_refused_command() {
    let mut d = up();
    d.fw.refuse = vec!["sup_wpa".into()];
    d.opts.apply(&["supwpa=1"]);
    let r = d.join(psk_req(), &mut Steps::default(), 2_000);
    assert_eq!(r, Err(JoinFail::Fw(FwError::Status(-23))));
    assert_eq!(d.fail_cmd, "iovar sup_wpa=1 -> Status(-23)");
    assert!(!d.fw.iovars_set().contains(&"join"));
}

#[test]
fn pmk_forms_are_tried_in_order_and_the_last_refusal_is_reported() {
    let mut d = up();
    d.fw.refuse = vec![format!("cmd:{}", cmd::SET_WSEC_PMK)];
    let r = d.join(psk_req(), &mut Steps::default(), 2_000);
    assert_eq!(r, Err(JoinFail::Fw(FwError::Status(-23))));
    let pmks: Vec<u16> = d.fw.calls.iter().filter_map(|c| match c {
        Call::CmdSet(n, b) if *n == cmd::SET_WSEC_PMK => Some(u16::from_le_bytes([b[0], b[1]])), _ => None }).collect();
    assert_eq!(pmks, vec![32, 21, 64], "raw, passphrase, hex");
    assert_eq!(d.fail_cmd, "WLC_SET_WSEC_PMK pmk=hex -> Status(-23)");
    // A single form when pinned, and a working one joins.
    let mut d = up();
    d.opts.apply(&["pmk=passphrase"]);
    d.fw.on("join", join_ok());
    assert_eq!(d.join(psk_req(), &mut Steps::default(), 2_000), Ok(()));
    assert_eq!(d.fw.cmd(cmd::SET_WSEC_PMK).map(|b| b[2]), Some(1));
    assert_eq!(d.sup_used, "firmware");
}

#[test]
fn join_method_and_sec_path_knobs() {
    let mut d = up();
    d.opts.apply(&["method=setssid", "sec=iovar", "mfp=0", "wpaie=0"]);
    d.fw.on(&format!("cmd:{}", cmd::SET_SSID), join_ok());
    assert_eq!(d.join(psk_req(), &mut Steps::default(), 2_000), Ok(()));
    let iov = d.fw.iovars_set();
    assert!(!iov.contains(&"join") && !iov.contains(&"wpaie"));
    for n in ["wpa_auth", "auth", "wsec", "mfp"] { assert!(iov.contains(&n), "{}", n); }
    assert!(d.fw.cmd(cmd::SET_WPA_AUTH).is_none());
    // joiniovar only: a refused join iovar is the failure, no SET_SSID.
    let mut d = up();
    d.opts.apply(&["method=joiniovar"]);
    d.fw.refuse = vec!["join".into()];
    assert_eq!(d.join(psk_req(), &mut Steps::default(), 2_000), Err(JoinFail::Fw(FwError::Status(-23))));
    assert!(d.fw.cmd(cmd::SET_SSID).is_none());
    assert_eq!(d.fail_cmd, "iovar join (ext join params) -> Status(-23)");
}

// ── pibrcmf-4way: AP RSN element, AKM 6 / MFP choice, msg2 IE = assoc IE ──

/// AP RSN IE: CCMP/CCMP, AKMs 2 and 6, given caps, optional group mgmt cipher.
fn ap_rsn_ie(caps: u16, gmgmt: bool) -> Vec<u8> {
    let mut b = vec![1, 0, 0x00, 0x0f, 0xac, 4, 1, 0, 0x00, 0x0f, 0xac, 4, 2, 0, 0x00, 0x0f, 0xac, 2, 0x00, 0x0f, 0xac, 6];
    b.extend_from_slice(&caps.to_le_bytes());
    if gmgmt { b.extend_from_slice(&[0, 0, 0x00, 0x0f, 0xac, 6]); }
    let mut ie = vec![48, b.len() as u8];
    ie.extend_from_slice(&b);
    ie
}

fn bss_with_rsn(rsn: &[u8]) -> BssEntry {
    let mut ies = vec![0, SSID.len() as u8];
    ies.extend_from_slice(SSID);
    ies.extend_from_slice(rsn);
    BssEntry { bssid: AP, ssid: SSID.to_vec(), channel: 36, chanspec: chanspec20(36), rssi: -38, capability: 0x0411, beacon_period: 100, ies }
}

#[test]
fn ap_rsn_element_is_decoded() {
    let r = parse_ap_rsn(&ap_rsn_ie(0x00cc, true)).unwrap();
    assert_eq!(r.akm, vec![0x000f_ac02, 0x000f_ac06]);
    assert!(r.mfpc() && r.mfpr() && r.has_akm(6));
    assert_eq!(r.group_mgmt, Some(0x000f_ac06));
    assert!(r.describe().starts_with("group=4 pairwise=4 akm=2,6 caps=0x00cc mfpc=1 mfpr=1 pmkids=0 group_mgmt=6 ie=30"));
    assert_eq!(find_rsn_ie(&bss_with_rsn(&ap_rsn_ie(0, false)).ies).map(|i| i[0]), Some(48));
}

#[test]
fn akm_and_mfp_knobs_shape_the_advertised_ie() {
    // Auto against an AP offering AKM 6 with MFPC: AKM 6, MFPC=1, wpa_auth PSK_SHA256, mfp 1.
    let mut d = up();
    d.last_bss = vec![bss_with_rsn(&ap_rsn_ie(0x0080, true))];
    d.fw.on("join", join_ok());
    assert_eq!(d.join(psk_req(), &mut Steps::default(), 2_000), Ok(()));
    assert_eq!(d.fw.iovar("wpaie").unwrap(), &crate::wifi_crypto::rsn_ie_psk_akm(0x000f_ac04, 6, 0x0080).to_vec());
    let wa = d.fw.cmd(cmd::SET_WPA_AUTH).or(d.fw.iovar("wpa_auth")).unwrap();
    assert_eq!(wa[..4], WPA2_AUTH_PSK_SHA256.to_le_bytes());
    assert_eq!(d.fw.iovar("mfp").unwrap()[..4], 1u32.to_le_bytes());
    assert!(d.rsn.ap.contains("akm=2,6") && d.rsn.chosen.starts_with("akm=6 mfpc=1 mfpr=0 mfp_iovar=1"), "{:?}", d.rsn);
    assert!(d.rsn.secret.starts_with("passphrase len=21 edge_whitespace=0"));
    // Knobs: AKM 2 with MFPC=1 MFPR=0.
    let mut d = up();
    d.last_bss = vec![bss_with_rsn(&ap_rsn_ie(0x0080, false))];
    assert!(d.opts.apply(&["akm=2", "mfpc=1", "mfpr=0"]).is_empty());
    d.fw.on("join", join_ok());
    assert_eq!(d.join(psk_req(), &mut Steps::default(), 2_000), Ok(()));
    assert_eq!(d.fw.iovar("wpaie").unwrap(), &crate::wifi_crypto::rsn_ie_psk_akm(0x000f_ac04, 2, 0x0080).to_vec());
    let wa = d.fw.cmd(cmd::SET_WPA_AUTH).or(d.fw.iovar("wpa_auth")).unwrap();
    assert_eq!(wa[..4], WPA2_AUTH_PSK.to_le_bytes());
    // An AP without RSN info and no knobs keeps the old IE (AKM 2, caps 0).
    let mut d = up();
    d.fw.on("join", join_ok());
    assert_eq!(d.join(psk_req(), &mut Steps::default(), 2_000), Ok(()));
    assert_eq!(d.fw.iovar("wpaie").unwrap(), &crate::wifi_crypto::rsn_ie_psk_ccmp().to_vec());
    assert!(d.opts.apply(&["akm=5", "mfpc=2"]).len() == 2);
}

/// AKM 6 host handshake (descriptor v3: KDF-SHA256 PTK, AES-CMAC MIC) with
/// PMF: the firmware reports an association RSN IE that differs from ours
/// (Broadcom firmware may set PTKSA replay-counter bits); message 2 must echo
/// the firmware's bytes. PTK, GTK and IGTK (index 4) go in through wsec_key.
#[test]
fn akm6_host_handshake_echoes_the_firmware_assoc_ie_and_installs_igtk() {
    use crate::wifi_crypto as wc;
    use wc::key_info as ki;
    let mut d = up();
    d.fw.refuse = vec!["sup_wpa".into()];
    d.last_bss = vec![bss_with_rsn(&ap_rsn_ie(0x0080, true))];
    let fw_ie = wc::rsn_ie_psk_akm(0x000f_ac04, 6, 0x008c).to_vec();
    let mut req_ies = vec![0, SSID.len() as u8];
    req_ies.extend_from_slice(SSID);
    req_ies.extend_from_slice(&[1, 2, 0x82, 0x84]);
    req_ies.extend_from_slice(&fw_ie);
    let mut info = (req_ies.len() as u32 + 4).to_le_bytes().to_vec();
    info.extend_from_slice(&0u32.to_le_bytes());
    d.fw.gets.insert("assoc_info".into(), info);
    d.fw.gets.insert("assoc_req_ies".into(), req_ies);
    let pmk = wc::wpa_psk(b"correct horse battery", SSID);
    let anonce = [0x6bu8; 32];
    let m1 = wc::build_eapol_key(2, 3 | ki::TYPE_PAIRWISE | ki::ACK, 16, &1u64.to_be_bytes(), &anonce, &[]);
    d.fw.on("join", vec![
        evf(ev::ASSOC, status::SUCCESS, 0, 0, &[]),
        evf(ev::LINK, status::SUCCESS, 0, event::FLAG_LINK, &[]),
        Frame::Data(eth(STA, AP, &m1)),
    ]);
    let (gtk, igtk) = ([0x33u8; 16], [0x44u8; 16]);
    let tk_out = std::rc::Rc::new(std::cell::RefCell::new(None));
    let tk2 = tk_out.clone();
    let want_ie = fw_ie.clone();
    d.fw.on_tx = Some(Box::new(move |f: &[u8]| {
        let pdu = &f[14..];
        let k = wc::parse_eapol_key(pdu).unwrap();
        assert_eq!(k.key_info & 7, 3, "descriptor v3");
        if k.key_info & ki::SECURE != 0 { return vec![]; }
        assert_eq!(k.key_data, &want_ie[..], "M2 RSN IE = firmware assoc IE");
        let ptk = wc::derive_ptk_sha256(&pmk, &AP, &STA, &anonce, &k.nonce);
        let mut z = pdu.to_vec();
        z[81..97].fill(0);
        assert_eq!(wc::aes_cmac(&ptk.kck, &[&z])[..], pdu[81..97], "M2 CMAC");
        *tk2.borrow_mut() = Some(ptk.tk);
        let mut kd = want_ie.clone();
        kd.extend_from_slice(&[0xdd, 22, 0x00, 0x0f, 0xac, 0x01, 0x01, 0x00]);
        kd.extend_from_slice(&gtk);
        kd.extend_from_slice(&[0xdd, 28, 0x00, 0x0f, 0xac, 0x09, 4, 0, 1, 0, 0, 0, 0, 0]);
        kd.extend_from_slice(&igtk);
        kd.push(0xdd);
        while kd.len() % 8 != 0 { kd.push(0); }
        let wrapped = wc::aes_key_wrap(&ptk.kek, &kd);
        let mut m3 = wc::build_eapol_key(2, 3 | ki::TYPE_PAIRWISE | ki::INSTALL | ki::ACK | ki::MIC | ki::SECURE | ki::ENCRYPTED_KEY_DATA,
                                         16, &2u64.to_be_bytes(), &anonce, &wrapped);
        wc::eapol_set_mic(&ptk.kck, &mut m3);
        vec![Frame::Data(eth(STA, AP, &m3))]
    }));
    let r = d.join(psk_req(), &mut Steps::default(), 5_000);
    assert_eq!(r, Ok(()), "fail_cmd {} rsn {:?}", d.fail_cmd, d.rsn);
    let keys = wsec_keys(&d.fw);
    assert_eq!(keys.len(), 3, "pairwise + group + igtk");
    assert_eq!(keys[0], wsec_key(0, &tk_out.borrow().unwrap(), Some(AP)).to_vec());
    assert_eq!(keys[1], wsec_key(1, &gtk, None).to_vec());
    assert_eq!(keys[2], wsec_key(4, &igtk, None).to_vec());
    assert!(d.rsn.msg1.starts_with("eapol_ver=2 desc=2 key_info=0x008b (ver=3 pairwise=1"), "{}", d.rsn.msg1);
    assert!(d.rsn.msg2_ie.starts_with("fw akm=6 30140100"), "{}", d.rsn.msg2_ie);
    assert_eq!(d.rsn.assoc_fw, hex(&fw_ie));
    // Nothing secret in the diagnostics: no PMK bytes anywhere.
    let all: String = d.rsn.lines().iter().map(|(_, v)| v.as_str()).collect();
    assert!(!all.contains(&hex(&pmk[..8])) && !all.contains("correct horse"));
    // msg2ie=ours would have sent our IE instead.
    let mut o = JoinOpts::default();
    o.apply(&["msg2ie=ours"]);
    assert_eq!(o.msg2ie, Msg2Ie::Ours);
}

/// The netctl profile path (`netctl save` -> slot file -> kernel load ->
/// autoconnect) and the inline path (`netctl connect <ssid> <pass>` ->
/// NcConnect) hand the radio the same secret bytes, so the same PMK.
#[test]
fn profile_and_inline_secret_give_the_same_pmk() {
    use netctl_abi::profiles::{self as pf, Profile};
    use netctl_abi::types::NcConnect;
    let ssid = b"Anvara";
    for pass in [&b"correct horse battery"[..], b"12345678", b"pa$$ word with spaces ",
                 b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"] {
        // Profile path: build, store in a slot image, load it back.
        let p = Profile::new_wifi([7; 16], ssid, netctl_abi::akm::AUTO, pass).unwrap();
        let mut img = vec![0u8; 64 * 1024];
        let n = pf::serialize(5, 0, &[p], &mut img).unwrap();
        let (_, view) = pf::select(Some(&img[..n]), None).unwrap();
        let loaded: Vec<Profile> = view.profiles().collect();
        assert_eq!(loaded.len(), 1);
        let from_profile = loaded[0].secret().to_vec();
        // Inline path.
        let mut c = NcConnect::new();
        c.secret[..pass.len()].copy_from_slice(pass);
        c.secret_len = pass.len() as u16;
        let inline = c.secret().to_vec();
        assert_eq!(from_profile, inline);
        assert_eq!(from_profile, pass);
        let a = Key::from_secret(&from_profile).psk(ssid).unwrap();
        let b = Key::from_secret(&inline).psk(ssid).unwrap();
        assert_eq!(a, b);
    }
    // IEEE 802.11i H.4 vector: the PBKDF2 the host supplicant uses is right.
    let k = Key::from_secret(b"password").psk(b"IEEE").unwrap();
    assert_eq!(&k[..4], &[0xf4, 0x2c, 0x6f, 0xc5]);
}

// ── lane pirxwedge: `dump N` capture for host replay ─────────────────────────

#[test]
fn dump_ctl_parses_count_default_and_cap() {
    assert_eq!(parse_dump("seq=7.123 dump 5"), Some(5));
    assert_eq!(parse_dump("dump"), Some(8));
    assert_eq!(parse_dump("dump 9999"), Some(DUMP_MAX));
    assert_eq!(parse_dump("dump x"), None);
    assert_eq!(parse_dump("dp off"), None);
    assert_eq!(parse_dp("seq=1 dump 4"), None, "dump must not be taken for a dp command");
}

#[test]
fn dump_line_round_trips_first_64_bytes_and_length() {
    let frame: Vec<u8> = (0..100u8).collect();
    let line = fmt_dump_line(3, &frame);
    assert!(line.starts_with("rxdump 3 len 100 000102"));
    let (len, bytes) = parse_dump_line(&format!("[brcmf] {}", line)).unwrap();
    assert_eq!(len, 100);
    assert_eq!(bytes, frame[..DUMP_BYTES].to_vec());
    let (len, bytes) = parse_dump_line(&fmt_dump_line(0, &[])).unwrap();
    assert_eq!((len, bytes.len()), (0, 0));
    let (len, bytes) = parse_dump_line(&fmt_dump_line(1, &[0xab; 10])).unwrap();
    assert_eq!((len, bytes), (10, vec![0xab; 10]));
}

/// Pi 5 with `pcie.rpi5=1`: eth0 (macb) registers first, wifi0 becomes netctl
/// ifindex 3. The host 4-way handshake must not depend on that: EAPOL goes
/// out through the firmware data path with the firmware's own MAC as source.
#[test]
fn host_handshake_ignores_the_netctl_ifindex_and_uses_the_firmware_mac() {
    use crate::brcmf::cfg::NETCTL_IFINDEX;
    use core::sync::atomic::Ordering;
    let run = |ifindex: u32| {
        NETCTL_IFINDEX.store(ifindex, Ordering::Relaxed);
        let mut d = up();
        d.fw.refuse = vec!["sup_wpa".into()];
        let _tk = ap_handshake(&mut d, b"correct horse battery", [0x33u8; 16]);
        let r = d.join(psk_req(), &mut Steps::default(), 5_000);
        assert_eq!(r, Ok(()), "ifindex {}: fail_cmd {}", ifindex, d.fail_cmd);
        let mac = d.mac;
        let tx: Vec<Vec<u8>> = d.fw.calls.iter().filter_map(|c| match c { Call::Tx(f) => Some(f.clone()), _ => None }).collect();
        assert_eq!(tx.len(), 2, "M2 + M4");
        for f in &tx {
            assert_eq!(&f[0..6], &AP, "EAPOL to the AP");
            assert_eq!(&f[6..12], &mac, "EAPOL from the firmware MAC");
            assert_eq!(&f[12..14], &[0x88, 0x8e]);
        }
        tx.len()
    };
    assert_eq!(run(2), run(3));
    NETCTL_IFINDEX.store(0, Ordering::Relaxed);
}

// ── lane piwifikexec: wrong-passphrase policy (WRONG_KEY on one weak BSS) ────
mod wrongkey {
    use crate::wifi_wrongkey::*;
    const P: [u8; 16] = [7; 16];
    const A: [u8; 6] = [0x8c, 0x30, 0x66, 0x6a, 0x6f, 0x9d];
    const B: [u8; 6] = [0x74, 0x83, 0xc2, 0xb7, 0xec, 0x19];

    /// T1: one WRONG_KEY verdict on one (weak) BSS must not block the profile.
    #[test]
    fn t1_single_bss_verdict_does_not_block() {
        let mut s = Vec::new();
        assert_eq!(note_wrong_key(&mut s, P, A, 1000, false, 2), WkVerdict::Defer(1));
        // The same BSS failing again is still one BSS.
        assert_eq!(note_wrong_key(&mut s, P, A, 20_000, false, 2), WkVerdict::Defer(1));
    }

    /// T2: a real wrong passphrase (two different BSSes) still blocks.
    #[test]
    fn t2_two_bsses_block() {
        let mut s = Vec::new();
        assert_eq!(note_wrong_key(&mut s, P, A, 1000, false, 2), WkVerdict::Defer(1));
        assert_eq!(note_wrong_key(&mut s, P, B, 30_000, false, 2), WkVerdict::Block(2));
        // Verdicts outside the window do not corroborate.
        let mut s = Vec::new();
        note_wrong_key(&mut s, P, A, 0, false, 2);
        assert_eq!(note_wrong_key(&mut s, P, B, WK_WINDOW_MS + 1, false, 2), WkVerdict::Defer(1));
    }

    /// T3: a profile proven this boot (or by a stored timestamp) is never blocked.
    #[test]
    fn t3_proven_profile_never_blocks() {
        let mut s = Vec::new();
        assert_eq!(note_wrong_key(&mut s, P, A, 1000, true, 2), WkVerdict::Defer(1));
        assert_eq!(note_wrong_key(&mut s, P, B, 2000, true, 2), WkVerdict::Defer(2));
        assert_eq!(note_wrong_key(&mut s, P, B, 3000, true, 1), WkVerdict::Defer(2));
    }

    /// T4: `wifi.wrongkey_bss=1` keeps the old behaviour (block on the first).
    #[test]
    fn t4_threshold_one_is_legacy() {
        let mut s = Vec::new();
        assert_eq!(note_wrong_key(&mut s, P, A, 1000, false, 1), WkVerdict::Block(1));
        // 0 is treated as 1.
        assert_eq!(note_wrong_key(&mut Vec::new(), P, A, 1000, false, 0), WkVerdict::Block(1));
    }

    /// T5: verdicts are per profile; success clears; knob parsing.
    #[test]
    fn t5_per_profile_and_clear_and_knob() {
        let q = [9u8; 16];
        let mut s = Vec::new();
        note_wrong_key(&mut s, P, A, 1000, false, 2);
        assert_eq!(note_wrong_key(&mut s, q, B, 2000, false, 2), WkVerdict::Defer(1));
        clear_profile(&mut s, &P);
        assert_eq!(note_wrong_key(&mut s, P, B, 3000, false, 2), WkVerdict::Defer(1));
        crate::wifi_pref::set_knobs("quiet wifi.wrongkey_bss=1");
        assert_eq!(crate::wifi_pref::wrongkey_bss(), 1);
        crate::wifi_pref::set_knobs("wifi.wrongkey_bss=2");
        assert_eq!(crate::wifi_pref::wrongkey_bss(), 2);
    }
}

/// A fresh firmware drops the early "wpaie" when wpa_auth changes: by default
/// the IE is set again after wpa_auth/wsec (and mfp); `wpaie=early` keeps one set.
#[test]
fn wpaie_is_set_again_after_wpa_auth_by_default() {
    let mut d = up();
    d.fw.on("join", join_ok());
    assert_eq!(d.join(psk_req(), &mut Steps::default(), 2_000), Ok(()));
    let names = d.fw.iovars_set();
    let ie: Vec<usize> = names.iter().enumerate().filter(|(_, n)| **n == "wpaie").map(|(i, _)| i).collect();
    assert_eq!(ie.len(), 2, "early and late set: {:?}", names);
    let wa = names.iter().position(|n| *n == "wpa_auth").or_else(|| d.fw.calls.iter().position(|c| matches!(c, Call::CmdSet(c, _) if *c == cmd::SET_WPA_AUTH)));
    assert!(wa.is_some());
    assert!(names.iter().position(|n| *n == "join").map_or(true, |j| ie[1] < j), "late wpaie before the join");
}

/// assoc_info with the old 8 byte buffer fails on every join; a 512 byte one reports the firmware's IE.
#[test]
fn assoc_info_is_read_with_a_full_size_buffer() {
    let mut d = up();
    d.fw.refuse = vec!["sup_wpa".into()];
    let fw_ie = crate::wifi_crypto::rsn_ie_psk_ccmp().to_vec();
    let mut req_ies = vec![0, SSID.len() as u8];
    req_ies.extend_from_slice(SSID);
    req_ies.extend_from_slice(&fw_ie);
    let mut info = (req_ies.len() as u32).to_le_bytes().to_vec();
    info.extend_from_slice(&[0u8; 28]);
    d.fw.gets.insert("assoc_info".into(), info);
    d.fw.gets.insert("assoc_req_ies".into(), req_ies);
    use crate::wifi_crypto as wc;
    use wc::key_info as ki;
    let m1 = wc::build_eapol_key(2, 2 | ki::TYPE_PAIRWISE | ki::ACK, 16, &1u64.to_be_bytes(), &[0x6bu8; 32], &[]);
    d.fw.on("join", vec![
        evf(ev::ASSOC, status::SUCCESS, 0, 0, &[]),
        evf(ev::LINK, status::SUCCESS, 0, event::FLAG_LINK, &[]),
        Frame::Data(eth(STA, AP, &m1)),
    ]);
    let _ = d.join(psk_req(), &mut Steps::default(), 500);
    assert!(d.rsn.assoc_fw.starts_with("30"), "assoc_fw {}", d.rsn.assoc_fw);
}
