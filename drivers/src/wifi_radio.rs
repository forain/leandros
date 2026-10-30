//! Wi-Fi radio class: the driver-facing half. The radio trait, its value types and the
//! `repr(C)` table (`KWifiRadioOps`) a radio driver registers with the kernel, which owns the
//! connection manager (`wifi_mgr::Mgr`) and runs `wifi_mgr::run` on its own kthread.
//!
//! This file is pure: it needs only `wifi_frame` and `netctl_abi`, and reaches the kernel through
//! `kenv::wifi_register` (a direct call in the built-in build, `kapi_wifi_register` in a module),
//! so the very same source serves `IWLWIFI=y` and `=m`.
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};

use netctl_abi::reason;
use crate::wifi_frame as wf;

// ── what a radio reports ────────────────────────────────────────────────────

/// One BSS seen by a scan.
#[derive(Clone, Debug)]
pub struct ScanHit {
    pub bss: wf::Bss,
    pub rssi: i8,
}

/// Why an attempt or a link ended.
#[derive(Clone, Debug)]
pub struct Fail {
    /// `netctl_abi::reason`.
    pub reason: u8,
    /// 802.11 status code (auth/assoc response), 0 = none.
    pub ieee_status: u16,
    /// 802.11 reason code (deauth/disassoc), 0 = none.
    pub ieee_reason: u16,
    /// The firmware asserted (the radio must be restarted).
    pub fw_assert: bool,
    pub msg: String,
}

impl Fail {
    pub fn new(reason: u8, msg: impl Into<String>) -> Fail {
        Fail { reason, ieee_status: 0, ieee_reason: 0, fw_assert: false, msg: msg.into() }
    }
    pub fn status(mut self, s: u16) -> Fail { self.ieee_status = s; self }
    pub fn ieee_reason(mut self, r: u16) -> Fail { self.ieee_reason = r; self }
    pub fn fw_assert(msg: impl Into<String>) -> Fail {
        Fail { reason: reason::FW_ERROR, ieee_status: 0, ieee_reason: 0, fw_assert: true, msg: msg.into() }
    }
    pub fn superseded() -> Fail { Fail::new(reason::SUPERSEDED, "preempted by a newer command") }
}

/// Context a radio's `connect` gets: progress reports and preemption (the manager's side, behind
/// four callbacks).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KConnCtx {
    pub ctx: usize,
    pub step: extern "C" fn(usize, u8),
    pub aborted: extern "C" fn(usize) -> bool,
    pub left_ms: extern "C" fn(usize) -> u64,
    pub sleep: extern "C" fn(usize, u64) -> bool,
}

pub struct ConnCtx<'a> {
    raw: &'a KConnCtx,
}

