//! Per-interface state beside the smoltcp stacks, and route selection.
//!
//! The net server runs one smoltcp `Interface` + `SocketSet` per stack. A
//! stack id ([`Sid`]) is 0 for loopback ([`LO`]) and `netif id + 1` for each
//! netif in `drivers::netdev`'s registry. A socket records the stack it lives
//! on when it is created (connect, first sendto, accept) and stays there.
//!
//! Route selection for a new flow ([`route_select`]): 127/8 goes to `lo`;
//! otherwise the stack whose on-link subnet holds the destination (longest
//! prefix, ties to the lower metric); otherwise the lowest-metric stack with
//! a default route. Only stacks that hold an address and have carrier count.
//!
//! Everything here is a leaf lock: it is read and written with no stack lock
//! held (the daemon updates a stack, drops it, then records the result here),
//! so route decisions never take a stack lock.
//!
//! Address lifetime (the daemon, `net_daemon` in lib.rs): a lease installs
//! address + default route; losing the lease, or the carrier for more than a
//! few seconds, removes both and resets the TCP connections on that stack.
//! A carrier blip shorter than that, or a link that comes back with the same
//! lease, keeps established connections.
//!
//! # API for /dev/netctl (lane L2)
//!
//! [`snapshot`] returns every netif's state ([`IfSnapshot`]); [`set_hook`]
//! registers `fn(netif id)`, called (task context, no lock held) whenever a
//! netif's carrier, address, route or DNS changed. Link-only events also go
//! through `drivers::netdev::set_link_hook`.

use core::sync::atomic::{AtomicU64, Ordering};
use smoltcp::wire::IpAddress;
use spin::Mutex;

use drivers::netdev::{self, MAX_NETIF};

/// Stack id: [`LO`] or `netif id + 1`.
pub(crate) type Sid = u8;
/// The loopback stack.
pub(crate) const LO: Sid = 0;
/// Size of the stack table: `lo`, one stack per possible netif, then the
/// dynamic per-netns stacks of `vif` (sids from [`FIRST_DYN`]).
pub(crate) use crate::vifsel::MAX_STACKS;
/// First dynamic sid.
pub(crate) const FIRST_DYN: Sid = 1 + MAX_NETIF as Sid;

#[inline]
pub(crate) fn sid_of(netif: u8) -> Sid { netif + 1 }

/// The NIC stacks that exist, in registry order.
pub(crate) fn nic_sids() -> impl Iterator<Item = Sid> { (0..netdev::count() as u8).map(sid_of) }

/// Every stack of namespace `ns`: netns 0 = lo + the NICs + its dynamic
/// stacks; another namespace = its own dynamic stacks, plus (until the void
/// gates go) the legacy lo + NICs.
pub(crate) fn ns_sids(ns: u16) -> alloc::vec::Vec<Sid> {
    let mut v: alloc::vec::Vec<Sid> = core::iter::once(LO).chain(nic_sids()).collect();
    v.extend(crate::vif::ns_sids_dyn(ns));
    v
}

/// An interface's IPv4 configuration as its stack currently uses it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) struct IfCfg { pub addr: [u8; 4], pub prefix: u8, pub gateway: Option<[u8; 4]> }

impl IfCfg {
    pub(crate) fn mask(&self) -> u32 {
        if self.prefix == 0 { 0 } else { u32::MAX << (32 - self.prefix.min(32) as u32) }
    }
    pub(crate) fn on_link(&self, dst: [u8; 4]) -> bool {
        let m = self.mask();
        u32::from_be_bytes(self.addr) & m == u32::from_be_bytes(dst) & m
    }
}

/// What routing and /proc need to know about one netif.
#[derive(Clone, Copy)]
pub(crate) struct IfRt {
    pub cfg: Option<IfCfg>,
    /// Carrier as the daemon last saw it. A stack without carrier takes no
    /// new flows; its address is kept for a grace period (see the daemon).
    pub carrier: bool,
    /// Route metric (NetworkManager's numbers: wired 100, Wi-Fi 600).
    pub metric: u32,
    /// DNS servers of the current lease.
    pub dns: [[u8; 4]; 3],
    pub ndns: u8,
}

