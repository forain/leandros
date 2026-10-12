//! The one Ethernet NIC the net server drives (`eth0`): a thin dispatch over
//! the drivers that can provide it. virtio-net wins when present (QEMU);
//! otherwise a Realtek RTL8168-family NIC (real PCs), then an Intel e1000e
//! (I219 laptops, QEMU's `-device e1000e`). One received frame per
//! `poll_receive`, one frame per `send_packet`.
//!
//! The net daemon is woken through `kick` / `kick_from_irq` (the r8169 RX
//! interrupt, a socket send, a reader freeing receive window) and otherwise
//! polls on a timer: 1 kHz while traffic flows, 100 Hz when idle. After it
//! drained the receive ring it calls `rx_rearm`, which re-enables the NIC's
//! interrupt (NAPI style: the ISR masks it again on the next event), re-reads
//! the interrupt status and says whether work slipped in meanwhile.
//!
//! Wi-Fi (`iwlwifi`) is a fallback backend: it carries `eth0` only while the
//! wired NIC has no link (or there is none) and Wi-Fi is associated. A
//! switch between the two bumps `link_generation`, so the net server picks up
//! the other interface's MAC address and restarts DHCP.

use core::sync::atomic::{AtomicBool, AtomicU8, Ordering};

const NONE: u8 = 0;
const VIRTIO: u8 = 1;
const R8169: u8 = 2;
const E1000E: u8 = 3;
const IWL: u8 = 4;

static BACKEND: AtomicU8 = AtomicU8::new(NONE);

/// Probe every NIC driver and pick the backend. Kernel init, once, before
/// the net server's `init`.
pub fn init(cmdline: &str) {
    crate::virtio_net::init();
    crate::r8169::init(cmdline);
    crate::e1000e::init(cmdline);
    crate::iwlwifi::init(cmdline);
    let b = if crate::virtio_net::device_count() > 0 {
        VIRTIO
    } else if crate::r8169::present() {
        R8169
    } else if crate::e1000e::present() {
        E1000E
    } else {
        NONE
    };
    BACKEND.store(b, Ordering::SeqCst);
}

/// The backend carrying traffic now: the wired one, unless it has no link
/// and Wi-Fi does (virtio-net, always up, is never displaced).
fn backend() -> u8 {
    let b = BACKEND.load(Ordering::Relaxed);
    if b == VIRTIO || !crate::iwlwifi::present() { return b; }
    let wired_up = match b {
        R8169 => crate::r8169::link().0,
        E1000E => crate::e1000e::link().0,
        _ => false,
    };
    if b == NONE || (!wired_up && crate::iwlwifi::link_up()) { IWL } else { b }
}

/// Bumped when traffic moves between the wired NIC and Wi-Fi.
static SWITCHES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
static LAST_ACTIVE: AtomicU8 = AtomicU8::new(NONE);

/// The poll-channel tag the net daemon parks on (besides its deadline and
/// the broadcast). Nothing else registers it on purpose; a hash collision
/// with another object's tag only costs the daemon an extra pass.
pub const NETD_TAG: u64 = sched::poll_tag(sched::poll_class::INET, 0xFFFF_FFF0);

/// Set by a kick, cleared by the daemon when it starts a pass. Only the
/// kick that sets it pays for the wake, so a burst of sends costs one.
static KICKED: AtomicBool = AtomicBool::new(false);

/// Wake the net daemon now (task context, no server lock held).
pub fn kick() {
    if !KICKED.swap(true, Ordering::AcqRel) { sched::wake_poll_tagged(NETD_TAG); }
}

/// `kick` for interrupt context: a bounded try-lock; when contended the wake
/// is deferred to the next tick rather than lost.
pub fn kick_from_irq() {
    if !KICKED.swap(true, Ordering::AcqRel) && !sched::try_wake_poll_tagged(NETD_TAG) {
        sched::request_poll_wake_tagged(NETD_TAG);
    }
}

/// Start of a daemon pass: consume the pending kick.
pub fn take_kick() -> bool { KICKED.swap(false, Ordering::AcqRel) }

