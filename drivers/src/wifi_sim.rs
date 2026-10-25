//! A simulated Wi-Fi interface for QEMU (`wifi_sim`): the control plane is
//! simulated, the data path is real. It runs the same connection manager
//! (`wifi_mgr`) the Intel driver runs with `iwlwifi.mgr=1`, and the STA side
//! of the WPA2 4-way handshake is the real `wifi_crypto::Supplicant`; the
//! simulated AP runs the authenticator side with its own PMK, so a wrong
//! passphrase fails exactly as on air (message 2's MIC does not verify, the
//! AP retransmits message 1, then deauthenticates with reason 15).
//!
//! Data path: once "associated", the interface's frames go through a second
//! virtio-net NIC (QEMU `-netdev user,net=10.0.3.0/24`), so DHCP and traffic
//! over `wifi0` are real. Without such a NIC the data path is a sink
//! (frames are dropped, nothing is received) and only the control plane runs.
//!
//! Activation (all opt-in; nothing happens on real hardware):
//! * a virtio-net NIC whose MAC starts with 02:57:53 ("WS") becomes the sim's
//!   data path and turns the sim on, with the default APs below, e.g.
//!   `-netdev user,id=wsim,net=10.0.3.0/24
//!    -device virtio-net-pci,netdev=wsim,mac=02:57:53:00:00:01`
//!   (`LEANDROS_WIFISIM=1` in driver.py adds exactly that);
//! * or the kernel command line: `wifisim=1` (data path: the 02:57:53 NIC,
//!   else virtio-net device 1, else none), `wifisim.nic=<n>|none`.
//! * `wifisim=0` keeps it off whatever the NICs are.
//!
//! Access points: `wifisim.aps=<ap>,<ap>,...` with
//! `<ssid>:<sec>[:<secret>[:<dBm>[:<channel>[:<bssid>[:<flags>]]]]]`,
//! `%XX` escapes in every field, `<sec>` one of open, psk (WPA2-Personal),
//! sae (WPA3-Personal only, MFP required), psk+sae (transition), `<bssid>`
//! as 12 hex digits (separators `-` or none), `<flags>`: `h` hidden SSID,
//! `r` refuses association (status 17), `1`..`4` the AP's data path (see
//! below), and RSN variants a WPA2 network can come in that this station
//! cannot join: `t` TKIP group cipher (WPA/WPA2 mixed mode), `s` PSK-SHA256
//! as the only AKM, `m` management frame protection required (MFPR), `x`
//! FT-PSK as the only AKM; `d` the AP deauthenticates (reason 6) during
//! every 4-way handshake, as a flaky real AP did; `l` a legacy AP (no WMM,
//! no HT; every other AP is an HT AP, see "HT access point emulation"
//! below). Default set:
//!
//! | SSID      | security | secret          | dBm | ch |
//! |-----------|----------|-----------------|-----|----|
//! | SimHome   | psk      | simhome-pass    | -45 | 6  |
//! | SimCafe   | psk      | cafe-pass-123   | -67 | 36 |
//! | SimWPA3   | sae      | wpa3-pass-123   | -55 | 11 |
//! | SimMixed  | psk+sae  | mixed-pass-123  | -60 | 1  |
//! | SimOpen   | open     |                 | -70 | 11 |
//! | SimHidden | psk, h   | hidden-pass-1   | -58 | 6  |
//! | SimFull   | psk, r   | full-pass-123   | -75 | 40 |
//! | SimTkip   | psk, t   | tkip-pass-123   | -72 | 1  |
//! | SimTkipOnly | psk, k | tkip-only-123   | -73 | 1  |
//!
//! Data paths: every virtio-net NIC whose MAC starts with 02:57:53 is one,
//! in MAC order (`...:01` is path 1). An AP uses path 1 unless its flags
//! name another, so two APs on two SLIRP networks (10.0.3.0/24 and
//! 10.0.4.0/24, say) give the station a different DHCP lease each: what
//! roaming between two real networks does to the address. The station keeps
//! one MAC (path 1's) whichever path carries its frames.
//!
//! `wifisim.selftest=1` runs the scan / wrong key / right key / disconnect /
//! aging / radio / autoconnect checks through the netctl seam at boot and
//! prints `[wifisim] selftest: PASS n/n` (or FAIL); `=2` adds a raw DHCP
//! DISCOVER/OFFER exchange over the data path (only while nothing else
//! polls the interface, i.e. before the net server drives wifi0).

#![allow(dead_code)]

use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};
use spin::Mutex;

use netctl_abi::{akm, bss_ext, nm_ap_flags, nm_sec, reason, scan_flags, wifi_caps, wifi_flags, wifi_state, NcBss};

use crate::wifi_api::{ConnectParams, Secret, WifiBackend, WifiCmd, WifiEvent};
use crate::wifi_crypto as wc;
use crate::wifi_frame as wf;
use crate::wifi_ht as wh;
use crate::wifi_mgr::{self as wm, ConnCtx, Fail, Mgr, Radio, ScanHit};

/// MAC prefix that marks a virtio-net NIC as the sim's data path.
pub const SIM_OUI: [u8; 3] = [0x02, 0x57, 0x53];

/// Simulated scan duration (one pass over the channels).
const SCAN_MS: u64 = 250;
/// Simulated auth / assoc round trips.
const MGMT_MS: u64 = 20;
/// The AP retransmits message 1 after this when message 2 fails its MIC
/// (802.11 default dot11RSNAConfigPairwiseUpdateTimeOut is 1 s; shorter here
/// to keep tests quick), and gives up after `M1_TRIES` messages.
const M1_RETRY_MS: u64 = 300;
const M1_TRIES: usize = 3;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sec { Open, Psk, Sae, PskSae }

/// How a WPA2 AP's RSN element deviates from plain PSK + CCMP.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RsnVar {
    Plain,
    /// Group cipher TKIP, pairwise CCMP + TKIP (WPA/WPA2 mixed mode).
    TkipGroup,
    /// Pure WPA/TKIP: group and pairwise TKIP only (not joinable).
    TkipOnly,
    /// AKM 00-0f-ac:6 (PSK-SHA256) only, MFP capable.
    Sha256Only,
    /// PSK with management frame protection required (MFPC + MFPR).
    MfpRequired,
    /// AKM 00-0f-ac:4 (FT-PSK) only.
    FtOnly,
}

#[derive(Clone)]
struct SimAp {
    ssid: Vec<u8>,
    bssid: [u8; 6],
    chan: u8,
    dbm: i8,
    sec: Sec,
    secret: Vec<u8>,
    hidden: bool,
    refuse_assoc: bool,
    on_air: bool,
    /// The AP's PMK, derived on first use.
    pmk: Option<[u8; 32]>,
    /// Index into `PATHS`: the data path its network is on.
    path: u8,
    rsn_var: RsnVar,
    /// Deauthenticates (reason 6, class 2 frame from a non-associated
    /// station) during the 4-way handshake.
    deauth_hs: bool,
    /// WMM + HT AP (flag `l` clears it).
    ht: bool,
}

struct Sim {
    aps: Vec<SimAp>,
    radio_on: bool,
    /// Index of the AP we are associated with.
    assoc: Option<usize>,
    /// A deauthentication the AP will send (self-test), reason code.
    pending_deauth: Option<u16>,
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static LINK_UP: AtomicBool = AtomicBool::new(false);
static LINK_GEN: AtomicU32 = AtomicU32::new(0);
/// virtio-net device index of the data path in use, -1 = none (sink).
static NIC: AtomicI32 = AtomicI32::new(-1);
/// Every data path the sim may use, in MAC order (see the module docs).
static PATHS: Mutex<Vec<usize>> = Mutex::new(Vec::new());
static MAC: Mutex<[u8; 6]> = Mutex::new([0x02, 0x57, 0x53, 0x00, 0x00, 0x01]);
static SIM: Mutex<Option<Sim>> = Mutex::new(None);
/// Something besides the self-test polls the data path (the net server).
static NET_POLLED: AtomicBool = AtomicBool::new(false);
static RX_FRAMES: AtomicU32 = AtomicU32::new(0);
static TX_FRAMES: AtomicU32 = AtomicU32::new(0);
static RX_DROPPED: AtomicU32 = AtomicU32::new(0);

/// The manager of the simulated interface.
pub static MGR: Mgr = Mgr::new("wifi_sim", "wifisim", mac_fn);

fn mac_fn() -> Option<[u8; 6]> { if present() { Some(*MAC.lock()) } else { None } }

fn log(msg: &str) {
    extern "C" { fn serial_print_str_raw(s: *const u8, len: usize); }
    let mut s = String::with_capacity(msg.len() + 12);
    s.push_str("[wifisim] ");
    s.push_str(msg);
    s.push('\n');
    // SAFETY: the kernel's raw serial writer takes a pointer and length.
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()); }
}

macro_rules! slog { ($($a:tt)*) => { log(&alloc::format!($($a)*)) }; }

fn now_ms() -> u64 { sched::monotonic_ns() / 1_000_000 }
fn msleep(ms: u64) { sched::kthread::msleep(ms) }

fn cmdline_val<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('='))).last()
}

fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            let h = |c: u8| (c as char).to_digit(16);
            if let (Some(a), Some(c)) = (h(b[i + 1]), h(b[i + 2])) {
                out.push((a * 16 + c) as u8);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn parse_bssid(s: &str) -> Option<[u8; 6]> {
    let hex: Vec<u8> = s.bytes().filter(|c| *c != b'-').collect();
    if hex.len() != 12 { return None; }
    let mut m = [0u8; 6];
    for i in 0..6 {
        let h = |c: u8| (c as char).to_digit(16);
        m[i] = (h(hex[2 * i])? * 16 + h(hex[2 * i + 1])?) as u8;
    }
    Some(m)
}

fn default_bssid(i: usize) -> [u8; 6] { [0x0a, 0x57, 0x53, 0xa0, 0x00, (i + 1) as u8] }

fn ap(i: usize, ssid: &str, sec: Sec, secret: &str, dbm: i8, chan: u8, flags: &str) -> SimAp {
    let mut a = SimAp {
        ssid: ssid.as_bytes().to_vec(), bssid: default_bssid(i), chan, dbm, sec, secret: secret.as_bytes().to_vec(),
        hidden: false, refuse_assoc: false, on_air: true, pmk: None, path: 0, rsn_var: RsnVar::Plain, deauth_hs: false,
        ht: true,
    };
    apply_flags(&mut a, flags);
    a
}

/// `<flags>` of a `wifisim.aps` entry (see the module docs).
fn apply_flags(a: &mut SimAp, flags: &str) {
    for c in flags.chars() {
        match c {
            'h' => a.hidden = true,
            'r' => a.refuse_assoc = true,
            '1'..='4' => a.path = c as u8 - b'1',
            't' => a.rsn_var = RsnVar::TkipGroup,
            'k' => a.rsn_var = RsnVar::TkipOnly,
            's' => a.rsn_var = RsnVar::Sha256Only,
            'm' => a.rsn_var = RsnVar::MfpRequired,
            'x' => a.rsn_var = RsnVar::FtOnly,
            'd' => a.deauth_hs = true,
            'l' => a.ht = false,
            _ => {}
        }
    }
}

fn default_aps() -> Vec<SimAp> {
    alloc::vec![
        ap(0, "SimHome", Sec::Psk, "simhome-pass", -45, 6, ""),
        ap(1, "SimCafe", Sec::Psk, "cafe-pass-123", -67, 36, ""),
        ap(2, "SimWPA3", Sec::Sae, "wpa3-pass-123", -55, 11, ""),
        ap(3, "SimMixed", Sec::PskSae, "mixed-pass-123", -60, 1, ""),
        ap(4, "SimOpen", Sec::Open, "", -70, 11, ""),
        ap(5, "SimHidden", Sec::Psk, "hidden-pass-1", -58, 6, "h"),
        ap(6, "SimFull", Sec::Psk, "full-pass-123", -75, 40, "r"),
        ap(7, "SimTkip", Sec::Psk, "tkip-pass-123", -72, 1, "t"),
        ap(8, "SimTkipOnly", Sec::Psk, "tkip-only-123", -73, 1, "k"),
    ]
}

fn parse_aps(spec: &str) -> Vec<SimAp> {
    let mut v = Vec::new();
    for (i, item) in spec.split(',').enumerate() {
        let f: Vec<&str> = item.split(':').collect();
        if f.len() < 2 || f[0].is_empty() { slog!("wifisim.aps: entry {} '{}' ignored (need ssid:sec)", i, item); continue; }
        let sec = match f[1] {
            "open" | "none" => Sec::Open,
            "psk" | "wpa2" => Sec::Psk,
            "sae" | "wpa3" => Sec::Sae,
            "psk+sae" | "sae+psk" | "mixed" => Sec::PskSae,
            other => { slog!("wifisim.aps: entry {}: unknown security '{}'", i, other); continue; }
        };
        let ssid = percent_decode(f[0]);
        if ssid.len() > 32 { slog!("wifisim.aps: entry {}: SSID longer than 32 bytes", i); continue; }
        let mut a = SimAp {
            ssid, bssid: default_bssid(i), chan: 6, dbm: -50, sec,
            secret: f.get(2).map(|s| percent_decode(s)).unwrap_or_default(),
            hidden: false, refuse_assoc: false, on_air: true, pmk: None, path: 0, rsn_var: RsnVar::Plain,
            deauth_hs: false, ht: true,
        };
        if let Some(d) = f.get(3).and_then(|s| s.parse::<i8>().ok()) { a.dbm = d; }
        if let Some(c) = f.get(4).and_then(|s| s.parse::<u8>().ok()) { a.chan = c; }
        if let Some(b) = f.get(5).and_then(|s| parse_bssid(s)) { a.bssid = b; }
        if let Some(fl) = f.get(6) { apply_flags(&mut a, fl); }
        if a.sec != Sec::Open && !(8..=63).contains(&a.secret.len()) && a.secret.len() != 64 {
            slog!("wifisim.aps: entry {} '{}': a WPA passphrase must be 8..63 bytes; AP ignored", i, wf::ssid_display(&a.ssid));
            continue;
        }
        v.push(a);
    }
    v
}

fn sec_name(s: Sec) -> &'static str {
    match s { Sec::Open => "open", Sec::Psk => "WPA2-PSK", Sec::Sae => "WPA3-SAE", Sec::PskSae => "WPA2/WPA3" }
}