impl IfRt {
    const fn new() -> Self { IfRt { cfg: None, carrier: true, metric: 100, dns: [[0; 4]; 3], ndns: 0 } }
}

pub(crate) static IFRT: Mutex<[IfRt; MAX_NETIF]> = Mutex::new([const { IfRt::new() }; MAX_NETIF]);

/// The metric a netif gets: wired 100 (+ its index among wired NICs), Wi-Fi 600.
pub(crate) fn default_metric(n: &netdev::Netif) -> u32 {
    let nth = netdev::iter().filter(|o| o.kind == n.kind && o.id < n.id).count() as u32;
    let base = match n.kind { netdev::Kind::Wired => 100 + nth, netdev::Kind::Wifi => 600 + nth };
    // Boot knob: `net.metric.<ifname>=<n>` on the kernel command line.
    let key = alloc::format!("net.metric.{}", n.name());
    match drivers::nvidia_probe::cmdline_opt(&key) {
        Some(v) if v > 0 => v.min(u32::MAX as u64) as u32,
        _ => base,
    }
}

/// Set a netif's route metric (`None` = the default). New flows follow the
/// lowest-metric default route at once; the netctl model is refreshed.
pub fn set_metric(netif: u8, metric: Option<u32>) -> bool {
    if (netif as usize) >= netdev::count().min(MAX_NETIF) { return false; }
    let Some(n) = netdev::get(netif as usize) else { return false };
    let m = metric.unwrap_or_else(|| default_metric(&n));
    IFRT.lock()[netif as usize].metric = m;
    crate::netctl::kick();
    true
}

/// A snapshot of one netif's routing state.
pub(crate) fn rt(netif: u8) -> IfRt { IFRT.lock()[netif as usize] }

fn v4(a: IpAddress) -> Option<[u8; 4]> {
    #[allow(unreachable_patterns)]
    match a { IpAddress::Ipv4(x) => Some(x.0), _ => None }
}

/// True for an address the loopback stack owns (127.0.0.0/8).
#[inline]
pub(crate) fn is_loopback(addr: IpAddress) -> bool {
    matches!(addr, IpAddress::Ipv4(v4) if v4.is_loopback())
}

/// A broadcast address: the limited broadcast 255.255.255.255, loopback's
/// 127.255.255.255, or the directed broadcast of an interface's subnet
/// (Linux RTN_BROADCAST routes; sending there needs SO_BROADCAST).
pub(crate) fn is_broadcast(addr: IpAddress) -> bool {
    let Some(a) = v4(addr) else { return false };
    if a == [255; 4] || a == [127, 255, 255, 255] { return true; }
    let n = netdev::count().min(MAX_NETIF);
    let t = IFRT.lock();
    (0..n).any(|i| t[i].cfg.map_or(false, |c| {
        c.prefix < 31 && (u32::from_be_bytes(c.addr) | !c.mask()) == u32::from_be_bytes(a)
    }))
}

/// The stack a new flow from a socket of network namespace `ns` to `dst`
/// leaves through, or `None` (no route). A namespace other than 0 first
/// consults its own dynamic stacks (`vif`); until the "void" gates go, what it
/// does not hold falls through to the netns-0 table exactly as before.
pub(crate) fn route_select(ns: u16, dst: IpAddress) -> Option<Sid> {
    if ns != 0 {
        if let Some(d) = v4(dst) { if let Some(s) = crate::vif::route_in_ns(ns, d) { return Some(s); } }
    }
    if is_loopback(dst) { return Some(LO); }
    let d = v4(dst)?;
    let n = netdev::count().min(MAX_NETIF);
    let t = IFRT.lock();
    // On-link: longest prefix, then lower metric.
    let mut best: Option<(u8, u32, usize)> = None;
    for i in 0..n {
        let r = &t[i];
        let Some(c) = r.cfg else { continue };
        if !r.carrier || !c.on_link(d) { continue; }
        let better = match best {
            None => true,
            Some((p, m, _)) => c.prefix > p || (c.prefix == p && r.metric < m),
        };
        if better { best = Some((c.prefix, r.metric, i)); }
    }
    if let Some((_, _, i)) = best { return Some(sid_of(i as u8)); }
    default_route_locked(&t, n).map(sid_of)
}

