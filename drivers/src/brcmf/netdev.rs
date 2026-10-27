//! The CYW43455 as the `wifi0` netif and as a `/dev/netctl` Wi-Fi backend,
//! the same two seams the laptop's iwlwifi plugs into:
//!
//! * data path: `drivers/src/netdev.rs` registers `BRCMF_OPS` (Kind::Wifi,
//!   name `wifi0`) at kernel init when [`present`]; frames cross between the
//!   net daemon and the driver thread through two small queues here;
//! * control: the shared connection manager (`wifi_mgr::Mgr`, the
//!   `WifiBackend` netctl/NetworkManager talk to) drives [`BrcmfRadio`], our
//!   `wifi_mgr::Radio`. Unlike iwlwifi there is no mac80211 below it: scan,
//!   MLME and the 4-way handshake are the firmware's (cfg.rs).
//!
//! Life cycle: [`init`] (kernel init, rpi5 builds only) marks the device
//! present so wifi0 exists from boot, like iwlwifi before its firmware loads.
//! Once the SDIO host, chip and firmware are up, `brcmf_boot` calls [`run`]
//! on its own `brcmf` thread with the BCDC `Fwil` ([`start`] spawns one). Every failure logs one `[brcmf] ...` line and leaves
//! wifi0 down; nothing here panics or blocks boot.

use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use spin::Mutex;

use netctl_abi::{akm, reason, wifi_caps, wifi_state};

use super::cfg::{self, Brcmf, Down, Env, JoinFail, JoinObserver, JoinReq, Key, Step};
use super::fwil::Fwil;
use crate::wifi_frame as wf;
use crate::wifi_mgr::{self as wm, ConnCtx, Fail, Radio, ScanHit};

pub const CAPS: u32 = wifi_caps::PSK | wifi_caps::BAND_2GHZ | wifi_caps::BAND_5GHZ | wifi_caps::SCAN_CONNECTED
    | wifi_caps::DIRECTED_SCAN;
const SCAN_TIMEOUT_MS: u64 = 8_000;
const JOIN_TIMEOUT_MS: u64 = 20_000;
const TXQ_MAX: usize = 128;
const RXQ_MAX: usize = 256;

pub static MGR: wm::Mgr = wm::Mgr::new("brcmfmac", "brcmf", mac_address);

static PRESENT: AtomicBool = AtomicBool::new(false);
static RUNNING: AtomicBool = AtomicBool::new(false);
static MAC: Mutex<Option<[u8; 6]>> = Mutex::new(None);
static COUNTRY: Mutex<[u8; 2]> = Mutex::new(*b"PT");
static STATE: Mutex<&'static str> = Mutex::new("waiting for the chip");
static RXQ: Mutex<VecDeque<Vec<u8>>> = Mutex::new(VecDeque::new());
static TXQ: Mutex<VecDeque<Vec<u8>>> = Mutex::new(VecDeque::new());
static LINK_UP: AtomicBool = AtomicBool::new(false);
static LINK_GEN: AtomicU32 = AtomicU32::new(0);
static RATE_MBPS: AtomicU32 = AtomicU32::new(0);
static RX_N: AtomicU64 = AtomicU64::new(0);
static RX_DROP: AtomicU64 = AtomicU64::new(0);
static TX_N: AtomicU64 = AtomicU64::new(0);
static TX_DROP: AtomicU64 = AtomicU64::new(0);
static BUS_ERR: AtomicU64 = AtomicU64::new(0);

// ── observability: what /proc/net/nic shows for wifi0 ─────────────────────