/// What the AP's beacon / probe response says.
fn ap_bss(a: &SimAp, reveal: bool) -> wf::Bss {
    let rsn = match a.sec {
        Sec::Open => None,
        Sec::Psk => Some(match a.rsn_var {
            RsnVar::Plain => wf::RsnInfo { version: 1, group: wm::SUITE_CCMP, pairwise: alloc::vec![wm::SUITE_CCMP],
                                           akm: alloc::vec![wm::AKM_PSK], caps: 0 },
            RsnVar::TkipGroup => wf::RsnInfo { version: 1, group: wm::SUITE_TKIP,
                                               pairwise: alloc::vec![wm::SUITE_CCMP, wm::SUITE_TKIP],
                                               akm: alloc::vec![wm::AKM_PSK], caps: 0 },
            RsnVar::TkipOnly => wf::RsnInfo { version: 1, group: wm::SUITE_TKIP, pairwise: alloc::vec![wm::SUITE_TKIP],
                                              akm: alloc::vec![wm::AKM_PSK], caps: 0 },
            RsnVar::Sha256Only => wf::RsnInfo { version: 1, group: wm::SUITE_CCMP, pairwise: alloc::vec![wm::SUITE_CCMP],
                                                akm: alloc::vec![wm::AKM_PSK_SHA256], caps: wm::RSN_CAP_MFPC },
            RsnVar::MfpRequired => wf::RsnInfo { version: 1, group: wm::SUITE_CCMP, pairwise: alloc::vec![wm::SUITE_CCMP],
                                                 akm: alloc::vec![wm::AKM_PSK], caps: wm::RSN_CAP_MFPC | wm::RSN_CAP_MFPR },
            RsnVar::FtOnly => wf::RsnInfo { version: 1, group: wm::SUITE_CCMP, pairwise: alloc::vec![wm::SUITE_CCMP],
                                            akm: alloc::vec![wm::AKM_FT_PSK], caps: 0 },
        }),
        Sec::Sae => Some(wf::RsnInfo { version: 1, group: wm::SUITE_CCMP, pairwise: alloc::vec![wm::SUITE_CCMP],
                                        akm: alloc::vec![wm::AKM_SAE], caps: wm::RSN_CAP_MFPC | wm::RSN_CAP_MFPR }),
        Sec::PskSae => Some(wf::RsnInfo { version: 1, group: wm::SUITE_CCMP, pairwise: alloc::vec![wm::SUITE_CCMP],
                                           akm: alloc::vec![wm::AKM_PSK, wm::AKM_SAE], caps: wm::RSN_CAP_MFPC }),
    };
    let band_24 = a.chan <= 14;
    let mut cap = wf::CAP_ESS;
    if a.sec != Sec::Open { cap |= wf::CAP_PRIVACY; }
    if band_24 { cap |= 0x0400; }
    let rates = if band_24 {
        alloc::vec![0x82, 0x84, 0x8b, 0x96, 0x0c, 0x12, 0x18, 0x24, 0x30, 0x48, 0x60, 0x6c]
    } else {
        alloc::vec![0x8c, 0x12, 0x98, 0x24, 0xb0, 0x48, 0x60, 0x6c]
    };
    wf::Bss {
        bssid: a.bssid,
        ssid: if a.hidden && !reveal { Vec::new() } else { a.ssid.clone() },
        channel: a.chan, beacon_interval: 100, capability: cap, tsf: now_ms() * 1000, rates, rsn,
        wpa1: a.sec == Sec::Psk && a.rsn_var == RsnVar::TkipGroup,
        ht: a.ht, vht: !band_24 && a.ht, dtim_period: 2, erp: None, rssi: a.dbm, ies: ap_ies(a),
    }
}

fn offers(a: &SimAp, want: u8) -> bool {
    match want {
        akm::NONE => a.sec == Sec::Open,
        akm::PSK => matches!(a.sec, Sec::Psk | Sec::PskSae),
        akm::SAE => matches!(a.sec, Sec::Sae | Sec::PskSae),
        _ => false,
    }
}

// ── activation ──────────────────────────────────────────────────────────────

/// Decide whether the sim runs and start it. Kernel init, after
/// `virtio_net::init` (it may claim a virtio-net device).
pub fn init(cmdline: &str) {
    crate::wifi_mgr::set_knobs(cmdline);
    let knob = cmdline_val(cmdline, "wifisim");
    if knob == Some("0") { return; }
    let n = crate::virtio_net::device_count();
    let marked = (0..n).find(|&i| crate::virtio_net::get_mac_address(i).map_or(false, |m| m[..3] == SIM_OUI));
    let explicit = knob == Some("1") || cmdline_val(cmdline, "wifisim.aps").is_some();
    if marked.is_none() && !explicit { return; }
    let nic = match cmdline_val(cmdline, "wifisim.nic") {
        Some("none") => None,
        Some(v) => v.parse::<usize>().ok().filter(|&i| i < n),
        None => marked.or(if explicit && n >= 2 { Some(1) } else { None }),
    };
    // Every marked NIC is a data path, in MAC order; an explicit
    // `wifisim.nic` (or the unmarked default) is the only one.
    let mut paths: Vec<usize> = Vec::new();
    if let Some(first) = nic {
        if marked.is_some() && cmdline_val(cmdline, "wifisim.nic").is_none() {
            let mut m: Vec<(usize, [u8; 6])> = (0..n)
                .filter_map(|i| crate::virtio_net::get_mac_address(i).filter(|m| m[..3] == SIM_OUI).map(|m| (i, m)))
                .collect();
            m.sort_by(|a, b| a.1.cmp(&b.1));
            paths = m.into_iter().map(|x| x.0).collect();
        } else {
            paths.push(first);
        }
    }
    let nic = paths.first().copied();
    let mac = nic.and_then(crate::virtio_net::get_mac_address).unwrap_or([0x02, 0x57, 0x53, 0x00, 0x00, 0x01]);
    *MAC.lock() = mac;
    NIC.store(nic.map_or(-1, |i| i as i32), Ordering::SeqCst);
    let npaths = paths.len();
    *PATHS.lock() = paths;
    HT_ON.store(cmdline_val(cmdline, "wifisim.ht") != Some("0"), Ordering::Relaxed);
    BA_ON.store(cmdline_val(cmdline, "wifisim.ba") != Some("0"), Ordering::Relaxed);
    REORDER_ON.store(cmdline_val(cmdline, "wifisim.reorder") != Some("0"), Ordering::Relaxed);
    if let Some(v) = cmdline_val(cmdline, "wifisim.ampdu") {
        let f: Vec<u32> = v.split(':').map(|x| x.parse::<u32>().unwrap_or(0).min(900)).collect();
        *AMPDU.lock() = (f.first().copied().unwrap_or(0), f.get(1).copied().unwrap_or(0), f.get(2).copied().unwrap_or(0));
    }
    let aps = match cmdline_val(cmdline, "wifisim.aps") { Some(s) => parse_aps(s), None => default_aps() };
    slog!("simulated Wi-Fi on: MAC {}, data path {}{}; {} access points:", wm::fmt_mac(&mac),
          match nic { Some(i) => alloc::format!("virtio-net device {}", i), None => String::from("none (sink)") },
          if npaths > 1 { alloc::format!(" (+{} more)", npaths - 1) } else { String::new() }, aps.len());
    {
        let (h, d, l) = *AMPDU.lock();
        slog!("  QoS/HT: {}; RX Block Ack {}, reorder {}; A-MPDU emulation hold {}‰ dup {}‰ loss {}‰",
              if HT_ON.load(Ordering::Relaxed) { "HT APs (wifisim.ht=1)" } else { "all legacy (wifisim.ht=0)" },
              if BA_ON.load(Ordering::Relaxed) { "on" } else { "declined (wifisim.ba=0)" },
              if REORDER_ON.load(Ordering::Relaxed) { "on" } else { "OFF (wifisim.reorder=0)" }, h, d, l);
    }
    for a in aps.iter() {
        slog!("  {} ch {:>3} {:>4} dBm  {:<10} {}{}{}{}{}'{}'", wm::fmt_mac(&a.bssid), a.chan, a.dbm, sec_name(a.sec),
              if a.ht { "" } else { "legacy " },
              if a.hidden { "hidden " } else { "" },
              if a.refuse_assoc { "refuses-assoc " } else if a.deauth_hs { "deauths-in-handshake " } else { "" },
              match a.rsn_var { RsnVar::Plain => "", RsnVar::TkipGroup => "tkip-group ", RsnVar::TkipOnly => "tkip-only ", RsnVar::Sha256Only => "psk-sha256-only ",
                                RsnVar::MfpRequired => "mfp-required ", RsnVar::FtOnly => "ft-psk-only " },
              if a.path > 0 { alloc::format!("path {} ", a.path + 1) } else { String::new() }, wf::ssid_display(&a.ssid));
    }
    *SIM.lock() = Some(Sim { aps, radio_on: true, assoc: None, pending_deauth: None });
    ACTIVE.store(true, Ordering::SeqCst);
    MGR.set_caps(SimRadio.caps());
    let selftest = cmdline_val(cmdline, "wifisim.selftest").and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
    if sched::kthread::spawn("wifisim", || {
        // "Firmware" start-up.
        msleep(200);
        wm::run(&MGR, &mut SimRadio);
        log("manager loop ended");
    }).is_none() {
        log("could not start the sim thread");
        return;
    }
    if selftest > 0 {
        if sched::kthread::spawn("wifisim-test", move || selftest_main(selftest)).is_none() {
            log("selftest: could not start the thread");
        }
    }
}

