//! The network interface registry: every network device the net server
//! drives is one *netif* here, with a name, a kind and a table of driver
//! entry points. The net server (`servers/net`) builds one smoltcp stack per
//! netif; nothing else in the kernel talks to a NIC driver's data path.
//!
//! # Registry API (consumed by servers/net and /dev/netctl)
//!
//! * [`count`] / [`get`] / [`iter`] / [`by_name`]: the registered netifs. A
//!   netif's `id` (0-based, dense, in registration order) never changes and
//!   is never reused; `ifindex()` is `id + 2` (Linux numbering: 1 is `lo`,
//!   which the net server owns). Registration happens only during
//!   [`init`] (kernel init, single-threaded), so the table is read lock-free.
//! * [`Netif`]: `name()`, `kind`, `driver()`, `mac()`, `link()`,
//!   `link_generation()`, `send()`, `tx_ready()`, `recv()`, `rx_rearm()`,
//!   `rx_irq_driven()`, `info_lines()`.
//! * [`set_link_hook`]: one callback, `fn(id, LinkEvent)`, run by the net
//!   daemon (task context, no lock held) when it sees a netif's carrier go
//!   up or down ([`note_link`] is what the daemon calls). `/dev/netctl`
//!   registers it to turn link changes into events.
//!
//! # Driver contract ([`NetifOps`])
//!
//! A plain `fn` table, no `dyn`, no allocation on the hot path. Every entry
//! takes the driver's own device index (`Netif::dev`). One received frame
//! per `recv`, one frame per `send`. `link_gen` is incremented at each
//! link-up transition: the net server restarts DHCP when it changes, so a
//! lease is requested as soon as a cable comes up.
//!
//! # Daemon wake-up
//!
//! The net daemon is woken through `kick` / `kick_from_irq` (an RX
//! interrupt, a socket send, a reader freeing receive window) and otherwise
//! polls on a timer: 1 kHz while traffic flows, 100 Hz when idle. After it
//! drained a netif's receive ring it calls `rx_rearm`, which re-enables the
//! NIC's interrupt (NAPI style: the ISR masks it again on the next event),
//! re-reads the interrupt status and says whether work slipped in meanwhile.
//!
//! # Interfaces
//!
//! Every device is its own netif, registered by [`init`] in this order:
//! every virtio-net device (QEMU), a Realtek RTL8168-family NIC (real PCs),
//! an Intel e1000e (I219 laptops, QEMU's `-device e1000e`) — the wired ones,
//! named `eth0`, `eth1`, ... — then Intel Wi-Fi (`iwlwifi`) as `wifi0`.
//! Wi-Fi registers at probe time, before its firmware loads, so its stack
//! and every INADDR_ANY listener exist from boot; its MAC address is picked
//! up when it associates (the link generation changes). Routing between the
//! interfaces is the net server's (`servers/net/src/iface.rs`).

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// Most netifs the registry holds (the net server sizes its stack table on
/// this, plus one for `lo`).
pub const MAX_NETIF: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind { Wired, Wifi }

/// A netif's driver entry points; `dev` is the driver's device index.
pub struct NetifOps {
    /// Short driver name for logs and /proc/net/nic.
    pub driver: fn(u8) -> &'static str,
    pub send: fn(u8, &[u8]) -> bool,
    /// Room for another frame below the driver's queue-depth high-water
    /// mark. False makes the net server hold its frames (smoltcp keeps them
    /// queued) instead of handing the driver frames it would drop; the
    /// driver kicks the daemon once the queue drained.
    pub tx_ready: fn(u8) -> bool,
    pub recv: fn(u8, &mut [u8]) -> Option<usize>,
    /// Re-enable the RX interrupt after a drain; true when a frame is
    /// already waiting (the daemon then polls again instead of sleeping).
    pub rx_rearm: fn(u8) -> bool,
    /// Received frames wake the daemon by interrupt (else it relies on polling).
    pub rx_irq_driven: fn(u8) -> bool,
    pub mac: fn(u8) -> Option<[u8; 6]>,
    /// (link up, Mbit/s, full duplex).
    pub link: fn(u8) -> (bool, u32, bool),
    /// Incremented at each link-up transition (and, for a device whose MAC
    /// changes, when it does).
    pub link_gen: fn(u8) -> u32,
    /// Driver details as `key: value` lines (for /proc/net/nic).
    pub info: fn(u8, &mut alloc::string::String),
}

/// One registered network interface. `Copy`: callers take a copy and call
/// through it without holding anything.
#[derive(Clone, Copy)]
pub struct Netif {
    /// Dense registry index, 0-based, stable for the life of the system.
    pub id: u8,
    pub kind: Kind,
    /// The driver's own device index, passed to every `ops` entry.
    pub dev: u8,
    pub ops: &'static NetifOps,
    name: [u8; 8],
    name_len: u8,
}

