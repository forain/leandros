//! Intel Wireless 8265 / 8275 (PCI 8086:24fd, "Windstorm Peak") Wi-Fi, MVM
//! firmware, as a second `eth0` backend behind `netdev`.
//!
//! Layers (Linux iwlwifi is the behavioural reference, consulted as
//! documentation): `iwl_trans` is the PCIe transport (registers, APM, firmware
//! download, rings, host commands); `iwl_fw` parses the `.ucode` TLV file;
//! `iwl_cmd` encodes the MVM commands and decodes notifications; this file is
//! the op mode: firmware bring-up, scanning, association + WPA2-PSK
//! (`wifi_frame`, `wifi_crypto`) and the data path.
//!
//! Shape:
//! * `init` (kernel init, boot context, quick): PCI enable, BAR0, take the NIC
//!   from the ME (NIC_READY), power it up (APM), log the hardware revision,
//!   RF id, the hardware RF-kill switch and the OTP MAC address, then power it
//!   back down. With `iwlwifi.start=1` it starts the `[iwlwifi]` thread.
//! * The `[iwlwifi]` thread: waits for the root filesystem, loads
//!   /lib/firmware/iwlwifi-8265-36.ucode, runs the INIT image (NVM read:
//!   MAC address, antennas, bands, channels; calibrations), the runtime image
//!   (paging, PHY DB, BT coex, DQA, aux station, PHY context, regulatory MCC,
//!   scan config), adds the station MAC context and scans; with credentials
//!   (/etc/leandros/wifi.conf or `wifi.ssid=`/`wifi.psk=`) it associates,
//!   runs the WPA2 4-way handshake, installs the keys and opens the data path.
//! * Everything is polled (no interrupt is delivered): the thread at 1 ms
//!   while it waits for the firmware, the net daemon through `poll_receive`.
//!
//! A failure at any step is logged with the registers and the firmware error
//! tables and leaves the NIC down; boot never waits for this driver.
//!
//! Kernel command line:
//!   iwlwifi.disable=1       do not touch the device at all
//!   iwlwifi.start=1         bring the firmware up (default 0: probe only)
//!   iwlwifi.fw=<path>       firmware file (default /lib/firmware/iwlwifi-8265-36.ucode)
//!   iwlwifi.scan=0          do not scan (bring-up only)
//!   iwlwifi.connect=0       scan but never associate
//!   iwlwifi.debug=1         log every firmware notification
//!   iwlwifi.csumcheck=1     verify IP/TCP/UDP checksums of received frames
//!                           (counters in the wlan stats line and /proc/net/nic;
//!                           the first 20 failures are logged)
//!   wifi.ssid=, wifi.psk=   credentials (percent-escapes allowed, e.g. %20);
//!                           /etc/leandros/wifi.conf (`ssid=` / `psk=` lines) wins
//!   iwlwifi.mgr=1           command-driven connection manager (`wifi_mgr`):
//!                           scan / connect / disconnect / radio from
//!                           /dev/netctl, autoconnect from the netctl
//!                           profiles (the wifi.conf / wifi.ssid= network
//!                           until netctl publishes any), AP list with aging,
//!                           failure reasons. Implies iwlwifi.start=1.
//!                           Default 0: the fixed scan/connect loop below.
//!   iwlwifi.softdisc=0      (mgr=1) leave a network by restarting the
//!                           firmware instead of the iwlmvm-style teardown
//!                           (flush, unassociated MAC context, REMOVE_STA,
//!                           unbind); the default tears down and falls back to
//!                           a restart on any error
//!   iwlwifi.scanconn=1      (mgr=1) scan while associated (UMAC scan on the
//!                           associated MAC, as iwlmvm does); default 0: scan
//!                           requests while associated are answered from the
//!                           cache
//!   iwlwifi.rc=0            no rate control: the fixed LQ table (every AP
//!                           rate fastest first, two tries each, antenna A);
//!                           default 1: `iwl_rs` (Linux rs.c, legacy rates)
//!   iwlwifi.txant=a|b       (rc=1) transmit on that antenna only; default:
//!                           A, toggled to B and back after failure runs
//!   iwlwifi.maxrate=<Mb/s>  cap the fastest data rate (e.g. 24, 5.5)
//!   iwlwifi.btcoex=wifi     BT_CONFIG mode BT_COEX_WIFI (Linux
//!                           bt_coex_active=0): Wi-Fi keeps the shared
//!                           antenna; default: mode NW (firmware arbitrates)
//!
//! TX statistics: the 60 s `wlan stats` / `wlan tx 60 s` / `wlan tx state`
//! lines (deltas), the full counters under the scan table in /proc/net/wifi
//! and the `tx_*` lines of /proc/net/nic.

#![allow(dead_code)]

use alloc::collections::VecDeque;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU8, Ordering};
use spin::Mutex;

use crate::iwl_assoc::{self as asc, MacState, TxParams};
use crate::iwl_cmd as cmd;
use crate::iwl_fw::{self, FwInfo};
use crate::iwl_rs::{self as rs, EnqFail, RateCtl, TxStats};
use crate::iwl_trans::{self as tr, ilog, log, msleep, now_ms, Dma, FwChunk, Mmio, RxPacket, Trans, TransError};
use crate::pci::{self, PciFunction};
use crate::wifi_frame as wf;
use crate::wifi_mgr::{self as wm, Fail};
use netctl_abi::{reason, wifi_caps, wifi_state};

const INTEL: u16 = 0x8086;
/// (device id, name, firmware file).
const IDS: &[(u16, &str, &str)] = &[
    (0x24fd, "Intel Wireless 8265/8275", "iwlwifi-8265-36.ucode"),
    (0x24f3, "Intel Wireless 8260", "iwlwifi-8000C-36.ucode"),
    (0x24f4, "Intel Wireless 8260", "iwlwifi-8000C-36.ucode"),
];

/// Data queue of the AP station (DQA "BSS client" queue).
const BSS_QUEUE: usize = cmd::DQA_BSS_CLIENT_QUEUE as usize;
/// Management frames (auth, assoc) to the AP station: the first DQA
/// management queue, on the VO FIFO with the management TID.
const MGMT_QUEUE: usize = 5;
const AUX_QUEUE: usize = cmd::DQA_AUX_QUEUE as usize;
const MAC_ID: u32 = 0;
const PHY_ID: u32 = 0;
const AUX_STA_ID: u8 = 0;
const AP_STA_ID: u8 = 1;

#[derive(Clone)]
struct Opts {
    start: bool,
    scan: bool,
    connect: bool,
    debug: bool,
    csumcheck: bool,
    /// Data frames through the LQ rate table (iwlwifi.lq=0: fixed 12 Mb/s).
    lq: bool,
    fw: Option<String>,
    ssid: Option<Vec<u8>>,
    psk: Option<Vec<u8>>,
    /// iwlwifi.mgr=1: the netctl-driven connection manager.
    mgr: bool,
    /// iwlwifi.softdisc (default 1): leave without a firmware restart.
    softdisc: bool,
    /// iwlwifi.scanconn=1: scan while associated.
    scanconn: bool,
}

/// Data-queue depth (frames in the BSS ring) above which `tx_ready` says no:
/// the net server holds its frames instead of piling up seconds of queue.
const TXQ_HIGH: u32 = 64;
/// Frames past this are refused outright (a reply the stack sends while
/// receiving does not ask `tx_ready` first).
const TXQ_CAP: usize = 96;
/// Once a held queue drains below this, the net daemon is kicked.
const TXQ_LOW: usize = 32;
/// Data-queue failures logged one by one per association.
const FAIL_LOG_MAX: u32 = 20;

/// Frames in the BSS data ring (updated on every enqueue and TX response).
static TXQ_DEPTH: AtomicU32 = AtomicU32::new(0);
/// `tx_ready` said no since the last drain, and how often that started.
static TX_HELD: AtomicBool = AtomicBool::new(false);
static TX_HOLDS: AtomicU32 = AtomicU32::new(0);
/// iwlwifi.rc (default on).
static RC_ON: AtomicBool = AtomicBool::new(true);
/// iwlwifi.txant: 0 automatic, else the antenna mask.
static TXANT: AtomicU8 = AtomicU8::new(0);
/// iwlwifi.maxrate in 500 kb/s units (0 = no cap).
static MAXRATE: AtomicU8 = AtomicU8::new(0);
/// iwlwifi.btcoex=wifi.
static BTCOEX_WIFI: AtomicBool = AtomicBool::new(false);

/// `<Mb/s>` (integer or with a .5) to 500 kb/s units.
fn parse_mbps(v: &str) -> Option<u8> {
    let (int, half) = match v.split_once('.') { Some((i, f)) => (i, f.starts_with('5')), None => (v, false) };
    let n: u32 = int.parse().ok()?;
    let r = n * 2 + half as u32;
    if r == 0 || r > 255 { None } else { Some(r as u8) }
}

fn cmdline_val<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_ascii_whitespace().filter_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('='))).last()
}

/// `%XX` escapes (so a cmdline value can carry spaces).
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

// ── shared state ────────────────────────────────────────────────────────────

static PRESENT: AtomicBool = AtomicBool::new(false);
/// Data path open (associated, keys installed).
static LINK_UP: AtomicBool = AtomicBool::new(false);
static LINK_GEN: AtomicU32 = AtomicU32::new(0);
static MAC: Mutex<[u8; 6]> = Mutex::new([0; 6]);
static DEV: Mutex<Option<Dev>> = Mutex::new(None);
static INFO: Mutex<Option<Info>> = Mutex::new(None);
/// iwlwifi.mgr=1 was given (the netctl seam is served).
static MGR_ON: AtomicBool = AtomicBool::new(false);

/// The connection manager of this interface (iwlwifi.mgr=1).
pub static MGR: wm::Mgr = wm::Mgr::new("iwlwifi", "iwlwifi", mac_address);

/// What /proc/net/nic and /proc/net/wifi show.
#[derive(Clone, Default)]
struct Info {
    name: &'static str,
    bdf: (u8, u8, u8),
    hw_rev: u32,
    rf_id: u32,
    fw: String,
    state: String,
    rfkill: bool,
    mcc: [u8; 2],
    bss: Vec<ScanEntry>,
    scan_at_ms: u64,
    ssid: Vec<u8>,
    bssid: [u8; 6],
    channel: u8,
    rssi: i8,
}

#[derive(Clone)]
struct ScanEntry {
    bss: wf::Bss,
    rssi: i8,
    seen_ms: u64,
}

fn set_state(s: &str) {
    if let Some(i) = INFO.lock().as_mut() { i.state = String::from(s); }
}

/// The device as the driver thread and the data path share it.
struct Dev {
    t: Trans,
    func: PciFunction,
    debug: bool,
    /// Command responses not yet claimed (by sequence number).
    resps: Vec<RxPacket>,
    /// Notifications for the driver thread.
    notifs: VecDeque<RxPacket>,
    last_phy: Option<cmd::PhyInfo>,
    /// Beacons / probe responses gathered while a scan runs.
    scanning: bool,
    scan_results: Vec<ScanEntry>,
    // Data path (W3).
    rx_eth: VecDeque<Vec<u8>>,
    rx_frames: u64,
    rx_dropped: u64,
    /// TX counters and histograms; `txs_mark` is the copy at the last
    /// 60 s stats line (the deltas).
    txs: TxStats,
    txs_mark: TxStats,
    /// Rate control of the AP station (iwlwifi.rc=1, associated).
    rc: Option<RateCtl>,
    /// Sequence numbers of commands sent without waiting (LQ_CMD from the
    /// data path): their responses are dropped.
    async_cmds: Vec<u16>,
    /// now_ms() of the last TX response on the data / management queue.
    last_txrsp_ms: [u64; 2],
    /// Data-queue failures logged this association.
    fail_logged: u32,
    /// ERP flags of the tracked BSS's last beacon, and whether they changed.
    erp: Option<u8>,
    erp_changed: bool,
    notif_dropped: u64,
    /// Receive filtering of the current association (duplicates, replays).
    guard: Option<wf::RxGuard>,
    rxs: RxStats,
    /// iwlwifi.csumcheck=1: verify IP/TCP/UDP checksums of received frames.
    csumcheck: bool,
    /// The association the data path transmits on (keys once installed).
    link: Option<LinkTx>,
    /// BSSID whose beacons are timed (for the DTIM fields of the MAC context).
    track: Option<[u8; 6]>,
    bcn: Option<BeaconSync>,
    /// Signal of the tracked BSS's last beacon.
    bcn_rssi: Option<i8>,
}

/// Received data frames the driver did not hand to the stack, by reason,
/// plus the iwlwifi.csumcheck counters.
#[derive(Clone, Copy, Default)]
struct RxStats {
    undecrypted: u64,
    foreign: u64,
    echo: u64,
    fragment: u64,
    duplicate: u64,
    replay: u64,
    malformed: u64,
    /// Not convertible to Ethernet (no LLC/SNAP, truncated).
    unconvertible: u64,
    amsdu: u64,
    csum_ok: u64,
    csum_bad: u64,
}

impl RxStats {
    fn line(&self) -> String {
        format!("undecrypted {} dup {} replay {} foreign {} echo {} frag {} malformed {} unconvertible {} amsdu {} csum ok/bad {}/{}",
                self.undecrypted, self.duplicate, self.replay, self.foreign, self.echo, self.fragment, self.malformed,
                self.unconvertible, self.amsdu, self.csum_ok, self.csum_bad)
    }
}

/// What `send_packet` needs to build frames for the AP.
#[derive(Clone)]
struct LinkTx {
    bssid: [u8; 6],
    own: [u8; 6],
    /// Pairwise CCMP key once the 4-way handshake installed it.
    tk: Option<[u8; 16]>,
    /// Next CCMP packet number.
    pn: u64,
    /// Data frames use the station's LQ rate table (else a fixed rate).
    lq: bool,
    fixed_rate: u32,
}

/// The last beacon of the tracked BSS: TSF, device time, DTIM count.
#[derive(Clone, Copy, Default)]
struct BeaconSync {
    tsf: u64,
    sys_ts: u32,
    dtim_count: u8,
    dtim_period: u8,
    beacon_int: u16,
}

impl Dev {
    /// Drain the RX ring and dispatch: responses, data, notifications.
    fn service(&mut self) {
        self.t.poll_int();
        let mut pkts = Vec::new();
        self.t.rx_poll(&mut pkts);
        for p in pkts { self.dispatch(p); }
    }