/// Bring-up stage reached (brcmf_boot and the init sequence set it).
static STAGE: Mutex<&'static str> = Mutex::new("not started");
static LAST_ERR: Mutex<String> = Mutex::new(String::new());
static MAC_SRC: Mutex<String> = Mutex::new(String::new());
static FW_VER: Mutex<String> = Mutex::new(String::new());
static SCAN_INFO: Mutex<String> = Mutex::new(String::new());
static JOIN_INFO: Mutex<String> = Mutex::new(String::new());
static COUNTERS: Mutex<String> = Mutex::new(String::new());
static FW_CAPS: Mutex<String> = Mutex::new(String::new());
static JOIN_OPTS: Mutex<String> = Mutex::new(String::new());
static RSN_INFO: Mutex<Vec<(&'static str, String)>> = Mutex::new(Vec::new());
/// The last `[brcmf]` lines (boot lines included), so they can be read with
/// no serial capture and no grep.
static LOG_RING: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
const LOG_KEEP: usize = 40;
pub const CTL_POLL_MS: u64 = 1_000;

pub fn set_stage(s: &'static str) { *STAGE.lock() = s; }
pub fn set_error(e: &str) { *LAST_ERR.lock() = String::from(e); }
pub fn set_mac_src(s: &str) { *MAC_SRC.lock() = String::from(s); }
pub fn set_fw_version(s: &str) { *FW_VER.lock() = String::from(s); }

/// Every `[brcmf]` line goes here too (see [`log`] and brcmf_boot's log).
pub fn note_log(line: &str) {
    let mut r = LOG_RING.lock();
    if r.len() == LOG_KEEP { r.pop_front(); }
    let t = sched::monotonic_ns() / 1_000_000;
    r.push_back(alloc::format!("{}.{:03} {}", t / 1000, t % 1000, line.trim_end()));
}

fn log(msg: &str) {
    note_log(msg);
    extern "C" { fn serial_print_str_raw(s: *const u8, len: usize); }
    let mut s = String::with_capacity(msg.len() + 10);
    s.push_str("[brcmf] ");
    s.push_str(msg);
    s.push('\n');
    // SAFETY: the kernel's raw serial writer takes a pointer and length.
    unsafe { serial_print_str_raw(s.as_ptr(), s.len()) };
}

fn cmdline_val<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('='))).last()
}

pub fn kernel_env() -> Env {
    Env {
        now_ms: || sched::monotonic_ns() / 1_000_000,
        sleep_ms: |ms| sched::kthread::msleep(ms),
        log,
        random: |b| sched::random::fill(b),
    }
}

/// Kernel init (from `netdev::init`): decide whether wifi0 exists. Only the
/// rpi5 build has the CYW43455; everywhere else this returns at once.
pub fn init(cmdline: &str) {
    if !cfg!(feature = "rpi5") { return; }
    if cmdline_val(cmdline, "brcmf.disable") == Some("1") {
        log("disabled (brcmf.disable=1)");
        return;
    }
    *COUNTRY.lock() = cfg::country_from_cmdline(cmdline);
    crate::wifi_mgr::set_knobs(cmdline);
    MGR.set_caps(CAPS);
    PRESENT.store(true, Ordering::Release);
}

pub fn present() -> bool { PRESENT.load(Ordering::Acquire) }

/// Register the control side for the netif `drivers/src/netdev.rs` gave us.
pub fn attach(ifindex: u32, name: &str) -> bool { MGR.attach(ifindex, name) }

/// Integration entry point: spawn the `brcmf` thread around a ready `Fwil`
/// (chip powered, firmware running, BCDC up). False when not started.
pub fn start<F: Fwil + Send + 'static>(fw: F) -> bool {
    if !present() { log("start: wifi0 not registered (not an rpi5 build, or brcmf.disable=1)"); return false; }
    if sched::kthread::spawn("brcmf", move || run(fw)).is_none() {
        log("start: could not spawn the brcmf thread; wifi0 stays down");
        return false;
    }
    true
}

/// What the bring-up code (brcmf_boot) lends the running driver: the
/// `/etc/brcmf.ctl` file and the bus-level diagnostics, so the ctl stays
/// serviced while wifi0 is up.
pub trait Ctl<F> {
    /// The new command text if the file changed since the last call.
    fn poll(&mut self) -> Option<String>;
    /// `diag`: bus/chip/firmware-console dump of the running device.
    fn diag(&mut self, fw: &mut F, lines: usize);
}