/// Register the control side with the netctl seam (the netif registry gives
/// the ifindex; `name` is the interface name, "wifi0").
pub fn attach(ifindex: u32, name: &str) -> bool { if present() { MGR.attach(ifindex, name) } else { false } }

/// The sim is on.
pub fn present() -> bool { ACTIVE.load(Ordering::Relaxed) }

/// The virtio-net device index the sim uses as its data path (the netif
/// registry must not register it as a wired interface).
pub fn claims_virtio(idx: usize) -> bool { present() && PATHS.lock().contains(&idx) }

// ── data path (netif ops) ───────────────────────────────────────────────────

pub fn link_up() -> bool { LINK_UP.load(Ordering::Relaxed) }

pub fn link_generation() -> u32 { LINK_GEN.load(Ordering::Relaxed) }

pub fn mac_address() -> Option<[u8; 6]> { mac_fn() }

pub fn send_packet(eth: &[u8]) -> bool {
    if !link_up() { return false; }
    TX_FRAMES.fetch_add(1, Ordering::Relaxed);
    match NIC.load(Ordering::Relaxed) {
        n if n >= 0 => crate::virtio_net::send_packet(n as usize, eth),
        _ => true,
    }
}

fn raw_receive(buf: &mut [u8]) -> Option<usize> {
    let n = NIC.load(Ordering::Relaxed);
    if n < 0 { return None; }
    if link_up() {
        // An HT association: the downlink goes through the A-MPDU
        // emulation and the station's reorder path.
        let mut g = HTPIPE.lock();
        if let Some(p) = g.as_mut() {
            let now = now_ms();
            for _ in 0..4 {
                if !p.ready.is_empty() { break; }
                if p.pump(now, &mut |b| crate::virtio_net::poll_receive(n as usize, b)) == 0 { break; }
            }
            let f = p.pop()?;
            let len = f.len().min(buf.len());
            buf[..len].copy_from_slice(&f[..len]);
            RX_FRAMES.fetch_add(1, Ordering::Relaxed);
            return Some(len);
        }
    }
    let len = crate::virtio_net::poll_receive(n as usize, buf)?;
    if !link_up() {
        // Not associated: nothing reaches the station.
        RX_DROPPED.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    RX_FRAMES.fetch_add(1, Ordering::Relaxed);
    Some(len)
}

pub fn poll_receive(buf: &mut [u8]) -> Option<usize> {
    NET_POLLED.store(true, Ordering::Relaxed);
    raw_receive(buf)
}

pub fn rx_rearm() -> bool {
    match NIC.load(Ordering::Relaxed) { n if n >= 0 => crate::virtio_net::rx_rearm(n as usize), _ => false }
}

/// `key: value` lines for /proc/net/nic.
pub fn info_lines(out: &mut String) {
    use core::fmt::Write;
    let _ = writeln!(out, "driver: wifi_sim");
    let _ = writeln!(out, "mac: {}", wm::fmt_mac(&*MAC.lock()));
    let n = NIC.load(Ordering::Relaxed);
    let _ = writeln!(out, "data_path: {}", if n >= 0 { alloc::format!("virtio-net device {}", n) } else { String::from("none (sink)") });
    let _ = writeln!(out, "link: {}", if link_up() { "up" } else { "down" });
    if let Some(s) = SIM.lock().as_ref() {
        if let Some(i) = s.assoc {
            let a = &s.aps[i];
            let _ = writeln!(out, "ssid: {}", wf::ssid_display(&a.ssid));
            let _ = writeln!(out, "bssid: {} channel {} signal {} dBm", wm::fmt_mac(&a.bssid), a.chan, a.dbm);
        }
    }
    let _ = writeln!(out, "rx_frames: {} rx_dropped: {} tx_frames: {}", RX_FRAMES.load(Ordering::Relaxed),
                     RX_DROPPED.load(Ordering::Relaxed), TX_FRAMES.load(Ordering::Relaxed));
    ht_info_lines(out);
    MGR.info_lines(out);
}

/// The BSS cache as a table (for /proc/net/wifi).
pub fn scan_lines(out: &mut String) { MGR.scan_lines("wifi_sim (simulated Wi-Fi)", out); }

// ── the radio ───────────────────────────────────────────────────────────────

struct SimRadio;

fn with_sim<R>(f: impl FnOnce(&mut Sim) -> R) -> Option<R> { SIM.lock().as_mut().map(f) }

fn set_link(up: bool) {
    let was = LINK_UP.swap(up, Ordering::SeqCst);
    if up && !was { LINK_GEN.fetch_add(1, Ordering::SeqCst); }
    if up != was { crate::netdev::kick(); }
}

/// The AP side of the WPA2 4-way handshake against the real supplicant.
/// Returns the pairwise key both sides installed.
/// The supplicant of the current association with the AP's side of its keys,
/// so the self-test can run a group rekey (EAPOL group message 1).
struct SimHs { sup: wc::Supplicant, kek: [u8; 16], kck: [u8; 16], rc: u64, tkip: bool }
static HS: Mutex<Option<SimHs>> = Mutex::new(None);
/// Key index + 1 of a group message 1 the next connected scan receives.
static M1_DURING_SCAN: AtomicU32 = AtomicU32::new(0);
static SIM_HELD: wm::gk::Held<Vec<u8>> = wm::gk::Held::new();

/// The AP sends a group-key handshake message 1 with a fresh GTK at `key_id`;
/// the supplicant unwraps it and the install is recorded like a real one.
fn sim_group_rekey(key_id: u8) -> Result<(), String> {
    let m1 = sim_group_m1(key_id)?;
    sim_group_deliver(&m1)
}

/// Group message 1 for a fresh GTK at `key_id`, as the AP would send it.
fn sim_group_m1(key_id: u8) -> Result<Vec<u8>, String> {
    use wc::key_info as ki;
    let mut g = HS.lock();
    let h = g.as_mut().ok_or_else(|| String::from("no handshake state"))?;
    let mut buf = [0u8; 32];
    sched::random::fill(&mut buf);
    let gtk = &buf[..if h.tkip { 32 } else { 16 }];
    let mut kd = Vec::new();
    kd.extend_from_slice(&[0xdd, 6 + gtk.len() as u8, 0x00, 0x0f, 0xac, 0x01, key_id & 3, 0x00]);
    kd.extend_from_slice(gtk);
    if kd.len() % 8 != 0 { kd.push(0xdd); while kd.len() % 8 != 0 { kd.push(0); } }
    let wrapped = wc::aes_key_wrap(&h.kek, &kd);
    h.rc += 1;
    let mut m1 = wc::build_eapol_key(2, 2 | ki::ACK | ki::MIC | ki::SECURE | ki::ENCRYPTED_KEY_DATA, 0, &h.rc.to_be_bytes(), &[0u8; 32], &wrapped);
    wc::eapol_set_mic(&h.kck, &mut m1);
    Ok(m1)
}

/// The supplicant receives a group message 1: the install is recorded like a real one.
fn sim_group_deliver(m1: &[u8]) -> Result<(), String> {
    let mut g = HS.lock();
    let h = g.as_mut().ok_or_else(|| String::from("no handshake state"))?;
    let mut done = false;
    for e in h.sup.on_eapol(m1) {
        match e {
            wc::HsEvent::InstallGtk { key_id: k, .. } => { wm::gk::note_install(k, h.tkip); done = true; }
            wc::HsEvent::Error(e) => return Err(alloc::format!("supplicant: {}", e)),
            _ => {}
        }
    }
    if done { Ok(()) } else { Err(String::from("no GTK install event")) }
}

fn four_way(ap_pmk: &[u8; 32], sta_pmk: &[u8; 32], aa: [u8; 6], spa: [u8; 6], group_tkip: bool, cx: &ConnCtx) -> Result<[u8; 16], Fail> {
    use wc::key_info as ki;
    let mut anonce = [0u8; 32];
    sched::random::fill(&mut anonce);
    let mut snonce = [0u8; 32];
    sched::random::fill(&mut snonce);
    let own_rsn = wc::rsn_ie_psk(if group_tkip { wm::SUITE_TKIP } else { wm::SUITE_CCMP });
    let mut sup = wc::Supplicant::new(*sta_pmk, spa, aa, own_rsn.to_vec(), snonce);
    sup.set_group_tkip(group_tkip);
    let mut rc: u64 = 1;
    let mut msg2_sent = false;
    for _try in 0..M1_TRIES {
        let m1 = wc::build_eapol_key(2, 2 | ki::TYPE_PAIRWISE | ki::ACK, 16, &rc.to_be_bytes(), &anonce, &[]);
        rc += 1;
        let m2 = sup.on_eapol(&m1).into_iter().find_map(|e| if let wc::HsEvent::Send(p) = e { Some(p) } else { None });
        let m2 = match m2 { Some(m) => m, None => return Err(Fail::new(reason::FW_ERROR, "supplicant did not answer message 1")) };
        msg2_sent = true;
        let k2 = match wc::parse_eapol_key(&m2) { Some(k) => k, None => return Err(Fail::new(reason::FW_ERROR, "malformed message 2")) };
        let ptk = wc::derive_ptk(ap_pmk, &aa, &spa, &anonce, &k2.nonce);
        if wc::eapol_mic_ok(&ptk.kck, &m2) {
            // Message 3: GTK KDE (and our RSN IE) wrapped with the KEK.
            // A mixed-mode AP hands out a 32-byte TKIP GTK.
            let mut gtk_buf = [0u8; 32];
            sched::random::fill(&mut gtk_buf);
            let gtk = &gtk_buf[..if group_tkip { 32 } else { 16 }];
            let mut kd = Vec::new();
            kd.extend_from_slice(&own_rsn);
            kd.extend_from_slice(&[0xdd, 6 + gtk.len() as u8, 0x00, 0x0f, 0xac, 0x01, 0x01, 0x00]);
            kd.extend_from_slice(&gtk);
            // Key data padding (IEEE 802.11 12.7.2): 0xdd then zeros to a multiple of 8.
            if kd.len() % 8 != 0 { kd.push(0xdd); while kd.len() % 8 != 0 { kd.push(0); } }
            let wrapped = wc::aes_key_wrap(&ptk.kek, &kd);
            let mut m3 = wc::build_eapol_key(2, 2 | ki::TYPE_PAIRWISE | ki::INSTALL | ki::ACK | ki::MIC | ki::SECURE | ki::ENCRYPTED_KEY_DATA,
                                             16, &rc.to_be_bytes(), &anonce, &wrapped);
            wc::eapol_set_mic(&ptk.kck, &mut m3);
            let mut m4 = None;
            let mut tk = None;
            for e in sup.on_eapol(&m3) {
                match e {
                    wc::HsEvent::Send(p) => m4 = Some(p),
                    wc::HsEvent::InstallPtk(k) => tk = Some(k),
                    wc::HsEvent::InstallGtk { key, .. } if key[..] != gtk[..] =>
                        return Err(Fail::new(reason::FW_ERROR, "the station installed a different group key")),
                    wc::HsEvent::Error(e) => return Err(Fail::new(reason::FW_ERROR, alloc::format!("supplicant: {}", e))),
                    wc::HsEvent::InstallGtk { key_id, .. } => wm::gk::note_install(key_id, group_tkip),
                    _ => {}
                }
            }
            let m4 = m4.ok_or_else(|| Fail::new(reason::FW_ERROR, "no message 4"))?;
            if !wc::eapol_mic_ok(&ptk.kck, &m4) { return Err(Fail::new(reason::FW_ERROR, "message 4 MIC failure")); }
            let tk = tk.ok_or_else(|| Fail::new(reason::FW_ERROR, "no pairwise key"))?;
            if tk != ptk.tk { return Err(Fail::new(reason::FW_ERROR, "the two sides derived different keys")); }
            *HS.lock() = Some(SimHs { sup, kek: ptk.kek, kck: ptk.kck, rc: rc + 1, tkip: group_tkip });
            return Ok(tk);
        }
        // Message 2's MIC does not verify: a different PMK. The AP drops it
        // and retransmits message 1 after its timeout.
        if !cx.sleep(M1_RETRY_MS) { return Err(Fail::superseded()); }
    }
    // The AP gives up: deauthentication, reason 15.
    Err(wm::handshake_failure(msg2_sent, Some(wm::IEEE_REASON_4WAY_TIMEOUT)))
}

impl Radio for SimRadio {
    fn caps(&self) -> u32 {
        // Mirrors what iwlwifi can do today (WPA2-PSK/CCMP; SAE deferred),
        // plus scanning while associated.
        wifi_caps::PSK | wifi_caps::BAND_2GHZ | wifi_caps::BAND_5GHZ | wifi_caps::SCAN_CONNECTED | wifi_caps::DIRECTED_SCAN
    }

    fn scan(&mut self, ssids: &[Vec<u8>], connected: bool) -> Result<Vec<ScanHit>, Fail> {
        // The AP's group message 1 arrives while the scan waits for its
        // completion: the wait does not claim it (iwlwifi's scan() path).
        let k = M1_DURING_SCAN.swap(0, Ordering::Relaxed);
        if k != 0 && connected {
            if let Ok(m1) = sim_group_m1(k as u8 - 1) { SIM_HELD.unclaimed(m1, true, connected); }
        }
        msleep(SCAN_MS);
        let hits = with_sim(|s| {
            if !s.radio_on { return Vec::new(); }
            s.aps.iter().filter(|a| a.on_air).map(|a| {
                let reveal = ssids.iter().any(|x| *x == a.ssid);
                ScanHit { bss: ap_bss(a, reveal), rssi: a.dbm }
            }).collect()
        }).unwrap_or_default();
        Ok(hits)
    }

    fn connect(&mut self, hit: &ScanHit, ssid: &[u8], akm_: u8, secret: &[u8], cx: &ConnCtx) -> Result<(), Fail> {
        let bssid = hit.bss.bssid;
        let find = |bssid: [u8; 6]| with_sim(|s| s.aps.iter().position(|a| a.bssid == bssid && a.on_air)).flatten();
        // Open System authentication.
        if !cx.sleep(MGMT_MS) { return Err(Fail::superseded()); }
        let i = match find(bssid) { Some(i) => i, None => return Err(Fail::new(reason::TIMEOUT, "no authentication response")) };
        cx.step(wifi_state::ASSOCIATING);
        if !cx.sleep(MGMT_MS) { return Err(Fail::superseded()); }
        let a = with_sim(|s| s.aps[i].clone()).ok_or_else(|| Fail::new(reason::FW_ERROR, "sim gone"))?;
        if a.refuse_assoc { return Err(Fail::new(reason::ASSOC_REJECTED, "association refused: status 17 (AP full)").status(17)); }
        if !offers(&a, akm_) { return Err(Fail::new(reason::ASSOC_REJECTED, "association refused: status 43 (invalid AKMP)").status(43)); }
        if akm_ != akm::NONE {
            cx.step(wifi_state::HANDSHAKE);
            if a.deauth_hs {
                if !cx.sleep(MGMT_MS) { return Err(Fail::superseded()); }
                slog!("{} deauthenticated us during the handshake (reason 6)", wm::fmt_mac(&bssid));
                return Err(wm::handshake_failure(false, Some(6)));
            }
            let ap_pmk = match a.pmk {
                Some(p) => p,
                None => {
                    let p = wm::pmk_for(&a.ssid, &a.secret);
                    with_sim(|s| s.aps[i].pmk = Some(p));
                    p
                }
            };
            let t0 = now_ms();
            let sta_pmk = wm::pmk_for(ssid, secret);
            slog!("PMK derived (PBKDF2-SHA1, 4096 iterations) in {} ms", now_ms() - t0);
            if cx.aborted() { return Err(Fail::superseded()); }
            four_way(&ap_pmk, &sta_pmk, bssid, *MAC.lock(), a.rsn_var == RsnVar::TkipGroup, cx)?;
            slog!("4-way handshake complete with {} (pairwise key agreed by both sides)", wm::fmt_mac(&bssid));
        }
        with_sim(|s| { s.assoc = Some(i); s.pending_deauth = None; });
        // The AP's network: its data path carries the frames from now on.
        let path = { let ps = PATHS.lock(); ps.get(a.path as usize).or(ps.first()).copied() };
        if let Some(p) = path { NIC.store(p as i32, Ordering::SeqCst); }
        start_ht(&a, *MAC.lock());
        set_link(true);
        Ok(())
    }

    fn poll_connected(&mut self, budget_ms: u64) -> Option<Fail> {
        // Frames a scan popped meanwhile reach the supplicant now.
        while let Some(m1) = SIM_HELD.pop() {
            if let Err(e) = sim_group_deliver(&m1) { log(&alloc::format!("held group message 1: {}", e)); }
        }
        msleep(budget_ms.min(50));
        with_sim(|s| {
            let i = s.assoc?;
            if let Some(r) = s.pending_deauth.take() {
                return Some(Fail::new(reason::DEAUTH_BY_AP, alloc::format!("deauthenticated by the AP (reason {})", r)).ieee_reason(r));
            }
            if !s.aps[i].on_air { return Some(Fail::new(reason::BEACON_LOSS, "beacons lost")); }
            None
        }).flatten()
    }

    fn leave(&mut self, _deauth: bool) {
        set_link(false);
        stop_ht();
        with_sim(|s| s.assoc = None);
    }

    fn settle(&mut self, _after: Option<&Fail>) -> bool { true }

    fn idle(&mut self, ms: u64) {
        // Frames for a station that is not associated go nowhere: drain them
        // unless the net server is polling (it drops them through
        // `poll_receive` itself).
        if !link_up() && !NET_POLLED.load(Ordering::Relaxed) {
            let mut buf = [0u8; 1600];
            for _ in 0..64 { if raw_receive(&mut buf).is_none() { break; } }
        }
        msleep(ms);
    }

    fn set_radio(&mut self, on: bool) -> bool {
        with_sim(|s| s.radio_on = on);
        if !on { set_link(false); stop_ht(); with_sim(|s| s.assoc = None); }
        true
    }

    fn signal(&mut self) -> Option<i8> { with_sim(|s| s.assoc.map(|i| s.aps[i].dbm)).flatten() }

    fn tx_rate_kbps(&mut self) -> u32 { 54_000 }
}

// ── HT access point emulation ───────────────────────────────────────────────
//
// An HT AP (every AP unless its flags hold `l`) advertises WMM + HT in its
// beacon (`ap_ies`). The station decides with the same wifi_ht code iwlwifi
// runs; once associated as HT, the AP asks for an RX Block Ack session with
// an ADDBA request and from then on sends the station's downlink as
// A-MPDU-like bursts of QoS data frames (up to 16 per burst) in which some
// subframes miss the station and are retransmitted at the head of the next
// burst (holes the reorder buffer must close), some are received but their
// Block Ack is lost so they come again (duplicates), and optionally some
// are given up on (a Block Ack Request then moves the window). Station side
// every frame goes through what iwlwifi runs: RxGuard (addresses, then
// duplicates in sequence order), the wifi_ht reorder buffer, the 802.11 ->
// Ethernet conversion.
//
// Knobs: `wifisim.ht=0` (APs and station legacy, frames straight through),
// `wifisim.ba=0` (the station declines ADDBA: QoS frames, no aggregation,
// no perturbation), `wifisim.reorder=0` (the session is accepted but the
// station does not reorder: what TCP sees without a reorder buffer),
// `wifisim.ampdu=<hold>:<dup>:<loss>` per-mille rates (default 30:10:0).

static HT_ON: AtomicBool = AtomicBool::new(true);
static BA_ON: AtomicBool = AtomicBool::new(true);
static REORDER_ON: AtomicBool = AtomicBool::new(true);
static AMPDU: Mutex<(u32, u32, u32)> = Mutex::new((30, 10, 0));
/// The downlink of the current HT association (None: frames go straight).
static HTPIPE: Mutex<Option<HtPipe>> = Mutex::new(None);

/// Frames per emulated A-MPDU.
const BURST: usize = 16;
/// Retransmissions wait at most this long for the next burst.
const RETX_MS: u64 = 2;

/// What the AP's beacon carries besides the basics: HT capabilities and
/// operation, the WMM parameters (hostapd's defaults), for an HT AP.
fn ap_ies(a: &SimAp) -> Vec<u8> {
    let mut v = Vec::new();
    if !a.ht || !HT_ON.load(Ordering::Relaxed) { return v; }
    let cap = wh::HtCfg { rx_streams: 2, tx_streams: 2, ldpc: false, stbc: true, sgi20: true, sm_ps: wh::SM_PS_DISABLED, ht40: false };
    v.extend_from_slice(&wh::ht_cap_ie(&cap));
    let mut op = [0u8; 24];
    op[0] = wh::IE_HT_OP;
    op[1] = 22;
    op[2] = a.chan;
    v.extend_from_slice(&op);
    v.extend_from_slice(&[0xdd, 0x18, 0x00, 0x50, 0xf2, 0x02, 0x01, 0x01, 0x01, 0x00,
                          0x03, 0xa4, 0x00, 0x00, 0x27, 0xa4, 0x00, 0x00, 0x42, 0x43, 0x5e, 0x00, 0x62, 0x32, 0x2f, 0x00]);
    v
}

/// Counters of the emulated downlink.
#[derive(Clone, Copy, Default, Debug)]
pub struct PipeStats {
    /// Unicast frames the AP sent (first transmissions).
    pub frames: u64,
    pub held: u64,
    pub retx: u64,
    pub dups: u64,
    pub lost: u64,
    pub bars: u64,
    /// Frames the station handed up with a sequence number below an
    /// earlier one (what the stack sees as reordering).
    pub out_of_order: u64,
    pub delivered: u64,
    /// Refused by the station's RxGuard (duplicates, foreign).
    pub dropped: u64,
}

/// The AP's downlink emulation and the station's receive path for one
/// association.
pub struct HtPipe {
    bssid: [u8; 6],
    own: [u8; 6],
    tid: u8,
    /// The AP's next sequence number on `tid`.
    seq: u16,
    /// Agreed Block Ack window (None: no session, frames go in order).
    ba: Option<u16>,
    /// Frames that missed the station: (frame with Retry set, sequence, when).
    held: Vec<(Vec<u8>, u16, u64)>,
    /// Highest sequence number given up on, until a BAR covers it.
    lost_max: Option<u16>,
    rng: u32,
    hold: u32,
    dup: u32,
    loss: u32,
    reorder: bool,
    agg: wh::RxAgg<Vec<u8>>,
    guard: wf::RxGuard,
    ready: VecDeque<Vec<u8>>,
    last_sn: Option<u16>,
    scratch: Vec<u8>,
    pub st: PipeStats,
}

impl HtPipe {
    pub fn new(bssid: [u8; 6], own: [u8; 6], hold: u32, dup: u32, loss: u32, reorder: bool, seed: u32) -> HtPipe {
        HtPipe { bssid, own, tid: 0, seq: 100, ba: None, held: Vec::new(), lost_max: None, rng: seed | 1, hold, dup, loss,
                 reorder, agg: wh::RxAgg::default(), guard: wf::RxGuard::new(bssid, own), ready: VecDeque::new(),
                 last_sn: None, scratch: alloc::vec![0u8; 2048], st: PipeStats::default() }
    }

    fn rnd(&mut self) -> u32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        x % 1000
    }

    /// The AP's ADDBA request, answered by the station with the shared code
    /// (`ba_on` false: declined), the answer read back by the AP. Ok(window)
    /// when a session runs.
    pub fn negotiate(&mut self, ba_on: bool) -> Result<u16, u16> {
        let params = wh::BaParams { amsdu: false, immediate: true, tid: self.tid, buf_size: wh::MAX_BA_BUF };
        let req = wh::build_addba_req(&self.own, &self.bssid, &self.bssid, 1, params, 0, self.seq);
        // Station.
        let resp = match wh::parse_back_action(&req) {
            Some(wh::BackAction::AddbaReq { dialog, params, timeout, ssn }) => {
                let (status, rp) = wh::answer_addba(&params, ba_on, wh::MAX_BA_BUF, true);
                if status == wh::STATUS_SUCCESS {
                    let mut out = Vec::new();
                    self.agg.start(rp.tid, ssn, rp.buf_size, &mut out);
                }
                wh::build_addba_resp(&self.bssid, &self.own, &self.bssid, dialog, status, rp, timeout)
            }
            _ => return Err(0xffff),
        };
        // AP.
        match wh::parse_back_action(&resp) {
            Some(wh::BackAction::AddbaResp { status: wh::STATUS_SUCCESS, params, .. }) => {
                self.ba = Some(params.buf_size);
                Ok(params.buf_size)
            }
            Some(wh::BackAction::AddbaResp { status, .. }) => Err(status),
            _ => Err(0xffff),
        }
    }

    pub fn session(&self) -> Option<u16> { self.ba }

    /// One downlink Ethernet frame: into `fresh` (this burst), `held`
    /// (next burst) or lost.
    fn ap_tx(&mut self, eth: &[u8], now: u64, fresh: &mut Vec<Vec<u8>>) {
        if eth.len() < 14 { return; }
        if eth[0] & 1 != 0 {
            // Group addressed: never aggregated.
            if let Some(f) = wh::build_qos_data_from_ap(eth, &self.bssid, self.tid, 0, false) { fresh.push(f); }
            return;
        }
        let sn = self.seq;
        self.seq = wh::sn_add(self.seq, 1);
        let f = match wh::build_qos_data_from_ap(eth, &self.bssid, self.tid, sn, false) { Some(f) => f, None => return };
        self.st.frames += 1;
        if self.ba.is_none() { fresh.push(f); return; }
        let r = self.rnd();
        let retry = |mut f: Vec<u8>| { f[1] |= 0x08; f };
        if r < self.loss {
            self.st.lost += 1;
            self.lost_max = Some(match self.lost_max { Some(m) if wh::sn_less(sn, m) => m, _ => sn });
        } else if r < self.loss + self.hold {
            self.st.held += 1;
            self.held.push((retry(f), sn, now));
        } else {
            let d = self.rnd() < self.dup;
            if d { self.st.dups += 1; fresh.push(f.clone()); fresh.push(retry(f)); } else { fresh.push(f); }
        }
    }

    /// One emulated burst: the retransmissions of the last one first, then
    /// up to BURST new frames from `pull`; a BAR when the AP gave up on
    /// frames; the station's reorder timeouts. Returns the frames pulled.
    pub fn pump(&mut self, now: u64, pull: &mut dyn FnMut(&mut [u8]) -> Option<usize>) -> usize {
        let old = core::mem::take(&mut self.held);
        let mut fresh = Vec::new();
        let mut scratch = core::mem::take(&mut self.scratch);
        let mut n = 0;
        while n < BURST {
            match pull(&mut scratch) {
                Some(len) => { let len = len.min(scratch.len()); self.ap_tx(&scratch[..len], now, &mut fresh); n += 1; }
                None => break,
            }
        }
        self.scratch = scratch;
        let mut burst = Vec::new();
        if !old.is_empty() && (n > 0 || now >= old[0].2 + RETX_MS) {
            for (f, _, _) in old { self.st.retx += 1; burst.push(f); }
        } else {
            // Not yet: they wait for the next burst ahead of the new holds.
            let newer = core::mem::replace(&mut self.held, old);
            self.held.extend(newer);
        }
        burst.extend(fresh);
        for f in burst { self.sta_rx(&f, now); }
        if let Some(m) = self.lost_max {
            // The window start of the AP: the oldest frame still owed, else the next one.
            let ssn = self.held.iter().map(|h| h.1).fold(self.seq, |a, s| if wh::sn_less(s, a) { s } else { a });
            if wh::sn_less(m, ssn) {
                self.lost_max = None;
                self.st.bars += 1;
                let bar = wh::build_bar(&self.own, &self.bssid, self.tid, ssn);
                self.sta_bar(&bar, now);
            }
        }
        let mut out = Vec::new();
        self.agg.tick(now, &mut out);
        for f in out { self.sta_deliver(&f); }
        n
    }

    fn sta_rx(&mut self, f: &[u8], now: u64) {
        if self.guard.check_addr(f).is_err() { self.st.dropped += 1; return; }
        if self.reorder {
            if let Some((tid, sn)) = wh::qos_tid_seq(f) {
                if let Some(Some(r)) = self.agg.tids.get_mut(tid as usize) {
                    let mut out = Vec::new();
                    match r.fast(sn) {
                        Some(wh::Fate::Direct) | Some(wh::Fate::PreSession) => {}
                        Some(_) => return,
                        None => {
                            r.rx(sn, f.to_vec(), now, &mut out);
                            for g in out { self.sta_deliver(&g); }
                            return;
                        }
                    }
                }
            }
        }
        self.sta_deliver(f);
    }

    fn sta_bar(&mut self, bar: &[u8], now: u64) {
        let (ta, tid, ssn) = match wh::parse_bar(bar) { Some(x) => x, None => return };
        if ta != self.bssid || wh::ctl_ra(bar) != Some(self.own) || !self.reorder { return; }
        let mut out = Vec::new();
        if let Some(Some(r)) = self.agg.tids.get_mut(tid as usize) { r.bar(ssn, now, &mut out); }
        for g in out { self.sta_deliver(&g); }
    }

    fn sta_deliver(&mut self, f: &[u8]) {
        if self.guard.check_seq(f, false).is_err() { self.st.dropped += 1; return; }
        if let Some((_, sn)) = wh::qos_tid_seq(f) {
            match self.last_sn {
                Some(l) if wh::sn_less(sn, l) => self.st.out_of_order += 1,
                _ => self.last_sn = Some(sn),
            }
        }
        let mut out = alloc::vec![0u8; f.len()];
        if let Some(n) = wf::data_to_ethernet(f, 0, 0, &mut out) {
            out.truncate(n);
            self.st.delivered += 1;
            self.ready.push_back(out);
        }
    }

    pub fn pop(&mut self) -> Option<Vec<u8>> { self.ready.pop_front() }

    /// The station's reorder counters.
    pub fn reorder_stats(&self) -> wh::ReorderStats { self.agg.stats() }
    pub fn stored(&self) -> usize { self.agg.stored() }
}

