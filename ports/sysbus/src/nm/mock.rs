//! A scripted kernel behind the same netctl event stream, so the whole shim
//! (and the applet above it) runs without Wi-Fi hardware.
//!
//! What it simulates, with timings of the order the real driver shows:
//!   * eth0 (ifindex 2): carrier up, DHCP bound 10.0.2.15/24 via 10.0.2.2,
//!     metric 100, holds the default route. DISCONNECT = admin down,
//!     CONNECT = admin up (DHCP again, 0.3 s).
//!   * wifi0 (ifindex 3) and its APs (built-in list or a scenario file).
//!     Scan: 1.2 s, results jitter by a few dB, one AP ("SimFlaky") comes
//!     and goes, requests within 10 s are coalesced. Connect: CONNECTING ->
//!     AUTHENTICATING (0.2 s) -> ASSOCIATING (0.4 s) -> HANDSHAKE (0.6 s) ->
//!     CONNECTED (0.9 s) + DHCP bound (1.4 s); a wrong key fails in the
//!     handshake (FAILED WRONG_KEY at 2.0 s, DISCONNECTED 0.1 s later), an
//!     SAE-only AP fails UNSUPPORTED_SECURITY (no WPA3 yet), a missing AP
//!     fails NO_AP. DISCONNECT reaches DISCONNECTED in 0.25 s.
//!   * Profiles: real A/B slot files in $LEANDROS_NM_MOCK_DIR (default
//!     /tmp/leandros-nm-mock-<uid>), parsed with netctl-abi exactly like the
//!     kernel does on RELOAD_PROFILES; CONNECT by uuid reads the secret there.
//!
//! LEANDROS_NM_MOCK_RO=1 makes the open read-only (a user outside netdev).
//!
//! Scenario file (LEANDROS_NM_MOCK=<path> or /etc/leandros/nm-mock), one AP
//! per line, `#` comments:
//!   ap ssid=Name bssid=02:00:00:00:00:01 freq=2437 rssi=-50 sec=psk|sae|mixed|open pass=secret
//!   noeth            (no wired interface)
//! The passwords here are test fixtures of a simulated network, nothing else.

use std::io;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use netctl_abi::profiles::Profile;
use netctl_abi::util::{freq_to_channel, quality_from_dbm, set_cstr};
use netctl_abi::*;

use super::backend::{self, Backend, DirStore, EventTx, ProfileStore};

const ETH: u32 = 2;
const WIFI: u32 = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sec {
    Open,
    Psk,
    Sae,
    Mixed,
}

#[derive(Clone)]
struct SimAp {
    ssid: Vec<u8>,
    bssid: [u8; 6],
    freq: u16,
    rssi: i16,
    sec: Sec,
    pass: Vec<u8>,
    /// Comes and goes between scans.
    flaky: bool,
    /// Last rssi reported (jittered).
    cur: i16,
    visible: bool,
}

struct Sim {
    eth: Option<NcIface>,
    eth_ip: NcIpv4,
    wifi: NcIface,
    wifi_ip: NcIpv4,
    ws: NcWifiState,
    aps: Vec<SimAp>,
    radio_on: bool,
    last_scan: Option<Instant>,
    scanning: bool,
    scan_gen: u32,
    scans: u32,
    attempts: u32,
    /// Bumped by every connect/disconnect/radio change; a scheduled step
    /// whose token is stale is dropped (the sequence was superseded).
    token: u64,
    profiles_gen: u32,
    rng: u32,
    t0: Instant,
}

pub struct MockBackend {
    sim: Arc<Mutex<Sim>>,
    tx: EventTx,
    writable: bool,
    store: DirStore,
}

fn now_ms(t0: Instant) -> u32 {
    t0.elapsed().as_millis() as u32 + 1
}

fn mac(s: &str) -> Option<[u8; 6]> {
    let mut m = [0u8; 6];
    let mut it = s.split(':');
    for b in m.iter_mut() {
        *b = u8::from_str_radix(it.next()?, 16).ok()?;
    }
    it.next().is_none().then_some(m)
}

