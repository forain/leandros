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
//! an Intel e1000e (I219 laptops, QEMU's `-device e1000e`), an Amazon ENA (EC2) — the wired ones,
//! named `eth0`, `eth1`, ... — then Intel Wi-Fi (`iwlwifi`) as `wifi0`, or on the Raspberry Pi 5 the
//! CYW43455 (`brcmf`).
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

static LATE_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// `register` for a driver that probes after boot (a loadable module): serialised,
/// and the net daemon is kicked so its next pass builds the stack for the new netif.
/// Readers need no lock: entries below `COUNT` never change.
pub fn register_late(kind: Kind, prefix: &str, ops: &'static NetifOps, dev: u8) -> Option<u8> {
    let id = { let _g = LATE_LOCK.lock(); register(kind, prefix, ops, dev) };
    if id.is_some() { kick(); }
    id
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
    #[cfg(feature = "r8169")] crate::r8169::init(cmdline);
    #[cfg(feature = "e1000e")] crate::e1000e::init(cmdline);
    #[cfg(feature = "ena")] crate::ena::init(cmdline);
    #[cfg(feature = "macb")] crate::macb::init(cmdline);
    #[cfg(feature = "iwlwifi")] crate::iwlwifi::init(cmdline);
    // The simulated Wi-Fi (QEMU only, opt-in) may claim a virtio-net device
    // as its data path; that device is then wifi0, not an ethN.
    #[cfg(feature = "wifi-sim")] crate::wifi_sim::init(cmdline);
    // CYW43455 (Raspberry Pi 5 only; inert in every other build).
    #[cfg(feature = "brcmf")] crate::brcmf::netdev::init(cmdline);
    for i in 0..crate::virtio_net::device_count() {
        #[cfg(feature = "wifi-sim")] if crate::wifi_sim::claims_virtio(i) { continue; }
        register(Kind::Wired, "eth", &VIRTIO_OPS, i as u8);
    }
    #[cfg(feature = "r8169")] if crate::r8169::present() { register(Kind::Wired, "eth", &R8169_OPS, 0); }
    #[cfg(feature = "e1000e")] if crate::e1000e::present() { register(Kind::Wired, "eth", &E1000E_OPS, 0); }
    #[cfg(feature = "ena")] if crate::ena::present() { register(Kind::Wired, "eth", &ENA_OPS, 0); }
    #[cfg(feature = "macb")] if crate::macb::present() { register(Kind::Wired, "eth", &crate::macb::MACB_OPS, 0); }
    // Wi-Fi netifs, then their control side on the /dev/netctl seam
    // (iwlwifi serves it only with iwlwifi.mgr=1).
    #[cfg(feature = "wifi-sim")] if crate::wifi_sim::present() {
        if let Some(n) = register(Kind::Wifi, "wifi", &WIFISIM_OPS, 0).and_then(|id| get(id as usize)) {
            crate::wifi_sim::attach(n.ifindex(), n.name());
        }
    }
    #[cfg(feature = "iwlwifi")] if crate::iwlwifi::present() {
        if let Some(n) = register(Kind::Wifi, "wifi", &IWL_OPS, 0).and_then(|id| get(id as usize)) {
            crate::iwlwifi::attach(n.ifindex(), n.name());
        }
    }
    #[cfg(feature = "brcmf")] if crate::brcmf::netdev::present() {
        if let Some(n) = register(Kind::Wifi, "wifi", &BRCMF_OPS, 0).and_then(|id| get(id as usize)) {
            crate::brcmf::netdev::attach(n.ifindex(), n.name());
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
    rx_irq_driven: |d| crate::virtio_net::rx_irq_armed(d as usize),
    mac: |d| crate::virtio_net::get_mac_address(d as usize),
    // virtio has no PHY and QEMU leaves VIRTIO_NET_F_SPEED_DUPLEX speed unknown
    // by default; report a nominal 1000 Mb/s full duplex while up so NM clients
    // show a speed (0 hid it), as QEMU's emulated NICs do.
    link: |d| {
        let up = crate::virtio_net::link_up(d as usize);
        (up, if up { 1000 } else { 0 }, true)
    },
    link_gen: |d| crate::virtio_net::link_generation(d as usize),
    info: |d, out| {
        use core::fmt::Write;
        let _ = writeln!(out, "driver: virtio-net");
        fmt_mac(out, crate::virtio_net::get_mac_address(d as usize));
        let _ = writeln!(out, "link: {}", if crate::virtio_net::link_up(d as usize) { "up" } else { "down" });
        #[cfg(feature = "r8169")] let _ = writeln!(out, "realtek: {}", crate::r8169::chip_name().unwrap_or("none"));
        #[cfg(feature = "e1000e")] let _ = writeln!(out, "intel: {}", crate::e1000e::chip_name().unwrap_or("none"));
        #[cfg(feature = "macb")] let _ = writeln!(out, "macb: {}", crate::macb::state());
    },
};