    fn dispatch(&mut self, p: RxPacket) {
        if p.is_response() && p.queue() == tr::CMD_QUEUE {
            self.t.cmd_done(p.index());
            if let Some(i) = self.async_cmds.iter().position(|&s| s == p.seq) {
                // A command nobody waits for (the rate table): drop its reply.
                self.async_cmds.swap_remove(i);
                return;
            }
            self.resps.push(p);
            return;
        }
        if p.group <= 1 && p.cmd == cmd::TX_CMD && p.is_response() {
            // TX status of a data or management frame: reclaim the ring.
            if let Some(r) = asc::parse_tx_resp(&p.data) {
                self.t.tx_reclaim(p.queue(), r.ssn as usize);
                self.on_tx_resp(p.queue(), p.index(), &r);
            }
            if p.queue() == MGMT_QUEUE {
                if self.notifs.len() >= 512 { self.notifs.pop_front(); self.notif_dropped += 1; }
                self.notifs.push_back(p);
            }
            return;
        }
        if p.group <= 1 {
            match p.cmd {
                cmd::REPLY_RX_PHY_CMD => { self.last_phy = cmd::parse_rx_phy(&p.data); return; }
                cmd::REPLY_RX_MPDU_CMD => { self.rx_mpdu(&p.data); return; }
                cmd::STATISTICS_NOTIFICATION | cmd::DTS_MEASUREMENT_NOTIFICATION | cmd::DEBUG_LOG_MSG
                | cmd::BEACON_NOTIFICATION => {
                    if self.debug { ilog!("notification {} ({} bytes)", cmd::cmd_name(0, p.cmd), p.data.len()); }
                    return;
                }
                _ => {}
            }
        }
        if self.debug {
            ilog!("notification {:02x}.{:02x} {} seq {:04x} ({} bytes)", p.group, p.cmd, cmd::cmd_name(p.group, p.cmd), p.seq, p.data.len());
        }
        if self.notifs.len() >= 512 { self.notifs.pop_front(); self.notif_dropped += 1; }
        self.notifs.push_back(p);
    }

    /// Account one TX response, feed rate control, release backpressure.
    fn on_tx_resp(&mut self, q: usize, index: usize, r: &asc::TxResp) {
        let now = now_ms();
        let data = q == BSS_QUEUE;
        match q { BSS_QUEUE => self.last_txrsp_ms[0] = now, MGMT_QUEUE => self.last_txrsp_ms[1] = now, _ => {} }
        let fin = self.rc.as_ref().map_or(r.initial_rate, |c| c.final_rate(r));
        self.txs.on_resp(r, fin, data);
        if self.debug || (!r.ok() && q == MGMT_QUEUE) {
            ilog!("TX status queue {} index {}: {:#06x} after {} retries (rate {:#x})", q, index, r.status, r.failure_frame,
                  r.initial_rate);
        }
        if !data { return; }
        let used = self.t.txq[BSS_QUEUE].as_ref().map_or(0, |t| t.used());
        TXQ_DEPTH.store(used as u32, Ordering::Relaxed);
        if used < TXQ_LOW && TX_HELD.swap(false, Ordering::AcqRel) { crate::netdev::kick(); }
        if !r.ok() && self.fail_logged < FAIL_LOG_MAX && !self.debug {
            self.fail_logged += 1;
            let b = rs::status_bucket(r.status);
            ilog!("TX data failure #{} (queue {} index {}): status {:#06x} ({}), {} retries, rts_fail {}, bt_kill {}, reduced_tpc {}, rate {:#x} -> {:#x}{}",
                  self.fail_logged, q, index, r.status, rs::STATUS_MEANING[b], r.failure_frame, r.failure_rts, r.bt_kill_count,
                  r.reduced_tpc, r.initial_rate, fin, if self.fail_logged == FAIL_LOG_MAX { " (no more logged; see /proc/net/wifi)" } else { "" });
        }
        if self.link.as_ref().map_or(false, |l| l.lq) {
            if let Some(c) = self.rc.as_mut() {
                let (from, ant) = (c.cur_r500k(), c.ant);
                let a = c.on_tx(r, now);
                if self.debug && (a != rs::Action::Stay || ant != c.ant) {
                    ilog!("rate control: {}M -> {}M, antenna {} -> {}", rs::mbps(from), rs::mbps(c.cur_r500k()), ant, c.ant);
                }
            }
            self.send_lq_if_due(now);
        }
    }

    /// Send rate control's new station table, without waiting for the reply
    /// (at most every rs::LQ_MIN_INTERVAL_MS).
    fn send_lq_if_due(&mut self, now: u64) {
        let lq = match self.rc.as_ref() { Some(c) if c.due(now) => c.lq(AP_STA_ID), _ => return };
        if let Ok(seq) = self.t.send_cmd(cmd::LQ_CMD, Some((cmd::hcmd_group(cmd::LQ_CMD, 0), 0)), &lq) {
            if self.async_cmds.len() >= 16 { self.async_cmds.remove(0); }
            self.async_cmds.push(seq);
            if let Some(c) = self.rc.as_mut() { c.mark_sent(now); }
        }
    }

    /// The association's data path is gone: forget its per-link state.
    fn clear_link(&mut self) {
        self.link = None;
        self.guard = None;
        self.track = None;
        self.rc = None;
        self.erp = None;
        self.erp_changed = false;
        TXQ_DEPTH.store(0, Ordering::Relaxed);
        if TX_HELD.swap(false, Ordering::AcqRel) { crate::netdev::kick(); }
        *ASSOC_MS.lock() = None;
    }

    fn rx_mpdu(&mut self, data: &[u8]) {
        let (frame, status) = match cmd::parse_rx_mpdu(data) { Some(x) => x, None => return };
        if status & cmd::RX_MPDU_STATUS_CRC_OK == 0 || status & cmd::RX_MPDU_STATUS_OVERRUN_OK == 0 { return; }
        let fc = match wf::frame_control(frame) { Some(f) => f, None => return };
        let phy = self.last_phy.unwrap_or_default();
        if wf::is_mgmt(fc) {
            let st = wf::subtype(fc);
            if st == wf::fc::STYPE_BEACON && self.track.is_some() && wf::addr3(frame) == self.track && frame.len() > 36 {
                let tsf = cmd::le64(frame, 24).unwrap_or(0);
                let bi = cmd::le16(frame, 32).unwrap_or(100);
                if let Some((count, period)) = asc::tim_dtim(&frame[36..]) {
                    self.bcn = Some(BeaconSync { tsf, sys_ts: phy.system_timestamp, dtim_count: count, dtim_period: period, beacon_int: bi });
                }
                let erp = asc::erp_ie(&frame[36..]);
                if erp != self.erp && self.link.is_some() {
                    self.erp = erp;
                    self.erp_changed = true;
                }
                self.bcn_rssi = Some(phy.signal_dbm());
            }
            if st == wf::fc::STYPE_BEACON || st == wf::fc::STYPE_PROBE_RESP {
                if self.scanning {
                    if let Some(b) = wf::parse_beacon(frame, phy.channel as u8) {
                        let rssi = phy.signal_dbm();
                        let now = now_ms();
                        match self.scan_results.iter_mut().find(|e| e.bss.bssid == b.bssid) {
                            Some(e) => {
                                // Keep the SSID a probe response reveals for a hidden network.
                                let ssid = if b.ssid.is_empty() { core::mem::take(&mut e.bss.ssid) } else { b.ssid.clone() };
                                e.bss = b;
                                e.bss.ssid = ssid;
                                e.rssi = e.rssi.max(rssi);
                                e.seen_ms = now;
                            }
                            None => if self.scan_results.len() < 256 { self.scan_results.push(ScanEntry { bss: b, rssi, seen_ms: now }); },
                        }
                    }
                }
                return;
            }
            // Other management frames go to the driver thread (auth, assoc, deauth).
            let mut v = Vec::with_capacity(frame.len() + 4);
            v.extend_from_slice(&status.to_le_bytes());
            v.extend_from_slice(frame);
            self.push_mgmt(v);
            return;
        }
        if wf::is_data(fc) { self.rx_data(frame, fc, status); }
    }

    fn push_mgmt(&mut self, v: Vec<u8>) {
        if self.notifs.len() >= 512 { self.notifs.pop_front(); self.notif_dropped += 1; }
        // Pseudo notification: group 0xff carries a received management frame.
        self.notifs.push_back(RxPacket { cmd: 0, group: 0xff, seq: 0x8000, data: v });
    }

    fn rx_data(&mut self, frame: &[u8], fc: u16, status: u32) {
        let (h, t) = match cmd::rx_crypto_lengths(fc, status) {
            Some(x) => x,
            None => { self.rx_dropped += 1; self.rxs.undecrypted += 1; return; }
        };
        // Null data frames carry nothing (power-save signalling).
        if fc & 0x0040 != 0 { return; }
        if let Some(g) = self.guard.as_mut() {
            if let Err(r) = g.check(frame, h != 0) {
                let c = match r {
                    wf::RxReject::Foreign => &mut self.rxs.foreign,
                    wf::RxReject::Echo => &mut self.rxs.echo,
                    wf::RxReject::Fragment => &mut self.rxs.fragment,
                    wf::RxReject::Duplicate => &mut self.rxs.duplicate,
                    wf::RxReject::Replay => &mut self.rxs.replay,
                    wf::RxReject::Malformed => &mut self.rxs.malformed,
                };
                *c += 1;
                if self.debug {
                    ilog!("rx: dropped {:?} data frame fc {:#06x} seq {:#06x} ({} bytes)", r, fc, cmd::le16(frame, 22).unwrap_or(0), frame.len());
                }
                return;
            }
        }
        if wf::is_amsdu(frame) {
            // A-MSDU: one Ethernet frame per subframe.
            let mut subs: Vec<Vec<u8>> = Vec::new();
            wf::amsdu_to_ethernet(frame, h, t, &mut |e| subs.push(e.to_vec()));
            self.rxs.amsdu += 1;
            if subs.is_empty() { self.rxs.unconvertible += 1; }
            for e in subs { self.rx_eth_frame(frame, &e); }
            return;
        }
        let mut out = [0u8; 2400];
        let n = match wf::data_to_ethernet(frame, h, t, &mut out) {
            Some(n) => n,
            None => { self.rxs.unconvertible += 1; return; }
        };
        self.rx_eth_frame(frame, &out[..n]);
    }

    /// One received Ethernet frame (from the 802.11 `frame`): EAPOL to the
    /// driver thread, everything else to the stack.
    fn rx_eth_frame(&mut self, frame: &[u8], eth: &[u8]) {
        let n = eth.len();
        if wf::is_eapol(eth) {
            let mut v = Vec::with_capacity(n);
            v.extend_from_slice(eth);
            if self.notifs.len() < 512 { self.notifs.push_back(RxPacket { cmd: 1, group: 0xff, seq: 0x8000, data: v }); }
            return;
        }
        if !LINK_UP.load(Ordering::Relaxed) { return; }
        if self.csumcheck {
            match wf::inet_checksum_ok(eth) {
                Some(true) => self.rxs.csum_ok += 1,
                Some(false) => {
                    self.rxs.csum_bad += 1;
                    if self.rxs.csum_bad <= 20 {
                        let fcv = wf::frame_control(frame).unwrap_or(0);
                        let pn = if fcv & wf::fc::PROTECTED != 0 { wf::ccmp_pn(&frame[wf::hdr_len(fcv).min(frame.len())..]) } else { None };
                        let phy = self.last_phy.unwrap_or_default();
                        let mut head = String::new();
                        for b in eth.iter().skip(14).take(40) { head.push_str(&format!("{:02x}", b)); }
                        ilog!("rx: bad IP/TCP/UDP checksum #{}: {} byte frame, fc {:#06x} seq {:#06x} pn {:?} rate {:#x}; ip {}",
                              self.rxs.csum_bad, frame.len(), fcv, cmd::le16(frame, 22).unwrap_or(0), pn, phy.rate_n_flags, head);
                    }
                }
                None => {}
            }
        }
        if self.rx_eth.len() >= 512 { self.rx_eth.pop_front(); self.rx_dropped += 1; }
        self.rx_eth.push_back(eth.to_vec());
        self.rx_frames += 1;
        crate::netdev::kick();
    }

    fn take_resp(&mut self, seq: u16) -> Option<RxPacket> {
        let i = self.resps.iter().position(|p| p.seq == seq)?;
        Some(self.resps.swap_remove(i))
    }
}

fn with_dev<R>(f: impl FnOnce(&mut Dev) -> R) -> Option<R> {
    let mut g = DEV.lock();
    g.as_mut().map(f)
}

// ── probe (W0) ──────────────────────────────────────────────────────────────