impl<'a> ConnCtx<'a> {
    pub fn from_raw(raw: &'a KConnCtx) -> ConnCtx<'a> { ConnCtx { raw } }
    pub fn raw_ptr(&self) -> *const KConnCtx { self.raw }
    /// Report the next step (AUTHENTICATING, ASSOCIATING, HANDSHAKE).
    pub fn step(&self, state: u8) { (self.raw.step)(self.raw.ctx, state) }
    /// A newer command (Connect, Disconnect, radio off) is waiting, or the attempt ran past
    /// `CONNECT_DEADLINE_MS`: stop and return [`Fail::superseded`] (or a TIMEOUT).
    pub fn aborted(&self) -> bool { (self.raw.aborted)(self.raw.ctx) }
    /// Milliseconds left of the attempt's overall deadline.
    pub fn left_ms(&self) -> u64 { (self.raw.left_ms)(self.raw.ctx) }
    /// Sleep up to `ms`, waking early when preempted. False when preempted.
    pub fn sleep(&self, ms: u64) -> bool { (self.raw.sleep)(self.raw.ctx, ms) }
}

/// What a Wi-Fi driver provides to the manager.
pub trait Radio {
    /// `netctl_abi::wifi_caps`.
    fn caps(&self) -> u32;
    /// Scan: wildcard plus directed probes for `ssids`. `connected` is true
    /// when associated (only called then if caps has SCAN_CONNECTED).
    fn scan(&mut self, ssids: &[Vec<u8>], connected: bool) -> Result<Vec<ScanHit>, Fail>;
    /// Authenticate, associate and (unless `akm == NONE`) run the 4-way
    /// handshake with `secret`; on success the data path is open.
    fn connect(&mut self, hit: &ScanHit, ssid: &[u8], akm: u8, secret: &[u8], cx: &ConnCtx) -> Result<(), Fail>;
    /// Associated: service the link for up to `budget_ms`. `Some` when it
    /// went away (deauth by AP, beacon loss, firmware error).
    fn poll_connected(&mut self, budget_ms: u64) -> Option<Fail>;
    /// Fast part of leaving (a few ms): close the data path, send a
    /// deauthentication when associated (`deauth`).
    fn leave(&mut self, deauth: bool);
    /// Slow part, after the state was reported: put the firmware back into a
    /// clean unassociated state (a restart after `after.fw_assert`). False
    /// when the device is gone for good.
    fn settle(&mut self, after: Option<&Fail>) -> bool;
    /// Idle: service the device for up to `ms`.
    fn idle(&mut self, ms: u64);
    /// Soft radio switch.
    fn set_radio(&mut self, on: bool) -> bool;
    /// Hardware kill switch engaged.
    fn hw_rfkill(&mut self) -> bool { false }
    /// The kill switch was released: bring the radio back (false = dead).
    fn rfkill_released(&mut self) -> bool { self.settle(None) }
    /// Current signal of the associated BSS.
    fn signal(&mut self) -> Option<i8> { None }
    /// TX rate in kbit/s, 0 = unknown.
    fn tx_rate_kbps(&mut self) -> u32 { 0 }
}

// ── the repr(C) table ───────────────────────────────────────────────────────

#[repr(C)]
#[derive(Clone, Copy)]
pub struct KSsid { pub len: u8, pub b: [u8; 32] }

pub const KRSN_PW: usize = 8;
pub const KRSN_AKM: usize = 12;

/// A BSS as a C struct: every field of `wifi_frame::Bss` explicitly (the sim and the Pi build
/// theirs from firmware events, so the IEs alone do not reproduce it). Pointers borrow the hit.
#[repr(C)]
pub struct KScanHit {
    pub bssid: [u8; 6],
    pub channel: u8,
    pub rssi: i8,
    pub dtim: u8,
    pub wpa1: u8,
    pub ht: u8,
    pub vht: u8,
    /// 0 = no ERP element, else 1 and `erp` holds the flags.
    pub has_erp: u8,
    pub erp: u8,
    pub capability: u16,
    pub beacon_interval: u16,
    pub tsf: u64,
    pub ssid: KSsid,
    pub ies: *const u8,
    pub ies_len: usize,
    pub rates: *const u8,
    pub rates_len: usize,
    pub has_rsn: u8,
    pub rsn_n_pw: u8,
    pub rsn_n_akm: u8,
    pub rsn_version: u16,
    pub rsn_caps: u16,
    pub rsn_group: u32,
    pub rsn_pw: [u32; KRSN_PW],
    pub rsn_akm: [u32; KRSN_AKM],
}

pub const KFAIL_MSG: usize = 120;

#[repr(C)]
pub struct KWifiFail {
    pub reason: u8,
    pub fw_assert: u8,
    pub ieee_status: u16,
    pub ieee_reason: u16,
    pub msg_len: u16,
    pub msg: [u8; KFAIL_MSG],
}

impl KWifiFail {
    pub const fn zero() -> KWifiFail { KWifiFail { reason: 0, fw_assert: 0, ieee_status: 0, ieee_reason: 0, msg_len: 0, msg: [0; KFAIL_MSG] } }
}

pub fn fail_to_k(f: &Fail) -> KWifiFail {
    let mut k = KWifiFail::zero();
    k.reason = f.reason;
    k.fw_assert = f.fw_assert as u8;
    k.ieee_status = f.ieee_status;
    k.ieee_reason = f.ieee_reason;
    let mut n = f.msg.len().min(KFAIL_MSG);
    while !f.msg.is_char_boundary(n) { n -= 1; }
    k.msg[..n].copy_from_slice(&f.msg.as_bytes()[..n]);
    k.msg_len = n as u16;
    k
}

pub fn fail_from_k(k: &KWifiFail) -> Fail {
    let n = (k.msg_len as usize).min(KFAIL_MSG);
    Fail {
        reason: k.reason, ieee_status: k.ieee_status, ieee_reason: k.ieee_reason, fw_assert: k.fw_assert != 0,
        msg: String::from_utf8_lossy(&k.msg[..n]).into_owned(),
    }
}

/// The scan hit as a C struct borrowing `h` (its IEs and rates).
pub fn hit_to_k(h: &ScanHit) -> KScanHit {
    let b = &h.bss;
    let mut ssid = KSsid { len: 0, b: [0; 32] };
    let n = b.ssid.len().min(32);
    ssid.b[..n].copy_from_slice(&b.ssid[..n]);
    ssid.len = n as u8;
    let mut k = KScanHit {
        bssid: b.bssid, channel: b.channel, rssi: h.rssi, dtim: b.dtim_period, wpa1: b.wpa1 as u8, ht: b.ht as u8, vht: b.vht as u8,
        has_erp: b.erp.is_some() as u8, erp: b.erp.unwrap_or(0), capability: b.capability, beacon_interval: b.beacon_interval,
        tsf: b.tsf, ssid, ies: b.ies.as_ptr(), ies_len: b.ies.len(), rates: b.rates.as_ptr(), rates_len: b.rates.len(),
        has_rsn: 0, rsn_n_pw: 0, rsn_n_akm: 0, rsn_version: 0, rsn_caps: 0, rsn_group: 0, rsn_pw: [0; KRSN_PW], rsn_akm: [0; KRSN_AKM],
    };
    if let Some(r) = &b.rsn {
        k.has_rsn = 1;
        k.rsn_version = r.version;
        k.rsn_caps = r.caps;
        k.rsn_group = r.group;
        let np = r.pairwise.len().min(KRSN_PW);
        let na = r.akm.len().min(KRSN_AKM);
        k.rsn_pw[..np].copy_from_slice(&r.pairwise[..np]);
        k.rsn_akm[..na].copy_from_slice(&r.akm[..na]);
        k.rsn_n_pw = np as u8;
        k.rsn_n_akm = na as u8;
    }
    k
}

/// Rebuild the BSS from the C struct.
pub fn hit_from_k(k: &KScanHit) -> Option<ScanHit> {
    let sl = |p: *const u8, n: usize| -> Vec<u8> { if p.is_null() || n == 0 { Vec::new() } else { unsafe { core::slice::from_raw_parts(p, n) }.to_vec() } };
    let rsn = if k.has_rsn != 0 {
        Some(wf::RsnInfo {
            version: k.rsn_version, group: k.rsn_group, caps: k.rsn_caps,
            pairwise: k.rsn_pw[..(k.rsn_n_pw as usize).min(KRSN_PW)].to_vec(),
            akm: k.rsn_akm[..(k.rsn_n_akm as usize).min(KRSN_AKM)].to_vec(),
        })
    } else { None };
    let bss = wf::Bss {
        bssid: k.bssid, ssid: k.ssid.b[..(k.ssid.len as usize).min(32)].to_vec(), channel: k.channel,
        beacon_interval: k.beacon_interval, capability: k.capability, tsf: k.tsf, rates: sl(k.rates, k.rates_len), rsn,
        wpa1: k.wpa1 != 0, ht: k.ht != 0, vht: k.vht != 0, dtim_period: k.dtim,
        erp: if k.has_erp != 0 { Some(k.erp) } else { None }, rssi: k.rssi, ies: sl(k.ies, k.ies_len),
    };
    Some(ScanHit { bss, rssi: k.rssi })
}

pub type ScanSink = extern "C" fn(usize, *const KScanHit);

/// The radio table. `size` lets the kernel accept older/newer tables. `fail` out-parameters are
/// filled when the call returns non-zero (scan/connect: failed; poll_connected: link gone).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct KWifiRadioOps {
    pub size: usize,
    pub caps: u32,
    pub mac: [u8; 6],
    pub name: [u8; 16],
    pub prefix: [u8; 12],
    pub scan: extern "C" fn(usize, *const KSsid, usize, bool, ScanSink, usize, *mut KWifiFail) -> i32,
    pub connect: extern "C" fn(usize, *const KScanHit, *const u8, usize, u8, *const u8, usize, *const KConnCtx, *mut KWifiFail) -> i32,
    pub poll_connected: extern "C" fn(usize, u64, *mut KWifiFail) -> i32,
    pub leave: extern "C" fn(usize, bool),
    /// `after` is null when there is no failure to recover from.
    pub settle: extern "C" fn(usize, *const KWifiFail) -> bool,
    pub idle: extern "C" fn(usize, u64),
    pub set_radio: extern "C" fn(usize, bool) -> bool,
    pub hw_rfkill: extern "C" fn(usize) -> bool,
    pub rfkill_released: extern "C" fn(usize) -> bool,
    pub signal: extern "C" fn(usize, *mut i8) -> bool,
    pub tx_rate_kbps: extern "C" fn(usize) -> u32,
    /// A command was queued: end the radio's park.
    pub kick: extern "C" fn(usize),
    /// The manager loop ended (the kernel thread is done with the table).
    pub stopped: extern "C" fn(usize),
}