#[cfg(feature = "r8169")] static R8169_OPS: NetifOps = NetifOps {
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

#[cfg(feature = "e1000e")] static E1000E_OPS: NetifOps = NetifOps {
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

#[cfg(feature = "ena")] static ENA_OPS: NetifOps = NetifOps {
    driver: |_| "ena",
    send: |_, b| crate::ena::send_packet(b),
    tx_ready: |_| crate::ena::tx_ready(),
    recv: |_, b| crate::ena::poll_receive(b),
    rx_rearm: |_| crate::ena::rx_rearm(),
    rx_irq_driven: |_| crate::ena::rx_irq_driven(),
    mac: |_| crate::ena::mac_address(),
    link: |_| crate::ena::link(),
    link_gen: |_| crate::ena::link_generation(),
    info: |_, out| crate::ena::info_lines(out),
};

#[cfg(feature = "wifi-sim")] static WIFISIM_OPS: NetifOps = NetifOps {
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

#[cfg(feature = "iwlwifi")] static IWL_OPS: NetifOps = NetifOps {
    driver: |_| "iwlwifi",
    send: |_, b| crate::iwlwifi::send_packet(b),
    tx_ready: |_| crate::iwlwifi::tx_ready(),
    recv: |_, b| crate::iwlwifi::poll_receive(b),
    rx_rearm: |_| crate::iwlwifi::rx_rearm(),
    rx_irq_driven: |_| crate::iwlwifi::rx_irq_driven(),
    mac: |_| crate::iwlwifi::mac_address(),
    link: |_| (crate::iwlwifi::link_up(), 0, true),
    link_gen: |_| crate::iwlwifi::link_generation(),
    info: |_, out| crate::iwlwifi::info_lines(out),
};

#[cfg(feature = "brcmf")] static BRCMF_OPS: NetifOps = NetifOps {
    driver: |_| "brcmfmac",
    send: |_, b| crate::brcmf::netdev::send_packet(b),
    tx_ready: |_| crate::brcmf::netdev::tx_ready(),
    recv: |_, b| crate::brcmf::netdev::poll_receive(b),
    rx_rearm: |_| crate::brcmf::netdev::rx_rearm(),
    rx_irq_driven: |_| crate::brcmf::netdev::rx_irq_driven(),
    mac: |_| crate::brcmf::netdev::mac_address(),
    link: |_| (crate::brcmf::netdev::link_up(), crate::brcmf::netdev::link_speed(), true),
    link_gen: |_| crate::brcmf::netdev::link_generation(),
    info: |_, out| crate::brcmf::netdev::info_lines(out),
};

/// The poll-channel tag the net daemon parks on (besides its deadline and
/// the broadcast). Nothing else registers it on purpose; a hash collision
/// with another object's tag only costs the daemon an extra pass.
pub const NETD_TAG: u64 = sched::poll_tag(sched::poll_class::INET, 0xFFFF_FFF0);
/// The exact key of `NETD_TAG` (same bit): kicks are keyed wakes, so a poller
/// that only shares the bit through keyed/foreign interests is not woken (the
/// daemon parks with the bare bit, never refined, so it is always reached).
/// Bit-only kicks woke ~13 tasks each on the laptop during video streaming.
pub const NETD_KEY: u64 = sched::poll_key(sched::poll_class::INET, 0xFFFF_FFF0);

/// Set by a kick, cleared by the daemon when it starts a pass. Only the
/// kick that sets it pays for the wake, so a burst of sends costs one.
static KICKED: AtomicBool = AtomicBool::new(false);

/// Wake the net daemon now (task context, no server lock held).
#[track_caller]
pub fn kick() {
    if !KICKED.swap(true, Ordering::AcqRel) {
        kick_site(core::panic::Location::caller());
        sched::wake_poll_key(NETD_KEY);
    }
}