/// Probe the first supported Intel Wi-Fi function. Boot context, quick.
pub fn init(cmdline: &str) {
    if cmdline_val(cmdline, "iwlwifi.disable") == Some("1") {
        log("disabled (iwlwifi.disable=1)");
        return;
    }
    let mut found = None;
    for &(did, name, fw) in IDS {
        if let Some(d) = pci::find_device(INTEL, did) { found = Some((d, name, fw)); break; }
    }
    let (dev, name, fw_file) = match found {
        Some(x) => x,
        None => { log("no Intel 8265/8260 Wi-Fi (8086:24fd/24f3/24f4) found; driver inert"); return; }
    };
    let opts = Opts {
        start: cmdline_val(cmdline, "iwlwifi.start") == Some("1"),
        scan: cmdline_val(cmdline, "iwlwifi.scan") != Some("0"),
        connect: cmdline_val(cmdline, "iwlwifi.connect") != Some("0"),
        debug: cmdline_val(cmdline, "iwlwifi.debug") == Some("1"),
        csumcheck: cmdline_val(cmdline, "iwlwifi.csumcheck") == Some("1"),
        lq: cmdline_val(cmdline, "iwlwifi.lq") != Some("0"),
        fw: cmdline_val(cmdline, "iwlwifi.fw").map(String::from),
        ssid: cmdline_val(cmdline, "wifi.ssid").map(percent_decode),
        psk: cmdline_val(cmdline, "wifi.psk").map(percent_decode),
        mgr: cmdline_val(cmdline, "iwlwifi.mgr") == Some("1"),
        softdisc: cmdline_val(cmdline, "iwlwifi.softdisc") != Some("0"),
        scanconn: cmdline_val(cmdline, "iwlwifi.scanconn") == Some("1"),
    };
    RC_ON.store(cmdline_val(cmdline, "iwlwifi.rc") != Some("0"), Ordering::Relaxed);
    TXANT.store(match cmdline_val(cmdline, "iwlwifi.txant") {
        Some("a") | Some("A") => asc::ANT_A, Some("b") | Some("B") => asc::ANT_B, _ => 0 }, Ordering::Relaxed);
    MAXRATE.store(cmdline_val(cmdline, "iwlwifi.maxrate").and_then(parse_mbps).unwrap_or(0), Ordering::Relaxed);
    BTCOEX_WIFI.store(cmdline_val(cmdline, "iwlwifi.btcoex") == Some("wifi"), Ordering::Relaxed);
    ilog!("TX: rate control {}, antenna {}, max rate {}, BT coex {}",
          if RC_ON.load(Ordering::Relaxed) { "on" } else { "off (iwlwifi.rc=0)" },
          match TXANT.load(Ordering::Relaxed) { 0 => "auto", 1 => "A", _ => "B" },
          match MAXRATE.load(Ordering::Relaxed) { 0 => String::from("none"), r => format!("{} Mb/s", rs::mbps(r)) },
          if BTCOEX_WIFI.load(Ordering::Relaxed) { "wifi (iwlwifi.btcoex=wifi)" } else { "nw" });
    let mut opts = opts;
    if opts.mgr && !opts.start {
        opts.start = true;
        log("iwlwifi.mgr=1 implies iwlwifi.start=1");
    }
    MGR_ON.store(opts.mgr, Ordering::SeqCst);
    let func = dev.function();
    let subsys = (func.read16(0x2c), func.read16(0x2e));
    ilog!("found {} 8086:{:04x} at {:02x}:{:02x}.{} (rev {:02x}, subsystem {:04x}:{:04x})",
          name, dev.device_id, dev.bus, dev.dev, dev.func, func.read8(0x08), subsys.0, subsys.1);

    if let Some(pm) = func.find_capability(pci::cap::PM) {
        let pmcsr = func.read16(pm + 4);
        if pmcsr & 3 != 0 {
            ilog!("PCI power state D{} -> D0", pmcsr & 3);
            func.write16(pm + 4, pmcsr & !3);
            msleep(10);
        }
    }
    if let Some(c) = func.find_capability(pci::cap::PCIE) {
        let lnkctl = func.read16(c + 0x10);
        let lnksta = func.read16(c + 0x12);
        let devctl2 = func.read16(c + 0x28);
        ilog!("PCIe link gen{} x{}, ASPM {}, LTR {}", lnksta & 0xf, (lnksta >> 4) & 0x3f,
              match lnkctl & 3 { 0 => "off", 1 => "L0s", 2 => "L1", _ => "L0s+L1" },
              if devctl2 & (1 << 10) != 0 { "on" } else { "off" });
    }
    // No retry timeout (Linux: keeps PCI TX retries from interfering with C3).
    func.write8(0x41, 0);
    func.enable_mem_and_bus_master();
    // Interrupts are polled: INTx off, MSI never enabled.
    func.set_intx_disable(true);

    let (va, size) = match func.map_bar(0, mm::kva::CacheMode::Uncached) {
        Some(x) => x,
        None => { log("BAR0 not mappable; giving up"); return; }
    };
    let m = Mmio(va);
    let hw_rev = m.r32(tr::CSR_HW_REV);
    if hw_rev == 0xffff_ffff {
        ilog!("CSR_HW_REV reads all-ones (BAR0 {} KiB): device not responding; giving up", size / 1024);
        return;
    }
    let mut t = match Trans::new(m, &[AUX_QUEUE, BSS_QUEUE, MGMT_QUEUE]) {
        Some(t) => t,
        None => { log("out of DMA memory for the rings; giving up"); return; }
    };
    t.hw_rev = hw_rev;
    t.disable_interrupts();
    ilog!("BAR0 {} KiB, CSR_HW_REV {:08x} (type {:#x}, step {}), HW_IF_CONFIG {:08x}, GP_CNTRL {:08x}",
          size / 1024, hw_rev, (hw_rev >> 4) & 0xfff, hw_rev & 0xf, m.r32(tr::CSR_HW_IF_CONFIG_REG), m.r32(tr::CSR_GP_CNTRL));

    let ready = t.prepare_card_hw();
    ilog!("NIC ownership: {}", if ready.is_ok() { "ours (NIC_READY)" } else { "NOT obtained (ME/AMT owns it?)" });
    let mut mac_otp = [0u8; 6];
    let mut rf_id = 0;
    if ready.is_ok() && t.finish_nic_init().is_ok() && t.grab() {
        rf_id = m.r32(tr::CSR_HW_RF_ID);
        let a0 = t.read_prph_nograb(tr::WFMP_MAC_ADDR_0);
        let a1 = t.read_prph_nograb(tr::WFMP_MAC_ADDR_1);
        t.release();
        mac_otp = cmd::mac_from_otp(a0, a1);
        ilog!("RF id {:08x}; OTP MAC {} (WFMP {:08x} {:08x})", rf_id, fmt_mac(&mac_otp), a0, a1);
    } else {
        ilog!("could not wake the MAC: {}", t.csr_dump());
    }
    t.hw_rf_id = rf_id;
    let rfkill = t.rfkill_on();
    ilog!("hardware RF-kill switch: {}", if rfkill { "ON (radio disabled; airplane switch or BIOS)" } else { "off (radio allowed)" });
    // Back to low power until the thread (if any) starts it.
    t.apm_stop();

    let mac = if cmd::valid_unicast_mac(&mac_otp) { mac_otp } else {
        // A locally administered placeholder until the NVM gives the real one.
        let mut r = [0u8; 6];
        sched::random::fill(&mut r);
        [0x02, r[1], r[2], r[3], r[4], r[5]]
    };
    *MAC.lock() = mac;
    *INFO.lock() = Some(Info {
        name, bdf: (dev.bus, dev.dev, dev.func), hw_rev, rf_id, fw: String::from(fw_file),
        state: String::from(if opts.start { "starting" } else { "probed (iwlwifi.start=1 to bring it up)" }),
        rfkill, ..Default::default()
    });
    *DEV.lock() = Some(Dev {
        t, func, debug: opts.debug, resps: Vec::new(), notifs: VecDeque::new(), last_phy: None,
        scanning: false, scan_results: Vec::new(), rx_eth: VecDeque::new(),
        rx_frames: 0, rx_dropped: 0, txs: TxStats::default(), txs_mark: TxStats::default(), rc: None,
        async_cmds: Vec::new(), last_txrsp_ms: [0; 2], fail_logged: 0, erp: None, erp_changed: false, notif_dropped: 0,
        guard: None, rxs: RxStats::default(), csumcheck: opts.csumcheck,
        link: None, track: None, bcn: None, bcn_rssi: None,
    });
    PRESENT.store(true, Ordering::SeqCst);

    if !opts.start {
        log("probe only; add iwlwifi.start=1 to the kernel command line to bring Wi-Fi up");
        return;
    }
    let path = opts.fw.clone().unwrap_or_else(|| format!("/lib/firmware/{}", fw_file));
    if sched::kthread::spawn("iwlwifi", move || driver_thread(opts, path)).is_none() {
        log("could not start the driver thread");
        set_state("thread spawn failed");
    }
}

fn fmt_mac(m: &[u8; 6]) -> String {
    format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
}

// ── host commands ───────────────────────────────────────────────────────────

#[derive(Debug)]
enum Err {
    Trans(TransError),
    Timeout(&'static str),
    FwError,
    Status(&'static str, u32),
    /// Deauthenticated / disassociated by the AP (802.11 reason code).
    Deauth(u16),
    Other(String),
}

impl From<TransError> for Err { fn from(e: TransError) -> Self { Err::Trans(e) } }

type Res<T> = Result<T, Err>;

/// Send a command and wait for its response payload.
/// Group-0 commands go out with a wide header in LONG_GROUP, as Linux MVM
/// does for every host command (iwl_trans_send_cmd: DEF_ID); see
/// cmd::hcmd_group.
fn send(id: u8, wide: Option<(u8, u8)>, payload: &[u8]) -> Res<Vec<u8>> {
    let (g, v) = wide.unwrap_or((0, 0));
    let hdr = Some((cmd::hcmd_group(id, g), v));
    let seq = match with_dev(|d| d.t.send_cmd(id, hdr, payload)) {
        Some(r) => r?,
        None => return Err(Err::Other(String::from("no device"))),
    };
    let t0 = now_ms();
    loop {
        let (r, fwerr) = with_dev(|d| { d.service(); (d.take_resp(seq), d.t.fw_error) }).unwrap_or((None, true));
        // A firmware error wins over a response that raced it: the reply is
        // not trustworthy once the firmware has asserted.
        if let (Some(p), false) = (r, fwerr) {
            if p.cmd != id {
                ilog!("response to {} carries command {:#04x}", cmd::cmd_name(wide.map_or(0, |w| w.0), id), p.cmd);
            }
            return Ok(p.data);
        }
        if fwerr {
            ilog!("firmware error while waiting for {} ({:02x})", cmd::cmd_name(wide.map_or(0, |w| w.0), id), id);
            return Err(Err::FwError);
        }
        if now_ms() - t0 > 2000 {
            ilog!("no response to {} ({:02x}) in 2 s", cmd::cmd_name(wide.map_or(0, |w| w.0), id), id);
            return Err(Err::Timeout("command response"));
        }
        msleep(1);
    }
}

/// A command whose response starts with a u32 status that must be 0.
fn send_status0(id: u8, payload: &[u8], what: &'static str) -> Res<()> {
    let r = send(id, None, payload)?;
    let st = cmd::le32(&r, 0).unwrap_or(0);
    if st != 0 { return Err(Err::Status(what, st)); }
    Ok(())
}

/// Wait for a notification `pred` accepts; others are handed to `other`.
fn wait_notif(timeout_ms: u64, mut pred: impl FnMut(&RxPacket) -> bool, mut other: impl FnMut(RxPacket)) -> Res<RxPacket> {
    let t0 = now_ms();
    loop {
        let (p, fwerr) = with_dev(|d| { d.service(); (d.notifs.pop_front(), d.t.fw_error) }).unwrap_or((None, true));
        match p {
            Some(p) => { if pred(&p) { return Ok(p); } other(p); continue; }
            None => {}
        }
        if fwerr { return Err(Err::FwError); }
        if now_ms() - t0 > timeout_ms { return Err(Err::Timeout("notification")); }
        msleep(1);
    }
}

/// The default handler for notifications nobody waits for.
fn log_async(p: RxPacket) {
    match (p.group, p.cmd) {
        (0, cmd::REPLY_ERROR) => {
            ilog!("firmware REPLY_ERROR: cmd {:#x} error {:#x} seq {:#x} ({} bytes)",
                  p.data.get(4).copied().unwrap_or(0), cmd::le32(&p.data, 0).unwrap_or(0),
                  cmd::le16(&p.data, 6).unwrap_or(0), p.data.len());
        }
        (0xff, _) => {}
        _ => {}
    }
}

// ── firmware bring-up (W1) ──────────────────────────────────────────────────

struct Fw {
    data: &'static [u8],
    info: FwInfo,
}

fn load_fw_file(path: &str) -> Res<Fw> {
    let t0 = now_ms();
    while !crate::firmware::root_ready() {
        if now_ms() - t0 > 120_000 { return Err(Err::Other(String::from("root filesystem not mounted within 120 s"))); }
        msleep(250);
    }
    let mut f = crate::firmware::request_firmware(path, 8 << 20)
        .map_err(|e| Err::Other(format!("{} not loaded: {:?}", path, e)))?;
    let va = f.map().ok_or_else(|| Err::Other(String::from("firmware mapping failed")))?;
    let len = f.len();
    let crc = f.crc32();
    // The image stays mapped for the life of the kernel (re-used on restart).
    core::mem::forget(f);
    let data: &'static [u8] = unsafe { core::slice::from_raw_parts(va as *const u8, len) };
    let info = iwl_fw::parse(data).map_err(|e| Err::Other(format!("{}: {}", path, e)))?;
    ilog!("firmware {} loaded: {} bytes, crc32 {:08x}, '{}', API {}, build {}{} after {} ms",
          path, len, crc, info.human, info.api_major(), info.build,
          info.fw_version.map_or(String::new(), |(a, b, c)| format!(", version {}.{:08x}.{}", a, b, c)), now_ms() - t0);
    ilog!("firmware: flags {:08x} api {:08x} {:08x} capa {:08x} {:08x} {:08x} {:08x} phy_config {:08x} (tx ant {:#x} rx ant {:#x}) cpus {} paging {} KiB scan channels {}",
          info.flags, info.api[0], info.api[1], info.capa[0], info.capa[1], info.capa[2], info.capa[3],
          info.phy_config, info.valid_tx_ant, info.valid_rx_ant, info.num_cpus, info.paging_mem_size / 1024, info.n_scan_channels);
    let mut need = String::new();
    for (ok, what) in [
        (info.has_capa(iwl_fw::capa::UMAC_SCAN), "UMAC scan"),
        (info.has_capa(iwl_fw::capa::DQA_SUPPORT), "DQA"),
        (info.has_api(iwl_fw::api::ADAPTIVE_DWELL) && !info.has_api(iwl_fw::api::ADAPTIVE_DWELL_V2), "scan API v7"),
        (info.cmd_ver(0, cmd::ADD_STA).map_or(false, |v| v < 12), "ADD_STA < v12"),
        (info.cmd_ver(0, cmd::PHY_CONTEXT_CMD).map_or(true, |v| v < 3), "PHY_CONTEXT v1"),
        (!info.has_capa(iwl_fw::capa::ULTRA_HB_CHANNELS), "no UHB channels"),
        (info.num_cpus == 2, "dual CPU image"),
    ] {
        if !ok { need.push_str(what); need.push_str(", "); }
    }
    if !need.is_empty() {
        ilog!("WARNING: this firmware differs from what the driver was written for (iwlwifi-8265-36): missing {}", need);
    }
    Ok(Fw { data, info })
}

fn chunks<'a>(fw: &'a Fw, img: usize, idx: &[usize]) -> Vec<FwChunk<'a>> {
    let s = &fw.info.images[img].sections;
    idx.iter().map(|&i| FwChunk { addr: s[i].offset, data: &fw.data[s[i].start..s[i].start + s[i].len] }).collect()
}

/// Download an image, wait for ALIVE, start the scheduler. Returns ALIVE.
fn start_image(fw: &Fw, img: usize, func: &PciFunction) -> Res<cmd::Alive> {
    let plan = fw.info.images[img].load_plan();
    let cpu1 = chunks(fw, img, &plan.cpu1);
    let cpu2 = chunks(fw, img, &plan.cpu2);
    let what = if img == iwl_fw::IMG_INIT { "INIT" } else { "runtime" };
    with_dev(|d| { d.notifs.clear(); d.resps.clear(); d.last_phy = None; });
    let r = with_dev(|d| d.t.start_fw(func, &cpu1, &cpu2, img == iwl_fw::IMG_INIT)).unwrap();
    if let Err(e) = r {
        ilog!("{} image download failed: {:?}; {}", what, e, with_dev(|d| d.t.csr_dump()).unwrap_or_default());
        return Err(e.into());
    }
    let p = wait_notif(1000, |p| p.group <= 1 && p.cmd == cmd::UCODE_ALIVE_NTFY, log_async);
    let p = match p {
        Ok(p) => p,
        Err(e) => {
            let (s1, s2, dump) = with_dev(|d| (d.t.read_prph(tr::SB_CPU_1_STATUS), d.t.read_prph(tr::SB_CPU_2_STATUS), d.t.csr_dump())).unwrap();
            ilog!("no ALIVE from the {} image within 1 s ({:?}): SecBoot CPU1 status {:#x} CPU2 status {:#x}; {}", what, e, s1, s2, dump);
            return Err(e);
        }
    };
    let a = cmd::parse_alive(&p.data).ok_or_else(|| Err::Other(format!("ALIVE of unknown size {}", p.data.len())))?;
    ilog!("{} image ALIVE: status {:#06x} ucode {}.{} type {}/{} UMAC {}.{}, LMAC error table {:#x}, UMAC error table {:#x}, SCD base {:#x}",
          what, a.status, a.ucode_major, a.ucode_minor, a.ver_type, a.ver_subtype, a.umac_major, a.umac_minor,
          a.lmac_error_table, a.umac_error_table, a.scd_base);
    if a.status != cmd::ALIVE_STATUS_OK {
        return Err(Err::Status("ALIVE", a.status as u32));
    }
    ERR_TABLES.lock().replace((a.lmac_error_table, a.umac_error_table));
    with_dev(|d| d.t.fw_alive(a.scd_base));
    Ok(a)
}

static ERR_TABLES: Mutex<Option<(u32, u32)>> = Mutex::new(None);

/// On a firmware error: dump both error tables and the registers.
fn dump_fw_error(context: &str) {
    let tables = *ERR_TABLES.lock();
    ilog!("firmware failure during {}: {}", context, with_dev(|d| d.t.csr_dump()).unwrap_or_default());
    if let Some((l, u)) = tables {
        with_dev(|d| { d.t.dump_error_table("LMAC", l); d.t.dump_error_table("UMAC", u); });
    }
}