fn default_aps() -> Vec<SimAp> {
    let ap = |ssid: &str, b: u8, freq: u16, rssi: i16, sec: Sec, pass: &str, flaky: bool| SimAp {
        ssid: ssid.as_bytes().to_vec(),
        bssid: [0x02, 0x5e, 0x1a, 0x00, 0x00, b],
        freq,
        rssi,
        sec,
        pass: pass.as_bytes().to_vec(),
        flaky,
        cur: rssi,
        visible: true,
    };
    vec![
        ap("SimHome-WPA2", 0x01, 2437, -45, Sec::Psk, "correct-horse", false),
        ap("SimHome-WPA2", 0x02, 5180, -58, Sec::Psk, "correct-horse", false),
        ap("SimMixed", 0x03, 5240, -55, Sec::Mixed, "mixed-pass-1", false),
        ap("SimCafe-WPA3", 0x04, 2462, -62, Sec::Sae, "sae-only-pass", false),
        ap("SimOpen", 0x05, 2412, -70, Sec::Open, "", false),
        ap("SimFlaky", 0x06, 2412, -80, Sec::Psk, "flaky-pass-1", true),
    ]
}

fn parse_scenario(text: &str) -> (Vec<SimAp>, bool) {
    let mut aps = Vec::new();
    let mut eth = true;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.split_whitespace();
        match words.next() {
            Some("noeth") => eth = false,
            Some("ap") => {
                let mut a = SimAp {
                    ssid: Vec::new(),
                    bssid: [0x02, 0x5e, 0x1a, 0x00, 0x01, aps.len() as u8 + 1],
                    freq: 2437,
                    rssi: -55,
                    sec: Sec::Psk,
                    pass: Vec::new(),
                    flaky: false,
                    cur: 0,
                    visible: true,
                };
                for w in words {
                    let Some((k, v)) = w.split_once('=') else { continue };
                    match k {
                        "ssid" => a.ssid = v.as_bytes().to_vec(),
                        "bssid" => a.bssid = mac(v).unwrap_or(a.bssid),
                        "freq" => a.freq = v.parse().unwrap_or(a.freq),
                        "rssi" => a.rssi = v.parse().unwrap_or(a.rssi),
                        "pass" => a.pass = v.as_bytes().to_vec(),
                        "flaky" => a.flaky = v == "1",
                        "sec" => {
                            a.sec = match v {
                                "open" => Sec::Open,
                                "sae" => Sec::Sae,
                                "mixed" => Sec::Mixed,
                                _ => Sec::Psk,
                            }
                        }
                        _ => {}
                    }
                }
                a.cur = a.rssi;
                if !a.ssid.is_empty() {
                    aps.push(a);
                }
            }
            _ => {}
        }
    }
    (aps, eth)
}

fn iface(ifindex: u32, kind: u8, name: &str, mac: [u8; 6], flags: u32, driver: &str) -> NcIface {
    let mut i = NcIface { ifindex, kind, mac, perm_mac: mac, mtu: 1500, flags, ..Default::default() };
    set_cstr(&mut i.name, name.as_bytes());
    set_cstr(&mut i.driver, driver.as_bytes());
    if kind == if_kind::WIFI {
        i.metric = METRIC_WIFI;
        i.wifi_caps = wifi_caps::PSK | wifi_caps::PSK_SHA256 | wifi_caps::BAND_2GHZ | wifi_caps::BAND_5GHZ;
    } else {
        i.metric = METRIC_WIRED;
        i.speed_mbps = 1000;
    }
    i.oper = if flags & iface_flags::CARRIER != 0 { oper::UP } else { oper::LOWER_DOWN };
    i
}

impl Sim {
    fn bss(&self, a: &SimAp) -> NcBss {
        let mut b = NcBss {
            ifindex: WIFI,
            bssid: a.bssid,
            freq_mhz: a.freq,
            signal_dbm: a.cur,
            quality: quality_from_dbm(a.cur as i32),
            ssid_len: a.ssid.len() as u8,
            ..Default::default()
        };
        b.ssid[..a.ssid.len()].copy_from_slice(&a.ssid);
        let (ch, band) = freq_to_channel(a.freq);
        b.channel = ch;
        b.band = band;
        b.beacon_int = 100;
        b.max_rate_kbps = if a.freq > 5000 { 866_700 } else { 144_400 };
        b.last_seen_ms = now_ms(self.t0);
        let ccmp = nm_sec::PAIR_CCMP | nm_sec::GROUP_CCMP;
        match a.sec {
            Sec::Open => {
                b.ext = bss_ext::JOINABLE;
            }
            Sec::Psk => {
                b.nm_flags = nm_ap_flags::PRIVACY;
                b.rsn_flags = ccmp | nm_sec::KEY_MGMT_PSK;
                b.ext = bss_ext::RSN | bss_ext::AKM_PSK | bss_ext::JOINABLE;
            }
            Sec::Sae => {
                b.nm_flags = nm_ap_flags::PRIVACY;
                b.rsn_flags = ccmp | nm_sec::KEY_MGMT_SAE;
                b.ext = bss_ext::RSN | bss_ext::AKM_SAE | bss_ext::MFPC | bss_ext::MFPR;
            }
            Sec::Mixed => {
                b.nm_flags = nm_ap_flags::PRIVACY;
                b.rsn_flags = ccmp | nm_sec::KEY_MGMT_PSK | nm_sec::KEY_MGMT_SAE;
                b.ext = bss_ext::RSN | bss_ext::AKM_PSK | bss_ext::AKM_SAE | bss_ext::MFPC
                    | bss_ext::TRANSITION | bss_ext::JOINABLE;
            }
        }
        b
    }

