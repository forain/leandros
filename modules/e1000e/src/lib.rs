//! e1000e as a loadable module: the SAME driver source as the built-in
//! (`drivers/src/e1000e.rs` + `e1000e_hw.rs`), with `kenv` pointing at the kmod-kapi shim.
//! Autoloads by PCI alias for every id the built-in table matches (`hw::DEVICES`, 57 entries).
//! It registers its netif (`eth`) and the suspend/resume/shutdown hooks from `init`.
#![no_std]

extern crate alloc;
use alloc::string::String;
use core::sync::atomic::{AtomicBool, Ordering};

mod kenv {
    pub use kmod_kapi::{irq, mm, netdev, pci, sched, serial_raw};
}

#[path = "../../../drivers/src/e1000e_hw.rs"]
mod e1000e_hw;
#[path = "../../../drivers/src/e1000e.rs"]
mod e1000e;

/// The alias list below must cover the whole driver table.
const _: () = assert!(e1000e_hw::DEVICES.len() == 57);

kmod_rt::module! {
    name: "e1000e",
    init: init,
    ids: [
        kmod_rt::pci(0x8086, 0x156f),
        kmod_rt::pci(0x8086, 0x1570),
        kmod_rt::pci(0x8086, 0x15b7),
        kmod_rt::pci(0x8086, 0x15b8),
        kmod_rt::pci(0x8086, 0x15b9),
        kmod_rt::pci(0x8086, 0x15d7),
        kmod_rt::pci(0x8086, 0x15d8),
        kmod_rt::pci(0x8086, 0x15e3),
        kmod_rt::pci(0x8086, 0x15d6),
        kmod_rt::pci(0x8086, 0x0d53),
        kmod_rt::pci(0x8086, 0x0d55),
        kmod_rt::pci(0x8086, 0x15bd),
        kmod_rt::pci(0x8086, 0x15be),
        kmod_rt::pci(0x8086, 0x15bb),
        kmod_rt::pci(0x8086, 0x15bc),
        kmod_rt::pci(0x8086, 0x15df),
        kmod_rt::pci(0x8086, 0x15e0),
        kmod_rt::pci(0x8086, 0x15e1),
        kmod_rt::pci(0x8086, 0x15e2),
        kmod_rt::pci(0x8086, 0x0d4e),
        kmod_rt::pci(0x8086, 0x0d4f),
        kmod_rt::pci(0x8086, 0x0d4c),
        kmod_rt::pci(0x8086, 0x0d4d),
        kmod_rt::pci(0x8086, 0x15fb),
        kmod_rt::pci(0x8086, 0x15fc),
        kmod_rt::pci(0x8086, 0x15f9),
        kmod_rt::pci(0x8086, 0x15fa),
        kmod_rt::pci(0x8086, 0x15f4),
        kmod_rt::pci(0x8086, 0x15f5),
        kmod_rt::pci(0x8086, 0x0dc5),
        kmod_rt::pci(0x8086, 0x0dc6),
        kmod_rt::pci(0x8086, 0x1a1e),
        kmod_rt::pci(0x8086, 0x1a1f),
        kmod_rt::pci(0x8086, 0x1a1c),
        kmod_rt::pci(0x8086, 0x1a1d),
        kmod_rt::pci(0x8086, 0x0dc7),
        kmod_rt::pci(0x8086, 0x0dc8),
        kmod_rt::pci(0x8086, 0x550a),
        kmod_rt::pci(0x8086, 0x550b),
        kmod_rt::pci(0x8086, 0x550c),
        kmod_rt::pci(0x8086, 0x550d),
        kmod_rt::pci(0x8086, 0x550e),
        kmod_rt::pci(0x8086, 0x550f),
        kmod_rt::pci(0x8086, 0x5510),
        kmod_rt::pci(0x8086, 0x5511),
        kmod_rt::pci(0x8086, 0x57a0),
        kmod_rt::pci(0x8086, 0x57a1),
        kmod_rt::pci(0x8086, 0x57b3),
        kmod_rt::pci(0x8086, 0x57b4),
        kmod_rt::pci(0x8086, 0x57b5),
        kmod_rt::pci(0x8086, 0x57b6),
        kmod_rt::pci(0x8086, 0x57b7),
        kmod_rt::pci(0x8086, 0x57b8),
        kmod_rt::pci(0x8086, 0x57b9),
        kmod_rt::pci(0x8086, 0x57ba),
        kmod_rt::pci(0x8086, 0x10d3),
        kmod_rt::pci(0x8086, 0x10f6)
    ]
}

