//! Broadcom brcmfmac (Raspberry Pi 5 CYW43455 over SDIO) as a loadable module: the SAME driver source as
//! the built-in (`drivers/src/brcmf/*`, `brcmf_boot.rs`) with the `kenv` seam pointing at the kmod-kapi shim.
//! The SDIO2 host, the pin mux and WL_REG_ON live in the module (the pins go through `kapi_pi5_pins`, which
//! answers only to a module named `brcmf`); the connection manager stays in the kernel (`kapi_wifi_register`).
//! Autoloads by the `of:` alias `brcm,bcm4329-fmac` (the wifi@1 node under sdio2). Off the Pi 5
//! (`kapi_soc_stepping` = -19) `init` returns -19 before it touches any pin. Otherwise it registers the
//! netif and starts a kthread that probes the host, waits for the root and runs the existing bring-up chain.
#![no_std]
#![allow(dead_code)]

extern crate alloc;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

pub mod kenv {
    pub use kmod_kapi::{mm, serial_raw};
    pub use crate::sched;
    use kmod_kapi::plat;

    pub fn ioremap_uc(pa: u64, len: usize) -> Option<usize> { mm::kva::ioremap(pa, len, mm::kva::CacheMode::Uncached) }
    pub fn dtb_blob() -> Option<&'static [u8]> { plat::dtb() }
    pub fn wifi_register(ops: &crate::wifi_radio::KWifiRadioOps, ctx: usize, legacy: *const crate::wifi_radio::KLegacy) -> i32 {
        kmod_kapi::wifi::register(ops as *const _ as *const u8, ctx, legacy as *const u8)
    }

    /// Pi 5 Wi-Fi pins over `kapi_pi5_pins` and the board MAC over `kapi_board_mac`.
    pub mod pi5 {
        use alloc::string::String;
        use kmod_kapi::plat;
        pub fn sdio2_pinmux() -> String {
            match plat::pi5_sdio2_mux() {
                Ok(m) => alloc::format!("pinmux={} sd2-fsel-before={:06x} writes={}", if m.stepping == 1 { "D0" } else { "C0" }, m.before, m.writes),
                Err(-19) => String::from("pinmux=skip(unknown stepping)"),
                Err(e) => alloc::format!("pinmux=fail({})", e),
            }
        }
        pub fn wl_reg_on(on: bool) { plat::pi5_wl_on(on) }
        pub fn wl_reg_on_ready() -> bool { plat::pi5_wl_on_ready() }
        pub fn wifi_mac() -> Option<[u8; 6]> { plat::mac(plat::board_mac::WIFI) }
    }