    fn wifi_iface(&self) -> NcIface {
        let mut i = self.wifi;
        i.flags = iface_flags::ADMIN_UP | iface_flags::RADIO_HW_ON | iface_flags::READY
            | if self.radio_on { iface_flags::RADIO_ON } else { 0 }
            | if self.ws.state == wifi_state::CONNECTED { iface_flags::CARRIER } else { 0 };
        i.oper = match self.ws.state {
            wifi_state::CONNECTED => oper::UP,
            wifi_state::CONNECTING..=wifi_state::HANDSHAKE => oper::DORMANT,
            _ => oper::LOWER_DOWN,
        };
        i
    }

    fn jitter(&mut self) -> i16 {
        // xorshift; deterministic per run.
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 17;
        self.rng ^= self.rng << 5;
        (self.rng % 9) as i16 - 4
    }

    fn set_ws(&mut self, state: u8, reason: u8) {
        self.ws.state = state;
        self.ws.reason = reason;
        self.ws.since_ms = now_ms(self.t0);
    }

    fn snapshot(&self, store: &dyn ProfileStore, writable: bool, out: &mut Vec<Event>) {
        let info = NcSnapshotInfo {
            reason: snapshot_reason::OPEN,
            abi_version: NETCTL_ABI_VERSION,
            writable: writable as u32,
            ..Default::default()
        };
        out.push(Event::SnapshotBegin(info));
        if let Some(e) = self.eth {
            out.push(Event::IfaceAdded(e));
            out.push(Event::Ipv4(self.eth_ip));
        }
        out.push(Event::IfaceAdded(self.wifi_iface()));
        out.push(Event::Ipv4(self.wifi_ip));
        out.push(Event::WifiState(self.ws));
        if self.radio_on {
            for a in self.aps.iter().filter(|a| a.visible) {
                out.push(Event::Bss(self.bss(a)));
            }
        }
        self.profiles(store, out);
        out.push(Event::SnapshotEnd(info));
    }

    fn profiles(&self, store: &dyn ProfileStore, out: &mut Vec<Event>) -> NcReload {
        let loaded = backend::load(store).unwrap_or(backend::Loaded { profiles: Vec::new(), global_flags: 0 });
        for p in &loaded.profiles {
            out.push(Event::Profile(p.meta(self.profiles_gen, 0)));
        }
        out.push(Event::ProfilesEnd(NcProfilesInfo {
            gen: self.profiles_gen,
            count: loaded.profiles.len() as u32,
            source: profiles_source::SLOT_A,
            global_flags: loaded.global_flags,
            ..Default::default()
        }));
        NcReload {
            size: NcReload::SIZE as u32,
            gen: self.profiles_gen,
            count: loaded.profiles.len() as u32,
            source: profiles_source::SLOT_A,
            status: profiles_status::OK,
            _pad: 0,
        }
    }
}

