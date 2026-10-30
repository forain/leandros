//! Wi-Fi radio class, kernel side: radios register a `KWifiRadioOps` table; the kernel owns the
//! connection manager (`wifi_mgr::Mgr`) for each and runs `wifi_mgr::run` on its own kthread
//! through the `KRadio` adapter. `/proc/net/wifi` and the netctl attach walk this registry, so
//! nothing outside a driver names a particular radio.
use alloc::string::String;
use alloc::vec::Vec;
use alloc::boxed::Box;
use spin::Mutex;

use crate::wifi_mgr::{self as wm, Mgr};
use crate::wifi_radio::*;
use crate::kenv::sched;

pub struct Entry { pub name: &'static str, pub mgr: &'static Mgr }

static REGISTRY: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
/// netif control side handed over before the radio registered: (driver name, ifindex, ifname).
static PENDING: Mutex<Vec<(String, u32, String)>> = Mutex::new(Vec::new());

/// `wifi_mgr::Radio` over a registered table.
pub struct KRadio { ops: KWifiRadioOps, ctx: usize }
// SAFETY: the table's `ctx` is only used by the one manager thread, as the driver intended.
unsafe impl Send for KRadio {}

extern "C" fn sink(v: usize, k: *const KScanHit) {
    let out = unsafe { &mut *(v as *mut Vec<ScanHit>) };
    if let Some(h) = hit_from_k(unsafe { &*k }) { out.push(h); }
}

impl Radio for KRadio {
    fn caps(&self) -> u32 { self.ops.caps }
    fn scan(&mut self, ssids: &[Vec<u8>], connected: bool) -> Result<Vec<ScanHit>, Fail> {
        let ks: Vec<KSsid> = ssids.iter().map(|s| { let mut k = KSsid { len: 0, b: [0; 32] }; let n = s.len().min(32); k.b[..n].copy_from_slice(&s[..n]); k.len = n as u8; k }).collect();
        let mut out: Vec<ScanHit> = Vec::new();
        let mut f = KWifiFail::zero();
        let rc = (self.ops.scan)(self.ctx, ks.as_ptr(), ks.len(), connected, sink, &mut out as *mut _ as usize, &mut f);
        if rc == 0 { Ok(out) } else { Err(fail_from_k(&f)) }
    }
    fn connect(&mut self, hit: &ScanHit, ssid: &[u8], akm: u8, secret: &[u8], cx: &ConnCtx) -> Result<(), Fail> {
        let k = hit_to_k(hit);
        let mut f = KWifiFail::zero();
        let rc = (self.ops.connect)(self.ctx, &k, ssid.as_ptr(), ssid.len(), akm, secret.as_ptr(), secret.len(), cx.raw_ptr(), &mut f);
        if rc == 0 { Ok(()) } else { Err(fail_from_k(&f)) }
    }
    fn poll_connected(&mut self, budget_ms: u64) -> Option<Fail> {
        let mut f = KWifiFail::zero();
        if (self.ops.poll_connected)(self.ctx, budget_ms, &mut f) == 0 { None } else { Some(fail_from_k(&f)) }
    }
    fn leave(&mut self, deauth: bool) { (self.ops.leave)(self.ctx, deauth) }
    fn settle(&mut self, after: Option<&Fail>) -> bool {
        match after {
            Some(a) => { let k = fail_to_k(a); (self.ops.settle)(self.ctx, &k) }
            None => (self.ops.settle)(self.ctx, core::ptr::null()),
        }
    }
    fn idle(&mut self, ms: u64) { (self.ops.idle)(self.ctx, ms) }
    fn set_radio(&mut self, on: bool) -> bool { (self.ops.set_radio)(self.ctx, on) }
    fn hw_rfkill(&mut self) -> bool { (self.ops.hw_rfkill)(self.ctx) }
    fn rfkill_released(&mut self) -> bool { (self.ops.rfkill_released)(self.ctx) }
    fn signal(&mut self) -> Option<i8> { let mut s = 0i8; if (self.ops.signal)(self.ctx, &mut s) { Some(s) } else { None } }
    fn tx_rate_kbps(&mut self) -> u32 { (self.ops.tx_rate_kbps)(self.ctx) }
}