/// Integration entry point on the caller's thread: init sequence, then the
/// connection manager loop. Returns only when the device is gone.
pub fn run<F: Fwil>(fw: F) { let _ = run_ctl(fw, None); }

/// [`run`] with the ctl file serviced in every state. Returns the bus and the
/// `retry ...` text when a full re-probe (WL_REG_ON cycle) was asked for.
pub fn run_ctl<F: Fwil>(fw: F, ctl: Option<Box<dyn Ctl<F>>>) -> Option<(F, String)> {
    if !present() || RUNNING.swap(true, Ordering::AcqRel) { return None; }
    let mut dev = Brcmf::new(fw, kernel_env());
    let cc = *COUNTRY.lock();
    set_stage("init (cur_etheraddr, country, event_msgs, WLC_UP)");
    let ok = match dev.init(cc) {
        Ok(mac) => {
            *MAC.lock() = Some(mac);
            LINK_GEN.fetch_add(1, Ordering::AcqRel);
            *STATE.lock() = "up";
            set_stage("up (connection manager running)");
            log(&alloc::format!("init ok: mac {} country {}{}", wm::fmt_mac(&mac), cc[0] as char, cc[1] as char));
            true
        }
        Err(e) => {
            *STATE.lock() = "init failed";
            set_stage("init failed");
            set_error(&alloc::format!("init: {:?}", e));
            log(&alloc::format!("init failed: {:?}; wifi0 stays down (brcmfctl diag|retry still work)", e));
            false
        }
    };
    let mut r = BrcmfRadio { dev, bus_errs: 0, ctl, next_ctl_ms: 0, retry: None, retry_phase: 0 };
    if ok {
        // Same as iwlwifi.mgr=1: the legacy network is the autoconnect profile
        // until netctl publishes the stored ones (which then include it).
        MGR.set_fallback_profiles(legacy_fallback());
        wm::run(&MGR, &mut r);
        *STATE.lock() = "stopped";
        log("manager loop ended");
    } else if r.ctl.is_some() {
        // Park, but keep answering the ctl until a retry.
        while r.retry.is_none() {
            r.check_ctl();
            sched::kthread::msleep(CTL_POLL_MS);
        }
    }
    set_link(false);
    RUNNING.store(false, Ordering::Release);
    let BrcmfRadio { dev, retry, .. } = r;
    retry.map(|t| { set_stage("retry requested"); (dev.into_fw(), t) })
}

const WIFI_CONF: &str = "/etc/leandros/wifi.conf";

/// /etc/leandros/wifi.conf (`ssid=` / `psk=`) as the fallback profile list.
/// Logs the SSID only; the file buffer is wiped after parsing.
fn legacy_fallback() -> Vec<netctl_abi::profiles::Profile> {
    let mut v = match crate::firmware::request_firmware(WIFI_CONF, 64 * 1024).ok().and_then(|f| f.to_vec()) {
        Some(v) => v,
        None => { log("no /etc/leandros/wifi.conf: autoconnect waits for netctl profiles"); return Vec::new(); }
    };
    let out = match netctl_abi::profiles::parse_legacy_wifi_conf(&v) {
        Some((ssid, psk)) => {
            let name = wf::ssid_display(ssid);
            match netctl_abi::profiles::legacy_profile(ssid, psk) {
                Some(p) => {
                    log(&alloc::format!("credentials from {}: SSID '{}' (passphrase not logged)", WIFI_CONF, name));
                    alloc::vec![p]
                }
                None => { log("wifi.conf: passphrase must be 8..63 characters (or 64 hex digits); not connecting"); Vec::new() }
            }
        }
        None => { log("wifi.conf: no usable ssid= line"); Vec::new() }
    };
    for b in v.iter_mut() { unsafe { core::ptr::write_volatile(b, 0) }; }
    out
}

// ── data path (NetifOps) ───────────────────────────────────────────────────