impl MockBackend {
    pub fn start(scenario: Option<std::path::PathBuf>, tx: EventTx) -> Self {
        let (aps, has_eth) = match &scenario {
            Some(p) => match std::fs::read_to_string(p) {
                Ok(t) => {
                    let (aps, eth) = parse_scenario(&t);
                    if aps.is_empty() { (default_aps(), eth) } else { (aps, eth) }
                }
                Err(_) => (default_aps(), true),
            },
            None => (default_aps(), true),
        };
        // SAFETY: getuid has no failure mode.
        let uid = unsafe { libc::getuid() };
        let dir = std::env::var("LEANDROS_NM_MOCK_DIR").unwrap_or_else(|_| format!("/tmp/leandros-nm-mock-{uid}"));
        let _ = std::fs::create_dir_all(&dir);
        let store = DirStore { dir: dir.into(), create: true };
        let writable = std::env::var("LEANDROS_NM_MOCK_RO").map(|v| v != "1").unwrap_or(true);
        let wired_flags = iface_flags::ADMIN_UP | iface_flags::CARRIER | iface_flags::RADIO_ON
            | iface_flags::RADIO_HW_ON | iface_flags::READY;
        let eth = has_eth.then(|| iface(ETH, if_kind::ETHERNET, "eth0", [0x52, 0x54, 0x00, 0x12, 0x34, 0x56], wired_flags, "virtio-net"));
        let mut eth_ip = NcIpv4 { ifindex: ETH, method: ip_method::AUTO, metric: METRIC_WIRED, ..Default::default() };
        if eth.is_some() {
            bind(&mut eth_ip, [10, 0, 2, 15], [10, 0, 2, 2], true);
        }
        let wifi = iface(WIFI, if_kind::WIFI, "wifi0", [0x02, 0x16, 0x3e, 0x5a, 0x11, 0x01], 0, "wifi_sim");
        let mut ws = NcWifiState { ifindex: WIFI, ..Default::default() };
        ws.state = wifi_state::DISCONNECTED;
        let t0 = Instant::now();
        let sim = Sim {
            eth,
            eth_ip,
            wifi,
            wifi_ip: NcIpv4 { ifindex: WIFI, method: ip_method::AUTO, metric: METRIC_WIFI, ..Default::default() },
            ws,
            aps,
            radio_on: true,
            last_scan: None,
            scanning: false,
            scan_gen: 0,
            scans: 0,
            attempts: 0,
            token: 0,
            profiles_gen: 1,
            rng: 0x2545_f491,
            t0,
        };
        let mut evs = Vec::new();
        sim.snapshot(&store, writable, &mut evs);
        for e in evs {
            let _ = tx.send(e);
        }
        super::log(format_args!("mock backend: {} simulated APs, profiles in {}", sim.aps.len(), store.dir.display()));
        let m = MockBackend { sim: Arc::new(Mutex::new(sim)), tx, writable, store };
        // A first scan shortly after "boot", like the driver's idle scan.
        m.schedule_scan(Duration::from_millis(300));
        m
    }

    fn send(&self, e: Event) {
        let _ = self.tx.send(e);
    }

    /// Run `f` after `after` unless the sequence was superseded (token moved).
    fn later(&self, after: Duration, token: u64, f: impl FnOnce(&mut Sim, &mut Vec<Event>) + Send + 'static) {
        let sim = self.sim.clone();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            tokio::time::sleep(after).await;
            let mut out = Vec::new();
            {
                let mut s = sim.lock().unwrap();
                if s.token != token {
                    return;
                }
                f(&mut s, &mut out);
            }
            for e in out {
                let _ = tx.send(e);
            }
        });
    }

    fn schedule_scan(&self, after: Duration) {
        let sim = self.sim.clone();
        let tx = self.tx.clone();
        {
            let mut s = self.sim.lock().unwrap();
            if s.scanning || !s.radio_on {
                return;
            }
            s.scanning = true;
            let info = NcScanInfo { ifindex: WIFI, n_bss: 0, scan_gen: s.scan_gen, time_ms: now_ms(s.t0) as u64, ..Default::default() };
            let _ = tx.send(Event::ScanStarted(info));
        }
        tokio::spawn(async move {
            tokio::time::sleep(after + Duration::from_millis(1200)).await;
            let mut out = Vec::new();
            {
                let mut s = sim.lock().unwrap();
                s.scanning = false;
                if !s.radio_on {
                    return;
                }
                s.scans += 1;
                s.scan_gen += 1;
                s.last_scan = Some(Instant::now());
                let scans = s.scans;
                for i in 0..s.aps.len() {
                    let j = s.jitter();
                    let a = &mut s.aps[i];
                    let was = a.visible;
                    a.visible = !a.flaky || scans % 3 != 0;
                    a.cur = a.rssi + j;
                    let a = a.clone();
                    if a.visible {
                        out.push(Event::Bss(s.bss(&a)));
                    } else if was {
                        // The flaky AP missed a scan: aged out at once here
                        // (the kernel waits 120 s; the shim's handling is the
                        // same either way).
                        out.push(Event::BssGone(s.bss(&a)));
                    }
                }
                let n = s.aps.iter().filter(|a| a.visible).count() as u32;
                out.push(Event::ScanDone(NcScanInfo {
                    ifindex: WIFI,
                    status: scan_status::OK,
                    n_bss: n,
                    scan_gen: s.scan_gen,
                    time_ms: now_ms(s.t0) as u64,
                    ..Default::default()
                }));
            }
            for e in out {
                let _ = tx.send(e);
            }
        });
    }

    fn wifi_event(s: &Sim, out: &mut Vec<Event>) {
        out.push(Event::WifiState(s.ws));
        out.push(Event::IfaceChanged(s.wifi_iface()));
    }
}