/// Read every NVM section the INIT image offers.
fn read_nvm() -> Res<Vec<(u16, Vec<u8>)>> {
    let mut out = Vec::new();
    for section in 0..cmd::NVM_MAX_SECTIONS {
        let mut data = Vec::new();
        let mut off: u16 = 0;
        loop {
            let r = send(cmd::NVM_ACCESS_CMD, None, &cmd::nvm_read_cmd(section, off, cmd::NVM_CHUNK))?;
            let (roff, len, _ty, status, d) = cmd::parse_nvm_resp(&r).ok_or_else(|| Err::Other(String::from("short NVM response")))?;
            if status != 0 {
                if off != 0 && status == cmd::NVM_READ_STATUS_NOT_VALID_ADDRESS { break; }
                data.clear();
                break;
            }
            if roff != off { return Err(Err::Other(format!("NVM section {} offset {} answered for {}", section, off, roff))); }
            data.extend_from_slice(d);
            off = off.wrapping_add(len);
            if len < cmd::NVM_CHUNK || data.len() > 32 * 1024 { break; }
        }
        if !data.is_empty() { out.push((section, data)); }
    }
    Ok(out)
}

struct Hw {
    nvm: cmd::NvmData,
    mac: [u8; 6],
    tx_ant: u8,
    rx_ant: u8,
    phy_db: cmd::PhyDb,
    paging: Vec<Dma>,
    channels: Vec<u8>,
}

/// INIT image: NVM, calibrations (PHY DB). Leaves the device stopped.
fn run_init(fw: &Fw, func: &PciFunction) -> Res<Hw> {
    // CSR_HW_IF_CONFIG radio type/step/dash come from phy_config and are
    // written by nic_init for every image, INIT included (iwl_mvm_nic_config).
    with_dev(|d| d.t.phy_config = cmd::phy_config(fw.info.phy_config, fw.info.valid_tx_ant, fw.info.valid_rx_ant));
    start_image(fw, iwl_fw::IMG_INIT, func)?;
    let t0 = now_ms();
    let sections = read_nvm()?;
    let get = |n: u16| sections.iter().find(|s| s.0 == n).map(|s| &s.1[..]);
    let list: Vec<String> = sections.iter().map(|s| format!("{}:{}", s.0, s.1.len())).collect();
    ilog!("NVM read in {} ms; sections (id:bytes) {}", now_ms() - t0, list.join(" "));
    let nvm = match (get(cmd::NVM_SECTION_SW), get(cmd::NVM_SECTION_REGULATORY), get(cmd::NVM_SECTION_PHY_SKU)) {
        (Some(sw), Some(reg), Some(sku)) => cmd::parse_nvm_8000(sw, reg, sku, get(cmd::NVM_SECTION_MAC_OVERRIDE))
            .ok_or_else(|| Err::Other(String::from("NVM sections too short")))?,
        _ => return Err(Err::Other(String::from("NVM is missing the SW, REGULATORY or PHY_SKU section"))),
    };
    // Linux (iwl_set_hw_address_family_8000) reads the OTP MAC while the
    // INIT image runs; the probe-time read is only a fallback.
    let mut otp = *MAC.lock();
    if nvm.mac_override.is_none() {
        let live = with_dev(|d| {
            if !d.t.grab() { return None; }
            let a0 = d.t.read_prph_nograb(tr::WFMP_MAC_ADDR_0);
            let a1 = d.t.read_prph_nograb(tr::WFMP_MAC_ADDR_1);
            d.t.release();
            Some(cmd::mac_from_otp(a0, a1))
        }).flatten();
        if let Some(m) = live {
            if m != otp { ilog!("OTP MAC after INIT ALIVE {} (probe read {})", fmt_mac(&m), fmt_mac(&otp)); }
            if cmd::valid_unicast_mac(&m) { otp = m; }
        }
    }
    let mac = match nvm.mac_override { Some(m) => m, None => otp };
    let tx_ant = if nvm.valid_tx_ant != 0 { fw.info.valid_tx_ant & nvm.valid_tx_ant } else { fw.info.valid_tx_ant };
    let rx_ant = if nvm.valid_rx_ant != 0 { fw.info.valid_rx_ant & nvm.valid_rx_ant } else { fw.info.valid_rx_ant };
    let chans: Vec<String> = nvm.channels.iter().map(|c| format!("{}{}", c.ch,
        if c.flags & cmd::chan_flags::ACTIVE == 0 { "p" } else if c.flags & cmd::chan_flags::RADAR != 0 { "r" } else { "" })).collect();
    ilog!("NVM version {:#x}: MAC {} ({}), antennas tx {:#x} rx {:#x}, SKU {:#x} (2.4 GHz {}, 5 GHz {}, 11n {}, 11ac {}{}), LAR {}, radio cfg {:#x}",
          nvm.nvm_version, fmt_mac(&mac), if nvm.mac_override.is_some() { "MAC_OVERRIDE section" } else { "OTP" },
          tx_ant, rx_ant, nvm.sku, nvm.band_24, nvm.band_52, nvm.ht, nvm.vht,
          if nvm.mimo_disabled { ", MIMO disabled" } else { "" }, if nvm.lar_enabled { "on" } else { "off" }, nvm.radio_cfg);
    ilog!("NVM channels ({}; p = passive only, r = radar): {}", nvm.channels.len(), chans.join(" "));
    *MAC.lock() = mac;
    with_dev(|d| d.t.phy_config = cmd::phy_config(fw.info.phy_config, tx_ant, rx_ant));

    if with_dev(|d| d.t.rfkill_on()).unwrap_or(false) {
        log("RF-kill is on: calibrations need the radio; waiting for the switch");
        let t1 = now_ms();
        while with_dev(|d| d.t.rfkill_on()).unwrap_or(false) {
            if now_ms() - t1 > 600_000 { return Err(Err::Other(String::from("RF-kill stayed on for 10 minutes"))); }
            msleep(1000);
        }
        log("RF-kill released");
    }
    send(cmd::TX_ANT_CONFIGURATION_CMD, None, &cmd::tx_ant_cfg(tx_ant))?;
    let (flow, event) = fw.info.default_calib[iwl_fw::IMG_INIT];
    let pc = cmd::phy_config(fw.info.phy_config, tx_ant, rx_ant);
    send(cmd::PHY_CONFIGURATION_CMD, None, &cmd::phy_cfg_cmd(pc, flow, event))?;
    let mut phy_db = cmd::PhyDb::default();
    let t2 = now_ms();
    wait_notif(2000, |p| p.group <= 1 && p.cmd == cmd::INIT_COMPLETE_NOTIF, |p| {
        if p.group <= 1 && p.cmd == cmd::CALIB_RES_NOTIF_PHY_DB {
            if let Some((ty, data)) = cmd::parse_phy_db(&p.data) { phy_db.add(ty, data); }
        } else { log_async(p); }
    })?;
    ilog!("INIT calibrations complete in {} ms: {} PHY DB entries", now_ms() - t2, phy_db.entries.len());
    with_dev(|d| d.t.stop_device());
    let channels = nvm.channels.iter().map(|c| c.ch).collect();
    Ok(Hw { nvm, mac, tx_ant, rx_ant, phy_db, paging: Vec::new(), channels })
}

/// Copy the paged part of the runtime image to host memory and tell the
/// firmware where it is (FW_PAGING_BLOCK_CMD).
fn setup_paging(fw: &Fw, hw: &mut Hw) -> Res<()> {
    let img = &fw.info.images[iwl_fw::IMG_REGULAR];
    let size = fw.info.paging_mem_size as usize;
    if size == 0 { return Ok(()); }
    let (css_i, data_i) = img.load_plan().paging.ok_or_else(|| Err::Other(String::from("paging TLV without paging sections")))?;
    let (blocks, last_pages) = iwl_fw::paging_layout(size);
    let css = img.sections[css_i];
    let pdata = img.sections[data_i];
    if pdata.len != size || css.len > iwl_fw::FW_PAGING_SIZE {
        return Err(Err::Other(format!("paging sections do not match the TLV ({} vs {} bytes, CSS {})", pdata.len, size, css.len)));
    }
    if hw.paging.is_empty() {
        let mut v = Vec::new();
        v.push(Dma::alloc(iwl_fw::FW_PAGING_SIZE).ok_or(Err::Trans(TransError::NoMemory))?);
        for _ in 0..blocks { v.push(Dma::alloc(iwl_fw::PAGING_BLOCK_SIZE).ok_or(Err::Trans(TransError::NoMemory))?); }
        hw.paging = v;
    }
    hw.paging[0].bytes_mut()[..css.len].copy_from_slice(&fw.data[css.start..css.start + css.len]);
    for b in 0..blocks {
        let off = b * iwl_fw::PAGING_BLOCK_SIZE;
        let n = (size - off).min(iwl_fw::PAGING_BLOCK_SIZE);
        hw.paging[1 + b].bytes_mut()[..n].copy_from_slice(&fw.data[pdata.start + off..pdata.start + off + n]);
    }
    let phys: Vec<u64> = hw.paging.iter().map(|d| d.phys as u64).collect();
    send(cmd::FW_PAGING_BLOCK_CMD, Some((cmd::LONG_GROUP, 0)), &cmd::paging_cmd(blocks as u32, last_pages as u32, &phys))?;
    ilog!("paging: {} KiB in {} blocks (+CSS), {} pages in the last", size / 1024, blocks, last_pages);
    Ok(())
}

/// Runtime image and the configuration Linux's iwl_mvm_up sends.
fn run_rt(fw: &Fw, func: &PciFunction, hw: &mut Hw) -> Res<()> {
    with_dev(|d| d.t.start_hw(func)).unwrap()?;
    start_image(fw, iwl_fw::IMG_REGULAR, func)?;
    setup_paging(fw, hw)?;
    send(cmd::REPLY_SF_CFG_CMD, None, &cmd::sf_cfg_cmd(cmd::SF_INIT_OFF, None))?;
    send(cmd::TX_ANT_CONFIGURATION_CMD, None, &cmd::tx_ant_cfg(hw.tx_ant))?;
    if !hw.phy_db.has_cfg() { log("WARNING: no PHY DB CFG section from the INIT image"); }
    for (ty, data) in hw.phy_db.ordered() {
        send(cmd::PHY_DB_CMD, None, &cmd::phy_db_cmd(ty, data))?;
    }
    let (flow, event) = fw.info.default_calib[iwl_fw::IMG_REGULAR];
    send(cmd::PHY_CONFIGURATION_CMD, None, &cmd::phy_cfg_cmd(cmd::phy_config(fw.info.phy_config, hw.tx_ant, hw.rx_ant), flow, event))?;
    if BTCOEX_WIFI.load(Ordering::Relaxed) {
        send(cmd::BT_CONFIG, None, &cmd::bt_config_wifi_cmd())?;
    } else {
        send(cmd::BT_CONFIG, None, &cmd::bt_config_cmd(fw.info.has_capa(iwl_fw::capa::BT_MPLUT_SUPPORT)))?;
    }
    send(cmd::DQA_ENABLE_CMD, Some((cmd::DATA_PATH_GROUP, 0)), &cmd::dqa_enable_cmd(tr::CMD_QUEUE as u32))?;
    // The aux station (scans and other off-channel work): queue first.
    enable_queue(AUX_QUEUE, AUX_STA_ID, cmd::MAX_TID_COUNT, cmd::TX_FIFO_MCAST)?;
    add_sta(&cmd::AddSta {
        modify: false, sta_id: AUX_STA_ID, mac_id_n_color: cmd::id_and_color(cmd::MAC_INDEX_AUX, 0), addr: [0; 6],
        station_type: cmd::STA_AUX_ACTIVITY, tid_disable_tx: 0xffff, modify_mask: 0, station_flags: 0,
        station_flags_msk: 0, assoc_id: 0, tfd_queue_msk: 1 << AUX_QUEUE,
    }, "aux station")?;
    let first = hw.channels.first().copied().unwrap_or(1);
    send(cmd::PHY_CONTEXT_CMD, None, &cmd::phy_ctxt_cmd(PHY_ID, 0, cmd::FW_CTXT_ACTION_ADD, first, hw.tx_ant, hw.rx_ant))?;
    if fw.info.has_capa(iwl_fw::capa::CT_KILL_BY_FW) && fw.info.has_capa(iwl_fw::capa::TEMP_THS_REPORT_SUPPORT) {
        send(cmd::TEMP_REPORTING_THRESHOLDS_CMD, Some((cmd::PHY_OPS_GROUP, 0)), &cmd::temp_report_ths_cmd())?;
    }
    send(cmd::POWER_TABLE_CMD, None, &cmd::device_power_cmd(false))?;
    if fw.info.has_capa(iwl_fw::capa::LAR_SUPPORT) && hw.nvm.lar_enabled {
        let r = send(cmd::MCC_UPDATE_CMD, None, &cmd::mcc_update_cmd(*b"ZZ", cmd::MCC_SOURCE_GET_CURRENT))?;
        match cmd::parse_mcc_resp(&r, fw.info.has_capa(iwl_fw::capa::MCC_UPDATE_11AX_SUPPORT)) {
            Some(m) => {
                let allowed: Vec<String> = m.channels.iter().enumerate()
                    .filter(|(_, f)| *f & cmd::chan_flags::VALID as u32 != 0)
                    .map(|(i, f)| format!("{}{}", cmd::EXT_NVM_CHANNELS.get(i).copied().unwrap_or(0),
                                          if f & cmd::chan_flags::ACTIVE as u32 == 0 { "p" } else { "" }))
                    .collect();
                ilog!("regulatory: MCC '{}{}' (status {}, source {:#x}, cap {:#x}); {} channels: {}",
                      m.mcc[0] as char, m.mcc[1] as char, m.status, m.source, m.cap, allowed.len(), allowed.join(" "));
                if let Some(i) = INFO.lock().as_mut() { i.mcc = m.mcc; }
                // Scan what the regulatory domain allows (fall back to the NVM list).
                let ch: Vec<u8> = m.channels.iter().enumerate()
                    .filter(|(_, f)| *f & cmd::chan_flags::VALID as u32 != 0)
                    .filter_map(|(i, _)| cmd::EXT_NVM_CHANNELS.get(i).copied()).collect();
                if !ch.is_empty() { hw.channels = ch; }
            }
            None => ilog!("MCC response not understood ({} bytes)", r.len()),
        }
    } else {
        log("LAR not enabled: using the NVM channel list");
    }
    let n = fw.info.n_scan_channels as usize;
    send(cmd::SCAN_CFG_CMD, Some((cmd::LONG_GROUP, 0)),
         &cmd::scan_config_v1(hw.tx_ant, hw.rx_ant, &hw.mac, AUX_STA_ID, &hw.channels, n))?;
    // The station MAC context (unassociated) the scans and the association use.
    mac_ctxt(cmd::FW_CTXT_ACTION_ADD, &MacState::unassoc(hw.mac))?;
    Ok(())
}

fn add_sta(s: &cmd::AddSta, what: &'static str) -> Res<()> {
    let r = send(cmd::ADD_STA, None, &cmd::add_sta_cmd(s))?;
    let st = cmd::le32(&r, 0).unwrap_or(0) & 0xff;
    if st != cmd::ADD_STA_SUCCESS { return Err(Err::Status(what, st)); }
    Ok(())
}