    /// The one quiesce hook (`brcmf_boot::start` registers it); the module hands it to the kernel from `init`.
    pub mod pm {
        use alloc::string::String;
        use core::sync::atomic::{AtomicUsize, Ordering};
        pub const PHASE_WIFI: u32 = kmod_abi::pm_phase::WIFI;
        pub type ShutdownFn = fn() -> Option<String>;
        static HOOK: AtomicUsize = AtomicUsize::new(0);
        pub fn register_shutdown(_name: &'static str, _phase: u32, f: ShutdownFn) { HOOK.store(f as usize, Ordering::Release); }
        pub fn unregister(_name: &str) { HOOK.store(0, Ordering::Release); }
        pub fn hook() -> Option<ShutdownFn> {
            match HOOK.load(Ordering::Acquire) { 0 => None, f => Some(unsafe { core::mem::transmute::<usize, ShutdownFn>(f) }) }
        }
    }
}

/// `sched` of the kmod-kapi shim plus the wedge markers (kernel-internal diagnostics, dropped here) and `cond_resched`.
pub mod sched {
    pub use kmod_kapi::sched::*;
    pub mod kthread {
        pub use kmod_kapi::sched::kthread::*;
        pub fn cond_resched() { kmod_kapi::sys_common::yield_now() }
    }
    pub mod wedge {
        pub const SLOT_NET: u32 = 0;
        pub const SLOT_BRCMF: u32 = 1;
        pub const SLOT_SDIO: u32 = 2;
        pub fn mark(_slot: u32, _code: u32, _arg: u64) {}
        pub fn set_namer(_slot: u32, _f: fn(u64) -> &'static str) {}
    }
}

pub mod netdev {
    pub use kmod_kapi::netdev::{kick, kick_from_irq};
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
        /// The file as consecutive slices of at most one page each.
        pub fn chunks(&self) -> impl Iterator<Item = &[u8]> + '_ {
            unsafe { core::slice::from_raw_parts(self.p, self.len) }.chunks(4096)
        }
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

/// The connection manager's registry lives in the kernel (it serves the netctl seam when the netif registers).
pub mod wifi_class {
    pub fn attach(_d: &str, _i: u32, _n: &str) -> bool { true }
    pub fn info_lines(_n: &str, _o: &mut alloc::string::String) {}
}

pub mod wifi_mgr {
    pub use crate::wifi_pure::*;
    pub use crate::wifi_radio::{ConnCtx, Fail, KConnCtx, Radio, ScanHit};
    /// The kernel parses the `wifi.*` knobs itself (they are manager state).
    pub fn set_knobs(_cmdline: &str) {}
    pub fn fmt_mac(m: &[u8; 6]) -> alloc::string::String {
        alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
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
#[path = "../../../drivers/src/brcmf/mod.rs"]
pub mod brcmf;
#[path = "../../../drivers/src/brcmf_boot.rs"]
pub mod brcmf_boot;

kmod_rt::module! {
    name: "brcmf",
    init: init,
    of: ["brcm,bcm4329-fmac"]
}

fn copy_out(s: &str, buf: *mut u8, cap: usize) -> usize {
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

use brcmf::netdev as bn;

extern "C" fn send(_: usize, b: *const u8, len: usize) -> bool { bn::send_packet(unsafe { core::slice::from_raw_parts(b, len) }) }
extern "C" fn tx_ready(_: usize) -> bool { bn::tx_ready() }
extern "C" fn recv(_: usize, b: *mut u8, cap: usize) -> i32 {
    match bn::poll_receive(unsafe { core::slice::from_raw_parts_mut(b, cap) }) { Some(n) => n as i32, None => -1 }
}
extern "C" fn rx_rearm(_: usize) -> bool { bn::rx_rearm() }
extern "C" fn rx_irq_driven(_: usize) -> bool { bn::rx_irq_driven() }
extern "C" fn mac(_: usize, out: *mut u8) -> bool {
    match bn::mac_address() {
        Some(m) => { unsafe { core::ptr::copy_nonoverlapping(m.as_ptr(), out, 6); } true }
        None => false,
    }
}
extern "C" fn link(_: usize, up: *mut bool, mbps: *mut u32, full: *mut bool) {
    unsafe { *up = bn::link_up(); *mbps = bn::link_speed(); *full = true; }
}
extern "C" fn link_gen(_: usize) -> u32 { bn::link_generation() }
extern "C" fn info(_: usize, buf: *mut u8, cap: usize) -> usize {
    let mut s = String::new();
    bn::info_lines(&mut s);
    copy_out(&s, buf, cap)
}
extern "C" fn driver_name(_: usize, buf: *mut u8, cap: usize) -> usize { copy_out("brcmfmac", buf, cap) }

static OPS: kmod_kapi::netdev::KNetifOps = kmod_kapi::netdev::KNetifOps {
    send, tx_ready, recv, rx_rearm, rx_irq_driven, mac, link, link_gen, info, driver_name,
};

/// Reboot/kexec quiesce (phase WIFI): the driver's own hook, registered by `brcmf_boot::start`.
extern "C" fn pm_shutdown(_: usize, buf: *mut u8, cap: usize) -> i32 {
    match kenv::pm::hook().and_then(|f| f()) { Some(m) => copy_out(&m, buf, cap) as i32, None => 0 }
}

static DONE: AtomicBool = AtomicBool::new(false);

fn init() -> i32 {
    if DONE.swap(true, Ordering::SeqCst) { return -16; }
    // Not a BCM2712: nothing to probe, and no pin is touched.
    if kmod_kapi::plat::soc_stepping().is_none() { DONE.store(false, Ordering::SeqCst); return -19; }
    let cmdline = kmod_kapi::cmdline();
    bn::init(cmdline);
    match kmod_kapi::netdev::register(kmod_kapi::netdev::Kind::Wifi, "wifi", &OPS, 0) {
        Ok(id) => kmod_rt::kinfo!("registered netif {}", id),
        Err(e) => { kmod_rt::kerr!("netdev_register failed: {}", e); DONE.store(false, Ordering::SeqCst); return e; }
    }
    let ops = kmod_abi::KPmOps {
        size: core::mem::size_of::<kmod_abi::KPmOps>() as u32, phase: kmod_abi::pm_phase::WIFI,
        suspend: None, resume: None, freeze: None, thaw: None, shutdown: Some(pm_shutdown), kexec_quiesce: None, resume_check: None, unload_quiesce: None,
    };
    let _ = kmod_kapi::pm::register2(&ops, 0);
    // Probe (power cycle + SDIO host bring-up, up to ~2 s), then the bring-up chain, off the loader's thread.
    let started = kmod_kapi::sched::kthread::spawn("brcmf-probe", move || {
        brcmf::sdio_host::boot_probe();
        brcmf_boot::start(kmod_kapi::cmdline());
    });
    if started.is_none() { kmod_rt::kerr!("could not spawn the probe thread"); return -12; }
    0
}