fn bind(ip: &mut NcIpv4, addr: [u8; 4], gw: [u8; 4], default_route: bool) {
    ip.state = ip_state::BOUND;
    ip.addr = addr;
    ip.prefix = 24;
    ip.gw = gw;
    ip.server = gw;
    ip.dns[0] = gw;
    ip.n_dns = 1;
    ip.lease_s = 86_400;
    ip.flags = ip_flags::HAS_GW | if default_route { ip_flags::DEFAULT_ROUTE } else { 0 };
}

fn unbind(ip: &mut NcIpv4) {
    let (ifindex, metric, method) = (ip.ifindex, ip.metric, ip.method);
    *ip = NcIpv4 { ifindex, metric, method, ..Default::default() };
}

impl Backend for MockBackend {
    fn name(&self) -> &'static str {
        "mock"
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn features(&self) -> u32 {
        features::WIFI | features::WIRED_ADMIN
    }

    fn scan(&mut self, ifindex: u32) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        if ifindex != 0 && ifindex != WIFI {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        let (radio, coalesce, t0, gen, n) = {
            let s = self.sim.lock().unwrap();
            let n = s.aps.iter().filter(|a| a.visible).count() as u32;
            let co = s.last_scan.is_some_and(|t| t.elapsed() < Duration::from_millis(SCAN_COALESCE_MS as u64));
            (s.radio_on, co, s.t0, s.scan_gen, n)
        };
        if !radio {
            return Err(io::Error::from_raw_os_error(libc::ENETDOWN));
        }
        if coalesce {
            // Cached results, fresh SCAN_DONE (what the kernel promises).
            let tx = self.tx.clone();
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(50)).await;
                let _ = tx.send(Event::ScanDone(NcScanInfo {
                    ifindex: WIFI,
                    status: scan_status::OK,
                    flags: scan_flags::COALESCED,
                    n_bss: n,
                    scan_gen: gen,
                    time_ms: now_ms(t0) as u64,
                    ..Default::default()
                }));
            });
        } else {
            self.schedule_scan(Duration::ZERO);
        }
        Ok(())
    }

    fn connect(&mut self, req: &mut NcConnect) -> io::Result<u32> {
        if !self.writable {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        let ifindex = if req.ifindex == 0 { WIFI } else { req.ifindex };
        if ifindex == ETH {
            let mut s = self.sim.lock().unwrap();
            let Some(mut e) = s.eth else { return Err(io::Error::from_raw_os_error(libc::ENODEV)) };
            s.attempts += 1;
            let attempt = s.attempts;
            if e.flags & iface_flags::ADMIN_UP == 0 {
                e.flags |= iface_flags::ADMIN_UP;
                s.eth = Some(e);
                s.eth_ip.state = ip_state::REQUESTING;
                self.send(Event::IfaceChanged(e));
                self.send(Event::Ipv4(s.eth_ip));
                let token = s.token;
                drop(s);
                self.later(Duration::from_millis(300), token, |s, out| {
                    bind(&mut s.eth_ip, [10, 0, 2, 15], [10, 0, 2, 2], true);
                    out.push(Event::Ipv4(s.eth_ip));
                    if s.wifi_ip.state == ip_state::BOUND {
                        s.wifi_ip.flags &= !ip_flags::DEFAULT_ROUTE;
                        out.push(Event::Ipv4(s.wifi_ip));
                    }
                });
            }
            return Ok(attempt);
        }
        if ifindex != WIFI {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        // What to join: inline credentials, or a loaded profile by uuid.
        let (ssid, secret, akm_) = if req.flags & connect_flags::INLINE != 0 {
            (req.ssid().to_vec(), req.secret().to_vec(), req.akm)
        } else {
            let loaded = backend::load(&self.store)?;
            let p: &Profile = loaded
                .profiles
                .iter()
                .find(|p| p.uuid == req.profile_uuid)
                .ok_or_else(|| io::Error::from_raw_os_error(libc::ENOENT))?;
            (p.ssid().to_vec(), p.secret().to_vec(), p.akm)
        };
        let mut s = self.sim.lock().unwrap();
        if !s.radio_on {
            return Err(io::Error::from_raw_os_error(libc::ENETDOWN));
        }
        s.token += 1;
        s.attempts += 1;
        let token = s.token;
        let attempt = s.attempts;
        let target = s
            .aps
            .iter()
            .filter(|a| a.visible && a.ssid == ssid)
            .filter(|a| req.flags & connect_flags::BSSID == 0 || a.bssid == req.bssid)
            .max_by_key(|a| a.cur)
            .cloned();
        // A connect replaces whatever was up.
        if s.wifi_ip.state != ip_state::NONE {
            unbind(&mut s.wifi_ip);
            self.send(Event::Ipv4(s.wifi_ip));
        }
        s.ws.attempt = attempt;
        s.ws.profile_uuid = req.profile_uuid;
        s.ws.ssid = [0; SSID_MAX];
        s.ws.ssid_len = ssid.len() as u8;
        s.ws.ssid[..ssid.len()].copy_from_slice(&ssid);
        s.ws.flags = 0;
        s.ws.akm = akm::AUTO;
        if let Some(a) = &target {
            s.ws.bssid = a.bssid;
            s.ws.freq_mhz = a.freq;
            s.ws.signal_dbm = a.cur;
        } else {
            s.ws.bssid = [0; 6];
        }
        s.set_ws(wifi_state::CONNECTING, reason::NONE);
        let mut out = Vec::new();
        MockBackend::wifi_event(&s, &mut out);
        drop(s);
        for e in out {
            self.send(e);
        }
        let fail = |at: u64, why: u8| {
            move |s: &mut Sim, out: &mut Vec<Event>| {
                let _ = at;
                s.set_ws(wifi_state::FAILED, why);
                MockBackend::wifi_event(s, out);
                s.set_ws(wifi_state::DISCONNECTED, why);
                s.ws.attempt = 0;
                s.ws.bssid = [0; 6];
            }
        };
        let Some(ap) = target else {
            self.later(Duration::from_millis(1500), token, fail(1500, reason::NO_AP));
            self.later(Duration::from_millis(1600), token, |s, out| MockBackend::wifi_event(s, out));
            return Ok(attempt);
        };
        let step = |st: u8| {
            move |s: &mut Sim, out: &mut Vec<Event>| {
                s.set_ws(st, reason::NONE);
                MockBackend::wifi_event(s, out);
            }
        };
        self.later(Duration::from_millis(200), token, step(wifi_state::AUTHENTICATING));
        if ap.sec == Sec::Sae {
            self.later(Duration::from_millis(300), token, fail(300, reason::UNSUPPORTED_SECURITY));
            self.later(Duration::from_millis(400), token, |s, out| MockBackend::wifi_event(s, out));
            return Ok(attempt);
        }
        if ap.sec != Sec::Open && secret.is_empty() {
            self.later(Duration::from_millis(300), token, fail(300, reason::NO_SECRET));
            self.later(Duration::from_millis(400), token, |s, out| MockBackend::wifi_event(s, out));
            return Ok(attempt);
        }
        self.later(Duration::from_millis(400), token, step(wifi_state::ASSOCIATING));
        if ap.sec != Sec::Open {
            self.later(Duration::from_millis(600), token, step(wifi_state::HANDSHAKE));
        }
        let right = ap.sec == Sec::Open || secret == ap.pass;
        let _ = akm_;
        if !right {
            self.later(Duration::from_millis(2000), token, fail(2000, reason::WRONG_KEY));
            self.later(Duration::from_millis(2100), token, |s, out| MockBackend::wifi_event(s, out));
            return Ok(attempt);
        }
        let sec = ap.sec;
        self.later(Duration::from_millis(900), token, move |s, out| {
            s.ws.akm = if sec == Sec::Open { akm::NONE } else { akm::PSK };
            s.ws.tx_rate_kbps = 144_400;
            s.set_ws(wifi_state::CONNECTED, reason::NONE);
            MockBackend::wifi_event(s, out);
            s.wifi_ip.state = ip_state::REQUESTING;
            out.push(Event::Ipv4(s.wifi_ip));
        });
        self.later(Duration::from_millis(1400), token, move |s, out| {
            let host = 10 + (attempt % 200) as u8;
            let default = s.eth_ip.state != ip_state::BOUND;
            bind(&mut s.wifi_ip, [192, 168, 77, host], [192, 168, 77, 1], default);
            out.push(Event::Ipv4(s.wifi_ip));
        });
        Ok(attempt)
    }

    fn disconnect(&mut self, ifindex: u32) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        let ifindex = if ifindex == 0 { WIFI } else { ifindex };
        let mut s = self.sim.lock().unwrap();
        if ifindex == ETH {
            let Some(mut e) = s.eth else { return Err(io::Error::from_raw_os_error(libc::ENODEV)) };
            if e.flags & iface_flags::ADMIN_UP != 0 {
                e.flags &= !iface_flags::ADMIN_UP;
                s.eth = Some(e);
                unbind(&mut s.eth_ip);
                self.send(Event::IfaceChanged(e));
                self.send(Event::Ipv4(s.eth_ip));
                if s.wifi_ip.state == ip_state::BOUND {
                    s.wifi_ip.flags |= ip_flags::DEFAULT_ROUTE;
                    self.send(Event::Ipv4(s.wifi_ip));
                }
            }
            return Ok(());
        }
        if ifindex != WIFI {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        s.token += 1;
        let token = s.token;
        // Already idle: nothing happens and nothing is reported (the kernel
        // contract: "returns 0 also when already disconnected").
        if matches!(s.ws.state, wifi_state::DISCONNECTED | wifi_state::RADIO_OFF | wifi_state::UNAVAILABLE) {
            return Ok(());
        }
        s.set_ws(wifi_state::DISCONNECTING, reason::USER);
        let mut out = Vec::new();
        MockBackend::wifi_event(&s, &mut out);
        drop(s);
        for e in out {
            self.send(e);
        }
        self.later(Duration::from_millis(250), token, |s, out| {
            unbind(&mut s.wifi_ip);
            out.push(Event::Ipv4(s.wifi_ip));
            s.set_ws(wifi_state::DISCONNECTED, reason::USER);
            s.ws.attempt = 0;
            s.ws.bssid = [0; 6];
            s.ws.profile_uuid = [0; 16];
            MockBackend::wifi_event(s, out);
        });
        Ok(())
    }

    fn set_radio(&mut self, ifindex: u32, on: bool) -> io::Result<()> {
        if !self.writable {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        if ifindex != 0 && ifindex != WIFI {
            return Err(io::Error::from_raw_os_error(libc::ENODEV));
        }
        let mut out = Vec::new();
        {
            let mut s = self.sim.lock().unwrap();
            if s.radio_on == on {
                return Ok(());
            }
            s.token += 1;
            s.radio_on = on;
            if on {
                s.set_ws(wifi_state::DISCONNECTED, reason::NONE);
            } else {
                if s.wifi_ip.state != ip_state::NONE {
                    unbind(&mut s.wifi_ip);
                    out.push(Event::Ipv4(s.wifi_ip));
                }
                s.ws.attempt = 0;
                s.ws.bssid = [0; 6];
                s.set_ws(wifi_state::RADIO_OFF, reason::RADIO_OFF);
                for a in s.aps.iter().filter(|a| a.visible) {
                    out.push(Event::BssGone(s.bss(a)));
                }
                s.last_scan = None;
            }
            MockBackend::wifi_event(&s, &mut out);
        }
        for e in out {
            self.send(e);
        }
        if on {
            self.schedule_scan(Duration::from_millis(200));
        }
        Ok(())
    }

    fn reload_profiles(&mut self) -> io::Result<NcReload> {
        if !self.writable {
            return Err(io::Error::from_raw_os_error(libc::EPERM));
        }
        let mut out = Vec::new();
        let r = {
            let mut s = self.sim.lock().unwrap();
            s.profiles_gen += 1;
            s.profiles(&self.store, &mut out)
        };
        for e in out {
            self.send(e);
        }
        Ok(r)
    }

    fn store(&self) -> &dyn ProfileStore {
        &self.store
    }
}
