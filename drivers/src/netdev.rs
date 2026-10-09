//! The one Ethernet NIC the net server drives (`eth0`): a thin dispatch over
//! the drivers that can provide it. virtio-net wins when present (QEMU);
//! otherwise a Realtek RTL8168-family NIC (real PCs). Both are polled: one
//! received frame per `poll_receive`, one frame per `send_packet`.

use core::sync::atomic::{AtomicU8, Ordering};

const NONE: u8 = 0;
const VIRTIO: u8 = 1;
const R8169: u8 = 2;

static BACKEND: AtomicU8 = AtomicU8::new(NONE);

/// Probe every NIC driver and pick the backend. Kernel init, once, before
/// the net server's `init`.
pub fn init(cmdline: &str) {
    crate::virtio_net::init();
    crate::r8169::init(cmdline);
    let b = if crate::virtio_net::device_count() > 0 {
        VIRTIO
    } else if crate::r8169::present() {
        R8169
    } else {
        NONE
    };
    BACKEND.store(b, Ordering::SeqCst);
}

fn backend() -> u8 { BACKEND.load(Ordering::Relaxed) }

/// True when there is a NIC for `eth0`.
pub fn present() -> bool { backend() != NONE }

/// Short driver name for logs and /proc/net/nic.
pub fn driver_name() -> &'static str {
    match backend() { VIRTIO => "virtio-net", R8169 => "r8169", _ => "none" }
}

pub fn mac_address() -> Option<[u8; 6]> {
    match backend() {
        VIRTIO => crate::virtio_net::get_mac_address(0),
        R8169 => crate::r8169::mac_address(),
        _ => None,
    }
}

pub fn send_packet(buf: &[u8]) -> bool {
    match backend() {
        VIRTIO => crate::virtio_net::send_packet(0, buf),
        R8169 => crate::r8169::send_packet(buf),
        _ => false,
    }
}

pub fn poll_receive(buf: &mut [u8]) -> Option<usize> {
    match backend() {
        VIRTIO => crate::virtio_net::poll_receive(0, buf),
        R8169 => crate::r8169::poll_receive(buf),
        _ => None,
    }
}

/// (link up, Mbit/s, full duplex). virtio-net has no link state: always up.
pub fn link() -> (bool, u32, bool) {
    match backend() {
        VIRTIO => (true, 0, true),
        R8169 => crate::r8169::link(),
        _ => (false, 0, false),
    }
}

/// Incremented at each link-up transition; the net server restarts DHCP
/// when it changes, so a lease is requested as soon as a cable comes up
/// instead of at the DHCP client's next 10 s retry.
pub fn link_generation() -> u32 {
    match backend() { R8169 => crate::r8169::link_generation(), _ => 0 }
}

/// Driver details as `key: value` lines (for /proc/net/nic).
pub fn info_lines(out: &mut alloc::string::String) {
    use core::fmt::Write;
    match backend() {
        VIRTIO => {
            let _ = writeln!(out, "driver: virtio-net");
            if let Some(m) = mac_address() {
                let _ = writeln!(out, "mac: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
            }
            let _ = writeln!(out, "link: up");
            let _ = writeln!(out, "realtek: {}", crate::r8169::chip_name().unwrap_or("none"));
        }
        R8169 => crate::r8169::info_lines(out),
        _ => {
            let _ = writeln!(out, "driver: none");
            let _ = writeln!(out, "realtek: none");
        }
    }
}