/// Enable a DQA queue: host pointers, then SCD_QUEUE_CFG to the firmware.
fn enable_queue(q: usize, sta_id: u8, tid: u8, fifo: u8) -> Res<()> {
    let mut ssn = 0u16;
    with_dev(|d| { let w = d.t.txq[q].as_ref().map_or(0, |t| t.write); d.t.txq_enable_fw(q, w); ssn = d.t.txq[q].as_ref().map_or(0, |t| t.write) as u16; });
    send(cmd::SCD_QUEUE_CFG, None, &cmd::scd_queue_cfg_cmd(q as u8, true, sta_id, tid, fifo, ssn))?;
    Ok(())
}

// ── MAC context ─────────────────────────────────────────────────────────────

fn mac_ctxt(action: u32, st: &MacState) -> Res<()> {
    send(cmd::MAC_CONTEXT_CMD, None, &asc::mac_ctxt_cmd(MAC_ID, action, st))?;
    Ok(())
}

// ── scan (W2) ───────────────────────────────────────────────────────────────

fn scan(fw: &Fw, hw: &Hw, ssids: &[&[u8]]) -> Res<Vec<ScanEntry>> {
    with_dev(|d| { d.scan_results.clear(); d.scanning = true; });
    let req = cmd::scan_req_umac_v7(0, MAC_ID as u8, &hw.mac, &hw.channels, fw.info.n_scan_channels as usize, ssids, false);
    let t0 = now_ms();
    let r = send(cmd::SCAN_REQ_UMAC, Some((cmd::LONG_GROUP, 0)), &req);
    if let Err(e) = r {
        with_dev(|d| d.scanning = false);
        return Err(e);
    }
    let st = r.as_ref().ok().and_then(|v| cmd::le32(v, 0)).unwrap_or(0);
    if st != 0 {
        with_dev(|d| d.scanning = false);
        return Err(Err::Status("scan request", st));
    }
    let done = wait_notif(20_000, |p| p.group <= 1 && p.cmd == cmd::SCAN_COMPLETE_UMAC, log_async);
    let results = with_dev(|d| { d.scanning = false; core::mem::take(&mut d.scan_results) }).unwrap_or_default();
    match done {
        Ok(p) => {
            if let Some((uid, last_iter, status, ebs)) = cmd::parse_scan_complete(&p.data) {
                ilog!("scan complete in {} ms (uid {}, status {}, last iter {}, ebs {}): {} BSSs",
                      now_ms() - t0, uid, status, last_iter, ebs, results.len());
            }
        }
        Err(e) => {
            ilog!("scan did not complete ({:?}); {} BSSs seen", e, results.len());
            let _ = send(cmd::SCAN_ABORT_UMAC, Some((cmd::LONG_GROUP, 0)), &0u32.to_le_bytes());
        }
    }
    Ok(results)
}

fn log_bss_list(list: &mut Vec<ScanEntry>) {
    list.sort_by(|a, b| b.rssi.cmp(&a.rssi));
    for e in list.iter() {
        let b = &e.bss;
        ilog!("  {} ch {:>3} {:>4} dBm  {:<14} {}{}  '{}'", fmt_mac(&b.bssid), b.channel, e.rssi, wf::security_summary(b),
              if b.ht { "HT" } else { "  " }, if b.vht { " VHT" } else { "    " }, wf::ssid_display(&b.ssid));
    }
}

// ── driver thread ───────────────────────────────────────────────────────────

fn driver_thread(opts: Opts, path: String) {
    set_state("loading firmware");
    let fw = match load_fw_file(&path) {
        Ok(f) => f,
        Err(e) => {
            ilog!("firmware: {:?}; Wi-Fi stays down", e);
            set_state("firmware missing");
            return;
        }
    };
    if let Some(i) = INFO.lock().as_mut() {
        i.fw = format!("{} (API {}, {})", path, fw.info.api_major(), fw.info.human);
    }
    let func = with_dev(|d| PciFunction { bus: d.func.bus, dev: d.func.dev, func: d.func.func }).unwrap();
    let mut attempt = 0;
    let mut hw = loop {
        attempt += 1;
        set_state("INIT firmware");
        let r = with_dev(|d| d.t.start_hw(&func)).unwrap();
        let r = r.map_err(Err::from).and_then(|_| run_init(&fw, &func));
        match r {
            Ok(h) => break h,
            Err(e) => {
                ilog!("INIT firmware failed (attempt {}): {:?}", attempt, e);
                dump_fw_error("INIT");
                with_dev(|d| d.t.stop_device());
                if attempt >= 3 { set_state("INIT firmware failed"); return; }
                msleep(1000);
            }
        }
    };
    loop {
        set_state("runtime firmware");
        match run_rt(&fw, &func, &mut hw) {
            Ok(()) => break,
            Err(e) => {
                ilog!("runtime firmware failed (attempt {}): {:?}", attempt, e);
                dump_fw_error("runtime bring-up");
                with_dev(|d| d.t.stop_device());
                attempt += 1;
                if attempt >= 4 { set_state("runtime firmware failed"); return; }
                msleep(1000);
            }
        }
    }
    *MAC.lock() = hw.mac;
    log("runtime firmware up");
    set_state("up");
    if !opts.scan { log("iwlwifi.scan=0: not scanning"); idle_forever(); }
    if opts.mgr {
        manager(fw, hw, func, opts);
        return;
    }

    let creds = load_credentials(&opts);
    // Firmware errors since the last good association: a firmware that
    // asserts on every attempt must not be restarted forever.
    let mut fw_errors = 0u32;
    loop {
        set_state("scanning");
        let ssids: Vec<&[u8]> = creds.as_ref().map(|c| alloc::vec![&c.0[..]]).unwrap_or_default();
        match scan(&fw, &hw, &ssids) {
            Ok(mut list) => {
                log_bss_list(&mut list);
                if let Some(i) = INFO.lock().as_mut() { i.bss = list.clone(); i.scan_at_ms = now_ms(); }
                if opts.connect {
                    if let Some(c) = creds.as_ref() {
                        if let Some(target) = pick_bss(&list, &c.0) {
                            let why = connect(&fw, &mut hw, &target, &c.0, &c.1, opts.lq);
                            let fw_err = matches!(why, Disc::FwError);
                            if fw_err { fw_errors += 1; } else if matches!(why, Disc::Lost(_)) { fw_errors = 0; }
                            match why {
                                Disc::Failed(e) => ilog!("connection failed: {}", e),
                                Disc::Lost(e) => ilog!("wlan: link down: {}", e),
                                Disc::FwError => dump_fw_error("connection"),
                            }
                            if fw_errors >= 3 {
                                log("firmware error on 3 connection attempts; Wi-Fi stays down");
                                with_dev(|d| d.t.stop_device());
                                set_state("firmware error");
                                return;
                            }
                            // A clean slate for the next attempt: restart the runtime firmware.
                            if !restart_rt(&fw, &func, &mut hw, fw_err) { set_state("runtime firmware failed"); return; }
                            continue;
                        } else {
                            ilog!("network '{}' not found in this scan", wf::ssid_display(&c.0));
                        }
                    }
                }
            }
            Err(e) => {
                ilog!("scan failed: {:?}", e);
                if matches!(e, Err::FwError) { dump_fw_error("scan"); set_state("firmware error"); return; }
            }
        }
        let last_scan = now_ms();
        set_state(if creds.is_some() { "not connected (rescanning in 30 s)" } else { "up, no credentials (rescanning in 30 s)" });
        while now_ms() - last_scan < 30_000 {
            let _ = with_dev(|d| { d.service(); while let Some(p) = d.notifs.pop_front() { log_async(p); } });
            msleep(100);
        }
    }
}

/// Stop the device and bring the runtime image back up (after a failed or
/// lost association, or a firmware error).
fn restart_rt(fw: &Fw, func: &PciFunction, hw: &mut Hw, after_error: bool) -> bool {
    *GROUP_REKEY.lock() = None;
    for attempt in 1..=3 {
        with_dev(|d| { d.t.stop_device(); d.clear_link(); d.async_cmds.clear(); d.notifs.clear(); d.resps.clear(); });
        set_state("restarting firmware");
        match run_rt(fw, func, hw) {
            Ok(()) => { if after_error { log("runtime firmware restarted after the error"); } return true; }
            Err(e) => { ilog!("runtime firmware restart failed (attempt {}): {:?}", attempt, e); dump_fw_error("restart"); }
        }
        msleep(1000);
    }
    false
}

fn idle_forever() -> ! {
    loop {
        let _ = with_dev(|d| { d.service(); while let Some(p) = d.notifs.pop_front() { log_async(p); } });
        msleep(100);
    }
}

/// (SSID, passphrase or 64-hex PSK).
fn load_credentials(opts: &Opts) -> Option<(Vec<u8>, Vec<u8>)> {
    let mut ssid = None;
    let mut psk = None;
    let mut src = "";
    if let Ok(f) = crate::firmware::request_firmware("/etc/leandros/wifi.conf", 64 * 1024) {
        if let Some(v) = f.to_vec() {
            for line in v.split(|&c| c == b'\n') {
                let line = trim(line);
                if line.starts_with(b"#") { continue; }
                if let Some(r) = line.strip_prefix(b"ssid=") { ssid = Some(unquote(r).to_vec()); }
                if let Some(r) = line.strip_prefix(b"psk=") { psk = Some(unquote(r).to_vec()); }
            }
            src = "/etc/leandros/wifi.conf";
        }
    }
    if ssid.is_none() {
        if let Some(s) = opts.ssid.clone() { ssid = Some(s); psk = opts.psk.clone(); src = "kernel command line"; }
    }
    match ssid {
        Some(s) if !s.is_empty() && s.len() <= 32 => {
            let p = psk.unwrap_or_default();
            if !(p.len() >= 8 && p.len() <= 63) && !(p.len() == 64 && p.iter().all(|c| c.is_ascii_hexdigit())) {
                ilog!("credentials from {}: the passphrase must be 8..63 characters (or 64 hex digits); not connecting", src);
                return None;
            }
            ilog!("credentials from {}: SSID '{}' (passphrase not logged)", src, wf::ssid_display(&s));
            Some((s, p))
        }
        Some(_) => { ilog!("credentials from {}: invalid SSID length", src); None }
        None => { log("no credentials (/etc/leandros/wifi.conf or wifi.ssid=/wifi.psk=): scanning only"); None }
    }
}

fn trim(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && (s[a] == b' ' || s[a] == b'\t' || s[a] == b'\r') { a += 1; }
    while b > a && (s[b - 1] == b' ' || s[b - 1] == b'\t' || s[b - 1] == b'\r') { b -= 1; }
    &s[a..b]
}

fn unquote(s: &[u8]) -> &[u8] {
    if s.len() >= 2 && s[0] == b'"' && s[s.len() - 1] == b'"' { &s[1..s.len() - 1] } else { s }
}

/// The strongest BSS of `ssid` we can join with WPA2-PSK/CCMP (or open).
fn pick_bss(list: &[ScanEntry], ssid: &[u8]) -> Option<ScanEntry> {
    let mut best: Option<&ScanEntry> = None;
    for e in list.iter().filter(|e| e.bss.ssid == ssid) {
        let ok = wf::supports_wpa2_psk_ccmp(&e.bss);
        if !ok {
            ilog!("{} '{}' uses {} which this driver cannot join (WPA2-PSK/CCMP only)", fmt_mac(&e.bss.bssid),
                  wf::ssid_display(&e.bss.ssid), wf::security_summary(&e.bss));
            continue;
        }
        if best.map_or(true, |b| e.rssi > b.rssi) { best = Some(e); }
    }
    best.cloned()
}

// ── association (W3) ────────────────────────────────────────────────────────

const AKM_PSK: u32 = 0x000f_ac02;

/// Why `connect` returned.
enum Disc {
    /// Never got associated (or the handshake failed): rescan later.
    Failed(String),
    /// Was connected; the link went away.
    Lost(String),
    /// The firmware asserted: restart it.
    FwError,
}

impl From<Err> for Fail {
    fn from(e: Err) -> Fail {
        match e {
            Err::FwError => Fail::fw_assert("firmware error"),
            Err::Deauth(r) => Fail::new(reason::DEAUTH_BY_AP, format!("deauthenticated by the AP (reason {})", r)).ieee_reason(r),
            other => Fail::new(reason::FW_ERROR, format!("{:?}", other)),
        }
    }
}

fn pmk_for(ssid: &[u8], psk: &[u8]) -> [u8; 32] {
    let t0 = now_ms();
    let pmk = wm::pmk_for(ssid, psk);
    if !(psk.len() == 64 && psk.iter().all(|c| c.is_ascii_hexdigit())) {
        ilog!("PMK derived (PBKDF2-SHA1, 4096 iterations) in {} ms", now_ms() - t0);
    }
    pmk
}

/// Queue a management frame to the AP station at the band's lowest rate.
fn tx_mgmt(frame: &[u8], channel: u8) -> Res<()> {
    if frame.len() < 24 { return Err(Err::Other(String::from("short management frame"))); }
    let st = wf::subtype(wf::frame_control(frame).unwrap_or(0));
    let pm = if st == wf::fc::STYPE_ASSOC_REQ { asc::PM_FRAME_ASSOC } else { asc::PM_FRAME_MGMT };
    let p = TxParams { sta_id: AP_STA_ID, rate: Some(asc::mgmt_rate(channel)), tid: asc::TID_MGMT, retries: 15,
                       pm_frame_timeout: pm, ccmp_key: None };
    let txc = asc::tx_cmd(&p, frame.len(), &frame[..24]);
    with_dev(|d| d.t.tx(MGMT_QUEUE, &txc, &frame[24..], frame.len(), AP_STA_ID))
        .ok_or_else(|| Err::Other(String::from("no device")))??;
    Ok(())
}

/// Ethernet frame -> 802.11 data frame to the AP (CCMP once a key is set).
/// `port_control` marks EAPOL (sent in the clear before the keys exist).
fn tx_data(d: &mut Dev, eth: &[u8], port_control: bool) -> bool {
    let l = match d.link.as_mut() { Some(l) => l, None => return false };
    let mut buf = [0u8; 2400];
    let n = match wf::ethernet_to_data(eth, &l.bssid, &l.own, None, &mut buf[..2400 - 8]) { Some(n) => n, None => return false };
    let key = if port_control && l.tk.is_none() { None } else { l.tk };
    let (mpdu, len) = if let Some(_) = key {
        // Room for the CCMP header after the 24-byte header.
        buf.copy_within(24..n, 32);
        l.pn += 1;
        buf[24..32].copy_from_slice(&asc::ccmp_header(l.pn, 0));
        let fc = u16::from_le_bytes([buf[0], buf[1]]) | wf::fc::PROTECTED;
        buf[0..2].copy_from_slice(&fc.to_le_bytes());
        (&buf[..n + 8], n + 8)
    } else {
        (&buf[..n], n)
    };
    let p = TxParams {
        sta_id: AP_STA_ID,
        rate: if l.lq && !port_control { None } else { Some(l.fixed_rate) },
        tid: asc::TID_NON_QOS, retries: 15,
        pm_frame_timeout: if port_control { asc::PM_FRAME_MGMT } else { asc::PM_FRAME_NONE },
        ccmp_key: key,
    };
    let txc = asc::tx_cmd(&p, len, &mpdu[..24]);
    let bc = len + if key.is_some() { 8 } else { 0 };
    let used = d.t.txq[BSS_QUEUE].as_ref().map_or(0, |t| t.used());
    if used >= TXQ_CAP && !port_control {
        d.txs.on_enqueue(Err(EnqFail::Capped));
        return false;
    }
    let r = d.t.tx(BSS_QUEUE, &txc, &mpdu[24..], bc, AP_STA_ID);
    TXQ_DEPTH.store(d.t.txq[BSS_QUEUE].as_ref().map_or(0, |t| t.used()) as u32, Ordering::Relaxed);
    match r {
        Ok(_) => { d.txs.on_enqueue(Ok(())); true }
        Err(TransError::NotReady) => { d.txs.on_enqueue(Err(EnqFail::NotReady)); false }
        Err(_) => { d.txs.on_enqueue(Err(EnqFail::Busy)); false }
    }
}