pub fn send_packet(frame: &[u8]) -> bool {
    if !LINK_UP.load(Ordering::Acquire) { TX_DROP.fetch_add(1, Ordering::Relaxed); return false; }
    let mut q = TXQ.lock();
    if q.len() >= TXQ_MAX { TX_DROP.fetch_add(1, Ordering::Relaxed); return false; }
    q.push_back(frame.to_vec());
    true
}
pub fn tx_ready() -> bool { TXQ.lock().len() < TXQ_MAX * 3 / 4 }
pub fn poll_receive(buf: &mut [u8]) -> Option<usize> {
    let f = RXQ.lock().pop_front()?;
    let n = f.len().min(buf.len());
    buf[..n].copy_from_slice(&f[..n]);
    Some(n)
}
pub fn rx_rearm() -> bool { !RXQ.lock().is_empty() }
pub fn rx_irq_driven() -> bool { RUNNING.load(Ordering::Acquire) }
pub fn mac_address() -> Option<[u8; 6]> { *MAC.lock() }
pub fn link_up() -> bool { LINK_UP.load(Ordering::Acquire) }
pub fn link_speed() -> u32 { RATE_MBPS.load(Ordering::Relaxed) }
pub fn link_generation() -> u32 { LINK_GEN.load(Ordering::Acquire) }

pub fn info_lines(out: &mut String) {
    use core::fmt::Write;
    let _ = writeln!(out, "driver: brcmfmac (CYW43455 FullMAC)");
    if let Some(m) = mac_address() { let _ = writeln!(out, "mac: {}", wm::fmt_mac(&m)); }
    let cc = *COUNTRY.lock();
    let _ = writeln!(out, "state: {}", *STATE.lock());
    let _ = writeln!(out, "country: {}{}", cc[0] as char, cc[1] as char);
    let _ = writeln!(out, "link: {}", if link_up() { "up" } else { "down" });
    let _ = writeln!(out, "rx: {} frames, {} dropped", RX_N.load(Ordering::Relaxed), RX_DROP.load(Ordering::Relaxed));
    let _ = writeln!(out, "tx: {} frames, {} dropped", TX_N.load(Ordering::Relaxed), TX_DROP.load(Ordering::Relaxed));
    let _ = writeln!(out, "bus errors: {}", BUS_ERR.load(Ordering::Relaxed));
    let _ = writeln!(out, "stage: {}", *STAGE.lock());
    let e = LAST_ERR.lock();
    let _ = writeln!(out, "last_error: {}", if e.is_empty() { "none" } else { e.as_str() });
    drop(e);
    let v = FW_VER.lock();
    let _ = writeln!(out, "firmware: {}", if v.is_empty() { "not running" } else { v.as_str() });
    drop(v);
    let ms = MAC_SRC.lock();
    let _ = writeln!(out, "mac_source: {}", if ms.is_empty() { "unknown (not reached)" } else { ms.as_str() });
    drop(ms);
    for (k, m) in [("caps", &FW_CAPS), ("join_opts", &JOIN_OPTS), ("last_scan", &SCAN_INFO), ("last_join", &JOIN_INFO), ("counters", &COUNTERS)] {
        let g = m.lock();
        let _ = writeln!(out, "{}: {}", k, if g.is_empty() { "none" } else { g.as_str() });
    }
    for (k, v) in RSN_INFO.lock().iter() {
        let _ = writeln!(out, "{}: {}", k, if v.is_empty() { "none" } else { v.as_str() });
    }
    let _ = writeln!(out, "ctl: /etc/brcmf.ctl (brcmfctl status|scan|join [method=auto|joiniovar|setssid supwpa=auto|1|0 pmk=auto|raw|hex|passphrase sec=auto|ioctl|iovar mfp=auto|0|1|2 wpaie=1|0 akm=auto|2|6 mfpc=auto|0|1 mfpr=auto|0|1 msg2ie=auto|ours|fw reset] [ssid=..]|diag|retry)");
    if RUNNING.load(Ordering::Acquire) { MGR.info_lines(out); }
    let _ = writeln!(out, "log (last {} [brcmf] lines):", LOG_KEEP);
    for l in LOG_RING.lock().iter() { let _ = writeln!(out, "  {}", l); }
}