/// After a successful association: an HT downlink when the AP and the
/// station's association request agree on HT (the AP reads the request's
/// elements with the station's parser), then the ADDBA exchange.
fn start_ht(a: &SimAp, own: [u8; 6]) {
    *HTPIPE.lock() = None;
    if !a.ht || !HT_ON.load(Ordering::Relaxed) { return; }
    // The station's request as iwlwifi builds it (HT capabilities + WMM
    // information); the AP checks both are there.
    let beacon = wh::parse_bss_ies(&ap_ies(a));
    if beacon.wmm.is_none() || beacon.ht_cap.is_none() || beacon.ht_op.is_none() { return; }
    let mut extra = Vec::new();
    extra.extend_from_slice(&wh::ht_cap_ie(&wh::iwl8265_ht_cfg(3, 3)));
    extra.extend_from_slice(&wh::wmm_info_ie(0));
    let req = wf::build_assoc_req(&a.bssid, &own, 0x0411, 10, &a.ssid, &[0x82, 0x84, 0x8b, 0x96], None, Some(&extra));
    let got = wh::parse_bss_ies(req.get(28..).unwrap_or(&[]));
    if got.wmm.is_none() || got.ht_cap.map_or(true, |c| c.rx_streams() == 0) {
        slog!("HT: the association request carries no HT/WMM elements; legacy downlink");
        return;
    }
    let (hold, dup, loss) = *AMPDU.lock();
    let mut seed = [0u8; 4];
    sched::random::fill(&mut seed);
    let mut p = HtPipe::new(a.bssid, own, hold, dup, loss, REORDER_ON.load(Ordering::Relaxed), u32::from_le_bytes(seed));
    let r = p.negotiate(BA_ON.load(Ordering::Relaxed));
    slog!("HT association with {}: {}", wm::fmt_mac(&a.bssid), match r {
        Ok(w) => alloc::format!("RX Block Ack TID 0 window {}; A-MPDU emulation hold {}‰ dup {}‰ loss {}‰, reorder {}", w, hold, dup, loss,
                                if p.reorder { "on" } else { "OFF (wifisim.reorder=0)" }),
        Err(s) => alloc::format!("ADDBA declined (status {}); QoS frames without aggregation", s),
    });
    *HTPIPE.lock() = Some(p);
}

