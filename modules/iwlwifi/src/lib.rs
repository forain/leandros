//! Intel Wireless 8265/8260 (iwlwifi, MVM firmware) as a loadable module: the SAME driver source as
//! the built-in (`drivers/src/iwlwifi.rs` and its iwl_* / wifi_* helpers), with the kernel seams
//! (`kenv`, pci, irq, firmware, netdev kick, command-line secrets, the poll-key wake) pointing at the
//! kmod-kapi shim. It registers its netif (`wifi`) through `kapi_netdev_register` and its radio through
//! `kapi_wifi_register`; the connection manager stays in the kernel (`wifi_mgr`, `wifi_class`).
//! Autoloads by PCI alias 8086:24fd / 24f3 / 24f4. With no device, `init` returns -19 before it
//! registers anything, so the module unloads cleanly. With a device the radio thread pins the module
//! (as every other netif module). No credential is ever logged here.
#![no_std]

extern crate alloc;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

// The driver source names the kernel's `crate::kdiag`.
mod kdiag {
    pub use kmod_kapi::kdiag::*;
}

pub mod kenv {
    pub use kmod_kapi::{mm, serial_raw};
    pub use crate::sched;
    pub fn wifi_register(ops: &crate::wifi_radio::KWifiRadioOps, ctx: usize, legacy: *const crate::wifi_radio::KLegacy) -> i32 {
        kmod_kapi::wifi::register(ops as *const _ as *const u8, ctx, legacy as *const u8)
    }
}

pub use kmod_kapi::{irq, pci};

/// `sched` of the kmod-kapi shim plus the poll-key wake the driver thread parks on, emulated over
/// one kernel wait queue (the poll-channel itself is kernel-internal).
pub mod sched {
    pub use kmod_kapi::sched::*;
    use core::sync::atomic::{AtomicU64, Ordering};
    use kmod_kapi::netdev::LinkMon;

    pub mod poll_class { pub const INET: u64 = 3; }
    pub const fn poll_key(class: u64, id: u64) -> u64 { (class << 56) ^ id }
    pub const fn poll_key_bit(key: u64) -> u64 { 1u64 << (key & 63) }
    pub struct PollTag { pub bits: u64, pub n: usize, pub keys: [u64; 3], pub nested: usize, pub foreign: bool }

    static WAKE: LinkMon = LinkMon::new();
    static DEADLINE: AtomicU64 = AtomicU64::new(0);
    pub fn wake_poll_key(_k: u64) { WAKE.notify(); }
    pub fn try_wake_poll_key(_k: u64) -> bool { WAKE.notify(); true }
    pub fn request_poll_wake_key(_k: u64) { WAKE.notify(); }
    pub fn block_on_poll_prepare_tag(deadline_ns: u64, _t: &PollTag) { DEADLINE.store(deadline_ns, Ordering::SeqCst); }
    pub fn block_on_poll_cancel() {}
    pub fn block_on_poll_commit() {
        let left = DEADLINE.load(Ordering::SeqCst).saturating_sub(monotonic_ns());
        if left >= 1_000_000 { let mut seen = 0u64; WAKE.wait(&mut seen, left / 1_000_000); }
    }
}

pub mod netdev {
    pub use kmod_kapi::netdev::{kick, kick_from_irq};
}

pub mod cmdline_secrets {
    /// Same contract as the kernel's `peek`; the value is copied out of the kernel (never logged).
    pub fn peek(key: &str) -> Option<alloc::vec::Vec<u8>> {
        let mut buf = [0u8; 160];
        let n = kmod_kapi::sys_common::secret_peek(key, &mut buf).ok()?;
        Some(buf[..n].to_vec())
    }
}

pub mod firmware {
    use alloc::vec::Vec;
    #[derive(Debug)]
    pub enum FwError { NoRoot, TooBig, Fs(i32) }
    pub fn root_ready() -> bool { kmod_kapi::sys_common::fw_root_ready() }
    pub struct Firmware { h: usize, p: *const u8, len: usize }
    unsafe impl Send for Firmware {}
    pub fn request_firmware(path: &str, max_len: usize) -> Result<Firmware, FwError> {
        if !root_ready() { return Err(FwError::NoRoot); }
        match kmod_kapi::sys_common::fw_request(path, max_len) {
            Some((h, p, len)) => Ok(Firmware { h, p, len }),
            None => Err(FwError::Fs(-2)),
        }
    }
    impl Firmware {
        pub fn len(&self) -> usize { self.len }
        pub fn map(&mut self) -> Option<usize> { Some(self.p as usize) }
        pub fn crc32(&self) -> u32 {
            let d = unsafe { core::slice::from_raw_parts(self.p, self.len) };
            let mut c = 0xffff_ffffu32;
            for &b in d { c ^= b as u32; for _ in 0..8 { c = if c & 1 != 0 { (c >> 1) ^ 0xedb8_8320 } else { c >> 1 }; } }
            !c
        }
        pub fn to_vec(&self) -> Option<Vec<u8>> { Some(unsafe { core::slice::from_raw_parts(self.p, self.len) }.to_vec()) }
    }
    impl Drop for Firmware { fn drop(&mut self) { kmod_kapi::sys_common::fw_release(self.h); } }
}

