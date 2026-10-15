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
//!   `link_generation()`, `send()`, `recv()`, `rx_rearm()`,
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
//! # Interfaces (this revision)
//!
//! One netif, `eth0`, carried by the first wired NIC found: virtio-net when
//! present (QEMU), otherwise a Realtek RTL8168-family NIC (real PCs), then an
//! Intel e1000e (I219 laptops, QEMU's `-device e1000e`). Wi-Fi (`iwlwifi`)
//! is still a fallback backend of that one netif: it carries `eth0` only
//! while the wired NIC has no link (or there is none) and Wi-Fi is
//! associated. A switch between the two bumps `link_generation`, so the net
//! server picks up the other interface's MAC address and restarts DHCP.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};

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
    pub fn send(&self, buf: &[u8]) -> bool { (self.ops.send)(self.dev, buf) }
    pub fn recv(&self, buf: &mut [u8]) -> Option<usize> { (self.ops.recv)(self.dev, buf) }
    pub fn rx_rearm(&self) -> bool { (self.ops.rx_rearm)(self.dev) }
    pub fn rx_irq_driven(&self) -> bool { (self.ops.rx_irq_driven)(self.dev) }
    pub fn mac(&self) -> Option<[u8; 6]> { (self.ops.mac)(self.dev) }
    pub fn link(&self) -> (bool, u32, bool) { (self.ops.link)(self.dev) }
    pub fn link_generation(&self) -> u32 { (self.ops.link_gen)(self.dev) }
    pub fn info_lines(&self, out: &mut alloc::string::String) { (self.ops.info)(self.dev, out) }
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

// ── the legacy eth0 backend (wired NIC, Wi-Fi fallback) ─────────────────────

const NONE: u8 = 0;
const VIRTIO: u8 = 1;
const R8169: u8 = 2;
const E1000E: u8 = 3;
const IWL: u8 = 4;

static BACKEND: AtomicU8 = AtomicU8::new(NONE);

/// Probe every NIC driver and register the netifs. Kernel init, once, before
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
    if backend() != NONE {
        register(Kind::Wired, "eth", &LEGACY_OPS, 0);
    }
}

static LEGACY_OPS: NetifOps = NetifOps {
    driver: |_| driver_name(),
    send: |_, b| send_packet(b),
    recv: |_, b| poll_receive(b),
    rx_rearm: |_| rx_rearm(),
    rx_irq_driven: |_| rx_irq_driven(),
    mac: |_| mac_address(),
    link: |_| link(),
    link_gen: |_| link_generation(),
    info: |_, out| info_lines(out),
};

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

fn rx_rearm() -> bool {
    match backend() {
        VIRTIO => crate::virtio_net::rx_rearm(0),
        R8169 => crate::r8169::rx_rearm(),
        E1000E => crate::e1000e::rx_rearm(),
        IWL => false,
        _ => false,
    }
}

fn rx_irq_driven() -> bool {
    match backend() { R8169 => crate::r8169::rx_irq_driven(), E1000E => crate::e1000e::rx_irq_driven(), _ => false }
}

fn driver_name() -> &'static str {
    match backend() { VIRTIO => "virtio-net", R8169 => "r8169", E1000E => "e1000e", IWL => "iwlwifi", _ => "none" }
}

fn mac_address() -> Option<[u8; 6]> {
    match backend() {
        VIRTIO => crate::virtio_net::get_mac_address(0),
        R8169 => crate::r8169::mac_address(),
        E1000E => crate::e1000e::mac_address(),
        IWL => crate::iwlwifi::mac_address(),
        _ => None,
    }
}

fn send_packet(buf: &[u8]) -> bool {
    match backend() {
        VIRTIO => crate::virtio_net::send_packet(0, buf),
        R8169 => crate::r8169::send_packet(buf),
        E1000E => crate::e1000e::send_packet(buf),
        IWL => crate::iwlwifi::send_packet(buf),
        _ => false,
    }
}

fn poll_receive(buf: &mut [u8]) -> Option<usize> {
    match backend() {
        VIRTIO => crate::virtio_net::poll_receive(0, buf),
        R8169 => crate::r8169::poll_receive(buf),
        E1000E => crate::e1000e::poll_receive(buf),
        IWL => crate::iwlwifi::poll_receive(buf),
        _ => None,
    }
}

/// (link up, Mbit/s, full duplex). virtio-net has no link state: always up.
fn link() -> (bool, u32, bool) {
    match backend() {
        VIRTIO => (true, 0, true),
        R8169 => crate::r8169::link(),
        E1000E => crate::e1000e::link(),
        IWL => (crate::iwlwifi::link_up(), 0, true),
        _ => (false, 0, false),
    }
}

fn link_generation() -> u32 {
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

fn info_lines(out: &mut alloc::string::String) {
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