fn copy_out(s: &str, buf: *mut u8, cap: usize) -> usize {
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

extern "C" fn send(_: usize, b: *const u8, len: usize) -> bool {
    e1000e::send_packet(unsafe { core::slice::from_raw_parts(b, len) })
}
extern "C" fn tx_ready(_: usize) -> bool { true }
extern "C" fn recv(_: usize, b: *mut u8, cap: usize) -> i32 {
    match e1000e::poll_receive(unsafe { core::slice::from_raw_parts_mut(b, cap) }) {
        Some(n) => n as i32,
        None => -1,
    }
}
extern "C" fn rx_rearm(_: usize) -> bool { e1000e::rx_rearm() }
extern "C" fn rx_irq_driven(_: usize) -> bool { e1000e::rx_irq_driven() }
extern "C" fn mac(_: usize, out: *mut u8) -> bool {
    match e1000e::mac_address() {
        Some(m) => { unsafe { core::ptr::copy_nonoverlapping(m.as_ptr(), out, 6); } true }
        None => false,
    }
}
extern "C" fn link(_: usize, up: *mut bool, mbps: *mut u32, full: *mut bool) {
    let (u, m, f) = e1000e::link();
    unsafe { *up = u; *mbps = m; *full = f; }
}
extern "C" fn link_gen(_: usize) -> u32 { e1000e::link_generation() }
extern "C" fn info(_: usize, buf: *mut u8, cap: usize) -> usize {
    let mut s = String::new();
    e1000e::info_lines(&mut s);
    copy_out(&s, buf, cap)
}
extern "C" fn driver_name(_: usize, buf: *mut u8, cap: usize) -> usize { copy_out("e1000e", buf, cap) }

static OPS: kmod_kapi::netdev::KNetifOps = kmod_kapi::netdev::KNetifOps {
    send, tx_ready, recv, rx_rearm, rx_irq_driven, mac, link, link_gen, info, driver_name,
};

fn pm_msg(r: Result<String, String>, buf: *mut u8, cap: usize) -> i32 {
    match r {
        Ok(m) => copy_out(&m, buf, cap) as i32,
        Err(m) => -(copy_out(&m, buf, cap).max(1) as i32),
    }
}
extern "C" fn pm_suspend(_: usize, buf: *mut u8, cap: usize) -> i32 { pm_msg(e1000e::pm_suspend(), buf, cap) }
extern "C" fn pm_resume(_: usize, buf: *mut u8, cap: usize) -> i32 { pm_msg(e1000e::pm_resume(), buf, cap) }
extern "C" fn pm_shutdown(_: usize, buf: *mut u8, cap: usize) -> i32 {
    match e1000e::shutdown() { Some(m) => copy_out(&m, buf, cap) as i32, None => 0 }
}

static DONE: AtomicBool = AtomicBool::new(false);

fn init() -> i32 {
    if DONE.swap(true, Ordering::SeqCst) { return -16; }
    e1000e::init(kmod_kapi::cmdline());
    if !e1000e::present() { return -19; }
    match kmod_kapi::netdev::register(kmod_kapi::netdev::Kind::Wired, "eth", &OPS, 0) {
        Ok(id) => kmod_rt::kinfo!("registered netif {}", id),
        Err(e) => { kmod_rt::kerr!("netdev_register failed: {}", e); return e; }
    }
    let _ = kmod_kapi::pm::register(Some(pm_suspend), Some(pm_resume), Some(pm_shutdown), 0);
    0
}