/// A kick arrived since the pass started (checked after the daemon published
/// its park, so a kick in between cancels the sleep instead of being lost).
pub fn kicked() -> bool { KICKED.load(Ordering::Acquire) }

/// After the daemon drained the receive ring: re-enable the RX interrupt and
/// flush deferred ring notifications. True when a frame is already waiting
/// (the daemon then polls again instead of sleeping).
pub fn rx_rearm() -> bool {
    match backend() {
        VIRTIO => crate::virtio_net::rx_rearm(0),
        R8169 => crate::r8169::rx_rearm(),
        E1000E => crate::e1000e::rx_rearm(),
        IWL => false,
        _ => false,
    }
}

/// Received frames wake the daemon by interrupt (else it relies on polling).
pub fn rx_irq_driven() -> bool {
    match backend() { R8169 => crate::r8169::rx_irq_driven(), E1000E => crate::e1000e::rx_irq_driven(), _ => false }
}

/// True when there is a NIC for `eth0`.
pub fn present() -> bool { backend() != NONE }

/// Short driver name for logs and /proc/net/nic.
pub fn driver_name() -> &'static str {
    match backend() { VIRTIO => "virtio-net", R8169 => "r8169", E1000E => "e1000e", IWL => "iwlwifi", _ => "none" }
}

pub fn mac_address() -> Option<[u8; 6]> {
    match backend() {
        VIRTIO => crate::virtio_net::get_mac_address(0),
        R8169 => crate::r8169::mac_address(),
        E1000E => crate::e1000e::mac_address(),
        IWL => crate::iwlwifi::mac_address(),
        _ => None,
    }
}

pub fn send_packet(buf: &[u8]) -> bool {
    match backend() {
        VIRTIO => crate::virtio_net::send_packet(0, buf),
        R8169 => crate::r8169::send_packet(buf),
        E1000E => crate::e1000e::send_packet(buf),
        IWL => crate::iwlwifi::send_packet(buf),
        _ => false,
    }
}

pub fn poll_receive(buf: &mut [u8]) -> Option<usize> {
    match backend() {
        VIRTIO => crate::virtio_net::poll_receive(0, buf),
        R8169 => crate::r8169::poll_receive(buf),
        E1000E => crate::e1000e::poll_receive(buf),
        IWL => crate::iwlwifi::poll_receive(buf),
        _ => None,
    }
}

/// (link up, Mbit/s, full duplex). virtio-net has no link state: always up.
pub fn link() -> (bool, u32, bool) {
    match backend() {
        VIRTIO => (true, 0, true),
        R8169 => crate::r8169::link(),
        E1000E => crate::e1000e::link(),
        IWL => (crate::iwlwifi::link_up(), 0, true),
        _ => (false, 0, false),
    }
}

/// Incremented at each link-up transition; the net server restarts DHCP
/// when it changes, so a lease is requested as soon as a cable comes up
/// instead of at the DHCP client's next 10 s retry.
pub fn link_generation() -> u32 {
    let b = backend();
    if LAST_ACTIVE.swap(b, Ordering::Relaxed) != b { SWITCHES.fetch_add(1, Ordering::Relaxed); }
    let own = match b {
        R8169 => crate::r8169::link_generation(),
        E1000E => crate::e1000e::link_generation(),
        IWL => crate::iwlwifi::link_generation(),
        _ => 0,
    };
    own.wrapping_add(SWITCHES.load(Ordering::Relaxed).wrapping_mul(0x1000))
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
            let _ = writeln!(out, "intel: {}", crate::e1000e::chip_name().unwrap_or("none"));
        }
        R8169 => crate::r8169::info_lines(out),
        E1000E => crate::e1000e::info_lines(out),
        IWL => crate::iwlwifi::info_lines(out),
        _ => {
            let _ = writeln!(out, "driver: none");
            let _ = writeln!(out, "realtek: none");
            let _ = writeln!(out, "intel: none");
        }
    }
    if backend() != IWL && crate::iwlwifi::present() {
        let _ = writeln!(out, "wifi: standby (see /proc/net/wifi)");
    }
}