/// The netif of the lowest-metric default route.
fn default_route_locked(t: &[IfRt; MAX_NETIF], n: usize) -> Option<u8> {
    let mut best: Option<(u32, usize)> = None;
    for i in 0..n {
        let r = &t[i];
        let Some(c) = r.cfg else { continue };
        if !r.carrier || c.gateway.is_none() { continue; }
        if best.map_or(true, |(m, _)| r.metric < m) { best = Some((r.metric, i)); }
    }
    best.map(|(_, i)| i as u8)
}

/// The stack an unrouted socket (a UDP bind to INADDR_ANY) is created on:
/// the default route's, else the first NIC stack that has an address, else
/// the first NIC stack. `None` without a NIC.
pub(crate) fn default_sid() -> Option<Sid> {
    let n = netdev::count().min(MAX_NETIF);
    if n == 0 { return None; }
    let t = IFRT.lock();
    if let Some(i) = default_route_locked(&t, n) { return Some(sid_of(i)); }
    if let Some(i) = (0..n).find(|&i| t[i].cfg.is_some() && t[i].carrier) { return Some(sid_of(i as u8)); }
    Some(sid_of(0))
}

/// The NIC stack holding `addr`, if any.
pub(crate) fn holder_of(addr: IpAddress) -> Option<Sid> {
    let a = v4(addr)?;
    let n = netdev::count().min(MAX_NETIF);
    let t = IFRT.lock();
    (0..n).find(|&i| t[i].cfg.map_or(false, |c| c.addr == a)).map(|i| sid_of(i as u8))
}

/// Weak host receive (Linux's default): a unicast IPv4 packet that arrived
/// on `netif` for an address another interface holds is that interface's,
/// so it goes to the holder's stack (sockets live per stack). `None` when
/// the packet is for `netif` itself or for no local address. Replies still
/// leave through the holder (its address, its neighbour cache); ARP stays
/// per interface (Linux arp_ignore=1), so peers only reach an address
/// through another interface when they already map it there.
pub(crate) fn weak_host_holder(netif: u8, dst: [u8; 4]) -> Option<Sid> {
    let n = netdev::count().min(MAX_NETIF);
    if n < 2 || dst == [0; 4] || dst == [255; 4] || dst[0] >= 224 || dst[0] == 127 { return None; }
    let t = IFRT.lock();
    if t.get(netif as usize).and_then(|r| r.cfg).map_or(false, |c| c.addr == dst) { return None; }
    (0..n).find(|&i| i != netif as usize && t[i].cfg.map_or(false, |c| c.addr == dst)).map(|i| sid_of(i as u8))
}

/// True when the stack can carry a new flow to `dst` (has an address and
/// carrier, and `dst` is on-link or it has a default route).
pub(crate) fn can_route(sid: Sid, dst: IpAddress) -> bool {
    if sid == LO { return is_loopback(dst); }
    let Some(d) = v4(dst) else { return false };
    let r = rt(sid - 1);
    match r.cfg {
        Some(c) => r.carrier && (c.on_link(d) || c.gateway.is_some()),
        None => false,
    }
}

/// The DNS servers of every leased interface, the default route's first,
/// deduplicated, at most 3 (what /etc/resolv.conf lists).
pub(crate) fn merged_dns() -> alloc::vec::Vec<[u8; 4]> {
    let n = netdev::count().min(MAX_NETIF);
    let t = IFRT.lock();
    let mut order: alloc::vec::Vec<usize> = (0..n).filter(|&i| t[i].cfg.is_some() && t[i].ndns > 0).collect();
    // Default-route interfaces by metric, then interfaces without a gateway.
    order.sort_by_key(|&i| (!(t[i].carrier && t[i].cfg.map_or(false, |c| c.gateway.is_some())), t[i].metric, i));
    let mut out: alloc::vec::Vec<[u8; 4]> = alloc::vec::Vec::new();
    for i in order {
        for d in &t[i].dns[..t[i].ndns as usize] {
            if out.len() < 3 && !out.contains(d) { out.push(*d); }
        }
    }
    out
}