fn set_link(up: bool) {
    let was = LINK_UP.swap(up, Ordering::AcqRel);
    if up && !was { LINK_GEN.fetch_add(1, Ordering::AcqRel); }
    if !up {
        TXQ.lock().clear();
        RATE_MBPS.store(0, Ordering::Relaxed);
    }
    crate::netdev::kick();
}

// ── the Radio ──────────────────────────────────────────────────────────────

pub struct BrcmfRadio<F: Fwil> {
    pub dev: Brcmf<F>,
    bus_errs: u32,
    ctl: Option<Box<dyn Ctl<F>>>,
    next_ctl_ms: u64,
    /// `retry ...` text: the manager loop is wound down and the bus handed back.
    retry: Option<String>,
    /// 1: report a kill switch once; 2: report it released and refuse to
    /// come back, which is how `wm::run` returns.
    retry_phase: u8,
}

struct Obs<'a, 'b> { cx: Option<&'a ConnCtx<'b>> }

impl JoinObserver for Obs<'_, '_> {
    fn aborted(&self) -> bool { self.cx.map_or(false, |c| c.aborted()) }
    fn step(&mut self, s: Step) {
        if let Some(c) = self.cx {
            c.step(match s { Step::Associating => wifi_state::ASSOCIATING, Step::Handshake => wifi_state::HANDSHAKE });
        }
    }
}

pub fn join_fail(e: JoinFail) -> Fail {
    use super::event::status as st;
    match e {
        JoinFail::Fw(e) => Fail::new(reason::FW_ERROR, alloc::format!("firmware error during join: {:?}", e)),
        JoinFail::NoNetwork => Fail::new(reason::NO_AP, "the firmware did not find the network"),
        JoinFail::Assoc { status, .. } if status == st::TIMEOUT || status == st::NO_ACK =>
            Fail::new(reason::TIMEOUT, alloc::format!("no answer from the AP (event status {})", status)),
        JoinFail::Assoc { status, reason: r } =>
            Fail::new(reason::ASSOC_REJECTED, alloc::format!("association failed (event status {}, reason {})", status, r))
                .status(r as u16),
        JoinFail::Handshake { msg2, reason: r } => wm::handshake_failure(msg2, r),
        JoinFail::Timeout => Fail::new(reason::TIMEOUT, "join timed out"),
        JoinFail::Aborted => Fail::superseded(),
    }
}

pub fn down_fail(d: Down) -> Fail {
    match d {
        Down::Deauth { reason: r, by_ap } | Down::Disassoc { reason: r, by_ap } =>
            Fail::new(reason::DEAUTH_BY_AP, alloc::format!("{} (reason {})", if by_ap { "deauthenticated by the AP" } else { "left by the firmware" }, r))
                .ieee_reason(r),
        Down::LinkLost { .. } => Fail::new(reason::BEACON_LOSS, "link lost (beacons)"),
        Down::SupFailed { status, reason: r } =>
            Fail::new(reason::FW_ERROR, alloc::format!("firmware supplicant failed (status {}, reason {})", status, r)),
        Down::JoinFailed { status, .. } => Fail::new(reason::TIMEOUT, alloc::format!("join failed (status {})", status)),
    }
}

fn now_ms() -> u64 { sched::monotonic_ns() / 1_000_000 }