/// Effective kicks (the ones that set `KICKED` and paid for a wake) by call
/// site, with the pid that made the last one: /proc/net/nic `netd_kick`.
const KICK_SITES: usize = 32;
static KICK_LOC: [AtomicUsize; KICK_SITES] = [const { AtomicUsize::new(0) }; KICK_SITES];
static KICK_CNT: [core::sync::atomic::AtomicU64; KICK_SITES] = [const { core::sync::atomic::AtomicU64::new(0) }; KICK_SITES];
static KICK_PID: [AtomicUsize; KICK_SITES] = [const { AtomicUsize::new(0) }; KICK_SITES];

fn kick_site(loc: &'static core::panic::Location<'static>) {
    let key = loc as *const _ as usize;
    for i in 0..KICK_SITES {
        let cur = KICK_LOC[i].load(Ordering::Relaxed);
        let hit = cur == key || (cur == 0 && KICK_LOC[i]
            .compare_exchange(0, key, Ordering::AcqRel, Ordering::Relaxed)
            .map_or_else(|v| v == key, |_| true));
        if hit {
            KICK_CNT[i].fetch_add(1, Ordering::Relaxed);
            KICK_PID[i].store(sched::current_pid() as usize, Ordering::Relaxed);
            return;
        }
    }
}

/// Visit every recorded kick site: (file, line, count, last pid).
pub fn kick_sites(f: &mut dyn FnMut(&'static str, u32, u64, usize)) {
    for i in 0..KICK_SITES {
        let key = KICK_LOC[i].load(Ordering::Acquire);
        if key == 0 { break; }
        let loc = unsafe { &*(key as *const core::panic::Location<'static>) };
        f(loc.file(), loc.line(), KICK_CNT[i].load(Ordering::Relaxed), KICK_PID[i].load(Ordering::Relaxed));
    }
}

/// `kick` for interrupt context: a bounded try-lock; when contended the wake
/// is deferred to the next tick rather than lost.
#[track_caller]
pub fn kick_from_irq() {
    if !KICKED.swap(true, Ordering::AcqRel) {
        kick_site(core::panic::Location::caller());
        if !sched::try_wake_poll_key(NETD_KEY) { sched::request_poll_wake_key(NETD_KEY); }
    }
}

/// Start of a daemon pass: consume the pending kick.
pub fn take_kick() -> bool { KICKED.swap(false, Ordering::AcqRel) }

/// A kick arrived since the pass started (checked after the daemon published
/// its park, so a kick in between cancels the sleep instead of being lost).
pub fn kicked() -> bool { KICKED.load(Ordering::Acquire) }

/// A wired driver's link-monitor thread parks here between passes instead of
/// polling the MAC every 250 ms. The ISR calls `notify` on a link-change (or
/// another monitor-relevant) interrupt, so a link down/up is acted on at once;
/// the monitor's own timeout is only the fallback (fast while traffic moves or
/// when no interrupt is wired, slow when idle).
pub struct LinkMon {
    wq: sched::ksync::KWaitQueue,
    events: core::sync::atomic::AtomicU64,
    /// Passes woken by `notify` / by the timeout (/proc/net/nic `linkmon`).
    pub irq_wakes: core::sync::atomic::AtomicU64,
    pub timer_wakes: core::sync::atomic::AtomicU64,
}

impl LinkMon {
    pub const fn new() -> Self {
        Self {
            wq: sched::ksync::KWaitQueue::new(),
            events: core::sync::atomic::AtomicU64::new(0),
            irq_wakes: core::sync::atomic::AtomicU64::new(0),
            timer_wakes: core::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Any context (ISR included).
    pub fn notify(&self) {
        self.events.fetch_add(1, Ordering::AcqRel);
        self.wq.wake_all();
    }

    /// Sleep up to `timeout_ms` or until an event newer than `*seen` arrives.
    /// Returns true when woken by an event.
    pub fn wait(&self, seen: &mut u64, timeout_ms: u64) -> bool {
        let s = *seen;
        let hit = self.wq.wait_event_timeout(|| self.events.load(Ordering::Acquire) != s, timeout_ms * 1_000_000);
        *seen = self.events.load(Ordering::Acquire);
        if hit { self.irq_wakes.fetch_add(1, Ordering::Relaxed); } else { self.timer_wakes.fetch_add(1, Ordering::Relaxed); }
        hit
    }

    pub fn line(&self) -> alloc::string::String {
        alloc::format!("linkmon: irq_wakes {} timer_wakes {}",
                       self.irq_wakes.load(Ordering::Relaxed), self.timer_wakes.load(Ordering::Relaxed))
    }
}