impl Netif {
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..self.name_len as usize]).unwrap_or("?")
    }
    /// Linux-style interface index (`lo` is 1).
    pub fn ifindex(&self) -> u32 { self.id as u32 + 2 }
    pub fn driver(&self) -> &'static str { (self.ops.driver)(self.dev) }
    pub fn send(&self, buf: &[u8]) -> bool {
        if crate::netshape::shaped(self.id) { return crate::netshape::send(self, buf); }
        (self.ops.send)(self.dev, buf)
    }
    pub fn tx_ready(&self) -> bool {
        if crate::netshape::shaped(self.id) { return crate::netshape::tx_ready(self); }
        (self.ops.tx_ready)(self.dev)
    }
    pub fn recv(&self, buf: &mut [u8]) -> Option<usize> {
        if crate::netshape::shaped(self.id) { return crate::netshape::recv(self, buf); }
        (self.ops.recv)(self.dev, buf)
    }
    pub fn rx_rearm(&self) -> bool {
        if crate::netshape::shaped(self.id) { return crate::netshape::rx_rearm(self); }
        (self.ops.rx_rearm)(self.dev)
    }
    pub fn rx_irq_driven(&self) -> bool { (self.ops.rx_irq_driven)(self.dev) }
    pub fn mac(&self) -> Option<[u8; 6]> { (self.ops.mac)(self.dev) }
    pub fn link(&self) -> (bool, u32, bool) { (self.ops.link)(self.dev) }
    pub fn link_generation(&self) -> u32 { (self.ops.link_gen)(self.dev) }
    pub fn info_lines(&self, out: &mut alloc::string::String) {
        (self.ops.info)(self.dev, out);
        crate::netshape::info_line(self.id, out);
    }
}

/// Write-once table: entries `< COUNT` are initialised and never change.
struct Table(core::cell::UnsafeCell<[Option<Netif>; MAX_NETIF]>);
unsafe impl Sync for Table {}
static TABLE: Table = Table(core::cell::UnsafeCell::new([None; MAX_NETIF]));
static COUNT: AtomicUsize = AtomicUsize::new(0);

/// Add a netif named `<prefix><n>` (n = how many of that prefix exist).
/// Kernel init only (single-threaded). `None` when the table is full.
fn register(kind: Kind, prefix: &str, ops: &'static NetifOps, dev: u8) -> Option<u8> {
    let n = COUNT.load(Ordering::Acquire);
    if n >= MAX_NETIF { return None; }
    let same = iter().filter(|i| i.name().starts_with(prefix)
        && i.name()[prefix.len()..].bytes().all(|b| b.is_ascii_digit())).count();
    let mut name = [0u8; 8];
    let p = prefix.as_bytes();
    name[..p.len()].copy_from_slice(p);
    let mut len = p.len();
    if same >= 10 { name[len] = b'0' + (same / 10) as u8; len += 1; }
    name[len] = b'0' + (same % 10) as u8;
    len += 1;
    let nif = Netif { id: n as u8, kind, dev, ops, name, name_len: len as u8 };
    unsafe { (*TABLE.0.get())[n] = Some(nif); }
    COUNT.store(n + 1, Ordering::Release);
    extern "C" { fn arch_serial_putc(b: u8); }
    for &b in b"[NET] netif " { unsafe { arch_serial_putc(b); } }
    for &b in nif.name().as_bytes() { unsafe { arch_serial_putc(b); } }
    for &b in b" = " { unsafe { arch_serial_putc(b); } }
    for &b in nif.driver().as_bytes() { unsafe { arch_serial_putc(b); } }
    for &b in b"\r\n" { unsafe { arch_serial_putc(b); } }
    Some(n as u8)
}

/// How many netifs are registered.
pub fn count() -> usize { COUNT.load(Ordering::Acquire) }

/// The netif with registry index `id`.
pub fn get(id: usize) -> Option<Netif> {
    if id >= count() { return None; }
    unsafe { (*TABLE.0.get())[id] }
}

/// Every registered netif, in registry order.
pub fn iter() -> impl Iterator<Item = Netif> { (0..count()).filter_map(get) }

/// The netif called `name` (`eth0`, `wifi0`, ...).
pub fn by_name(name: &str) -> Option<Netif> { iter().find(|i| i.name() == name) }

/// What the net daemon saw happen to a netif's carrier.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LinkEvent { Up { speed: u32 }, Down }

static LINK_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register the link-event callback (one; a later call replaces it).
pub fn set_link_hook(f: fn(u8, LinkEvent)) { LINK_HOOK.store(f as usize, Ordering::Release); }

/// Called by the net daemon (no lock held) on a carrier transition.
pub fn note_link(id: u8, ev: LinkEvent) {
    let f = LINK_HOOK.load(Ordering::Acquire);
    if f != 0 {
        let f: fn(u8, LinkEvent) = unsafe { core::mem::transmute(f) };
        f(id, ev);
    }
}

// ── probe and the per-driver op tables ──────────────────────────────────────