impl<F: Fwil> BrcmfRadio<F> {
    /// Services `/etc/brcmf.ctl` (about once a second) and refreshes the
    /// counters line.
    fn check_ctl(&mut self) {
        let now = now_ms();
        if now < self.next_ctl_ms { return; }
        self.next_ctl_ms = now + CTL_POLL_MS;
        *COUNTERS.lock() = self.dev.counters_line();
        *FW_CAPS.lock() = self.dev.caps.clone().unwrap_or_else(|| String::from("unknown (cap refused)"));
        *JOIN_OPTS.lock() = self.dev.opts.describe();
        *RSN_INFO.lock() = self.dev.rsn.lines().iter().map(|(k, v)| (*k, v.clone())).collect();
        let Some(text) = self.ctl.as_mut().and_then(|c| c.poll()) else { return };
        let text = String::from(text.trim());
        match cfg::parse_ctl(&text) {
            cfg::CtlCmd::Status => {
                let mut s = String::new();
                info_lines(&mut s);
                for l in s.lines().filter(|l| !l.starts_with("  ") && !l.starts_with("log ")) { log(&alloc::format!("ctl: {}", l)); }
            }
            cfg::CtlCmd::Scan => {
                if !self.dev.radio_on() { log("ctl: scan: radio is off / init failed"); return; }
                match Radio::scan(self, &[], false) {
                    Ok(hits) => {
                        log(&alloc::format!("ctl: scan: {} BSS", hits.len()));
                        for h in hits.iter().take(12) {
                            log(&alloc::format!("ctl:   {} ch {} {} dBm '{}'", wm::fmt_mac(&h.bss.bssid), h.bss.channel, h.rssi,
                                                wf::ssid_display(&h.bss.ssid)));
                        }
                    }
                    Err(f) => log(&alloc::format!("ctl: scan failed: {}", f.msg)),
                }
            }
            cfg::CtlCmd::Join { ssid, knobs } => {
                let bad = self.dev.opts.apply(&knobs);
                if !bad.is_empty() { log(&alloc::format!("ctl: join: unknown knob(s) {:?} ignored", bad)); }
                *JOIN_OPTS.lock() = self.dev.opts.describe();
        *RSN_INFO.lock() = self.dev.rsn.lines().iter().map(|(k, v)| (*k, v.clone())).collect();
                log(&alloc::format!("ctl: join opts: {}", self.dev.opts.describe()));
                self.ctl_join(ssid)
            }
            cfg::CtlCmd::Diag(lines) => {
                log(&alloc::format!("ctl: diag: {}", self.dev.counters_line()));
                if let Some(c) = self.ctl.as_mut() { c.diag(&mut self.dev.fw, lines); }
            }
            cfg::CtlCmd::Retry(_) => {
                log("ctl: retry: stopping wifi0 and re-probing the chip");
                self.retry = Some(text);
                self.retry_phase = 1;
            }
            cfg::CtlCmd::Other(c) => log(&alloc::format!("ctl: '{}' is not available while the driver runs (status|scan|join|diag|retry)", c)),
        }
    }

    /// `join [ssid=..]`: a direct scan + firmware join with the wifi.conf
    /// credentials, outside the manager, logging exactly where it stops. On
    /// success the link is dropped again and autoconnect takes over.
    fn ctl_join(&mut self, want: Option<&str>) {
        if !self.dev.radio_on() { log("ctl: join: radio is off / init failed"); return; }
        let profiles = legacy_fallback();
        let p = match want {
            Some(w) => profiles.iter().find(|p| p.ssid() == w.as_bytes()),
            None => profiles.first(),
        };
        let Some(p) = p else { log("ctl: join: no matching network in /etc/leandros/wifi.conf"); return; };
        let ssid = p.ssid().to_vec();
        let name = wf::ssid_display(&ssid);
        let hits = match Radio::scan(self, &[ssid.clone()], false) {
            Ok(h) => h,
            Err(f) => { log(&alloc::format!("ctl: join '{}': scan failed: {}", name, f.msg)); return; }
        };
        let Some(best) = hits.iter().filter(|h| h.bss.ssid == ssid).max_by_key(|h| h.rssi) else {
            log(&alloc::format!("ctl: join '{}': not in the scan ({} other BSS)", name, hits.len()));
            *JOIN_INFO.lock() = alloc::format!("ctl join '{}': not found in scan", name);
            return;
        };
        let key = if p.secret().is_empty() { Key::Open } else { Key::from_secret(p.secret()) };
        let req = JoinReq { ssid: ssid.clone(), bssid: Some(best.bss.bssid), channel: best.bss.channel, key };
        log(&alloc::format!("ctl: join '{}' via {} ch {} {} dBm", name, wm::fmt_mac(&best.bss.bssid), best.bss.channel, best.rssi));
        let r = self.dev.join(req, &mut Obs { cx: None }, JOIN_TIMEOUT_MS);
        let line = match &r {
            Ok(()) => alloc::format!("ctl join '{}': OK (rate {} kbit/s); left again, autoconnect takes over", name, self.dev.rate_kbps()),
            Err(e @ JoinFail::Fw(_)) => alloc::format!("ctl join '{}': FAILED {:?} at '{}' [{}] -> {}", name, e, self.dev.fail_cmd,
                                                       self.dev.opts.describe(), join_fail(*e).msg),
            Err(e) => alloc::format!("ctl join '{}': FAILED {:?} (handshake: {}) [{}] -> {}", name, e, self.dev.sup_used,
                                     self.dev.opts.describe(), join_fail(*e).msg),
        };
        log(&line);
        *JOIN_INFO.lock() = line;
        if r.is_ok() { let _ = self.dev.disconnect(); }
    }