fn stop_ht() { *HTPIPE.lock() = None; }

/// The `ht:` / `rx_ba:` lines of /proc/net/nic.
fn ht_info_lines(out: &mut String) {
    use core::fmt::Write;
    let (hold, dup, loss) = *AMPDU.lock();
    match HTPIPE.lock().as_ref() {
        None => { let _ = writeln!(out, "ht: {}; link legacy (no QoS)", if HT_ON.load(Ordering::Relaxed) { "wifisim.ht=1" } else { "wifisim.ht=0" }); }
        Some(p) => {
            let s = p.reorder_stats();
            let _ = writeln!(out, "ht: wifisim.ht=1; link qos 1 ht 1; ampdu emulation hold {} dup {} loss {} (per mille), reorder {}",
                             hold, dup, loss, if p.reorder { "on" } else { "off" });
            let _ = writeln!(out, "rx_ba: sessions {} window {}; reorder direct {} buffered {} released {} old {} dup {} skipped {} timeouts {} bars {} max_stored {} stored {}; emu frames {} held {} retx {} dups {} lost {} bars {} out_of_order {} delivered {} dropped {}",
                             p.agg.active(), p.ba.unwrap_or(0), s.direct, s.buffered, s.released, s.old, s.dup, s.skipped, s.timeouts,
                             s.bars, s.max_stored, p.stored(), p.st.frames, p.st.held, p.st.retx, p.st.dups, p.st.lost, p.st.bars,
                             p.st.out_of_order, p.st.delivered, p.st.dropped);
        }
    }
}