/// Wait for a management frame from `bssid` of subtype `st` (a deauth or
/// disassoc from it ends the wait with `Err::Deauth`).
fn wait_mgmt(bssid: &[u8; 6], st: u16, timeout_ms: u64) -> Res<Vec<u8>> {
    let p = wait_notif(timeout_ms, |p| {
        p.group == 0xff && p.cmd == 0 && p.data.len() > 28
            && wf::frame_control(&p.data[4..]).map_or(false, |fc| wf::subtype(fc) == st || wf::subtype(fc) == wf::fc::STYPE_DEAUTH
                                                       || wf::subtype(fc) == wf::fc::STYPE_DISASSOC)
            && wf::addr3(&p.data[4..]).as_ref() == Some(bssid)
    }, log_async)?;
    let frame = p.data[4..].to_vec();
    let sub = wf::subtype(wf::frame_control(&frame).unwrap_or(0));
    if sub == wf::fc::STYPE_DEAUTH || sub == wf::fc::STYPE_DISASSOC {
        return Err(Err::Deauth(wf::parse_deauth_reason(&frame).unwrap_or(0)));
    }
    Ok(frame)
}

fn status_name(s: u16) -> &'static str {
    match s {
        0 => "success", 1 => "unspecified failure", 10 => "capabilities not supported",
        12 => "denied for another reason", 13 => "authentication algorithm not supported",
        17 => "AP full", 18 => "basic rates not supported", 30 => "try again later (PMF)",
        31 => "robust management frame policy violation", 40 => "invalid information element",
        41 => "invalid group cipher", 42 => "invalid pairwise cipher", 43 => "invalid AKMP",
        _ => "?",
    }
}

/// The legacy (iwlwifi.mgr=0) attempt: connect, then stay until the link
/// goes away.
fn connect(_fw: &Fw, hw: &mut Hw, target: &ScanEntry, ssid: &[u8], psk: &[u8], lq: bool) -> Disc {
    let b = &target.bss;
    let bssid = b.bssid;
    let ch = b.channel;
    ilog!("connecting to '{}' {} on channel {} ({} dBm, {})", wf::ssid_display(ssid), fmt_mac(&bssid), ch, target.rssi,
          wf::security_summary(b));
    if let Some(i) = INFO.lock().as_mut() { i.ssid = ssid.to_vec(); i.bssid = bssid; i.channel = ch; i.rssi = target.rssi; }
    let pmk = pmk_for(ssid, psk);
    let mut prog = Prog::default();
    match connect_steps(hw, b, target.rssi, Some(&pmk), lq, &|| false, &|_| {}, &mut prog) {
        Ok(()) => {}
        Err(f) if f.fw_assert => return Disc::FwError,
        Err(f) => return Disc::Failed(f.msg),
    }
    // Connected: stay here until the link goes away.
    LINK_UP.store(true, Ordering::SeqCst);
    LINK_GEN.fetch_add(1, Ordering::SeqCst);
    set_state("connected");
    ilog!("wlan: link up — '{}' {} channel {} (wifi0)",
          wf::ssid_display(ssid), fmt_mac(&bssid), ch);
    crate::netdev::kick();
    let r = monitor_link(&bssid);
    LINK_UP.store(false, Ordering::SeqCst);
    with_dev(|d| { d.clear_link(); d.rx_eth.clear(); });
    set_state("disconnected");
    r
}

/// What an attempt did to the firmware (what a teardown must undo).
#[derive(Default, Clone, Copy)]
struct Prog {
    /// PHY moved to the BSS channel, MAC bound to it.
    bound: bool,
    /// Session-protection time event (unique id) possibly still running.
    time_event: Option<u32>,
    /// The AP station exists (queues enabled).
    sta: bool,
    /// 802.11 authenticated (a deauthentication is due when leaving).
    authed: bool,
    /// MAC context associated, quota given.
    assoc: bool,
    bssid: [u8; 6],
    channel: u8,
}

/// Authenticate, associate and (with a PMK) run the WPA2 4-way handshake.
/// `aborted` is polled at every wait (a newer command preempts the
/// attempt: `Fail::superseded`); `step` reports progress
/// (ASSOCIATING, HANDSHAKE). Without a PMK the network is open: no RSN IE,
/// no handshake, no keys. `scan_rssi`: the BSS's signal in the scan (the
/// initial rate when no beacon was timed yet).
fn connect_steps(hw: &mut Hw, b: &wf::Bss, scan_rssi: i8, pmk: Option<&[u8; 32]>, lq: bool, aborted: &dyn Fn() -> bool,
                 step: &dyn Fn(u8), prog: &mut Prog) -> Result<(), Fail> {
    let bssid = b.bssid;
    let ch = b.channel;
    let band_24 = ch <= 14;
    prog.bssid = bssid;
    prog.channel = ch;
    // Our rates: the AP's legacy rates (BSS membership selectors dropped).
    let rates: Vec<u8> = b.rates.iter().copied().filter(|r| asc::legacy_rate(*r).is_some()).collect();
    if b.rates.iter().any(|&r| r & 0x7f == 127) {
        ilog!("note: the AP lists the 'HT PHY required' selector; this driver associates as a legacy (non-HT) station and may be refused");
    }
    if pmk.is_some() {
        match b.rsn.as_ref() {
            Some(r) => {
                if r.caps & (1 << 6) != 0 { return Err(Fail::new(reason::UNSUPPORTED_SECURITY, "the AP requires management frame protection (802.11w), not supported")); }
                if !r.akm.contains(&AKM_PSK) { return Err(Fail::new(reason::UNSUPPORTED_SECURITY, "the AP offers no plain PSK (00-0f-ac:2) AKM")); }
            }
            None => return Err(Fail::new(reason::UNSUPPORTED_SECURITY, "the AP has no RSN element (not WPA2)")),
        }
    }
    let (cck_ack, ofdm_ack) = asc::ack_rates(&b.rates, band_24);
    let mut ms = MacState::unassoc(hw.mac);
    ms.bssid = bssid;
    ms.beacon_int = b.beacon_interval as u32;
    ms.cck_ack = cck_ack;
    ms.ofdm_ack = ofdm_ack;
    ms.short_slot = !band_24 || b.capability & 0x0400 != 0;
    ms.short_preamble = band_24 && b.capability & 0x0020 != 0;
    // ERP (802.11g next to 802.11b stations): CTS-to-self protection, long
    // preamble when the AP asks for it.
    let erp = if band_24 { b.erp } else { None };
    ms.use_cts_prot = erp.map_or(false, |e| e & asc::ERP_USE_PROTECTION != 0);
    if erp.map_or(false, |e| e & asc::ERP_BARKER_PREAMBLE != 0) { ms.short_preamble = false; }

    // Channel, binding, quota (none until associated), MAC context with the BSSID.
    send(cmd::PHY_CONTEXT_CMD, None, &cmd::phy_ctxt_cmd(PHY_ID, 0, cmd::FW_CTXT_ACTION_MODIFY, ch, hw.tx_ant, hw.rx_ant))?;
    let r = send(cmd::BINDING_CONTEXT_CMD, None, &asc::binding_cmd(PHY_ID, cmd::FW_CTXT_ACTION_ADD, &[MAC_ID]))?;
    if cmd::le32(&r, 0).unwrap_or(0) != 0 { return Err(Err::Status("binding", cmd::le32(&r, 0).unwrap_or(0)).into()); }
    prog.bound = true;
    send(cmd::TIME_QUOTA_CMD, None, &asc::time_quota_cmd(None))?;
    mac_ctxt(cmd::FW_CTXT_ACTION_MODIFY, &ms)?;
    with_dev(|d| { d.track = Some(bssid); d.bcn = None; d.bcn_rssi = None; });

    // The AP station and its two queues (data TID 0, management TID 8).
    enable_queue(BSS_QUEUE, AP_STA_ID, asc::TID_NON_QOS, cmd::TX_FIFO_BE)?;
    enable_queue(MGMT_QUEUE, AP_STA_ID, asc::TID_MGMT, cmd::TX_FIFO_VO)?;
    let queues = (1u32 << BSS_QUEUE) | (1 << MGMT_QUEUE);
    add_sta(&cmd::AddSta {
        modify: false, sta_id: AP_STA_ID, mac_id_n_color: cmd::id_and_color(MAC_ID, 0), addr: bssid,
        station_type: cmd::STA_LINK, tid_disable_tx: 0xffff, modify_mask: 0, station_flags: 0,
        station_flags_msk: 0, assoc_id: 0, tfd_queue_msk: queues,
    }, "AP station")?;
    prog.sta = true;
    with_dev(|d| {
        d.link = Some(LinkTx { bssid, own: hw.mac, tk: None, pn: 0, lq, fixed_rate: asc::legacy_rate(24).unwrap() });
        d.guard = Some(wf::RxGuard::new(bssid, hw.mac));
        d.rc = None;
        d.fail_logged = 0;
        d.erp = erp;
        d.erp_changed = false;
    });

    // Session protection: stay on the channel through auth/assoc/EAPOL.
    let r = send(cmd::TIME_EVENT_CMD, None, &asc::time_event_cmd(MAC_ID, cmd::FW_CTXT_ACTION_ADD, 0, 2000, 500))?;
    match asc::parse_time_event_resp(&r) {
        Some((0, uid)) => {
            prog.time_event = Some(uid);
            match wait_notif(1000, |p| p.group <= 1 && p.cmd == cmd::TIME_EVENT_NOTIFICATION, log_async) {
                Ok(p) => {
                    let (u, action, status) = asc::parse_time_event_notif(&p.data).unwrap_or((0, 0, 0));
                    if status != 1 || u != uid { ilog!("time event {:#x}: action {:#x} status {} (expected start of {:#x})", u, action, status, uid); }
                }
                Err(Err::FwError) => return Err(Fail::fw_assert("firmware error waiting for session protection")),
                Err(e) => ilog!("session protection did not start ({:?}); trying anyway", e),
            }
        }
        other => ilog!("time event refused: {:?}; trying anyway", other),
    }
    if aborted() { return Err(Fail::superseded()); }

    // Open System authentication.
    let mut authed = false;
    for attempt in 1..=3 {
        tx_mgmt(&wf::build_auth_open(&bssid, &hw.mac), ch)?;
        match wait_mgmt(&bssid, wf::fc::STYPE_AUTH, 400) {
            Ok(f) => match wf::parse_auth(&f) {
                Some(a) if a.seq == 2 && a.status == 0 => { authed = true; break; }
                Some(a) => return Err(Fail::new(reason::AUTH_REJECTED, format!("authentication refused: status {} ({}), alg {} seq {}",
                                                                         a.status, status_name(a.status), a.alg, a.seq)).status(a.status)),
                None => {}
            },
            Err(Err::Timeout(_)) => ilog!("no authentication response (attempt {})", attempt),
            Err(Err::Deauth(r)) => return Err(Fail::new(reason::AUTH_REJECTED, format!("deauthenticated during authentication (reason {})", r)).ieee_reason(r)),
            Err(e) => return Err(e.into()),
        }
        if aborted() { return Err(Fail::superseded()); }
    }
    if !authed { return Err(Fail::new(reason::TIMEOUT, "authentication timed out")); }
    prog.authed = true;
    log("authenticated (Open System)");
    step(wifi_state::ASSOCIATING);
    if aborted() { return Err(Fail::superseded()); }

    // Association.
    let rsn = crate::wifi_crypto::rsn_ie_psk_ccmp();
    let cap = 0x0001 | if pmk.is_some() { 0x0010 } else { 0 } | if band_24 { b.capability & 0x0420 } else { 0 };
    let req = wf::build_assoc_req(&bssid, &hw.mac, cap, 10, &b.ssid, &rates, if pmk.is_some() { Some(&rsn[..]) } else { None }, None);
    let mut resp = None;
    for attempt in 1..=3 {
        tx_mgmt(&req, ch)?;
        match wait_mgmt(&bssid, wf::fc::STYPE_ASSOC_RESP, 500) {
            Ok(f) => { resp = wf::parse_assoc_resp(&f); if resp.is_some() { break; } }
            Err(Err::Timeout(_)) => ilog!("no association response (attempt {})", attempt),
            Err(Err::Deauth(r)) => return Err(Fail::new(reason::ASSOC_REJECTED, format!("deauthenticated during association (reason {})", r)).ieee_reason(r)),
            Err(e) => return Err(e.into()),
        }
        if aborted() { return Err(Fail::superseded()); }
    }
    let resp = resp.ok_or_else(|| Fail::new(reason::TIMEOUT, "association timed out"))?;
    if resp.status != 0 {
        return Err(Fail::new(reason::ASSOC_REJECTED, format!("association refused: status {} ({})", resp.status, status_name(resp.status))).status(resp.status));
    }
    let aid = resp.aid & 0x3fff;
    ilog!("associated: AID {}, AP capability {:#06x}", aid, resp.capability);

    // Station associated, its rate table, the MAC context associated.
    add_sta(&cmd::AddSta {
        modify: true, sta_id: AP_STA_ID, mac_id_n_color: cmd::id_and_color(MAC_ID, 0), addr: bssid,
        station_type: cmd::STA_LINK, tid_disable_tx: 0xffff, modify_mask: 0, station_flags: 0,
        station_flags_msk: (3 << 26) | (3 << 28), assoc_id: aid, tfd_queue_msk: queues,
    }, "AP station (associated)")?;
    // The slot time the AP grants in its response.
    if band_24 { ms.short_slot = resp.capability & 0x0400 != 0; }
    let ap_rates: Vec<u8> = b.rates.iter().chain(resp.rates.iter()).copied().collect();
    let maxrate = MAXRATE.load(Ordering::Relaxed);
    if RC_ON.load(Ordering::Relaxed) {
        let rssi = with_dev(|d| d.bcn_rssi).flatten().or(if scan_rssi != 0 { Some(scan_rssi) } else { None });
        let txant = TXANT.load(Ordering::Relaxed);
        if txant != 0 && hw.tx_ant & txant == 0 { ilog!("iwlwifi.txant: antenna mask {:#x} is not valid (valid {:#x}); automatic", txant, hw.tx_ant); }
        let cfg = rs::RateCfg { ap_rates: ap_rates.clone(), max_r500k: maxrate, valid_ant: hw.tx_ant,
                                fixed_ant: if txant != 0 { Some(txant) } else { None }, band_24, rssi };
        let mut c = RateCtl::new(&cfg);
        send(cmd::LQ_CMD, None, &c.lq(AP_STA_ID))?;
        c.mark_sent(now_ms());
        let list: Vec<String> = c.rates.iter().map(|&r| rs::mbps(r)).collect();
        ilog!("rate control: start at {} Mb/s on antenna {} (signal {:?} dBm); rates {} Mb/s, chain floor {} Mb/s; ERP {:?}, short slot {}",
              rs::mbps(c.cur_r500k()), if c.ant == asc::ANT_B { "B" } else { "A" }, rssi, list.join("/"),
              rs::mbps(c.rates[c.floor]), erp, ms.short_slot);
        with_dev(|d| d.rc = Some(c));
    } else {
        send(cmd::LQ_CMD, None, &asc::lq_cmd_legacy(AP_STA_ID, &ap_rates, maxrate))?;
        ilog!("rate control off (iwlwifi.rc=0): fixed table; ERP {:?}, short slot {}", erp, ms.short_slot);
    }
    // DTIM timing from a recent beacon (wait briefly for one).
    let t0 = now_ms();
    let mut bcn = None;
    while now_ms() - t0 < 300 {
        bcn = with_dev(|d| { d.service(); d.bcn }).flatten();
        if bcn.is_some() { break; }
        msleep(5);
    }
    let bcn = bcn.unwrap_or(BeaconSync { tsf: b.tsf, sys_ts: 0, dtim_count: 0, dtim_period: b.dtim_period.max(1), beacon_int: b.beacon_interval });
    let offs = bcn.dtim_count as u64 * bcn.beacon_int as u64 * 1024;
    ms.assoc = true;
    ms.aid = aid;
    ms.dtim_period = bcn.dtim_period.max(1) as u32;
    ms.beacon_int = bcn.beacon_int as u32;
    ms.dtim_tsf = bcn.tsf + offs;
    ms.dtim_time = bcn.sys_ts.wrapping_add(offs as u32);
    ms.beacon_arrive_time = bcn.sys_ts;
    mac_ctxt(cmd::FW_CTXT_ACTION_MODIFY, &ms)?;
    *ASSOC_MS.lock() = Some(AssocMac { ms: ms.clone(), band_24, cap_short_preamble: band_24 && resp.capability & 0x0020 != 0 });
    send(cmd::TIME_QUOTA_CMD, None, &asc::time_quota_cmd(Some(PHY_ID)))?;
    send(cmd::REPLY_SF_CFG_CMD, None, &cmd::sf_cfg_cmd(cmd::SF_FULL_ON, Some(4096)))?;
    prog.assoc = true;

    let pmk = match pmk {
        Some(p) => p,
        None => { log("open network: no handshake"); return Ok(()); }
    };
    // WPA2 4-way handshake.
    step(wifi_state::HANDSHAKE);
    let mut snonce = [0u8; 32];
    sched::random::fill(&mut snonce);
    let mut sup = crate::wifi_crypto::Supplicant::new(*pmk, hw.mac, bssid, rsn.to_vec(), snonce);
    let t1 = now_ms();
    // EAPOL-Key frames we sent before completion (>= 1: message 2 went out).
    let mut sent = 0u32;
    while !sup.is_complete() {
        if aborted() { return Err(Fail::superseded()); }
        let left = 6000u64.saturating_sub(now_ms() - t1);
        if left == 0 {
            ilog!("4-way handshake timed out after {} message(s) to the AP", sent);
            return Err(wm::handshake_failure(sent > 0, None));
        }
        let p = match wait_notif(left.min(100), |p| p.group == 0xff && (p.cmd == 1 || p.cmd == 0), log_async) {
            Ok(p) => p,
            Err(Err::Timeout(_)) => continue,
            Err(e) => return Err(e.into()),
        };
        if p.cmd == 0 {
            // A management frame meanwhile: deauth / disassoc ends it.
            let f = &p.data[4..];
            let st = wf::subtype(wf::frame_control(f).unwrap_or(0));
            if (st == wf::fc::STYPE_DEAUTH || st == wf::fc::STYPE_DISASSOC) && wf::addr3(f) == Some(bssid) {
                let r = wf::parse_deauth_reason(f).unwrap_or(0);
                ilog!("AP ended the association during the handshake (reason {}){}", r,
                      if r == 15 { ": 4-way handshake timeout, likely a wrong passphrase" } else { "" });
                prog.authed = false;
                return Err(wm::handshake_failure(sent > 0, Some(r)));
            }
            continue;
        }
        handle_eapol(&mut sup, &p.data, &mut sent)?;
    }
    log("4-way handshake complete; keys installed");
    *GROUP_REKEY.lock() = Some(sup);
    Ok(())
}