    /// Move frames both ways and service the bus for about `ms`. Returns a
    /// link-down cause while connected.
    fn service(&mut self, ms: u64) -> Option<Fail> {
        let t0 = sched::monotonic_ns() / 1_000_000;
        loop {
            self.check_ctl();
            if self.retry.is_some() { return None; }
            while let Some(f) = { let x = TXQ.lock().pop_front(); x } {
                if self.dev.tx(&f).is_ok() { TX_N.fetch_add(1, Ordering::Relaxed); } else { TX_DROP.fetch_add(1, Ordering::Relaxed); }
            }
            let before = self.dev.rx.events + self.dev.rx.rx_frames;
            match self.dev.poll_link(2_000) {
                Ok(Some(d)) => { set_link(false); return Some(down_fail(d)); }
                Ok(None) => self.bus_errs = 0,
                Err(e) => {
                    BUS_ERR.fetch_add(1, Ordering::Relaxed);
                    self.bus_errs += 1;
                    if self.bus_errs == 1 { log(&alloc::format!("bus error: {:?}", e)); }
                    if self.bus_errs >= 50 && self.dev.connected() {
                        set_link(false);
                        return Some(Fail::fw_assert(alloc::format!("bus failing: {:?}", e)));
                    }
                }
            }
            let mut got = false;
            while let Some(f) = self.dev.take_rx() {
                if !link_up() { continue; }
                RX_N.fetch_add(1, Ordering::Relaxed);
                let mut q = RXQ.lock();
                if q.len() >= RXQ_MAX { q.pop_front(); RX_DROP.fetch_add(1, Ordering::Relaxed); }
                q.push_back(f);
                got = true;
            }
            if got { crate::netdev::kick(); }
            let now = sched::monotonic_ns() / 1_000_000;
            if now.saturating_sub(t0) >= ms { return None; }
            if self.dev.rx.events + self.dev.rx.rx_frames == before && TXQ.lock().is_empty() {
                sched::kthread::msleep(1);
            }
        }
    }
}

impl<F: Fwil> Radio for BrcmfRadio<F> {
    fn caps(&self) -> u32 { CAPS }

    fn scan(&mut self, ssids: &[Vec<u8>], _connected: bool) -> Result<Vec<ScanHit>, Fail> {
        let list: Vec<&[u8]> = ssids.iter().map(|s| s.as_slice()).collect();
        let res = self.dev.scan(&list, SCAN_TIMEOUT_MS, &mut Obs { cx: None })
            .map_err(|e| Fail::new(reason::FW_ERROR, alloc::format!("escan failed: {:?}", e)))?;
        let raw = res.len();
        let mut hits = Vec::with_capacity(res.len());
        for e in res {
            if let Some(mut b) = wf::parse_beacon(&e.beacon_frame(), e.channel) {
                if b.ssid.is_empty() { b.ssid = e.ssid.clone(); }
                b.rssi = e.rssi;
                hits.push(ScanHit { bss: b, rssi: e.rssi });
            }
        }
        let best = hits.iter().max_by_key(|h| h.rssi)
            .map(|h| alloc::format!("; best '{}' ch {} {} dBm", wf::ssid_display(&h.bss.ssid), h.bss.channel, h.rssi))
            .unwrap_or_default();
        *SCAN_INFO.lock() = alloc::format!("at {} s: {} BSS from the firmware, {} parsed{}", now_ms() / 1000, raw, hits.len(), best);
        Ok(hits)
    }

