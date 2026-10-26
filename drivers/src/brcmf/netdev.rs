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
//! Once the SDIO host, chip and firmware are up, the integration code calls
//! [`start`] (spawns the `brcmf` thread) or [`run`] (on its own thread) with
//! the BCDC `Fwil`. Every failure logs one `[brcmf] ...` line and leaves
//! wifi0 down; nothing here panics or blocks boot.

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

fn log(msg: &str) {
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

/// Integration entry point on the caller's thread: init sequence, then the
/// connection manager loop. Returns only when the device is gone.
pub fn run<F: Fwil>(fw: F) {
    if !present() || RUNNING.swap(true, Ordering::AcqRel) { return; }
    let mut dev = Brcmf::new(fw, kernel_env());
    let cc = *COUNTRY.lock();
    match dev.init(cc) {
        Ok(mac) => {
            *MAC.lock() = Some(mac);
            LINK_GEN.fetch_add(1, Ordering::AcqRel);
            *STATE.lock() = "up";
            log(&alloc::format!("init ok: mac {} country {}{}", wm::fmt_mac(&mac), cc[0] as char, cc[1] as char));
        }
        Err(e) => {
            *STATE.lock() = "init failed";
            log(&alloc::format!("init failed: {:?}; wifi0 stays down", e));
            RUNNING.store(false, Ordering::Release);
            return;
        }
    }
    let mut r = BrcmfRadio { dev, bus_errs: 0 };
    wm::run(&MGR, &mut r);
    *STATE.lock() = "stopped";
    log("manager loop ended");
    RUNNING.store(false, Ordering::Release);
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

impl<F: Fwil> BrcmfRadio<F> {
    /// Move frames both ways and service the bus for about `ms`. Returns a
    /// link-down cause while connected.
    fn service(&mut self, ms: u64) -> Option<Fail> {
        let t0 = sched::monotonic_ns() / 1_000_000;
        loop {
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
        let mut hits = Vec::with_capacity(res.len());
        for e in res {
            if let Some(mut b) = wf::parse_beacon(&e.beacon_frame(), e.channel) {
                if b.ssid.is_empty() { b.ssid = e.ssid.clone(); }
                b.rssi = e.rssi;
                hits.push(ScanHit { bss: b, rssi: e.rssi });
            }
        }
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
        self.dev.join(req, &mut Obs { cx: Some(cx) }, timeout).map_err(join_fail)?;
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