/// The connection manager's registry lives in the kernel; these are the entry points the driver
/// calls for the netctl seam, which the kernel serves itself when the netif registers.
pub mod wifi_class {
    pub struct NoMgr;
    impl NoMgr { pub fn scan_lines(&self, _n: &str, _o: &mut alloc::string::String) {} }
    pub fn attach(_d: &str, _i: u32, _n: &str) -> bool { true }
    pub fn lookup(_n: &str) -> Option<&'static NoMgr> { None }
    pub fn count() -> usize { 0 }
    pub fn scan_lines(_o: &mut alloc::string::String) {}
    pub fn info_lines(_n: &str, _o: &mut alloc::string::String) {}
}

pub mod wifi_mgr {
    pub use crate::wifi_pure::*;
    pub use crate::wifi_radio::{ConnCtx, Fail, KConnCtx, Radio, ScanHit};
    /// The kernel parses the `wifi.*` knobs itself (they are manager state).
    pub fn set_knobs(_cmdline: &str) {}
}

#[path = "../../../drivers/src/wifi_pure.rs"]
pub mod wifi_pure;
#[path = "../../../drivers/src/wifi_radio.rs"]
pub mod wifi_radio;
#[path = "../../../drivers/src/wifi_frame.rs"]
pub mod wifi_frame;
#[path = "../../../drivers/src/wifi_ht.rs"]
pub mod wifi_ht;
#[path = "../../../drivers/src/wifi_crypto.rs"]
pub mod wifi_crypto;
#[path = "../../../drivers/src/iwl_fw.rs"]
pub mod iwl_fw;
#[path = "../../../drivers/src/iwl_cmd.rs"]
pub mod iwl_cmd;
#[path = "../../../drivers/src/iwl_assoc.rs"]
pub mod iwl_assoc;
#[path = "../../../drivers/src/iwl_rs.rs"]
pub mod iwl_rs;
#[path = "../../../drivers/src/iwl_agg.rs"]
pub mod iwl_agg;
#[path = "../../../drivers/src/iwl_trans.rs"]
pub mod iwl_trans;
#[path = "../../../drivers/src/iwlwifi.rs"]
#[allow(dead_code)]
pub mod iwlwifi;

kmod_rt::module! {
    name: "iwlwifi",
    init: init,
    ids: [
        kmod_rt::pci(0x8086, 0x24fd),
        kmod_rt::pci(0x8086, 0x24f3),
        kmod_rt::pci(0x8086, 0x24f4),
    ]
}

fn copy_out(s: &str, buf: *mut u8, cap: usize) -> usize {
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

extern "C" fn send(_: usize, b: *const u8, len: usize) -> bool { iwlwifi::send_packet(unsafe { core::slice::from_raw_parts(b, len) }) }
extern "C" fn tx_ready(_: usize) -> bool { iwlwifi::tx_ready() }
extern "C" fn recv(_: usize, b: *mut u8, cap: usize) -> i32 {
    match iwlwifi::poll_receive(unsafe { core::slice::from_raw_parts_mut(b, cap) }) { Some(n) => n as i32, None => -1 }
}
extern "C" fn rx_rearm(_: usize) -> bool { iwlwifi::rx_rearm() }
extern "C" fn rx_irq_driven(_: usize) -> bool { iwlwifi::rx_irq_driven() }
extern "C" fn mac(_: usize, out: *mut u8) -> bool {
    match iwlwifi::mac_address() {
        Some(m) => { unsafe { core::ptr::copy_nonoverlapping(m.as_ptr(), out, 6); } true }
        None => false,
    }
}
extern "C" fn link(_: usize, up: *mut bool, mbps: *mut u32, full: *mut bool) {
    unsafe { *up = iwlwifi::link_up(); *mbps = 0; *full = true; }
}
extern "C" fn link_gen(_: usize) -> u32 { iwlwifi::link_generation() }
extern "C" fn info(_: usize, buf: *mut u8, cap: usize) -> usize {
    let mut s = String::new();
    iwlwifi::info_lines(&mut s);
    wifi_pure::gk::lines(&mut s);
    copy_out(&s, buf, cap)
}
extern "C" fn driver_name(_: usize, buf: *mut u8, cap: usize) -> usize { copy_out("iwlwifi", buf, cap) }

static OPS: kmod_kapi::netdev::KNetifOps = kmod_kapi::netdev::KNetifOps {
    send, tx_ready, recv, rx_rearm, rx_irq_driven, mac, link, link_gen, info, driver_name,
};

extern "C" fn pm_shutdown(_: usize, buf: *mut u8, cap: usize) -> i32 {
    match iwlwifi::kexec_stop() { Some(m) => copy_out(&m, buf, cap) as i32, None => 0 }
}

static DONE: AtomicBool = AtomicBool::new(false);

fn init() -> i32 {
    if DONE.swap(true, Ordering::SeqCst) { return -16; }
    iwlwifi::init(kmod_kapi::cmdline());
    if !iwlwifi::present() { DONE.store(false, Ordering::SeqCst); return -19; }
    match kmod_kapi::netdev::register(kmod_kapi::netdev::Kind::Wifi, "wifi", &OPS, 0) {
        Ok(id) => kmod_rt::kinfo!("registered netif {}", id),
        Err(e) => { kmod_rt::kerr!("netdev_register failed: {}", e); return e; }
    }
    let _ = kmod_kapi::pm::register(None, None, Some(pm_shutdown), 0);
    0
}