/// Legacy network (wifi.conf / wifi.ssid=) as the autoconnect profile until netctl publishes the
/// stored ones. Never logged.
#[repr(C)]
pub struct KLegacy { pub ssid: *const u8, pub ssid_len: usize, pub psk: *const u8, pub psk_len: usize }

// ── driver side: Box<dyn Radio> behind the table ────────────────────────────

struct Holder {
    r: Box<dyn Radio>,
    kick: fn(),
    done: Arc<AtomicBool>,
}

fn h<'a>(c: usize) -> &'a mut Holder { unsafe { &mut *(c as *mut Holder) } }

extern "C" fn t_scan(c: usize, ss: *const KSsid, n: usize, connected: bool, sink: ScanSink, sctx: usize, fail: *mut KWifiFail) -> i32 {
    let ssids: Vec<Vec<u8>> = (0..n).map(|i| { let s = unsafe { &*ss.add(i) }; s.b[..(s.len as usize).min(32)].to_vec() }).collect();
    match h(c).r.scan(&ssids, connected) {
        Ok(hits) => { for x in &hits { let k = hit_to_k(x); sink(sctx, &k); } 0 }
        Err(f) => { unsafe { *fail = fail_to_k(&f); } 1 }
    }
}
extern "C" fn t_connect(c: usize, hit: *const KScanHit, ssid: *const u8, sl: usize, akm: u8, sec: *const u8, secl: usize, cx: *const KConnCtx, fail: *mut KWifiFail) -> i32 {
    let Some(hit) = hit_from_k(unsafe { &*hit }) else {
        unsafe { *fail = fail_to_k(&Fail::new(reason::TIMEOUT, "scan hit not parseable")); }
        return 1;
    };
    let ssid = unsafe { core::slice::from_raw_parts(ssid, sl) };
    let sec: &[u8] = if secl == 0 { &[] } else { unsafe { core::slice::from_raw_parts(sec, secl) } };
    match h(c).r.connect(&hit, ssid, akm, sec, &ConnCtx::from_raw(unsafe { &*cx })) {
        Ok(()) => 0,
        Err(f) => { unsafe { *fail = fail_to_k(&f); } 1 }
    }
}
extern "C" fn t_poll(c: usize, ms: u64, fail: *mut KWifiFail) -> i32 {
    match h(c).r.poll_connected(ms) { None => 0, Some(f) => { unsafe { *fail = fail_to_k(&f); } 1 } }
}
extern "C" fn t_leave(c: usize, d: bool) { h(c).r.leave(d) }
extern "C" fn t_settle(c: usize, after: *const KWifiFail) -> bool {
    let a = if after.is_null() { None } else { Some(fail_from_k(unsafe { &*after })) };
    h(c).r.settle(a.as_ref())
}
extern "C" fn t_idle(c: usize, ms: u64) { h(c).r.idle(ms) }
extern "C" fn t_set_radio(c: usize, on: bool) -> bool { h(c).r.set_radio(on) }
extern "C" fn t_rfkill(c: usize) -> bool { h(c).r.hw_rfkill() }
extern "C" fn t_rfkill_rel(c: usize) -> bool { h(c).r.rfkill_released() }
extern "C" fn t_signal(c: usize, out: *mut i8) -> bool {
    match h(c).r.signal() { Some(s) => { unsafe { *out = s; } true } None => false }
}
extern "C" fn t_rate(c: usize) -> u32 { h(c).r.tx_rate_kbps() }
extern "C" fn t_kick(c: usize) { (h(c).kick)() }
extern "C" fn t_stopped(c: usize) { h(c).done.store(true, Ordering::Release); }