    fn connect(&mut self, hit: &ScanHit, ssid: &[u8], akm_: u8, secret: &[u8], cx: &ConnCtx) -> Result<(), Fail> {
        if akm_ != akm::NONE && akm_ != akm::PSK {
            return Err(Fail::new(reason::UNSUPPORTED_SECURITY, "brcmf joins open and WPA2-PSK networks only"));
        }
        if akm_ == akm::PSK && secret.is_empty() { return Err(Fail::new(reason::NO_SECRET, "WPA2-PSK needs a passphrase")); }
        let key = if akm_ == akm::NONE { Key::Open } else { Key::from_secret(secret) };
        let req = JoinReq { ssid: ssid.to_vec(), bssid: Some(hit.bss.bssid), channel: hit.bss.channel, key };
        let timeout = cx.left_ms().clamp(1_000, JOIN_TIMEOUT_MS);
        let name = wf::ssid_display(ssid);
        if let Err(e) = self.dev.join(req, &mut Obs { cx: Some(cx) }, timeout) {
            let f = join_fail(e);
            *JOIN_INFO.lock() = alloc::format!("at {} s: '{}' via {} FAILED {:?}: {}", now_ms() / 1000, name, wm::fmt_mac(&hit.bss.bssid), e, f.msg);
            log(&alloc::format!("join '{}' failed: {:?}: {}", name, e, f.msg));
            return Err(f);
        }
        *JOIN_INFO.lock() = alloc::format!("at {} s: '{}' via {} associated", now_ms() / 1000, name, wm::fmt_mac(&hit.bss.bssid));
        RATE_MBPS.store(self.dev.rate_kbps() / 1000, Ordering::Relaxed);
        set_link(true);
        log(&alloc::format!("associated with {} on channel {}", wm::fmt_mac(&hit.bss.bssid), hit.bss.channel));
        Ok(())
    }

    fn poll_connected(&mut self, budget_ms: u64) -> Option<Fail> { self.service(budget_ms) }

    fn leave(&mut self, _deauth: bool) {
        set_link(false);
        // Disassociates only while the firmware still has a link (a no-op
        // after the AP already dropped us).
        if let Err(e) = self.dev.disconnect() { log(&alloc::format!("disassoc failed: {:?}", e)); }
    }

    fn settle(&mut self, _after: Option<&Fail>) -> bool {
        RXQ.lock().clear();
        true
    }

    fn idle(&mut self, ms: u64) { let _ = self.service(ms); }

    // `retry` while up: one kill-switch pulse that cannot be released makes
    // `wm::run` return (state UNAVAILABLE); run_ctl then hands the bus back.
    fn hw_rfkill(&mut self) -> bool {
        self.check_ctl();
        match self.retry_phase {
            1 => { self.retry_phase = 2; true }
            _ => false,
        }
    }

    fn rfkill_released(&mut self) -> bool {
        if self.retry_phase == 2 { return false; }
        self.settle(None)
    }

    fn set_radio(&mut self, on: bool) -> bool {
        if !on { set_link(false); }
        self.dev.set_radio(on).is_ok()
    }

    fn signal(&mut self) -> Option<i8> { self.dev.rssi() }

    fn tx_rate_kbps(&mut self) -> u32 {
        let k = self.dev.rate_kbps();
        RATE_MBPS.store(k / 1000, Ordering::Relaxed);
        k
    }
}