fn cstr(b: &[u8]) -> &str {
    let n = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    core::str::from_utf8(&b[..n]).unwrap_or("")
}

/// Register a radio. `legacy` (SSID, PSK) is the wifi.conf / wifi.ssid= network, used as the
/// autoconnect profile until netctl publishes the stored ones; it is never logged.
/// Returns the registry index, or -errno.
pub fn register_raw(ops: *const KWifiRadioOps, ctx: usize, legacy: *const KLegacy) -> i32 {
    if ops.is_null() { return -22; }
    let ops = unsafe { *ops };
    if ops.size < core::mem::size_of::<KWifiRadioOps>() { return -22; }
    let name: &'static str = Box::leak(String::from(cstr(&ops.name)).into_boxed_str());
    let prefix: &'static str = Box::leak(String::from(cstr(&ops.prefix)).into_boxed_str());
    if name.is_empty() || prefix.is_empty() { return -22; }
    let mgr: &'static Mgr = Box::leak(Box::new(Mgr::new(name, prefix, no_mac)));
    mgr.set_mac(ops.mac);
    mgr.set_caps(ops.caps);
    mgr.set_waker_raw(ops.kick, ctx);
    if !legacy.is_null() {
        let l = unsafe { &*legacy };
        let ssid = unsafe { core::slice::from_raw_parts(l.ssid, l.ssid_len) };
        let psk = unsafe { core::slice::from_raw_parts(l.psk, l.psk_len) };
        if let Some(p) = netctl_abi::profiles::legacy_profile(ssid, psk) {
            mgr.set_fallback_profiles(alloc::vec![p]);
        }
    }
    let idx = {
        let mut r = REGISTRY.lock();
        if r.iter().any(|e| e.name == name) { return -17; }
        r.push(Entry { name, mgr });
        r.len() as i32 - 1
    };
    // The netif may have been attached already.
    let pend = { let mut p = PENDING.lock(); p.iter().position(|e| e.0 == name).map(|i| p.remove(i)) };
    if let Some((_, ifindex, ifname)) = pend { mgr.attach(ifindex, &ifname); }
    let mut radio = KRadio { ops, ctx };
    let th = alloc::format!("wifi-{}", prefix);
    let th: &'static str = Box::leak(th.into_boxed_str());
    if sched::kthread::spawn(th, move || {
        wm::run(mgr, &mut radio);
        (radio.ops.stopped)(radio.ctx);
    }).is_none() {
        REGISTRY.lock().retain(|e| e.name != name);
        return -12;
    }
    idx
}

fn no_mac() -> Option<[u8; 6]> { None }

/// The netctl control side for the netif the net core registered for `driver`. Attaches at once
/// when the radio is registered, else when it registers.
pub fn attach(driver: &str, ifindex: u32, ifname: &str) -> bool {
    if let Some(m) = lookup(driver) { return m.attach(ifindex, ifname); }
    let mut p = PENDING.lock();
    p.retain(|e| e.0 != driver);
    p.push((String::from(driver), ifindex, String::from(ifname)));
    true
}

pub fn lookup(name: &str) -> Option<&'static Mgr> {
    REGISTRY.lock().iter().find(|e| e.name == name).map(|e| e.mgr)
}

/// Wait (bounded) for `name` to register: for self-tests that start before the radio thread.
pub fn lookup_wait(name: &str, ms: u64) -> Option<&'static Mgr> {
    let t0 = sched::monotonic_ns() / 1_000_000;
    loop {
        if let Some(m) = lookup(name) { return Some(m); }
        if sched::monotonic_ns() / 1_000_000 - t0 > ms { return None; }
        sched::kthread::msleep(10);
    }
}

pub fn count() -> usize { REGISTRY.lock().len() }

/// `/proc/net/wifi`: the first registered radio's BSS table (each manager titles its own).
pub fn scan_lines(out: &mut String) {
    let first = REGISTRY.lock().first().map(|e| (e.name, e.mgr));
    if let Some((n, m)) = first { m.scan_lines(n, out); }
}

/// One manager's info block.
pub fn info_lines(name: &str, out: &mut String) {
    if let Some(m) = lookup(name) { m.info_lines(out); }
}