/// Self-test of the pipeline with synthetic frames and a fake clock: every
/// frame arrives once and in order through holes, duplicates, losses and
/// the reorder timeout. Returns (delivered, out_of_order, lost, detail).
fn pipe_selftest(hold: u32, dup: u32, loss: u32, n: u32) -> (u64, u64, u64, String) {
    let bssid = [0x0a, 0x57, 0x53, 0xee, 0, 1];
    let own = [0x02, 0x57, 0x53, 0, 0, 1];
    let mut p = HtPipe::new(bssid, own, hold, dup, loss, true, 0x5eed_1234);
    if p.negotiate(true) != Ok(wh::MAX_BA_BUF) { return (0, 0, 0, String::from("ADDBA not accepted")); }
    let mut next = 0u32;
    let mut now = 1000u64;
    let mut got: Vec<u32> = Vec::new();
    let mut eth = alloc::vec![0u8; 64];
    eth[0..6].copy_from_slice(&own);
    eth[6..12].copy_from_slice(&[0x52, 0x55, 0x0a, 0, 3, 2]);
    eth[12..14].copy_from_slice(&[0x08, 0x00]);
    while next < n || p.stored() > 0 || !p.held.is_empty() {
        let mut pull = |b: &mut [u8]| -> Option<usize> {
            if next >= n { return None; }
            let mut e = eth.clone();
            e[14..18].copy_from_slice(&next.to_be_bytes());
            next += 1;
            b[..e.len()].copy_from_slice(&e);
            Some(e.len())
        };
        p.pump(now, &mut pull);
        while let Some(e) = p.pop() { got.push(u32::from_be_bytes([e[14], e[15], e[16], e[17]])); }
        now += 1;
        if now > 1000 + 100_000 { break; }
        if next >= n { now += 50; }
    }
    let in_order = got.windows(2).all(|w| w[0] < w[1]);
    let s = p.reorder_stats();
    let detail = alloc::format!("{} sent, {} delivered, in order {}, held {} dups {} lost {} bars {}; reorder buffered {} timeouts {} old {} dup {}",
                                n, got.len(), in_order, p.st.held, p.st.dups, p.st.lost, p.st.bars, s.buffered, s.timeouts, s.old, s.dup);
    (got.len() as u64, if in_order { p.st.out_of_order } else { p.st.out_of_order.max(1) }, p.st.lost, detail)
}

// ── self-test controls ──────────────────────────────────────────────────────

/// Take an AP off the air (or back): beacon loss when associated, aging.
pub fn set_on_air(ssid: &[u8], on: bool) {
    with_sim(|s| for a in s.aps.iter_mut().filter(|a| a.ssid == ssid) { a.on_air = on; });
}

/// The associated AP deauthenticates us with `reason`.
pub fn deauth(reason: u16) { with_sim(|s| s.pending_deauth = Some(reason)); }

/// Set an AP's signal level.
pub fn set_signal(ssid: &[u8], dbm: i8) {
    with_sim(|s| for a in s.aps.iter_mut().filter(|a| a.ssid == ssid) { a.dbm = dbm; });
}

// ── self-test (wifisim.selftest=1|2) ────────────────────────────────────────

struct T {
    pass: u32,
    fail: u32,
}

impl T {
    fn check(&mut self, name: &str, ok: bool, detail: String) {
        if ok { self.pass += 1; slog!("selftest: {}: ok{}", name, if detail.is_empty() { String::new() } else { alloc::format!(" ({})", detail) }); }
        else { self.fail += 1; slog!("selftest: {}: FAIL {}", name, detail); }
    }
}

fn wait<F: FnMut() -> bool>(timeout_ms: u64, mut f: F) -> Option<u64> {
    let t0 = now_ms();
    loop {
        if f() { return Some(now_ms() - t0); }
        if now_ms() - t0 > timeout_ms { return None; }
        msleep(5);
    }
}

/// Wait for an event after `seq` matching `f`; returns (ms since `t0`, event).
fn wait_event(seq: u64, t0: u64, timeout_ms: u64, mut f: impl FnMut(&WifiEvent) -> bool) -> Option<(u64, WifiEvent)> {
    let mut found = None;
    wait(timeout_ms, || {
        for (_, t, e) in MGR.events_since(seq) {
            if f(&e) { found = Some((t.saturating_sub(t0), e)); return true; }
        }
        false
    })?;
    found
}

fn state_ev(e: &WifiEvent, state: u8) -> Option<netctl_abi::NcWifiState> {
    if let WifiEvent::State(s) = e { if s.state == state { return Some(*s); } }
    None
}

fn connect_params(attempt: u32, ssid: &str, a: u8, secret: &str, uuid: u8) -> ConnectParams {
    let mut u = [0u8; 16];
    if uuid != 0 { u = [uuid; 16]; u[6] = 0x40 | (uuid & 0x0f); u[8] = 0x80 | (uuid & 0x3f); }
    ConnectParams {
        attempt, profile_uuid: u, ssid: wm::ssid(ssid.as_bytes()), bssid: None, akm: a, pmf: netctl_abi::pmf::DEFAULT,
        hidden: false, secret: Secret::new(secret.as_bytes()).unwrap(),
    }
}

fn find_bss(ssid: &str) -> Option<NcBss> {
    let mut v = Vec::new();
    MGR.bss_snapshot(&mut v);
    v.into_iter().find(|b| b.ssid() == ssid.as_bytes())
}

fn find_bssid(bssid: [u8; 6]) -> Option<NcBss> {
    let mut v = Vec::new();
    MGR.bss_snapshot(&mut v);
    v.into_iter().find(|b| b.bssid == bssid)
}

/// Connect and wait for the outcome: Ok(ms to CONNECTED) or Err((ms, reason)).
fn do_connect(p: ConnectParams) -> Result<u64, (u64, u8, String)> {
    let seq = MGR.event_seq();
    let attempt = p.attempt;
    let t0 = now_ms();
    if let Err(e) = MGR.submit(WifiCmd::Connect(p)) { return Err((0, 0, alloc::format!("submit errno {}", e))); }
    match wait_event(seq, t0, 20_000, |e| matches!(e, WifiEvent::State(s) if (s.state == wifi_state::CONNECTED && s.attempt == attempt)
                                                                             || s.state == wifi_state::FAILED)) {
        Some((ms, WifiEvent::State(s))) if s.state == wifi_state::CONNECTED => Ok(ms),
        Some((ms, WifiEvent::State(s))) => Err((ms, s.reason, alloc::format!("FAILED {} (attempt {})", wm::reason_name(s.reason), s.attempt))),
        _ => Err((20_000, 0, String::from("no CONNECTED/FAILED within 20 s"))),
    }
}

fn disconnect(block: bool) -> Option<u64> {
    let seq = MGR.event_seq();
    let t0 = now_ms();
    MGR.submit(WifiCmd::Disconnect { block_autoconnect: block }).ok()?;
    wait_event(seq, t0, 10_000, |e| state_ev(e, wifi_state::DISCONNECTED).is_some()).map(|(ms, _)| ms)
}

fn scan(force: bool, ssids: &[&str]) -> Option<(u64, netctl_abi::NcScanInfo)> {
    let seq = MGR.event_seq();
    let t0 = now_ms();
    let ss = ssids.iter().map(|s| wm::ssid(s.as_bytes())).collect();
    MGR.submit(WifiCmd::Scan { ssids: ss, force }).ok()?;
    // The periodic background scan may finish meanwhile: not ours.
    match wait_event(seq, t0, 10_000, |e| matches!(e, WifiEvent::ScanDone(i) if i.flags & scan_flags::BACKGROUND == 0))? {
        (ms, WifiEvent::ScanDone(i)) => Some((ms, i)),
        _ => None,
    }
}

fn profile(ssid: &str, secret: &str, prio: i32, id: u8) -> netctl_abi::profiles::Profile {
    let mut u = [id; 16];
    u[6] = 0x40 | (id & 0x0f);
    u[8] = 0x80 | (id & 0x3f);
    let mut p = netctl_abi::profiles::Profile::new_wifi(u, ssid.as_bytes(), akm::AUTO, secret.as_bytes()).unwrap();
    p.priority = prio;
    p
}