/// Feed one EAPOL frame (Ethernet II) to the supplicant and act on it.
/// `sent` counts the EAPOL-Key frames transmitted.
fn handle_eapol(sup: &mut crate::wifi_crypto::Supplicant, eth: &[u8], sent: &mut u32) -> Res<()> {
    if eth.len() < 14 { return Ok(()); }
    let src = &eth[0..6];
    for ev in sup.on_eapol(&eth[14..]) {
        use crate::wifi_crypto::HsEvent;
        match ev {
            HsEvent::Send(pdu) => {
                let mut f = Vec::with_capacity(14 + pdu.len());
                f.extend_from_slice(&eth[6..12]); // to the AP
                f.extend_from_slice(src);         // from us
                f.extend_from_slice(&crate::wifi_crypto::ETHERTYPE_EAPOL.to_be_bytes());
                f.extend_from_slice(&pdu);
                if !with_dev(|d| tx_data(d, &f, true)).unwrap_or(false) { log("EAPOL transmit failed"); } else { *sent += 1; }
            }
            HsEvent::InstallPtk(tk) => {
                // Let message 4 leave in the clear before the key changes TX.
                msleep(5);
                let r = send(cmd::ADD_STA_KEY, None, &asc::add_sta_key_ccmp(AP_STA_ID, 0, 0, &tk, false))?;
                let st = cmd::le32(&r, 0).unwrap_or(0) & 0xff;
                if st != cmd::ADD_STA_SUCCESS { return Err(Err::Status("pairwise key", st)); }
                with_dev(|d| {
                    if let Some(l) = d.link.as_mut() { l.tk = Some(tk); l.pn = 0; }
                    if let Some(g) = d.guard.as_mut() { g.reset_key(0); }
                });
                log("pairwise key (CCMP) installed");
            }
            HsEvent::InstallGtk { key_id, key, rsc: _ } => {
                if key.len() != 16 { return Err(Err::Other(String::from("group key is not CCMP-128"))); }
                let mut k = [0u8; 16];
                k.copy_from_slice(&key);
                let r = send(cmd::ADD_STA_KEY, None, &asc::add_sta_key_ccmp(AP_STA_ID, 1 + (key_id & 1), key_id, &k, true))?;
                let st = cmd::le32(&r, 0).unwrap_or(0) & 0xff;
                if st != cmd::ADD_STA_SUCCESS { return Err(Err::Status("group key", st)); }
                with_dev(|d| if let Some(g) = d.guard.as_mut() { g.reset_key(key_id); });
                ilog!("group key {} (CCMP) installed", key_id);
            }
            HsEvent::Complete => {}
            HsEvent::Error(e) => ilog!("EAPOL: {}", e),
        }
    }
    Ok(())
}

/// Last time the connected-link statistics line was logged.
static LAST_STATS_MS: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// The associated MAC context, kept so an ERP change in the beacons can
/// update its protection and preamble.
#[derive(Clone)]
struct AssocMac {
    ms: MacState,
    band_24: bool,
    /// The AP's capability allows short preamble.
    cap_short_preamble: bool,
}
static ASSOC_MS: Mutex<Option<AssocMac>> = Mutex::new(None);

/// The beacons' ERP element changed: protection / preamble into the MAC context.
fn update_erp() {
    let erp = match with_dev(|d| d.erp) { Some(e) => e, None => return };
    let ms = {
        let mut g = ASSOC_MS.lock();
        let a = match g.as_mut() { Some(a) if a.band_24 => a, _ => return };
        let cts = erp.map_or(false, |e| e & asc::ERP_USE_PROTECTION != 0);
        let sp = a.cap_short_preamble && !erp.map_or(false, |e| e & asc::ERP_BARKER_PREAMBLE != 0);
        if a.ms.use_cts_prot == cts && a.ms.short_preamble == sp { return; }
        ilog!("ERP changed ({:?}): CTS protection {} -> {}, short preamble {} -> {}", erp, a.ms.use_cts_prot, cts,
              a.ms.short_preamble, sp);
        a.ms.use_cts_prot = cts;
        a.ms.short_preamble = sp;
        a.ms.clone()
    };
    if let Err(e) = mac_ctxt(cmd::FW_CTXT_ACTION_MODIFY, &ms) { ilog!("MAC context update (ERP) failed: {:?}", e); }
}

/// Data / management queue occupancy and the age of their last TX response.
fn queue_line(d: &Dev, now: u64) -> String {
    let mut s = String::new();
    for (slot, q) in [(0usize, BSS_QUEUE), (1, MGMT_QUEUE)] {
        if let Some(t) = d.t.txq[q].as_ref() {
            let age = if d.last_txrsp_ms[slot] == 0 { String::from("never") } else { format!("{} ms", now - d.last_txrsp_ms[slot]) };
            s.push_str(&format!("{}q{} used {} read {} write {} last TX_RSP {}", if slot == 0 { "" } else { "; " }, q, t.used(),
                                t.read, t.write, age));
        }
    }
    s
}

/// Connected: handle group rekeys, deauthentication, missed beacons for up
/// to `budget_ms`. `Some` when the link went away.
fn monitor_once(bssid: &[u8; 6], budget_ms: u64) -> Option<Fail> {
    let t0 = now_ms();
    loop {
        let spent = now_ms() - t0;
        if spent >= budget_ms { break; }
        // Rate control's table (when the data path went quiet before it went out) and ERP changes.
        let erp_changed = with_dev(|d| { d.send_lq_if_due(now_ms()); core::mem::replace(&mut d.erp_changed, false) }).unwrap_or(false);
        if erp_changed { update_erp(); }
        match wait_notif((budget_ms - spent).min(100), |_| true, |_| {}) {
            Ok(p) => {
                if p.group == 0xff && p.cmd == 0 {
                    let f = &p.data[4..];
                    let st = wf::subtype(wf::frame_control(f).unwrap_or(0));
                    if (st == wf::fc::STYPE_DEAUTH || st == wf::fc::STYPE_DISASSOC) && wf::addr3(f).as_ref() == Some(bssid) {
                        let r = wf::parse_deauth_reason(f).unwrap_or(0);
                        return Some(Fail::new(reason::DEAUTH_BY_AP, format!("{} by the AP (reason {})",
                            if st == wf::fc::STYPE_DEAUTH { "deauthenticated" } else { "disassociated" }, r)).ieee_reason(r));
                    }
                } else if p.group == 0xff && p.cmd == 1 {
                    // EAPOL after completion: group rekey.
                    let mut n = 0;
                    let r = GROUP_REKEY.lock().as_mut().map(|s| handle_eapol(s, &p.data, &mut n));
                    if let Some(Err(e)) = r { ilog!("group rekey failed: {:?}", e); }
                } else if p.group <= 1 && p.cmd == cmd::MISSED_BEACONS_NOTIFICATION {
                    let since_rx = cmd::le32(&p.data, 4).unwrap_or(0);
                    let consec = cmd::le32(&p.data, 8).unwrap_or(0);
                    if since_rx >= 30 {
                        return Some(Fail::new(reason::BEACON_LOSS, format!("{} beacons missed ({} since the last frame)", consec, since_rx)));
                    }
                } else {
                    log_async(p);
                }
            }
            Err(Err::FwError) => return Some(Fail::fw_assert("firmware error while associated")),
            Err(_) => {}
        }
    }
    let now = now_ms();
    let last = LAST_STATS_MS.load(Ordering::Relaxed);
    if last == 0 { LAST_STATS_MS.store(now, Ordering::Relaxed); }
    else if now - last > 60_000 {
        LAST_STATS_MS.store(now, Ordering::Relaxed);
        if let Some((rx, rxd, txs, delta, rxs, state)) = with_dev(|d| {
            let delta = d.txs.delta(&d.txs_mark);
            d.txs_mark = d.txs;
            let rc = d.rc.as_ref().map_or(String::from("rc off"), |c| c.summary(now));
            (d.rx_frames, d.rx_dropped, d.txs, delta, d.rxs, format!("{}; {}; holds {}", rc, queue_line(d, now),
                                                                      TX_HOLDS.load(Ordering::Relaxed)))
        }) {
            ilog!("wlan stats: rx {} (dropped {}), tx {} (failed {}); rx filtered: {}", rx, rxd, txs.ok, txs.failed_total(), rxs.line());
            ilog!("wlan tx 60 s: {}", delta.line());
            ilog!("wlan tx state: {}", state);
        }
    }
    None
}

/// Connected (iwlwifi.mgr=0): stay until the link goes away.
fn monitor_link(bssid: &[u8; 6]) -> Disc {
    LAST_STATS_MS.store(now_ms(), Ordering::Relaxed);
    loop {
        if let Some(f) = monitor_once(bssid, 1000) {
            return if f.fw_assert { Disc::FwError } else { Disc::Lost(f.msg) };
        }
    }
}

/// The completed supplicant, kept for group-key rekeys.
static GROUP_REKEY: Mutex<Option<crate::wifi_crypto::Supplicant>> = Mutex::new(None);

// ── connection manager (iwlwifi.mgr=1) ──────────────────────────────────────

/// The firmware side of `wifi_mgr`: scans, attempts, the connected link and
/// the clean-up after leaving.
struct IwlRadio {
    fw: Fw,
    hw: Hw,
    func: PciFunction,
    opts: Opts,
    /// What the current / last attempt set up in the firmware.
    prog: Prog,
    /// Firmware errors since the last good association.
    fw_errors: u32,
    radio_off: bool,
}

/// iwlwifi.mgr=1: hand the running firmware to the connection manager.
fn manager(fw: Fw, hw: Hw, func: PciFunction, opts: Opts) {
    // The legacy network (wifi.conf / wifi.ssid=) as a profile, used until
    // /dev/netctl publishes the stored profiles (which then include it).
    let mut fallback = Vec::new();
    if opts.connect {
        if let Some((ssid, psk)) = load_credentials(&opts) {
            match netctl_abi::profiles::legacy_profile(&ssid, &psk) {
                Some(p) => fallback.push(p),
                None => log("legacy credentials not usable as a profile"),
            }
        }
    }
    MGR.set_fallback_profiles(fallback);
    let mut r = IwlRadio { fw, hw, func, opts, prog: Prog::default(), fw_errors: 0, radio_off: false };
    set_state("up (manager)");
    wm::run(&MGR, &mut r);
    set_state("down (firmware failed)");
    log("connection manager stopped: Wi-Fi stays down");
}