fn fixed<const N: usize>(s: &str) -> [u8; N] {
    let mut a = [0u8; N];
    let n = s.len().min(N);
    a[..n].copy_from_slice(&s.as_bytes()[..n]);
    a
}

/// Hand a radio to the kernel's connection manager. `name`/`prefix` label the manager (log lines,
/// /proc/net/wifi); `kick` ends the radio's park when a command is queued. Returns a flag that
/// becomes true when the manager loop has ended (the radio is then idle and can be torn down).
pub fn register(r: Box<dyn Radio>, name: &str, prefix: &str, mac: [u8; 6], kick: fn(), legacy: Option<(&[u8], &[u8])>)
                -> Result<Arc<AtomicBool>, i32> {
    let done = Arc::new(AtomicBool::new(false));
    let caps = r.caps();
    let holder = Box::into_raw(Box::new(Holder { r, kick, done: done.clone() }));
    let ops = KWifiRadioOps {
        size: core::mem::size_of::<KWifiRadioOps>(), caps, mac, name: fixed(name), prefix: fixed(prefix),
        scan: t_scan, connect: t_connect, poll_connected: t_poll, leave: t_leave, settle: t_settle, idle: t_idle,
        set_radio: t_set_radio, hw_rfkill: t_rfkill, rfkill_released: t_rfkill_rel, signal: t_signal,
        tx_rate_kbps: t_rate, kick: t_kick, stopped: t_stopped,
    };
    let lg = legacy.map(|(s, p)| KLegacy { ssid: s.as_ptr(), ssid_len: s.len(), psk: p.as_ptr(), psk_len: p.len() });
    let rc = crate::kenv::wifi_register(&ops, holder as usize, lg.as_ref().map_or(core::ptr::null(), |l| l as *const KLegacy));
    if rc < 0 {
        // SAFETY: the kernel kept nothing.
        drop(unsafe { Box::from_raw(holder) });
        return Err(rc);
    }
    Ok(done)
}
