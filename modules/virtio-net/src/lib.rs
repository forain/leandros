//! virtio-net as a loadable module: the SAME driver source as the built-in
//! (`drivers/src/virtio_net.rs`), with `kenv` pointing at the kmod-kapi shim.
//! Autoloads by PCI alias (1af4:1041 modern, 1af4:1000 transitional). Every device found
//! becomes its own netif (`ethN`), registered from `init` with the device index as ctx.
#![no_std]

extern crate alloc;
use alloc::string::String;
use core::fmt::Write;
use core::sync::atomic::{AtomicBool, Ordering};

mod kenv {
    pub use kmod_kapi::{irq, mm, netdev, pci, sched, serial_raw};
}

#[path = "../../../drivers/src/virtio.rs"]
#[allow(dead_code)]
mod virtio;
#[path = "../../../drivers/src/virtio_net.rs"]
mod virtio_net;

kmod_rt::module! {
    name: "virtio_net",
    init: init,
    ids: [kmod_rt::pci(0x1af4, 0x1041), kmod_rt::pci(0x1af4, 0x1000)]
}

fn copy_out(s: &str, buf: *mut u8, cap: usize) -> usize {
    let n = s.len().min(cap);
    unsafe { core::ptr::copy_nonoverlapping(s.as_ptr(), buf, n); }
    n
}

extern "C" fn send(d: usize, b: *const u8, len: usize) -> bool {
    virtio_net::send_packet(d, unsafe { core::slice::from_raw_parts(b, len) })
}
extern "C" fn tx_ready(_: usize) -> bool { true }
extern "C" fn recv(d: usize, b: *mut u8, cap: usize) -> i32 {
    match virtio_net::poll_receive(d, unsafe { core::slice::from_raw_parts_mut(b, cap) }) {
        Some(n) => n as i32,
        None => -1,
    }
}
extern "C" fn rx_rearm(d: usize) -> bool { virtio_net::rx_rearm(d) }
extern "C" fn rx_irq_driven(d: usize) -> bool { virtio_net::rx_irq_armed(d) }
extern "C" fn mac(d: usize, out: *mut u8) -> bool {
    match virtio_net::get_mac_address(d) {
        Some(m) => { unsafe { core::ptr::copy_nonoverlapping(m.as_ptr(), out, 6); } true }
        None => false,
    }
}
// virtio has no PHY: a nominal 1000 Mb/s full duplex while up, as the built-in reports.
extern "C" fn link(d: usize, up: *mut bool, mbps: *mut u32, full: *mut bool) {
    let u = virtio_net::link_up(d);
    unsafe { *up = u; *mbps = if u { 1000 } else { 0 }; *full = true; }
}
extern "C" fn link_gen(d: usize) -> u32 { virtio_net::link_generation(d) }
extern "C" fn info(d: usize, buf: *mut u8, cap: usize) -> usize {
    let mut s = String::new();
    let _ = writeln!(s, "driver: virtio-net");
    if let Some(m) = virtio_net::get_mac_address(d) {
        let _ = writeln!(s, "mac: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
    }
    let _ = writeln!(s, "link: {}", if virtio_net::link_up(d) { "up" } else { "down" });
    copy_out(&s, buf, cap)
}
extern "C" fn driver_name(_: usize, buf: *mut u8, cap: usize) -> usize { copy_out("virtio-net", buf, cap) }

static OPS: kmod_kapi::netdev::KNetifOps = kmod_kapi::netdev::KNetifOps {
    send, tx_ready, recv, rx_rearm, rx_irq_driven, mac, link, link_gen, info, driver_name,
};

static DONE: AtomicBool = AtomicBool::new(false);

fn init() -> i32 {
    if DONE.swap(true, Ordering::SeqCst) { return -16; }
    virtio_net::init();
    let n = virtio_net::device_count();
    if n == 0 { return -19; }
    for i in 0..n {
        match kmod_kapi::netdev::register(kmod_kapi::netdev::Kind::Wired, "eth", &OPS, i) {
            Ok(id) => kmod_rt::kinfo!("registered netif {} (device {})", id, i),
            Err(e) => { kmod_rt::kerr!("netdev_register failed: {}", e); return e; }
        }
    }
    0
}