impl IwlRadio {
    /// Undo what `prog` says an attempt set up, the way iwlmvm leaves a BSS:
    /// end the session-protection event, flush the station's queues, MAC
    /// context back to unassociated, no quota, disable the queues, remove
    /// the station, unbind the MAC from the PHY context.
    fn soft_teardown(&mut self) -> Res<()> {
        let p = self.prog;
        if let Some(uid) = p.time_event {
            // It may have ended on its own; the firmware then answers an error.
            if let Err(e) = send(cmd::TIME_EVENT_CMD, None, &asc::time_event_cmd(MAC_ID, cmd::FW_CTXT_ACTION_REMOVE, uid, 0, 0)) {
                if matches!(e, Err::FwError) { return Err(e); }
            }
        }
        if p.sta {
            // TXPATH_FLUSH v1: queues, flush_ctl DUMP_TX_FIFO_FLUSH, reserved.
            let mut f = Vec::with_capacity(8);
            f.extend_from_slice(&((1u32 << BSS_QUEUE) | (1 << MGMT_QUEUE)).to_le_bytes());
            f.extend_from_slice(&2u16.to_le_bytes());
            f.extend_from_slice(&0u16.to_le_bytes());
            send(cmd::TXPATH_FLUSH, None, &f)?;
        }
        if p.bound {
            mac_ctxt(cmd::FW_CTXT_ACTION_MODIFY, &MacState::unassoc(self.hw.mac))?;
            send(cmd::TIME_QUOTA_CMD, None, &asc::time_quota_cmd(None))?;
        }
        if p.sta {
            for (q, tid, fifo) in [(BSS_QUEUE, asc::TID_NON_QOS, cmd::TX_FIFO_BE), (MGMT_QUEUE, asc::TID_MGMT, cmd::TX_FIFO_VO)] {
                let ssn = with_dev(|d| d.t.txq[q].as_ref().map_or(0, |t| t.write) as u16).unwrap_or(0);
                send(cmd::SCD_QUEUE_CFG, None, &cmd::scd_queue_cfg_cmd(q as u8, false, AP_STA_ID, tid, fifo, ssn))?;
            }
            send(cmd::REMOVE_STA, None, &[AP_STA_ID, 0, 0, 0])?;
        }
        if p.bound {
            let r = send(cmd::BINDING_CONTEXT_CMD, None, &asc::binding_cmd(PHY_ID, cmd::FW_CTXT_ACTION_REMOVE, &[]))?;
            let st = cmd::le32(&r, 0).unwrap_or(0);
            if st != 0 { return Err(Err::Status("unbind", st)); }
        }
        Ok(())
    }

    fn restart(&mut self, after_error: bool) -> bool {
        let ok = restart_rt(&self.fw, &self.func, &mut self.hw, after_error);
        self.prog = Prog::default();
        if ok { set_state("up (manager)"); }
        ok
    }
}

impl wm::Radio for IwlRadio {
    fn caps(&self) -> u32 {
        let mut c = wifi_caps::PSK | wifi_caps::BAND_2GHZ | wifi_caps::DIRECTED_SCAN;
        if self.hw.nvm.band_52 { c |= wifi_caps::BAND_5GHZ; }
        if self.opts.scanconn { c |= wifi_caps::SCAN_CONNECTED; }
        c
    }

    fn scan(&mut self, ssids: &[Vec<u8>], connected: bool) -> Result<Vec<wm::ScanHit>, Fail> {
        set_state(if connected { "connected (scanning)" } else { "scanning" });
        let refs: Vec<&[u8]> = ssids.iter().map(|s| &s[..]).collect();
        let r = scan(&self.fw, &self.hw, &refs);
        set_state(if connected { "connected" } else { "up (manager)" });
        match r {
            Ok(mut list) => {
                log_bss_list(&mut list);
                if let Some(i) = INFO.lock().as_mut() { i.bss = list.clone(); i.scan_at_ms = now_ms(); }
                Ok(list.into_iter().map(|e| wm::ScanHit { bss: e.bss, rssi: e.rssi }).collect())
            }
            Err(Err::FwError) => { dump_fw_error("scan"); Err(Fail::fw_assert("firmware error during the scan")) }
            Err(e) => Err(e.into()),
        }
    }

    fn connect(&mut self, hit: &wm::ScanHit, ssid: &[u8], akm_: u8, secret: &[u8], cx: &wm::ConnCtx) -> Result<(), Fail> {
        let b = &hit.bss;
        if let Some(i) = INFO.lock().as_mut() { i.ssid = ssid.to_vec(); i.bssid = b.bssid; i.channel = b.channel; i.rssi = hit.rssi; }
        set_state("connecting");
        let pmk = if akm_ == netctl_abi::akm::NONE { None } else { Some(pmk_for(ssid, secret)) };
        if cx.aborted() { return Err(Fail::superseded()); }
        self.prog = Prog::default();
        let mut prog = Prog::default();
        let r = connect_steps(&mut self.hw, b, hit.rssi, pmk.as_ref(), self.opts.lq, &|| cx.aborted(), &|s| cx.step(s), &mut prog);
        self.prog = prog;
        if let Err(f) = &r {
            if f.fw_assert { dump_fw_error("connection"); }
            return r;
        }
        self.fw_errors = 0;
        LINK_UP.store(true, Ordering::SeqCst);
        LINK_GEN.fetch_add(1, Ordering::SeqCst);
        LAST_STATS_MS.store(now_ms(), Ordering::Relaxed);
        set_state("connected");
        ilog!("wlan: link up — '{}' {} channel {}", wf::ssid_display(ssid), fmt_mac(&b.bssid), b.channel);
        crate::netdev::kick();
        Ok(())
    }

    fn poll_connected(&mut self, budget_ms: u64) -> Option<Fail> {
        let bssid = self.prog.bssid;
        let r = monitor_once(&bssid, budget_ms);
        if let Some(f) = &r { if f.fw_assert { dump_fw_error("connection"); } }
        r
    }

    fn leave(&mut self, deauth: bool) {
        LINK_UP.store(false, Ordering::SeqCst);
        if deauth && self.prog.authed {
            // Deauthentication, reason 3 (leaving), before the station goes.
            match tx_mgmt(&wf::build_deauth(&self.prog.bssid, &self.hw.mac, wm::IEEE_REASON_LEAVING), self.prog.channel) {
                Ok(()) => {
                    // Give it a moment on air (its TX status comes back on the ring).
                    let t0 = now_ms();
                    while now_ms() - t0 < 20 { with_dev(|d| d.service()); msleep(2); }
                }
                Err(e) => ilog!("deauthentication not sent: {:?}", e),
            }
            self.prog.authed = false;
        }
        with_dev(|d| { d.clear_link(); d.bcn = None; d.bcn_rssi = None; d.rx_eth.clear(); });
        *GROUP_REKEY.lock() = None;
        set_state("disconnected");
    }

    fn settle(&mut self, after: Option<&Fail>) -> bool {
        if self.radio_off { return true; }
        let fw_assert = after.map_or(false, |f| f.fw_assert);
        if fw_assert {
            self.fw_errors += 1;
            if self.fw_errors >= 3 {
                log("firmware error on 3 consecutive attempts; Wi-Fi stays down");
                with_dev(|d| d.t.stop_device());
                set_state("firmware error");
                return false;
            }
            return self.restart(true);
        }
        let p = self.prog;
        if !(p.bound || p.sta || p.time_event.is_some()) { return true; }
        if !self.opts.softdisc { return self.restart(false); }
        let t0 = now_ms();
        match self.soft_teardown() {
            Ok(()) => {
                self.prog = Prog::default();
                ilog!("left the BSS without a firmware restart ({} ms)", now_ms() - t0);
                true
            }
            Err(e) => {
                ilog!("teardown failed ({:?}); restarting the firmware", e);
                if matches!(e, Err::FwError) { dump_fw_error("teardown"); }
                self.restart(matches!(e, Err::FwError))
            }
        }
    }

    fn idle(&mut self, ms: u64) {
        let t0 = now_ms();
        while now_ms() - t0 < ms {
            let _ = with_dev(|d| { d.service(); while let Some(p) = d.notifs.pop_front() { log_async(p); } });
            msleep(10);
        }
    }

    fn set_radio(&mut self, on: bool) -> bool {
        if on == !self.radio_off { return true; }
        if !on {
            with_dev(|d| { d.t.stop_device(); d.clear_link(); d.async_cmds.clear(); d.notifs.clear(); d.resps.clear(); });
            LINK_UP.store(false, Ordering::SeqCst);
            self.radio_off = true;
            self.prog = Prog::default();
            set_state("radio off");
            log("radio off (firmware stopped)");
            true
        } else {
            self.radio_off = false;
            log("radio on: starting the firmware");
            self.restart(false)
        }
    }

    fn hw_rfkill(&mut self) -> bool {
        let on = with_dev(|d| d.t.rfkill_on()).unwrap_or(false);
        if let Some(i) = INFO.lock().as_mut() { i.rfkill = on; }
        on
    }

    fn rfkill_released(&mut self) -> bool {
        if self.radio_off { return true; }
        // The firmware does not survive the switch: start it again.
        self.restart(false)
    }

    fn signal(&mut self) -> Option<i8> { with_dev(|d| d.bcn_rssi).flatten() }
}

/// Register the netctl seam for this interface (iwlwifi.mgr=1 only; with
/// mgr=0 nothing is served and boot behaviour is unchanged).
pub fn attach(ifindex: u32, name: &str) -> bool {
    if !present() || !MGR_ON.load(Ordering::Relaxed) { return false; }
    MGR.attach(ifindex, name)
}

/// iwlwifi.mgr=1 is on.
pub fn manager_on() -> bool { MGR_ON.load(Ordering::Relaxed) }

// ── netdev interface ────────────────────────────────────────────────────────

/// A supported Wi-Fi function was found and probed.
pub fn present() -> bool { PRESENT.load(Ordering::Relaxed) }

/// Associated with keys installed: frames can flow.
pub fn link_up() -> bool { LINK_UP.load(Ordering::Relaxed) }

pub fn link_generation() -> u32 { LINK_GEN.load(Ordering::Relaxed) }

pub fn mac_address() -> Option<[u8; 6]> { if present() { Some(*MAC.lock()) } else { None } }

/// Room in the data queue below the high-water mark (backpressure for the
/// net server). Lock-free; a false answer is remembered so the drain kicks
/// the daemon.
pub fn tx_ready() -> bool {
    if !link_up() || TXQ_DEPTH.load(Ordering::Relaxed) < TXQ_HIGH { return true; }
    if !TX_HELD.swap(true, Ordering::AcqRel) { TX_HOLDS.fetch_add(1, Ordering::Relaxed); }
    false
}

pub fn send_packet(eth: &[u8]) -> bool {
    if !link_up() { return false; }
    let mut g = DEV.lock();
    match g.as_mut() { Some(d) => tx_data(d, eth, false), None => false }
}

pub fn poll_receive(buf: &mut [u8]) -> Option<usize> {
    if !link_up() { return None; }
    let mut g = DEV.try_lock()?;
    let d = g.as_mut()?;
    d.service();
    let f = d.rx_eth.pop_front()?;
    let n = f.len().min(buf.len());
    buf[..n].copy_from_slice(&f[..n]);
    Some(n)
}

/// `key: value` lines for /proc/net/nic.
pub fn info_lines(out: &mut String) {
    use core::fmt::Write;
    let info = INFO.lock().clone();
    let _ = writeln!(out, "driver: iwlwifi");
    if let Some(i) = info {
        let _ = writeln!(out, "device: {} at {:02x}:{:02x}.{}", i.name, i.bdf.0, i.bdf.1, i.bdf.2);
        let _ = writeln!(out, "hw_rev: {:08x} rf_id: {:08x}", i.hw_rev, i.rf_id);
        let _ = writeln!(out, "firmware: {}", i.fw);
        let _ = writeln!(out, "state: {}", i.state);
        let _ = writeln!(out, "rfkill: {}", if i.rfkill { "on" } else { "off" });
        if i.mcc != [0, 0] { let _ = writeln!(out, "regulatory: {}{}", i.mcc[0] as char, i.mcc[1] as char); }
        if link_up() {
            let _ = writeln!(out, "ssid: {}", wf::ssid_display(&i.ssid));
            let _ = writeln!(out, "bssid: {} channel {} signal {} dBm", fmt_mac(&i.bssid), i.channel, i.rssi);
        }
    }
    let _ = writeln!(out, "mac: {}", fmt_mac(&*MAC.lock()));
    let _ = writeln!(out, "link: {}", if link_up() { "up" } else { "down" });
    if let Some(d) = DEV.try_lock() {
        if let Some(d) = d.as_ref() {
            let _ = writeln!(out, "rx_frames: {} rx_dropped: {} tx_frames: {} tx_failed: {} fw_rx_packets: {}",
                             d.rx_frames, d.rx_dropped, d.txs.ok, d.txs.failed_total(), d.t.rx_packets);
            let _ = writeln!(out, "rx_filtered: {}", d.rxs.line());
            let _ = writeln!(out, "tx_counts: enq {} ok {} fail_fw {} busy {} notready {} capped {}", d.txs.enq, d.txs.ok,
                             d.txs.fail_fw, d.txs.busy, d.txs.notready, d.txs.capped);
            if let Some(c) = d.rc.as_ref() { let _ = writeln!(out, "tx_rate: {}", c.summary(now_ms())); }
        }
    }
    if manager_on() { MGR.info_lines(out); }
}

/// The last scan as a table (for /proc/net/wifi).
pub fn scan_lines(out: &mut String) {
    // No Intel radio but the simulated one runs: show its list here, so
    // /proc/net/wifi and /bin/wifiscan work for it too.
    if !present() && crate::wifi_sim::present() { return crate::wifi_sim::scan_lines(out); }
    if manager_on() {
        let name = INFO.lock().as_ref().map_or("iwlwifi", |i| i.name);
        MGR.scan_lines(name, out);
        return tx_lines(out);
    }
    scan_table(out);
    tx_lines(out);
}

/// TX counters, histograms, rate control and queues, after the scan table:
/// a blank line, a `# TX` header, then `key: value` lines (none of them
/// looks like a BSSID row).
fn tx_lines(out: &mut String) {
    use core::fmt::Write;
    let g = match DEV.try_lock() { Some(g) => g, None => return };
    let d = match g.as_ref() { Some(d) => d, None => return };
    let now = now_ms();
    let _ = writeln!(out);
    let _ = writeln!(out, "# TX ({}; since boot)", if link_up() { "associated" } else { "not associated" });
    d.txs.detail(out);
    let _ = writeln!(out, "tx_hold: now {} episodes {} (stack held at {} queued frames, cap {})", TX_HELD.load(Ordering::Relaxed),
                     TX_HOLDS.load(Ordering::Relaxed), TXQ_HIGH, TXQ_CAP);
    let _ = writeln!(out, "tx_queues: {}", queue_line(d, now));
    match d.rc.as_ref() {
        Some(c) => { let _ = writeln!(out, "tx_rate: {}", c.summary(now)); }
        None => { let _ = writeln!(out, "tx_rate: {}", if RC_ON.load(Ordering::Relaxed) { "no rate control (not associated)" } else { "rate control off (iwlwifi.rc=0)" }); }
    }
    let _ = writeln!(out, "erp: {:?} cts_protection {}", d.erp, ASSOC_MS.lock().as_ref().map_or(false, |a| a.ms.use_cts_prot));
}

fn scan_table(out: &mut String) {
    use core::fmt::Write;
    let info = INFO.lock().clone();
    let i = match info { Some(i) => i, None => { let _ = writeln!(out, "no Wi-Fi device"); return; } };
    let _ = writeln!(out, "# {} — {}; last scan {} s ago", i.name, i.state,
                     if i.scan_at_ms == 0 { 0 } else { (now_ms() - i.scan_at_ms) / 1000 });
    let _ = writeln!(out, "{:<17} {:>4} {:>6}  {:<14} {}", "BSSID", "CH", "SIGNAL", "SECURITY", "SSID");
    for e in i.bss.iter() {
        let _ = writeln!(out, "{:<17} {:>4} {:>6}  {:<14} {}", fmt_mac(&e.bss.bssid), e.bss.channel, e.rssi,
                         wf::security_summary(&e.bss), wf::ssid_display(&e.bss.ssid));
    }
}