fn selftest_main(level: u32) {
    let mut t = T { pass: 0, fail: 0 };
    let t_start = now_ms();
    slog!("selftest: level {} starting", level);
    // Ready: the manager reports DISCONNECTED and the boot scan is in.
    let ready = wait(10_000, || MGR.state().state == wifi_state::DISCONNECTED && MGR.last_scan().is_some());
    t.check("ready", ready.is_some(), alloc::format!("state {} after {} ms", wm::state_name(MGR.state().state), now_ms() - t_start));
    if ready.is_none() { finish(t); return; }

    // 1. A fresh scan lists every AP with the right security bits.
    msleep(2_100);
    let s = scan(true, &[]);
    t.check("scan", s.map_or(false, |(_, i)| i.flags & scan_flags::COALESCED == 0 && i.status == 0),
            alloc::format!("{:?}", s.map(|(ms, i)| (ms, i.n_bss, i.scan_gen, i.flags))));
    let home = find_bss("SimHome");
    let wpa3 = find_bss("SimWPA3");
    let mixed = find_bss("SimMixed");
    let open = find_bss("SimOpen");
    let cafe = find_bss("SimCafe");
    let hid = find_bssid(default_bssid(5));
    t.check("bss-psk", home.map_or(false, |b| b.ext & (bss_ext::AKM_PSK | bss_ext::JOINABLE | bss_ext::RSN) == (bss_ext::AKM_PSK | bss_ext::JOINABLE | bss_ext::RSN)
            && b.rsn_flags == nm_sec::KEY_MGMT_PSK | nm_sec::PAIR_CCMP | nm_sec::GROUP_CCMP && b.nm_flags == nm_ap_flags::PRIVACY && b.freq_mhz == 2437),
            alloc::format!("{:?}", home.map(|b| (b.ext, b.rsn_flags, b.freq_mhz, b.signal_dbm, b.quality))));
    t.check("bss-sae-only-not-joinable", wpa3.map_or(false, |b| b.ext & bss_ext::AKM_SAE != 0 && b.ext & bss_ext::MFPR != 0
            && b.ext & bss_ext::JOINABLE == 0 && b.rsn_flags & nm_sec::KEY_MGMT_SAE != 0),
            alloc::format!("{:?}", wpa3.map(|b| (b.ext, b.rsn_flags))));
    t.check("bss-transition", mixed.map_or(false, |b| b.ext & (bss_ext::TRANSITION | bss_ext::JOINABLE | bss_ext::AKM_SAE | bss_ext::AKM_PSK)
            == (bss_ext::TRANSITION | bss_ext::JOINABLE | bss_ext::AKM_SAE | bss_ext::AKM_PSK)), alloc::format!("{:?}", mixed.map(|b| b.ext)));
    t.check("bss-open", open.map_or(false, |b| b.nm_flags == 0 && b.rsn_flags == 0 && b.ext & bss_ext::JOINABLE != 0),
            alloc::format!("{:?}", open.map(|b| (b.nm_flags, b.ext))));
    t.check("bss-5ghz", cafe.map_or(false, |b| b.freq_mhz == 5180 && b.band == netctl_abi::band::GHZ_5), alloc::format!("{:?}", cafe.map(|b| b.freq_mhz)));
    // WPA/WPA2 mixed mode: PSK with a TKIP group cipher is joinable
    // (pairwise CCMP, the 32-byte TKIP GTK is only received with).
    let tkip = find_bss("SimTkip");
    t.check("bss-tkip-group-joinable", tkip.map_or(false, |b| b.ext & bss_ext::JOINABLE != 0 && b.ext & bss_ext::AKM_PSK != 0
            && b.rsn_flags & nm_sec::GROUP_TKIP != 0), alloc::format!("{:?}", tkip.map(|b| (b.ext, b.rsn_flags))));
    let r = do_connect(connect_params(10, "SimTkip", akm::AUTO, "tkip-pass-123", 0));
    t.check("connect-tkip-group", r.is_ok() && MGR.state().akm == akm::PSK, alloc::format!("{:?}", r));
    // The AP rekeys the group key (new key index): the install is counted
    // and the key index changes; the TKIP receive counters move.
    let (i0, k0) = (wm::gk::installs(), wm::gk::key_id());
    let rk = sim_group_rekey(2);
    let (i1, k1) = (wm::gk::installs(), wm::gk::key_id());
    t.check("gtk-rekey-counted", rk.is_ok() && i1 == i0 + 1 && k1 == 2 && k0 != 2, alloc::format!("{:?} installs {}->{} key {}->{}", rk, i0, i1, k0, k1));
    let mut gl = String::new();
    wm::gk::lines(&mut gl);
    t.check("gtk-proc-line", gl.contains("cipher tkip key_index 2 installs") && gl.contains("gtk_rx: tkip_fw"), gl.replace('\n', " | "));
    let r0 = [wm::gk::rx(wm::gk::FW), wm::gk::rx(wm::gk::SW), wm::gk::rx(wm::gk::MIC), wm::gk::rx(wm::gk::REPLAY), wm::gk::rx(wm::gk::UNDECRYPTED)];
    for k in [wm::gk::FW, wm::gk::SW, wm::gk::MIC, wm::gk::REPLAY, wm::gk::UNDECRYPTED] { wm::gk::note_rx(k); }
    let r1 = [wm::gk::rx(wm::gk::FW), wm::gk::rx(wm::gk::SW), wm::gk::rx(wm::gk::MIC), wm::gk::rx(wm::gk::REPLAY), wm::gk::rx(wm::gk::UNDECRYPTED)];
    t.check("gtk-rx-counters-move", (0..5).all(|i| r1[i] == r0[i] + 1), alloc::format!("{:?} -> {:?}", r0, r1));
    // An undecrypted group frame under key index 2 is classified by its
    // CCMP header's key index; a foreign one is counted apart.
    let mut uf = alloc::vec![0u8; 24 + 8 + 8];
    uf[0] = 0x08; uf[1] = 0x42; uf[4..10].copy_from_slice(&[0xff; 6]); uf[24 + 3] = 0x20 | (2 << 6);
    let (u0, f0) = (wm::gk::undecrypted(true, 2), wm::gk::undecrypted(false, 2));
    wm::gk::note_undecrypted(&uf, 0x4208, 0, false);
    wm::gk::note_undecrypted(&uf, 0x4208, 0, true);
    let mut ul = String::new();
    wm::gk::lines(&mut ul);
    t.check("undecrypted-classified", wm::gk::undecrypted(true, 2) == u0 + 1 && wm::gk::undecrypted(false, 2) == f0
            && ul.contains("rx_undecrypted: uc k0") && ul.contains("foreign "), ul.replace('\n', " | "));
    let _ = disconnect(true);
    let tko = find_bss("SimTkipOnly");
    t.check("bss-tkip-only-not-joinable", tko.map_or(false, |b| b.ext & bss_ext::JOINABLE == 0), alloc::format!("{:?}", tko.map(|b| (b.ext, b.rsn_flags))));
    t.check("bss-hidden", hid.map_or(false, |b| b.ext & bss_ext::HIDDEN != 0 && b.ssid_len == 0), alloc::format!("{:?}", hid.map(|b| (b.ext, b.ssid_len))));

    // 2. A request right after a scan is answered from the cache.
    let c = scan(false, &[]);
    t.check("scan-coalesced", c.map_or(false, |(ms, i)| i.flags & scan_flags::COALESCED != 0 && ms < 500), alloc::format!("{:?}", c.map(|(ms, i)| (ms, i.flags))));

    // 3. A directed probe reveals the hidden SSID.
    msleep(2_100);
    let d = scan(true, &["SimHidden"]);
    let hid = find_bssid(default_bssid(5));
    t.check("scan-directed-hidden", d.is_some() && hid.map_or(false, |b| b.ssid() == b"SimHidden"), alloc::format!("{:?}", hid.map(|b| b.ssid_len)));

    // 4. Wrong passphrase: FAILED(wrong-key), then DISCONNECTED(wrong-key).
    let seq = MGR.event_seq();
    let r = do_connect(connect_params(11, "SimHome", akm::AUTO, "not-the-pass", 1));
    let seen: Vec<(u8, u8)> = MGR.events_since(seq).iter().filter_map(|(_, _, e)| if let WifiEvent::State(s) = e { Some((s.state, s.reason)) } else { None }).collect();
    let follow = wait(2_000, || { let s = MGR.state(); s.state == wifi_state::DISCONNECTED && s.reason == reason::WRONG_KEY }).is_some();
    let mut path = seen.iter().map(|x| x.0).collect::<Vec<u8>>();
    path.dedup();
    let want = [wifi_state::CONNECTING, wifi_state::AUTHENTICATING, wifi_state::ASSOCIATING, wifi_state::HANDSHAKE, wifi_state::FAILED];
    t.check("connect-wrong-key", matches!(r, Err((_, reason::WRONG_KEY, _))) && follow && path.windows(5).any(|w| w == want),
            alloc::format!("{:?}; states {:?}", r, seen));

    // 5. Right passphrase: CONNECTED, data path up.
    let gen0 = link_generation();
    let r = do_connect(connect_params(12, "SimHome", akm::AUTO, "simhome-pass", 1));
    let st = MGR.state();
    t.check("connect-right-key", r.is_ok() && st.akm == akm::PSK && link_up() && link_generation() != gen0 && st.attempt == 12,
            alloc::format!("{:?} ms; akm {} attempt {} link {}", r, st.akm, st.attempt, link_up()));
    // An HT AP: the association runs an RX Block Ack session (unless the
    // knobs say otherwise).
    let sess = HTPIPE.lock().as_ref().map(|p| p.session());
    let want = if !HT_ON.load(Ordering::Relaxed) { None } else if !BA_ON.load(Ordering::Relaxed) { Some(None) } else { Some(Some(wh::MAX_BA_BUF)) };
    t.check("ht-addba", sess == want, alloc::format!("session {:?}, expected {:?}", sess, want));
    // The reorder path with synthetic frames and a fake clock: holes,
    // duplicates, then losses (BAR / timeout) - every frame once, in order.
    let (got, ooo, _, d) = pipe_selftest(300, 50, 0, 2000);
    t.check("ht-reorder", got == 2000 && ooo == 0, d);
    let (got, ooo, lost, d) = pipe_selftest(200, 30, 20, 2000);
    t.check("ht-reorder-loss", got + lost == 2000 && lost > 0 && ooo == 0, d);

    // 6. Data path: DHCP over the second NIC (level 2, nothing else polling).
    if level >= 2 {
        if NIC.load(Ordering::Relaxed) < 0 {
            t.check("dhcp", true, String::from("skipped: no data-path NIC"));
        } else if NET_POLLED.load(Ordering::Relaxed) {
            t.check("dhcp", true, String::from("skipped: the net server drives wifi0 (its lease is checked from userland)"));
        } else {
            let r = dhcp_probe();
            t.check("dhcp", r.is_ok(), match r { Ok(s) => s, Err(e) => e });
        }
    }

    // 7. Scanning while associated keeps the link.
    msleep(2_100);
    let (i0, k0) = (wm::gk::installs(), wm::gk::key_id());
    let nk = if k0 == 1 { 2 } else { 1 };
    M1_DURING_SCAN.store(nk as u32 + 1, Ordering::Relaxed);
    let s = scan(true, &[]);
    t.check("scan-while-connected", s.map_or(false, |(_, i)| i.flags & scan_flags::COALESCED == 0) && MGR.state().state == wifi_state::CONNECTED,
            alloc::format!("{:?} state {}", s.map(|(ms, i)| (ms, i.flags)), wm::state_name(MGR.state().state)));
    // Group message 1 popped by the scan's wait still installs the key (lane eapoldrop).
    let t1 = now_ms();
    while now_ms() - t1 < 2_000 && wm::gk::installs() == i0 { msleep(20); }
    let (i1, k1, ec) = (wm::gk::installs(), wm::gk::key_id(), wm::gk::eapol_counts());
    t.check("gtk-rekey-during-scan", i1 == i0 + 1 && k1 == nk, alloc::format!("installs {}->{} key {}->{} (want {}) eapol rx/held/dropped {:?}", i0, i1, k0, k1, nk, ec));

    // 7b. Two explicit user scans (netctl's flags) while associated are both
    // fresh: the scan generation advances each time.
    {
        let ufo = netctl_abi::scan_req_flags::USER & netctl_abi::scan_req_flags::FORCE != 0;
        msleep(2_100);
        let g0 = MGR.last_scan().map_or(0, |i| i.scan_gen);
        let a = scan(ufo, &[]);
        msleep(2_100);
        let b = scan(ufo, &[]);
        let (ga, gb) = (a.map_or(0, |x| x.1.scan_gen), b.map_or(0, |x| x.1.scan_gen));
        t.check("scan-user-twice-connected", ga != g0 && gb != ga && MGR.state().state == wifi_state::CONNECTED,
                alloc::format!("gen {} -> {} -> {}", g0, ga, gb));
    }
    // 7c. No periodic background scan while associated with a good signal
    // (the 120 s scan cost the laptop 2 s heartbeat gaps): only a weak
    // signal (roaming trigger) starts one; user scans stay fresh (7b).
    {
        let good = wm::connected_bg_scan_due(wm::CONNECTED_SCAN_MS * 2, -50);
        let unknown = wm::connected_bg_scan_due(wm::CONNECTED_SCAN_MS * 2, 0);
        let weak = wm::connected_bg_scan_due(wm::CONNECTED_SCAN_MS, wm::ROAM_SCAN_DBM - 10);
        let weak_soon = wm::connected_bg_scan_due(wm::CONNECTED_SCAN_MS - 1, wm::ROAM_SCAN_DBM - 10);
        t.check("no-periodic-scan-connected", !good && !unknown && weak && !weak_soon,
                alloc::format!("good {} unknown {} weak {} weak_soon {}", good, unknown, weak, weak_soon));
    }

    // 8. Disconnect: DISCONNECTED(user) well within 2 s, autoconnect blocked.
    let ms = disconnect(true);
    let st = MGR.state();
    t.check("disconnect-fast", ms.map_or(false, |m| m < netctl_abi::DISCONNECT_DEADLINE_MS as u64) && st.reason == reason::USER
            && st.flags & wifi_flags::AUTOCONNECT_BLOCKED != 0 && !link_up(), alloc::format!("{:?} ms, reason {}", ms, wm::reason_name(st.reason)));
    // Disconnect again while disconnected: still answered.
    let ms2 = disconnect(true);
    t.check("disconnect-idempotent", ms2.map_or(false, |m| m < 500), alloc::format!("{:?} ms", ms2));

    // 9. Failure reasons.
    let r = do_connect(connect_params(13, "SimWPA3", akm::AUTO, "wpa3-pass-123", 2));
    t.check("sae-only-unsupported", matches!(r, Err((_, reason::UNSUPPORTED_SECURITY, _))), alloc::format!("{:?}", r));
    let r = do_connect(connect_params(14, "NoSuchNetwork", akm::AUTO, "whatever-123", 3));
    t.check("no-ap", matches!(r, Err((_, reason::NO_AP, _))), alloc::format!("{:?}", r));
    let r = do_connect(connect_params(15, "SimFull", akm::AUTO, "full-pass-123", 4));
    t.check("assoc-rejected", matches!(r, Err((_, reason::ASSOC_REJECTED, _))) && MGR.state().ieee_status == 17, alloc::format!("{:?} status {}", r, MGR.state().ieee_status));
    let r = do_connect(connect_params(16, "SimHome", akm::NONE, "", 0));
    t.check("open-profile-on-psk-ap", matches!(r, Err((_, reason::UNSUPPORTED_SECURITY, _))), alloc::format!("{:?}", r));
    t.check("submit-sae-eopnotsupp", MGR.submit(WifiCmd::Connect(connect_params(17, "SimWPA3", akm::SAE, "wpa3-pass-123", 2))) == Err(netctl_abi::errno::EOPNOTSUPP), String::new());
    let r = do_connect(connect_params(18, "SimMixed", akm::AUTO, "mixed-pass-123", 5));
    t.check("transition-joins-as-psk", r.is_ok() && MGR.state().akm == akm::PSK, alloc::format!("{:?} akm {}", r, MGR.state().akm));

    // 10. A new CONNECT while connected replaces the link (no disconnect first).
    let seq = MGR.event_seq();
    let r = do_connect(connect_params(19, "SimOpen", akm::NONE, "", 6));
    let sup = MGR.events_since(seq).iter().any(|(_, _, e)| matches!(e, WifiEvent::State(s) if s.state == wifi_state::DISCONNECTED && s.reason == reason::SUPERSEDED));
    t.check("connect-replaces-link", r.is_ok() && sup && MGR.state().akm == akm::NONE, alloc::format!("{:?} superseded-event {}", r, sup));

    // 11. Beacon loss and deauthentication by the AP.
    let seq = MGR.event_seq();
    let t0 = now_ms();
    set_on_air(b"SimOpen", false);
    let lost = wait_event(seq, t0, 3_000, |e| state_ev(e, wifi_state::DISCONNECTED).map_or(false, |s| s.reason == reason::BEACON_LOSS));
    set_on_air(b"SimOpen", true);
    t.check("beacon-loss", lost.is_some() && !link_up(), alloc::format!("{:?} ms", lost.map(|x| x.0)));
    let _ = do_connect(connect_params(20, "SimHome", akm::AUTO, "simhome-pass", 1));
    let seq = MGR.event_seq();
    let t0 = now_ms();
    deauth(4);
    let d = wait_event(seq, t0, 3_000, |e| state_ev(e, wifi_state::DISCONNECTED).map_or(false, |s| s.reason == reason::DEAUTH_BY_AP && s.ieee_reason == 4));
    t.check("deauth-by-ap", d.is_some(), alloc::format!("{:?} ms", d.map(|x| x.0)));
    // (The network has no profile, so nothing reconnects.)
    let _ = disconnect(true);

    // 12. Aging: an AP that left stays listed until the age-out window passes.
    // (Fresh scans are rate-limited to one per 2 s, hence the 2.1 s steps.)
    MGR.set_age_out_ms(4_000);
    msleep(2_100);
    let _ = scan(true, &[]);
    set_on_air(b"SimCafe", false);
    msleep(2_100);
    let _ = scan(true, &[]);
    let kept = find_bss("SimCafe").is_some();
    msleep(2_100);
    let seq = MGR.event_seq();
    let _ = scan(true, &[]);
    let gone = find_bss("SimCafe").is_none();
    let gone_ev = MGR.events_since(seq).iter().any(|(_, _, e)| matches!(e, WifiEvent::BssGone(b) if b.ssid() == b"SimCafe"));
    t.check("bss-aging", kept && gone && gone_ev, alloc::format!("kept-after-2.4s {} gone-after-4.7s {} event {}", kept, gone, gone_ev));
    MGR.set_age_out_ms(netctl_abi::BSS_AGE_OUT_MS as u64);
    set_on_air(b"SimCafe", true);

    // 13. Radio off: RADIO_OFF, cache dropped; on: DISCONNECTED and a scan.
    let seq = MGR.event_seq();
    let t0 = now_ms();
    let _ = MGR.submit(WifiCmd::SetRadio(false));
    let off = wait_event(seq, t0, 3_000, |e| state_ev(e, wifi_state::RADIO_OFF).is_some());
    let mut v = Vec::new();
    MGR.bss_snapshot(&mut v);
    let scan_refused = MGR.submit(WifiCmd::Scan { ssids: Vec::new(), force: true }) == Err(netctl_abi::errno::ENETDOWN);
    t.check("radio-off", off.is_some() && v.is_empty() && scan_refused, alloc::format!("{:?} ms, {} BSSs", off.map(|x| x.0), v.len()));
    let seq = MGR.event_seq();
    let t0 = now_ms();
    let _ = MGR.submit(WifiCmd::SetRadio(true));
    let on = wait_event(seq, t0, 3_000, |e| state_ev(e, wifi_state::DISCONNECTED).is_some());
    let rescan = wait_event(seq, t0, 5_000, |e| matches!(e, WifiEvent::ScanDone(i) if i.n_bss > 0));
    t.check("radio-on", on.is_some() && rescan.is_some(), alloc::format!("{:?} / scan {:?} ms", on.map(|x| x.0), rescan.map(|x| x.0)));

    // 14. Autoconnect: highest priority wins; DISCONNECT blocks it until
    // RELOAD; a wrong-key profile is skipped for the next candidate.
    let seq = MGR.event_seq();
    let t0 = now_ms();
    crate::wifi_api::set_profiles(1000, alloc::vec![profile("SimHome", "simhome-pass", 0, 0x21), profile("SimMixed", "mixed-pass-123", 10, 0x22)]);
    let _ = MGR.submit(WifiCmd::ProfilesChanged);
    let ac = wait_event(seq, t0, 15_000, |e| state_ev(e, wifi_state::CONNECTED).is_some());
    let st = MGR.state();
    t.check("autoconnect-priority", ac.is_some() && st.ssid() == b"SimMixed" && st.flags & wifi_flags::AUTO != 0 && st.attempt & wm::AUTO_ATTEMPT_BIT != 0,
            alloc::format!("{:?} ms to '{}' flags {:#x}", ac.map(|x| x.0), wf::ssid_display(st.ssid()), st.flags));
    let _ = disconnect(true);
    msleep(3_000);
    t.check("autoconnect-blocked-after-disconnect", MGR.state().state == wifi_state::DISCONNECTED, String::from(wm::state_name(MGR.state().state)));
    let seq = MGR.event_seq();
    let t0 = now_ms();
    crate::wifi_api::set_profiles(1001, alloc::vec![profile("SimHome", "wrong-pass-99", 20, 0x21), profile("SimMixed", "mixed-pass-123", 10, 0x22)]);
    let _ = MGR.submit(WifiCmd::ProfilesChanged);
    let ac = wait_event(seq, t0, 20_000, |e| state_ev(e, wifi_state::CONNECTED).is_some());
    let wk = MGR.events_since(seq).iter().any(|(_, _, e)| matches!(e, WifiEvent::State(s) if s.state == wifi_state::FAILED && s.reason == reason::WRONG_KEY && s.ssid() == b"SimHome"));
    let st = MGR.state();
    t.check("autoconnect-skips-wrong-key", ac.is_some() && wk && st.ssid() == b"SimMixed",
            alloc::format!("{:?} ms, wrong-key seen {}, now '{}'", ac.map(|x| x.0), wk, wf::ssid_display(st.ssid())));
    // Link loss with a profile: reconnects by itself.
    let seq = MGR.event_seq();
    let t0 = now_ms();
    deauth(7);
    let back = wait_event(seq, t0, 10_000, |e| state_ev(e, wifi_state::CONNECTED).is_some());
    t.check("autoconnect-after-link-loss", back.is_some(), alloc::format!("{:?} ms", back.map(|x| x.0)));
    crate::wifi_api::set_profiles(1002, Vec::new());
    let _ = MGR.submit(WifiCmd::ProfilesChanged);
    let _ = disconnect(true);

    // 15. Disconnect during the handshake preempts the attempt quickly.
    let seq = MGR.event_seq();
    let _ = MGR.submit(WifiCmd::Connect(connect_params(21, "SimHome", akm::AUTO, "not-the-pass", 1)));
    let _ = wait_event(seq, now_ms(), 5_000, |e| state_ev(e, wifi_state::HANDSHAKE).is_some());
    let ms = disconnect(true);
    t.check("disconnect-preempts-attempt", ms.map_or(false, |m| m < netctl_abi::DISCONNECT_DEADLINE_MS as u64), alloc::format!("{:?} ms", ms));

    // Level 2: stay associated (autoconnect to SimHome) so the boot's
    // userland can check the net server's DHCP lease and traffic on wifi0.
    if level >= 2 {
        crate::wifi_api::set_profiles(1003, alloc::vec![profile("SimHome", "simhome-pass", 0, 0x21)]);
        let _ = MGR.submit(WifiCmd::ProfilesChanged);
        let up = wait(15_000, || MGR.state().state == wifi_state::CONNECTED);
        t.check("left-associated", up.is_some(), alloc::format!("wifi0 -> SimHome for the userland checks ({:?} ms)", up));
    }
    finish(t);
}