/// Probe every NIC driver and register the netifs. Kernel init, once, before
/// the net server's `init`.
pub fn init(cmdline: &str) {
    crate::virtio_net::init();
    crate::r8169::init(cmdline);
    crate::e1000e::init(cmdline);
    crate::iwlwifi::init(cmdline);
    // The simulated Wi-Fi (QEMU only, opt-in) may claim a virtio-net device
    // as its data path; that device is then wifi0, not an ethN.
    crate::wifi_sim::init(cmdline);
    for i in 0..crate::virtio_net::device_count() {
        if crate::wifi_sim::claims_virtio(i) { continue; }
        register(Kind::Wired, "eth", &VIRTIO_OPS, i as u8);
    }
    if crate::r8169::present() { register(Kind::Wired, "eth", &R8169_OPS, 0); }
    if crate::e1000e::present() { register(Kind::Wired, "eth", &E1000E_OPS, 0); }
    // Wi-Fi netifs, then their control side on the /dev/netctl seam
    // (iwlwifi serves it only with iwlwifi.mgr=1).
    if crate::wifi_sim::present() {
        if let Some(n) = register(Kind::Wifi, "wifi", &WIFISIM_OPS, 0).and_then(|id| get(id as usize)) {
            crate::wifi_sim::attach(n.ifindex(), n.name());
        }
    }
    if crate::iwlwifi::present() {
        if let Some(n) = register(Kind::Wifi, "wifi", &IWL_OPS, 0).and_then(|id| get(id as usize)) {
            crate::iwlwifi::attach(n.ifindex(), n.name());
        }
    }
    crate::netshape::init(cmdline);
}

fn fmt_mac(out: &mut alloc::string::String, m: Option<[u8; 6]>) {
    use core::fmt::Write;
    if let Some(m) = m {
        let _ = writeln!(out, "mac: {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
    }
}

static VIRTIO_OPS: NetifOps = NetifOps {
    driver: |_| "virtio-net",
    send: |d, b| crate::virtio_net::send_packet(d as usize, b),
    tx_ready: |_| true,
    recv: |d, b| crate::virtio_net::poll_receive(d as usize, b),
    rx_rearm: |d| crate::virtio_net::rx_rearm(d as usize),
    rx_irq_driven: |_| false,
    mac: |d| crate::virtio_net::get_mac_address(d as usize),
    link: |d| (crate::virtio_net::link_up(d as usize), 0, true),
    link_gen: |d| crate::virtio_net::link_generation(d as usize),
    info: |d, out| {
        use core::fmt::Write;
        let _ = writeln!(out, "driver: virtio-net");
        fmt_mac(out, crate::virtio_net::get_mac_address(d as usize));
        let _ = writeln!(out, "link: {}", if crate::virtio_net::link_up(d as usize) { "up" } else { "down" });
        let _ = writeln!(out, "realtek: {}", crate::r8169::chip_name().unwrap_or("none"));
        let _ = writeln!(out, "intel: {}", crate::e1000e::chip_name().unwrap_or("none"));
    },
};

static R8169_OPS: NetifOps = NetifOps {
    driver: |_| "r8169",
    send: |_, b| crate::r8169::send_packet(b),
    tx_ready: |_| true,
    recv: |_, b| crate::r8169::poll_receive(b),
    rx_rearm: |_| crate::r8169::rx_rearm(),
    rx_irq_driven: |_| crate::r8169::rx_irq_driven(),
    mac: |_| crate::r8169::mac_address(),
    link: |_| crate::r8169::link(),
    link_gen: |_| crate::r8169::link_generation(),
    info: |_, out| crate::r8169::info_lines(out),
};

static E1000E_OPS: NetifOps = NetifOps {
    driver: |_| "e1000e",
    send: |_, b| crate::e1000e::send_packet(b),
    tx_ready: |_| true,
    recv: |_, b| crate::e1000e::poll_receive(b),
    rx_rearm: |_| crate::e1000e::rx_rearm(),
    rx_irq_driven: |_| crate::e1000e::rx_irq_driven(),
    mac: |_| crate::e1000e::mac_address(),
    link: |_| crate::e1000e::link(),
    link_gen: |_| crate::e1000e::link_generation(),
    info: |_, out| crate::e1000e::info_lines(out),
};

static WIFISIM_OPS: NetifOps = NetifOps {
    driver: |_| "wifi_sim",
    send: |_, b| crate::wifi_sim::send_packet(b),
    tx_ready: |_| true,
    recv: |_, b| crate::wifi_sim::poll_receive(b),
    rx_rearm: |_| crate::wifi_sim::rx_rearm(),
    rx_irq_driven: |_| false,
    mac: |_| crate::wifi_sim::mac_address(),
    link: |_| (crate::wifi_sim::link_up(), 54, true),
    link_gen: |_| crate::wifi_sim::link_generation(),
    info: |_, out| crate::wifi_sim::info_lines(out),
};

static IWL_OPS: NetifOps = NetifOps {
    driver: |_| "iwlwifi",
    send: |_, b| crate::iwlwifi::send_packet(b),
    tx_ready: |_| crate::iwlwifi::tx_ready(),
    recv: |_, b| crate::iwlwifi::poll_receive(b),
    rx_rearm: |_| false,
    rx_irq_driven: |_| false,
    mac: |_| crate::iwlwifi::mac_address(),
    link: |_| (crate::iwlwifi::link_up(), 0, true),
    link_gen: |_| crate::iwlwifi::link_generation(),
    info: |_, out| crate::iwlwifi::info_lines(out),
};

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