// ── per-netif counters (/proc/net/dev, /proc/net/nic) ───────────────────────

pub(crate) struct Stats {
    pub rx_bytes: AtomicU64,
    pub rx_pkts: AtomicU64,
    pub rx_drop: AtomicU64,
    pub tx_bytes: AtomicU64,
    pub tx_pkts: AtomicU64,
    pub tx_drop: AtomicU64,
    /// Times the driver said its queue was full and smoltcp held its frames.
    pub tx_held: AtomicU64,
    /// Unicast IPv4 frames that arrived here for another interface's
    /// address and were handed to that interface's stack (weak host).
    pub rx_weak_host: AtomicU64,
}

impl Stats {
    const fn new() -> Self {
        Stats { rx_bytes: AtomicU64::new(0), rx_pkts: AtomicU64::new(0), rx_drop: AtomicU64::new(0),
                tx_bytes: AtomicU64::new(0), tx_pkts: AtomicU64::new(0), tx_drop: AtomicU64::new(0),
                tx_held: AtomicU64::new(0), rx_weak_host: AtomicU64::new(0) }
    }
    pub(crate) fn rx(&self, len: usize) {
        self.rx_pkts.fetch_add(1, Ordering::Relaxed);
        self.rx_bytes.fetch_add(len as u64, Ordering::Relaxed);
    }
    pub(crate) fn tx(&self, len: usize) {
        self.tx_pkts.fetch_add(1, Ordering::Relaxed);
        self.tx_bytes.fetch_add(len as u64, Ordering::Relaxed);
    }
}

pub(crate) static STATS: [Stats; MAX_NETIF] = [const { Stats::new() }; MAX_NETIF];

// ── per-netif DHCP client state (/proc/net/nic) ─────────────────────────────

pub(crate) struct DhcpState {
    /// Lease: address, prefix, router, DNS servers, tick it was taken.
    pub lease: Option<([u8; 4], u8, Option<[u8; 4]>, alloc::vec::Vec<[u8; 4]>, u64)>,
    pub restarts: u32,
    pub leases: u32,
    pub lost: u32,
}

pub(crate) static DHCP: [Mutex<DhcpState>; MAX_NETIF] =
    [const { Mutex::new(DhcpState { lease: None, restarts: 0, leases: 0, lost: 0 }) }; MAX_NETIF];

pub(crate) fn serial(parts: &[&[u8]]) {
    extern "C" { fn arch_serial_putc(b: u8); }
    for p in parts { for &b in *p { unsafe { arch_serial_putc(b); } } }
}

/// The DHCP client was restarted because the link came up.
pub(crate) fn note_dhcp_restart(netif: u8) {
    DHCP[netif as usize].lock().restarts += 1;
    let nif = netdev::get(netif as usize);
    let name = nif.as_ref().map(|n| n.name()).unwrap_or("?");
    serial(&[b"[NET] ", name.as_bytes(), b" link up: DHCP discover restarted\r\n"]);
}

pub(crate) fn note_dhcp_lease(netif: u8, addr: [u8; 4], prefix: u8, router: Option<[u8; 4]>, dns: &[[u8; 4]]) {
    let mut d = DHCP[netif as usize].lock();
    d.leases += 1;
    d.lease = Some((addr, prefix, router, dns.to_vec(), sched::ticks()));
}

/// smoltcp also reports Deconfigured when it (re)starts without a lease;
/// only a lease that existed counts as lost. True when one did.
pub(crate) fn note_dhcp_lost(netif: u8) -> bool {
    let mut d = DHCP[netif as usize].lock();
    if d.lease.take().is_some() { d.lost += 1; true } else { false }
}