fn finish(t: T) {
    let n = t.pass + t.fail;
    if t.fail == 0 { slog!("selftest: PASS {}/{}", t.pass, n); } else { slog!("selftest: FAIL {} of {} checks failed", t.fail, n); }
}

/// DHCP DISCOVER over the data path, wait for an OFFER (QEMU slirp).
fn dhcp_probe() -> Result<String, String> {
    let mac = *MAC.lock();
    let mut xid = [0u8; 4];
    sched::random::fill(&mut xid);
    let mut bootp = alloc::vec![0u8; 240];
    bootp[0] = 1; bootp[1] = 1; bootp[2] = 6;
    bootp[4..8].copy_from_slice(&xid);
    bootp[10] = 0x80; // broadcast reply
    bootp[28..34].copy_from_slice(&mac);
    bootp[236..240].copy_from_slice(&[99, 130, 83, 99]);
    bootp.extend_from_slice(&[53, 1, 1, 55, 3, 1, 3, 6, 255]);
    while bootp.len() < 300 { bootp.push(0); }
    let udp_len = 8 + bootp.len();
    let ip_len = 20 + udp_len;
    let mut f = Vec::with_capacity(14 + ip_len);
    f.extend_from_slice(&[0xff; 6]);
    f.extend_from_slice(&mac);
    f.extend_from_slice(&[0x08, 0x00]);
    let mut ip = [0u8; 20];
    ip[0] = 0x45;
    ip[2..4].copy_from_slice(&(ip_len as u16).to_be_bytes());
    ip[8] = 64; ip[9] = 17;
    ip[16..20].copy_from_slice(&[255; 4]);
    let mut sum: u32 = 0;
    for i in 0..10 { sum += u16::from_be_bytes([ip[2 * i], ip[2 * i + 1]]) as u32; }
    while sum >> 16 != 0 { sum = (sum & 0xffff) + (sum >> 16); }
    ip[10..12].copy_from_slice(&(!(sum as u16)).to_be_bytes());
    f.extend_from_slice(&ip);
    f.extend_from_slice(&68u16.to_be_bytes());
    f.extend_from_slice(&67u16.to_be_bytes());
    f.extend_from_slice(&(udp_len as u16).to_be_bytes());
    f.extend_from_slice(&[0, 0]);
    f.extend_from_slice(&bootp);
    let t0 = now_ms();
    let mut next_tx = 0u64;
    let mut buf = [0u8; 1600];
    while now_ms() - t0 < 5_000 {
        if now_ms() >= next_tx {
            if !send_packet(&f) { return Err(String::from("send failed (link down?)")); }
            next_tx = now_ms() + 1_000;
        }
        while let Some(n) = raw_receive(&mut buf) {
            let p = &buf[..n];
            if n < 14 + 20 + 8 + 240 || p[12..14] != [0x08, 0x00] || p[23] != 17 { continue; }
            let ihl = ((p[14] & 0x0f) as usize) * 4;
            let u = 14 + ihl;
            if p.len() < u + 8 + 240 || p[u + 2..u + 4] != 68u16.to_be_bytes() { continue; }
            let b = &p[u + 8..];
            if b[0] != 2 || b[4..8] != xid { continue; }
            let yi = [b[16], b[17], b[18], b[19]];
            let si = [p[26], p[27], p[28], p[29]];
            return Ok(alloc::format!("OFFER {}.{}.{}.{} from {}.{}.{}.{} in {} ms", yi[0], yi[1], yi[2], yi[3], si[0], si[1], si[2], si[3], now_ms() - t0));
        }
        msleep(10);
    }
    Err(String::from("no DHCP OFFER within 5 s"))
}