// ── public snapshot + change hook (for /dev/netctl) ─────────────────────────

/// One netif as /dev/netctl reports it.
#[derive(Clone)]
pub struct IfSnapshot {
    /// Registry id (`drivers::netdev`); `ifindex = id + 2`.
    pub id: u8,
    pub name: alloc::string::String,
    pub kind: netdev::Kind,
    pub driver: &'static str,
    pub mac: Option<[u8; 6]>,
    pub carrier: bool,
    /// Mbit/s, 0 when unknown.
    pub speed: u32,
    /// Address and prefix length, when configured.
    pub addr: Option<([u8; 4], u8)>,
    pub gateway: Option<[u8; 4]>,
    pub dns: alloc::vec::Vec<[u8; 4]>,
    pub metric: u32,
    /// New flows to off-link destinations leave through this netif.
    pub default_route: bool,
    /// The DHCP client holds a lease (vs. the QEMU pre-lease address).
    pub leased: bool,
    /// Administratively up (`set_admin`); false = DHCP stopped, no address.
    pub admin_up: bool,
}

/// Per-netif administrative state: true = down (`/dev/netctl` DISCONNECT on
/// a wired interface). The daemon applies it on its next pass.
static ADMIN_DOWN: [core::sync::atomic::AtomicBool; MAX_NETIF] =
    [const { core::sync::atomic::AtomicBool::new(false) }; MAX_NETIF];

pub(crate) fn admin_down(netif: u8) -> bool {
    ADMIN_DOWN.get(netif as usize).map_or(false, |a| a.load(Ordering::Acquire))
}

/// Set a netif administratively up or down and wake the net daemon, which
/// stops DHCP and removes the address (down) or restarts DHCP (up), then
/// reports the change through the hook. Returns false for an unknown netif.
pub fn set_admin(netif: u8, up: bool) -> bool {
    let Some(a) = ADMIN_DOWN.get(netif as usize) else { return false };
    if (netif as usize) >= netdev::count() { return false; }
    a.store(!up, Ordering::Release);
    netdev::kick();
    crate::netctl::kick();
    true
}

/// Every registered netif's state, in registry order.
pub fn snapshot() -> alloc::vec::Vec<IfSnapshot> {
    let n = netdev::count().min(MAX_NETIF);
    let def = {
        let t = IFRT.lock();
        default_route_locked(&t, n)
    };
    netdev::iter().take(n).map(|nif| {
        let r = rt(nif.id);
        let (up, speed, _) = nif.link();
        IfSnapshot {
            id: nif.id,
            name: alloc::string::String::from(nif.name()),
            kind: nif.kind,
            driver: nif.driver(),
            mac: nif.mac(),
            carrier: up,
            speed,
            addr: r.cfg.map(|c| (c.addr, c.prefix)),
            gateway: r.cfg.and_then(|c| c.gateway),
            dns: r.dns[..r.ndns as usize].to_vec(),
            metric: r.metric,
            default_route: def == Some(nif.id),
            leased: DHCP[nif.id as usize].lock().lease.is_some(),
            admin_up: !admin_down(nif.id),
        }
    }).collect()
}

static HOOK: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Register the change callback (one; a later call replaces it).
pub fn set_hook(f: fn(u8)) { HOOK.store(f as usize, Ordering::Release); }

/// A netif's carrier, address, route or DNS changed. No lock held.
pub(crate) fn notify(netif: u8) {
    let f = HOOK.load(Ordering::Acquire);
    if f != 0 {
        let f: fn(u8) = unsafe { core::mem::transmute(f) };
        f(netif);
    }
}

/// True when any NIC (not loopback) holds an IPv4 address. Cheap: one leaf lock.
pub fn any_ipv4_address() -> bool {
    let n = netdev::count().min(MAX_NETIF);
    let t = IFRT.lock();
    t.iter().take(n).any(|r| r.cfg.is_some())
}
