//! `/dev/netctl`: the network control node (char 10:242, `root:netdev 0664`).
//!
//! The protocol is frozen in docs/servers/netctl.md and its bytes in the
//! `netctl-abi` crate; this is the kernel side of it.
//!
//! * **Permissions.** The VFS checks the node's mode at open(2): anyone may
//!   open it read-only, only root and `netdev` (gid 108) members for writing
//!   (`dyn_dev_meta`, EACCES otherwise). `VFS_OPEN` tells us the access mode
//!   and we record it per open; a mutating ioctl (nr >= 0x20) on a read-only
//!   open answers EPERM here, whatever the VFS did. No re-check per call: a
//!   descriptor passed or inherited keeps the capability of its open.
//! * **Model.** Everything a client can learn is cached here (`Model`):
//!   interfaces, IPv4, Wi-Fi state, the BSS cache, the last scan, profile
//!   metadata. Snapshots are generated from the model under the same lock
//!   that emits deltas, so a snapshot and the deltas after it never overlap
//!   or leave a gap. The model is fed by
//!   - the Wi-Fi backends through `drivers::wifi_api`'s event sink
//!     ([`wifi_sink`]), in the order they made the changes;
//!   - the `[netctl]` kthread, which every [`POLL_MS`] diffs the wired
//!     interfaces and IPv4 state ([`links`], the seam to the IP stacks) and
//!     discovers newly registered Wi-Fi backends;
//!   - profile (re)loads ([`reload_profiles`]): at boot once the root
//!     filesystem is up, and on `NETCTL_RELOAD_PROFILES`.
//! * **Event queues.** One bounded queue of 64-byte records per open
//!   (`EVENT_QUEUE_RECORDS`). Every open starts with a full snapshot. An
//!   event that does not fit drops the whole queue; the next read/poll then
//!   finds `RESYNC` and a fresh snapshot (lossless overflow).
//!
//! Lock order: `ST` is a leaf. It is never held while calling a Wi-Fi
//! backend, touching user memory, reading a file or waking pollers (wakes
//! use the try-variant, so the sink is safe under a backend's own lock).

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::mem::size_of;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use drivers::wifi_api::{self, ConnectParams, Secret, WifiBackend, WifiCmd, WifiEvent};
use ipc::{port, Message};
use netctl_abi::*;
use spin::Mutex;

mod links;
mod profiles;

/// How often the kthread diffs interface/IPv4 state and looks for new Wi-Fi
/// backends.
const POLL_MS: u64 = 250;
/// Most simultaneous opens (each holds up to 64 KiB of queue).
const MAX_OPENS: usize = 64;
/// Records copied out per read(2) call at most (16 KiB).
const READ_MAX_RECORDS: usize = 256;

const VFS_OPEN: u64 = 0x10;
const VFS_READ: u64 = 0x11;
const VFS_WRITE: u64 = 0x12;
const VFS_CLOSE: u64 = 0x13;
const VFS_IOCTL: u64 = 0x28;
const VFS_POLL: u64 = 0x2F;

const FIONREAD: u32 = 0x541B;
const EAGAIN: i32 = 11;
const ENFILE: i32 = 23;

// ── Model ───────────────────────────────────────────────────────────────────

struct WifiPart {
    state: NcWifiState,
    last_scan: Option<NcScanInfo>,
    /// The kthread has queried the backend's static facts (driver, caps).
    discovered: bool,
    /// The boot-time `WIFI_RADIO_OFF` was applied (or not needed).
    radio_applied: bool,
}

struct Link {
    iface: NcIface,
    ipv4: NcIpv4,
    wifi: Option<WifiPart>,
}

struct Model {
    /// Ascending ifindex.
    links: Vec<Link>,
    bss: Vec<NcBss>,
    /// Profile metadata as loaded (without BLOCKED_WRONG_KEY, added on output).
    profiles: Vec<NcProfileMeta>,
    pinfo: NcProfilesInfo,
    /// Profiles whose last attempt failed WRONG_KEY (cleared by reload and by
    /// an explicit CONNECT of that uuid).
    blocked: Vec<[u8; 16]>,
    /// A load has happened (boot or reload).
    loaded: bool,
}

struct Open {
    id: u32,
    writable: bool,
    q: VecDeque<NcEventRecord>,
    /// Logical events queued or dropped (the records' `seq`).
    seq: u32,
    /// Logical events actually queued (VFS_POLL's edge counter).
    pushed: u64,
    /// Overflowed: `Some(dropped)` until the reader's next read/poll turns it
    /// into RESYNC + snapshot.
    resync: Option<u32>,
}

struct State {
    model: Model,
    opens: Vec<Open>,
}

static ST: Mutex<State> = Mutex::new(State {
    model: Model {
        links: Vec::new(),
        bss: Vec::new(),
        profiles: Vec::new(),
        pinfo: NcProfilesInfo {
            gen: 0, count: 0, source: 0, status: profiles_status::NO_FILE, n_legacy: 0, _pad: 0,
            global_flags: 0, seq: 0, _rsv: [0; 24],
        },
        blocked: Vec::new(),
        loaded: false,
    },
    opens: Vec::new(),
});

/// Serialises profile loads (file I/O happens under it, so it sleeps).
static RELOAD_LOCK: sched::ksync::KMutex<()> = sched::ksync::KMutex::new(());
static PROFILES_GEN: AtomicU32 = AtomicU32::new(0);
static ATTEMPT: AtomicU32 = AtomicU32::new(0);
static CMDLINE: Mutex<&'static str> = Mutex::new("");
static UP: AtomicBool = AtomicBool::new(false);
static PORT: AtomicU32 = AtomicU32::new(u32::MAX);

/// The IPC port behind /dev/netctl (the kernel routes the node's ioctls to
/// it), `None` before `init`.
pub fn device_port() -> Option<u32> {
    match PORT.load(Ordering::Acquire) { u32::MAX => None, p => Some(p) }
}

fn now_ms() -> u32 { (sched::monotonic_ns() / 1_000_000) as u32 }

fn tag_of(open_id: u32) -> u64 { sched::poll_tag(sched::poll_class::NETCTL, open_id) }

/// Wake readers/pollers of the opens in `mask` (an OR of their tags). Never
/// blocks: under contention the wake is deferred to the next tick.
fn wake(mask: u64) {
    if mask != 0 && !sched::try_wake_poll_tagged(mask) {
        sched::request_poll_wake_tagged(mask);
    }
}

impl Open {
    fn new(id: u32, writable: bool) -> Self {
        Open { id, writable, q: VecDeque::new(), seq: 0, pushed: 0, resync: None }
    }

    /// Queue one logical event. True when the reader should be woken.
    fn push(&mut self, ev: &Event, flags: u8) -> bool {
        self.seq = self.seq.wrapping_add(1);
        if let Some(d) = self.resync.as_mut() {
            *d = d.saturating_add(1);
            return false;
        }
        let enc = ev.encode(flags, self.seq, now_ms());
        if self.q.len() + enc.len() > EVENT_QUEUE_RECORDS {
            let queued = self.q.iter().filter(|r| r.part == 0).count() as u32;
            self.q.clear();
            self.resync = Some(queued.saturating_add(1));
            return true;
        }
        self.q.extend(enc.records().iter().copied());
        self.pushed += 1;
        true
    }

    /// Turn a pending overflow into RESYNC + a fresh snapshot.
    fn materialize(&mut self, m: &Model) {
        if let Some(d) = self.resync.take() {
            let mut r = NcResync::default();
            r.dropped = d;
            self.push(&Event::Resync(r), 0);
            snapshot(m, self, snapshot_reason::RESYNC);
        }
    }

    fn ready(&self) -> bool { !self.q.is_empty() || self.resync.is_some() }
}

impl Model {
    fn link(&self, ifindex: u32) -> Option<&Link> { self.links.iter().find(|l| l.iface.ifindex == ifindex) }
    fn link_mut(&mut self, ifindex: u32) -> Option<&mut Link> { self.links.iter_mut().find(|l| l.iface.ifindex == ifindex) }

    fn insert_link(&mut self, l: Link) -> usize {
        let pos = self.links.iter().position(|x| x.iface.ifindex > l.iface.ifindex).unwrap_or(self.links.len());
        self.links.insert(pos, l);
        pos
    }

    fn profile_meta(&self, i: usize) -> NcProfileMeta {
        let mut m = self.profiles[i];
        if self.blocked.contains(&m.uuid) { m.flags |= profile_flags::BLOCKED_WRONG_KEY; }
        m
    }

    fn scan_gen(&self, ifindex: u32) -> u32 {
        self.links.iter()
            .filter(|l| ifindex == 0 || l.iface.ifindex == ifindex)
            .filter_map(|l| l.wifi.as_ref().and_then(|w| w.last_scan).map(|s| s.scan_gen))
            .max().unwrap_or(0)
    }
}

/// Queue a full snapshot (docs/servers/netctl.md, "Snapshot on open").
fn snapshot(m: &Model, o: &mut Open, reason: u32) {
    const F: u8 = event_flags::SNAPSHOT;
    let mut info = NcSnapshotInfo::default();
    info.reason = reason;
    info.abi_version = NETCTL_ABI_VERSION;
    info.n_ifaces = m.links.len() as u32;
    info.n_bss = m.bss.len() as u32;
    info.n_profiles = m.profiles.len() as u32;
    info.profiles_gen = m.pinfo.gen;
    info.writable = o.writable as u32;
    o.push(&Event::SnapshotBegin(info), F);
    for l in &m.links { o.push(&Event::IfaceAdded(l.iface), F); }
    for l in &m.links { o.push(&Event::Ipv4(l.ipv4), F); }
    for l in &m.links {
        if let Some(w) = &l.wifi { o.push(&Event::WifiState(w.state), F); }
    }
    for b in &m.bss { o.push(&Event::Bss(*b), F); }
    for l in &m.links {
        if let Some(s) = l.wifi.as_ref().and_then(|w| w.last_scan) { o.push(&Event::ScanDone(s), F); }
    }
    for i in 0..m.profiles.len() { o.push(&Event::Profile(m.profile_meta(i)), F); }
    o.push(&Event::ProfilesEnd(m.pinfo), F);
    o.push(&Event::SnapshotEnd(info), F);
}

/// Queue a delta to every open; returns the wake mask.
fn emit(opens: &mut [Open], ev: &Event) -> u64 {
    let mut mask = 0;
    for o in opens.iter_mut() {
        if o.push(ev, 0) { mask |= tag_of(o.id); }
    }
    mask
}

/// Queue the profile batch (PROFILE… PROFILES_END) to every open.
fn emit_profiles(st: &mut State) -> u64 {
    let mut mask = 0;
    for i in 0..st.model.profiles.len() {
        let m = st.model.profile_meta(i);
        mask |= emit(&mut st.opens, &Event::Profile(m));
    }
    mask | emit(&mut st.opens, &Event::ProfilesEnd(st.model.pinfo))
}

// ── Wi-Fi interfaces ────────────────────────────────────────────────────────

fn name16(s: &[u8]) -> [u8; IFNAMSIZ] {
    let mut n = [0u8; IFNAMSIZ];
    let k = s.len().min(IFNAMSIZ - 1);
    n[..k].copy_from_slice(&s[..k]);
    n
}

/// `wifiN`, N = this interface's rank among the Wi-Fi interfaces.
fn wifi_name(m: &Model, ifindex: u32) -> [u8; IFNAMSIZ] {
    let n = m.links.iter().filter(|l| l.wifi.is_some() && l.iface.ifindex < ifindex).count();
    let mut b = *b"wifi\0\0\0\0\0\0\0\0\0\0\0\0";
    if n < 10 { b[4] = b'0' + n as u8; } else { b[4] = b'0' + (n / 10) as u8; b[5] = b'0' + (n % 10) as u8; }
    b
}

/// Derive the interface fields that follow the Wi-Fi link state.
fn wifi_iface_follow(i: &mut NcIface, s: &NcWifiState) {
    use wifi_state as ws;
    i.oper = match s.state {
        ws::CONNECTED => oper::UP,
        ws::CONNECTING | ws::AUTHENTICATING | ws::ASSOCIATING | ws::HANDSHAKE => oper::DORMANT,
        ws::DISCONNECTED | ws::FAILED | ws::DISCONNECTING => oper::LOWER_DOWN,
        _ => oper::DOWN,
    };
    let mut f = i.flags & !(iface_flags::CARRIER | iface_flags::RADIO_ON | iface_flags::RADIO_HW_ON | iface_flags::READY);
    f |= iface_flags::ADMIN_UP;
    if s.state == ws::CONNECTED { f |= iface_flags::CARRIER; }
    if s.state != ws::UNAVAILABLE { f |= iface_flags::READY; }
    let radio_off = s.state == ws::RADIO_OFF;
    if !(radio_off && s.reason != reason::RFKILL) { f |= iface_flags::RADIO_ON; }
    if !(radio_off && s.reason == reason::RFKILL) { f |= iface_flags::RADIO_HW_ON; }
    i.flags = f;
}

/// A Wi-Fi link the model has not seen yet (minimal: the kthread fills in
/// driver, caps and MAC from the backend later).
fn new_wifi_link(m: &Model, ifindex: u32, state: NcWifiState) -> Link {
    let mut i = NcIface::default();
    i.ifindex = ifindex;
    i.kind = if_kind::WIFI;
    i.name = match ifindex.checked_sub(2).and_then(|id| drivers::netdev::get(id as usize)) {
        Some(n) => name16(n.name().as_bytes()),
        None => wifi_name(m, ifindex),
    };
    i.mtu = 1500;
    i.metric = METRIC_WIFI;
    let mut s = state;
    s.ifindex = ifindex;
    wifi_iface_follow(&mut i, &s);
    let mut v = NcIpv4::default();
    v.ifindex = ifindex;
    v.method = ip_method::AUTO;
    v.metric = METRIC_WIFI;
    Link { iface: i, ipv4: v, wifi: Some(WifiPart { state: s, last_scan: None, discovered: false, radio_applied: false }) }
}

/// Make sure `ifindex` exists as a Wi-Fi link; emits IFACE_ADDED + IPV4 for
/// a new one. Returns the wake mask.
fn ensure_wifi_link(st: &mut State, ifindex: u32, state: NcWifiState) -> u64 {
    if let Some(l) = st.model.link_mut(ifindex) {
        // Known from the netif registry, first word from its backend.
        if l.wifi.is_none() {
            let mut s = state;
            s.ifindex = ifindex;
            l.wifi = Some(WifiPart { state: s, last_scan: None, discovered: false, radio_applied: false });
        }
        return 0;
    }
    let l = new_wifi_link(&st.model, ifindex, state);
    let (iface, ipv4) = (l.iface, l.ipv4);
    st.model.insert_link(l);
    emit(&mut st.opens, &Event::IfaceAdded(iface)) | emit(&mut st.opens, &Event::Ipv4(ipv4))
}

/// The `drivers::wifi_api` event sink. May run under a backend's own lock:
/// it never calls a backend and never blocks on the run queue.
fn wifi_sink(ifindex: u32, ev: &WifiEvent) {
    let mut st = ST.lock();
    let mut mask = 0;
    match *ev {
        WifiEvent::State(mut s) => {
            s.ifindex = ifindex;
            mask |= ensure_wifi_link(&mut st, ifindex, s);
            mask |= emit(&mut st.opens, &Event::WifiState(s));
            let st_ = &mut *st;
            let mut changed = None;
            if let Some(l) = st_.model.link_mut(ifindex) {
                if let Some(w) = l.wifi.as_mut() { w.state = s; }
                let before = l.iface;
                wifi_iface_follow(&mut l.iface, &s);
                if l.iface != before { changed = Some(l.iface); }
            }
            if let Some(i) = changed { mask |= emit(&mut st_.opens, &Event::IfaceChanged(i)); }
            if s.state == wifi_state::FAILED && s.reason == reason::WRONG_KEY
                && !util::uuid_is_nil(&s.profile_uuid)
                && st_.model.profiles.iter().any(|p| p.uuid == s.profile_uuid)
                && !st_.model.blocked.contains(&s.profile_uuid)
            {
                st_.model.blocked.push(s.profile_uuid);
                mask |= emit_profiles(st_);
            }
        }
        WifiEvent::Signal(mut g) => {
            g.ifindex = ifindex;
            if let Some(w) = st.model.link_mut(ifindex).and_then(|l| l.wifi.as_mut()) {
                w.state.signal_dbm = g.signal_dbm;
                w.state.tx_rate_kbps = g.tx_rate_kbps;
            }
            mask |= emit(&mut st.opens, &Event::WifiSignal(g));
        }
        WifiEvent::ScanStarted(mut i) => {
            i.ifindex = ifindex;
            mask |= emit(&mut st.opens, &Event::ScanStarted(i));
        }
        WifiEvent::ScanDone(mut i) => {
            i.ifindex = ifindex;
            if let Some(w) = st.model.link_mut(ifindex).and_then(|l| l.wifi.as_mut()) { w.last_scan = Some(i); }
            mask |= emit(&mut st.opens, &Event::ScanDone(i));
        }
        WifiEvent::Bss(mut b) => {
            b.ifindex = ifindex;
            let bss = &mut st.model.bss;
            match bss.iter().position(|x| x.ifindex == ifindex && x.bssid == b.bssid) {
                Some(i) => bss[i] = b,
                None if bss.len() < MAX_BSS => bss.push(b),
                // Cache full: the client still hears of it, the snapshot not.
                None => {}
            }
            mask |= emit(&mut st.opens, &Event::Bss(b));
        }
        WifiEvent::BssGone(mut b) => {
            b.ifindex = ifindex;
            st.model.bss.retain(|x| !(x.ifindex == ifindex && x.bssid == b.bssid));
            mask |= emit(&mut st.opens, &Event::BssGone(b));
        }
    }
    drop(st);
    wake(mask);
}

/// Every registered Wi-Fi backend, ifindex ascending.
fn backends() -> Vec<(u32, &'static dyn WifiBackend)> {
    let mut v = Vec::new();
    wifi_api::for_each(|ix, b| v.push((ix, b)));
    v
}

/// The kthread's half of Wi-Fi: register newly seen backends with the model
/// (state, BSS cache, last scan) and keep driver/caps/MAC current.
fn discover_wifi() {
    for (ix, be) in backends() {
        // Query outside the lock (a backend may emit while holding its own).
        let mac = be.mac();
        let caps = be.caps();
        let driver = name16(be.driver().as_bytes());
        let known = ST.lock().model.link(ix).and_then(|l| l.wifi.as_ref()).is_some();
        let (state, bss, last) = if known {
            (None, Vec::new(), None)
        } else {
            let mut b = Vec::new();
            be.bss_snapshot(&mut b);
            (Some(be.state()), b, be.last_scan())
        };

        let mut st = ST.lock();
        let mut mask = 0;
        let mut apply_radio_off = false;
        if st.model.link(ix).and_then(|l| l.wifi.as_ref()).is_none() {
            let s = state.unwrap_or_default();
            mask |= ensure_wifi_link(&mut st, ix, s);
            if let Some(mut s) = state {
                s.ifindex = ix;
                mask |= emit(&mut st.opens, &Event::WifiState(s));
            }
            for mut b in bss {
                b.ifindex = ix;
                if st.model.bss.len() < MAX_BSS && !st.model.bss.iter().any(|x| x.ifindex == ix && x.bssid == b.bssid) {
                    st.model.bss.push(b);
                    mask |= emit(&mut st.opens, &Event::Bss(b));
                }
            }
            if let Some(mut s) = last {
                s.ifindex = ix;
                if let Some(w) = st.model.link_mut(ix).and_then(|l| l.wifi.as_mut()) { w.last_scan = Some(s); }
            }
        }
        let radio_off_at_boot = st.model.loaded && st.model.pinfo.global_flags & profiles::GLOBAL_RADIO_OFF != 0;
        let loaded = st.model.loaded;
        let st_ = &mut *st;
        let mut changed = None;
        if let Some(l) = st_.model.link_mut(ix) {
            let before = l.iface;
            if let Some(m) = mac { l.iface.mac = m; if l.iface.perm_mac == [0; 6] { l.iface.perm_mac = m; } }
            l.iface.wifi_caps = caps;
            l.iface.driver = driver;
            if let Some(w) = l.wifi.as_mut() {
                w.discovered = true;
                if loaded && !w.radio_applied {
                    w.radio_applied = true;
                    apply_radio_off = radio_off_at_boot;
                }
            }
            if l.iface != before { changed = Some(l.iface); }
        }
        if let Some(i) = changed { mask |= emit(&mut st_.opens, &Event::IfaceChanged(i)); }
        drop(st);
        wake(mask);
        if apply_radio_off { let _ = be.submit(WifiCmd::SetRadio(false)); }
    }
}

// ── Wired interfaces and IPv4 ───────────────────────────────────────────────

/// Diff the adapter's view of every interface (and its IPv4 state) against
/// the model and emit the changes. Run by the kthread and by the net
/// server's change hook. A Wi-Fi interface's oper state and flags follow its
/// backend's link state once one is registered (`wifi_iface_follow`).
fn refresh_links() {
    let mut views = Vec::new();
    links::collect(&mut views);
    let mut st = ST.lock();
    let st_ = &mut *st;
    let mut mask = 0;

    // Interfaces that went away (the registry never unregisters today; a
    // backend-only Wi-Fi link stays).
    let mut i = 0;
    while i < st_.model.links.len() {
        let l = &st_.model.links[i];
        if l.wifi.is_none() && !views.iter().any(|v| v.iface.ifindex == l.iface.ifindex) {
            let gone = st_.model.links.remove(i);
            mask |= emit(&mut st_.opens, &Event::IfaceRemoved(gone.iface));
        } else {
            i += 1;
        }
    }
    for v in &views {
        let ix = v.iface.ifindex;
        match st_.model.link_mut(ix) {
            None => {
                st_.model.insert_link(Link { iface: v.iface, ipv4: v.ipv4, wifi: None });
                mask |= emit(&mut st_.opens, &Event::IfaceAdded(v.iface));
                mask |= emit(&mut st_.opens, &Event::Ipv4(v.ipv4));
                continue;
            }
            Some(l) => {
                let mut want = v.iface;
                if let Some(w) = &l.wifi {
                    // The backend knows more than the registry here.
                    want.wifi_caps = l.iface.wifi_caps;
                    if want.mac == [0; 6] { want.mac = l.iface.mac; want.perm_mac = l.iface.perm_mac; }
                    if l.iface.driver != [0; 16] && want.driver == [0; 16] { want.driver = l.iface.driver; }
                    let metric = want.metric;
                    wifi_iface_follow(&mut want, &w.state);
                    want.metric = metric;
                }
                if l.iface != want {
                    l.iface = want;
                    mask |= emit(&mut st_.opens, &Event::IfaceChanged(want));
                }
                if l.ipv4 != v.ipv4 {
                    l.ipv4 = v.ipv4;
                    mask |= emit(&mut st_.opens, &Event::Ipv4(v.ipv4));
                }
            }
        }
    }
    drop(st);
    wake(mask);
}

/// `crate::set_iface_hook`: a netif's carrier, address, route or DNS
/// changed (net daemon, no lock held).
fn iface_hook(_netif: u8) { refresh_links(); }

// ── Profiles ────────────────────────────────────────────────────────────────

/// Load the profile slots and the legacy credentials, publish the list to
/// the Wi-Fi backends and every open. Synchronous; sleeps on file I/O.
fn reload_profiles() -> NcReload {
    let _g = RELOAD_LOCK.lock();
    let cmdline = *CMDLINE.lock();
    let mut out = NcReload::new();
    match profiles::load(cmdline) {
        Ok(l) => {
            let gen = PROFILES_GEN.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
            let n = l.list.len();
            let metas: Vec<NcProfileMeta> = l.list.iter().enumerate().map(|(i, p)| {
                p.meta(gen, if i >= n - l.n_legacy { profile_flags::LEGACY } else { 0 })
            }).collect();
            let mut info = NcProfilesInfo::default();
            info.gen = gen;
            info.count = n as u32;
            info.source = l.source;
            info.status = l.status;
            info.n_legacy = l.n_legacy as u8;
            info.global_flags = l.global_flags;
            info.seq = l.seq;
            // Backends first: when the ioctl returns, CONNECT by uuid sees it.
            wifi_api::set_profiles(gen, l.list);
            let mask = {
                let mut st = ST.lock();
                st.model.profiles = metas;
                st.model.blocked.clear();
                st.model.pinfo = info;
                st.model.loaded = true;
                emit_profiles(&mut st)
            };
            wake(mask);
            for (_, be) in backends() { let _ = be.submit(WifiCmd::ProfilesChanged); }
            out.gen = gen;
            out.count = n as u32;
            out.source = l.source;
            out.status = l.status;
        }
        Err(()) => {
            let st = ST.lock();
            out.gen = st.model.pinfo.gen;
            out.count = st.model.pinfo.count;
            out.source = st.model.pinfo.source;
            out.status = profiles_status::IO_ERROR;
        }
    }
    out
}

// ── The kthread ─────────────────────────────────────────────────────────────

fn worker() {
    // Boot load once the root filesystem is mounted (where wifi.conf was
    // read before netctl). Without a root after 60 s, load anyway: the
    // cmdline's wifi.ssid= still counts.
    let t0 = sched::monotonic_ns();
    let mut loaded = false;
    loop {
        if !loaded && (drivers::firmware::root_ready() || sched::monotonic_ns() - t0 > 60_000_000_000) {
            let r = reload_profiles();
            loaded = true;
            log_load(&r);
        }
        refresh_links();
        discover_wifi();
        sched::kthread::msleep(POLL_MS);
    }
}

fn log_load(r: &NcReload) {
    extern "C" { fn arch_serial_putc(b: u8); }
    let mut s = alloc::string::String::new();
    use core::fmt::Write;
    let _ = write!(s, "[netctl] profiles gen {}: {} loaded (source {}, status {})\r\n", r.gen, r.count, r.source, r.status);
    for &b in s.as_bytes() { unsafe { arch_serial_putc(b); } }
}

// ── Entry ───────────────────────────────────────────────────────────────────

/// Register /dev/netctl and start the `[netctl]` kthread. After the net
/// server's `init`; `cmdline` is the kernel command line (legacy
/// `wifi.ssid=`/`wifi.psk=`).
pub fn init(cmdline: &'static str) {
    if UP.swap(true, Ordering::AcqRel) { return; }
    *CMDLINE.lock() = cmdline;
    let p = match port::create(0) { Some(p) => p, None => return };
    if !port::register_handler(p, handle_msg) { return; }
    // Seed the model so the very first open already lists lo and eth0.
    refresh_links();
    crate::set_iface_hook(iface_hook);
    wifi_api::set_event_sink(wifi_sink);
    PORT.store(p, Ordering::Release);
    vfs::register_device_notify_open(NETCTL_PATH, p, 0, NETCTL_MAJOR, NETCTL_MINOR);
    let _ = sched::kthread::spawn("[netctl]", worker);
}

// ── VFS protocol ────────────────────────────────────────────────────────────

fn arg(msg: &Message, n: usize) -> u64 {
    u64::from_le_bytes(msg.data[n * 8..n * 8 + 8].try_into().unwrap())
}

fn reply(v: i64) -> Message {
    let mut m = Message::empty();
    m.tag = 0x8000_0000_0000_0000;
    m.data[0..8].copy_from_slice(&v.to_le_bytes());
    m
}

fn err(e: i32) -> Message { reply(-(e as i64)) }

fn copy_in(ptr: usize, buf: &mut [u8]) -> bool {
    ptr != 0 && sched::with_current_address_space(|a| a.read_user_buf(ptr, buf)).unwrap_or(false)
}

fn copy_out(ptr: usize, buf: &[u8]) -> bool {
    ptr != 0 && sched::with_current_address_space_mut(|a| a.write_user_buf(ptr, buf)).unwrap_or(false)
}

/// Read a command struct: its `size` first, then min(size, ours) bytes,
/// zero-filling the rest. Returns the value and the caller's length.
fn read_cmd<T: Pod>(ptr: usize, abi1: usize) -> Result<(T, usize), i32> {
    let mut sz = [0u8; 4];
    if !copy_in(ptr, &mut sz) { return Err(errno::EFAULT); }
    let size = u32::from_le_bytes(sz) as usize;
    if size < abi1 { return Err(errno::EINVAL); }
    let n = size.min(size_of::<T>());
    let mut v = T::zeroed();
    if !copy_in(ptr, &mut v.as_bytes_mut()[..n]) { return Err(errno::EFAULT); }
    Ok((v, size))
}

fn handle_msg(msg: &Message, _caller: u32, _port: u32) -> Message {
    match msg.tag {
        VFS_OPEN => {
            let flags = arg(msg, 1) as u32;
            let open_id = arg(msg, 4) as u32;
            let mut st = ST.lock();
            if st.opens.len() >= MAX_OPENS { return err(ENFILE); }
            let st_ = &mut *st;
            st_.opens.retain(|o| o.id != open_id);
            let mut o = Open::new(open_id, flags & 3 != 0);
            snapshot(&st_.model, &mut o, snapshot_reason::OPEN);
            st_.opens.push(o);
            reply(0)
        }
        VFS_CLOSE => {
            let open_id = arg(msg, 1) as u32;
            ST.lock().opens.retain(|o| o.id != open_id);
            reply(0)
        }
        VFS_READ => do_read(arg(msg, 1) as usize, arg(msg, 2) as usize, arg(msg, 4) as u32),
        VFS_WRITE => err(errno::EINVAL),
        VFS_POLL => {
            let open_id = arg(msg, 4) as u32;
            let mut st = ST.lock();
            let st_ = &mut *st;
            let (rev, seq) = match st_.opens.iter_mut().find(|o| o.id == open_id) {
                Some(o) => {
                    o.materialize(&st_.model);
                    (if o.ready() { 1u64 } else { 0 }, o.pushed)
                }
                None => (0x8 | 0x10, 0), // POLLERR | POLLHUP: unknown open
            };
            drop(st);
            let mut m = Message::empty();
            m.data[0..8].copy_from_slice(&rev.to_le_bytes());
            m.data[8..16].copy_from_slice(&seq.to_le_bytes());
            m.data[24..32].copy_from_slice(&tag_of(open_id).to_le_bytes());
            m.data[32] = 1;
            m
        }
        VFS_IOCTL => {
            let cmd = arg(msg, 1) as u32;
            let ptr = arg(msg, 2) as usize;
            let open_id = arg(msg, 4) as u32;
            match do_ioctl(cmd, ptr, open_id) {
                Ok(v) => reply(v as i64),
                Err(e) => err(e),
            }
        }
        _ => err(errno::EINVAL),
    }
}

fn do_read(buf: usize, count: usize, open_id: u32) -> Message {
    if count < EVENT_RECORD_SIZE { return err(errno::EINVAL); }
    let max = (count / EVENT_RECORD_SIZE).min(READ_MAX_RECORDS);
    let mut out: Vec<u8> = Vec::new();
    {
        let mut st = ST.lock();
        let st_ = &mut *st;
        let o = match st_.opens.iter_mut().find(|o| o.id == open_id) {
            Some(o) => o,
            None => return err(errno::EINVAL),
        };
        o.materialize(&st_.model);
        let mut n = 0;
        // Whole events while they fit; if not even the first one fits, as
        // many of its parts as do.
        while n < max {
            let Some(front) = o.q.front() else { break };
            let parts = (front.nparts.max(1) as usize).saturating_sub(front.part as usize).max(1);
            let take = if n + parts <= max { parts } else if n == 0 { max } else { break };
            for _ in 0..take {
                if let Some(r) = o.q.pop_front() { out.extend_from_slice(r.as_bytes()); n += 1; }
            }
        }
    }
    if out.is_empty() { return err(EAGAIN); }
    if !copy_out(buf, &out) { return err(errno::EFAULT); }
    reply(out.len() as i64)
}

/// The ifindex a command addresses: 0 means the first Wi-Fi interface.
enum Target {
    Wifi(u32, &'static dyn WifiBackend),
    Wired(u32),
}

fn target(ifindex: u32) -> Result<Target, i32> {
    if ifindex == 0 {
        return wifi_api::first().map(|(ix, b)| Target::Wifi(ix, b)).ok_or(errno::ENODEV);
    }
    if let Some(b) = wifi_api::backend(ifindex) { return Ok(Target::Wifi(ifindex, b)); }
    let kind = ST.lock().model.link(ifindex).map(|l| l.iface.kind);
    match kind {
        Some(if_kind::ETHERNET) => Ok(Target::Wired(ifindex)),
        _ => Err(errno::ENODEV),
    }
}

/// Wi-Fi backends addressed by "ifindex, 0 = all".
fn wifi_targets(ifindex: u32) -> Result<Vec<(u32, &'static dyn WifiBackend)>, i32> {
    if ifindex == 0 {
        let v = backends();
        if v.is_empty() { Err(errno::ENODEV) } else { Ok(v) }
    } else {
        wifi_api::backend(ifindex).map(|b| alloc::vec![(ifindex, b)]).ok_or(errno::ENODEV)
    }
}

/// Submit to several backends: Ok if any accepted, else the first error.
fn submit_all(targets: &[(u32, &'static dyn WifiBackend)], cmd: impl Fn() -> WifiCmd) -> Result<i64, i32> {
    let mut first_err = None;
    let mut ok = false;
    for (_, b) in targets {
        match b.submit(cmd()) {
            Ok(()) => ok = true,
            Err(e) => { first_err.get_or_insert(e); }
        }
    }
    if ok { Ok(0) } else { Err(first_err.unwrap_or(errno::ENODEV)) }
}

fn do_ioctl(cmd: u32, ptr: usize, open_id: u32) -> Result<i64, i32> {
    let writable = {
        let st = ST.lock();
        match st.opens.iter().find(|o| o.id == open_id) {
            Some(o) => o.writable,
            None => return Err(errno::EINVAL),
        }
    };
    if cmd == FIONREAD {
        let n = {
            let mut st = ST.lock();
            let st_ = &mut *st;
            let o = st_.opens.iter_mut().find(|o| o.id == open_id).ok_or(errno::EINVAL)?;
            o.materialize(&st_.model);
            (o.q.len() * EVENT_RECORD_SIZE) as i32
        };
        return if copy_out(ptr, &n.to_le_bytes()) { Ok(0) } else { Err(errno::EFAULT) };
    }
    if !is_netctl_ioctl(cmd) { return Err(errno::ENOTTY); }
    if is_mutating(cmd) && !writable { return Err(errno::EPERM); }
    match ioc_nr(cmd) {
        0x00 => {
            let mut sz = [0u8; 4];
            if !copy_in(ptr, &mut sz) { return Err(errno::EFAULT); }
            let size = u32::from_le_bytes(sz) as usize;
            if size < NcVersion::SIZE { return Err(errno::EINVAL); }
            let mut v = NcVersion::new();
            v.abi_version = NETCTL_ABI_VERSION;
            v.event_size = EVENT_RECORD_SIZE as u32;
            v.queue_records = EVENT_QUEUE_RECORDS as u32;
            v.max_ifaces = MAX_IFACES as u16;
            v.max_bss = MAX_BSS as u16;
            v.max_profiles = MAX_PROFILES as u16;
            v.features = links::FEATURES | if wifi_api::first().is_some() { features::WIFI } else { 0 };
            if !copy_out(ptr, v.as_bytes()) { return Err(errno::EFAULT); }
            Ok(0)
        }
        nr @ 0x01..=0x05 => query(nr, ptr),
        0x06 => {
            let mut st = ST.lock();
            let st_ = &mut *st;
            let o = st_.opens.iter_mut().find(|o| o.id == open_id).ok_or(errno::EINVAL)?;
            snapshot(&st_.model, o, snapshot_reason::REQUESTED);
            let t = tag_of(o.id);
            drop(st);
            wake(t);
            Ok(0)
        }
        0x20 => {
            let (s, _) = read_cmd::<NcScan>(ptr, NcScan::SIZE)?;
            if s.n_ssids as usize > SCAN_MAX_SSIDS { return Err(errno::EINVAL); }
            let mut ssids = Vec::new();
            for x in &s.ssids[..s.n_ssids as usize] {
                if x.len as usize > SSID_MAX { return Err(errno::EINVAL); }
                ssids.push(*x);
            }
            let force = s.flags & scan_req_flags::FORCE != 0;
            let t = wifi_targets(s.ifindex)?;
            submit_all(&t, || WifiCmd::Scan { ssids: ssids.clone(), force })
        }
        0x21 => connect(ptr),
        0x22 => {
            let (r, _) = read_cmd::<NcIfReq>(ptr, NcIfReq::SIZE)?;
            match target(r.ifindex)? {
                Target::Wired(ix) => links::set_admin(ix, false).map(|_| 0),
                Target::Wifi(_, b) => {
                    let block = r.flags & disconnect_flags::KEEP_AUTOCONNECT == 0;
                    b.submit(WifiCmd::Disconnect { block_autoconnect: block }).map(|_| 0)
                }
            }
        }
        0x23 => {
            let (r, _) = read_cmd::<NcRadio>(ptr, NcRadio::SIZE)?;
            let t = wifi_targets(r.ifindex)?;
            let on = r.on != 0;
            submit_all(&t, || WifiCmd::SetRadio(on))
        }
        0x24 => {
            let (_, size) = read_cmd::<NcReload>(ptr, NcReload::SIZE)?;
            let r = reload_profiles();
            let n = size.min(NcReload::SIZE);
            if !copy_out(ptr, &r.as_bytes()[..n]) { return Err(errno::EFAULT); }
            Ok(0)
        }
        _ => Err(errno::ENOTTY),
    }
}

fn connect(ptr: usize) -> Result<i64, i32> {
    let (mut c, _) = read_cmd::<NcConnect>(ptr, NcConnect::SIZE)?;
    let res = connect_inner(&c);
    c.wipe_secret();
    let attempt = res?;
    // Only the out field goes back (never echo the secret through us).
    if attempt != 0 && !copy_out(ptr + 12, &attempt.to_le_bytes()) { return Err(errno::EFAULT); }
    Ok(0)
}

fn connect_inner(c: &NcConnect) -> Result<u32, i32> {
    let (ifindex, be) = match target(c.ifindex)? {
        Target::Wired(ix) => { links::set_admin(ix, true)?; return Ok(0); }
        Target::Wifi(ix, b) => (ix, b),
    };
    let bssid = if c.flags & connect_flags::BSSID != 0 { Some(c.bssid) } else { None };
    let mut p = if c.flags & connect_flags::INLINE != 0 {
        let ssid = c.ssid();
        if ssid.is_empty() || c.ssid_len as usize > SSID_MAX || c.akm > akm::MAX || c.pmf > pmf::REQUIRED
            || c.secret_len as usize > SECRET_MAX || !util::secret_valid_for(c.akm, c.secret())
        {
            return Err(errno::EINVAL);
        }
        ConnectParams {
            attempt: 0,
            profile_uuid: c.profile_uuid,
            ssid: NcSsid::from_bytes(ssid).ok_or(errno::EINVAL)?,
            bssid,
            akm: c.akm,
            pmf: c.pmf,
            hidden: c.flags & connect_flags::HIDDEN != 0,
            secret: Secret::new(c.secret()).ok_or(errno::EINVAL)?,
        }
    } else {
        if util::uuid_is_nil(&c.profile_uuid) { return Err(errno::ENOENT); }
        wifi_api::with_profiles(|_, list| {
            let p = list.iter().find(|p| p.uuid == c.profile_uuid).ok_or(errno::ENOENT)?;
            if p.kind != if_kind::WIFI { return Err(errno::EINVAL); }
            Ok(ConnectParams {
                attempt: 0,
                profile_uuid: p.uuid,
                ssid: NcSsid::from_bytes(p.ssid()).ok_or(errno::EINVAL)?,
                bssid,
                akm: p.akm,
                pmf: p.pmf,
                hidden: p.hidden() || c.flags & connect_flags::HIDDEN != 0,
                secret: Secret::new(p.secret()).ok_or(errno::EINVAL)?,
            })
        })?
    };
    if p.akm == akm::SAE && be.caps() & wifi_caps::SAE == 0 { return Err(errno::EOPNOTSUPP); }
    let attempt = loop {
        // Strictly increasing within a boot and below 2^31: autoconnect
        // attempts (wifi_mgr) carry bit 31, so the two never collide.
        let a = ATTEMPT.fetch_add(1, Ordering::Relaxed).wrapping_add(1) & !drivers::wifi_mgr::AUTO_ATTEMPT_BIT;
        if a != 0 { break a; }
    };
    p.attempt = attempt;
    let uuid = p.profile_uuid;
    be.submit(WifiCmd::Connect(p))?;
    let _ = ifindex;
    // An explicit CONNECT lifts the wrong-key block of that profile.
    let mask = {
        let mut st = ST.lock();
        if st.model.blocked.contains(&uuid) {
            st.model.blocked.retain(|u| *u != uuid);
            emit_profiles(&mut st)
        } else {
            0
        }
    };
    wake(mask);
    Ok(attempt)
}

/// `NETCTL_GET_*` list queries (nr 1..=5).
fn query(nr: u8, ptr: usize) -> Result<i64, i32> {
    let (mut q, size) = read_cmd::<NcQuery>(ptr, NcQuery::SIZE)?;
    let own = match nr {
        0x01 => NcIface::SIZE,
        0x02 => NcIpv4::SIZE,
        0x03 => NcWifiState::SIZE,
        0x04 => NcBss::SIZE,
        _ => NcProfileMeta::SIZE,
    };
    let stride = if q.elem_size == 0 { own } else { q.elem_size as usize };
    if stride < own { return Err(errno::EINVAL); }
    let ix = q.ifindex;
    // Collect under the lock, copy out without it.
    let mut elems: Vec<u8> = Vec::new();
    let (count, gen) = {
        let st = ST.lock();
        let m = &st.model;
        if nr != 0x05 && ix != 0 && m.link(ix).is_none() { return Err(errno::ENODEV); }
        let mut push = |b: &[u8]| { elems.extend_from_slice(b); };
        let mut count = 0u32;
        let gen = match nr {
            0x01 => { for l in m.links.iter().filter(|l| ix == 0 || l.iface.ifindex == ix) { push(l.iface.as_bytes()); count += 1; } 0 }
            0x02 => { for l in m.links.iter().filter(|l| ix == 0 || l.iface.ifindex == ix) { push(l.ipv4.as_bytes()); count += 1; } 0 }
            0x03 => {
                for l in m.links.iter().filter(|l| ix == 0 || l.iface.ifindex == ix) {
                    if let Some(w) = &l.wifi { push(w.state.as_bytes()); count += 1; }
                }
                0
            }
            0x04 => { for b in m.bss.iter().filter(|b| ix == 0 || b.ifindex == ix) { push(b.as_bytes()); count += 1; } m.scan_gen(ix) }
            _ => { for i in 0..m.profiles.len() { push(m.profile_meta(i).as_bytes()); count += 1; } m.pinfo.gen }
        };
        (count, gen)
    };
    let n = (q.cap.min(count)) as usize;
    if n > 0 {
        let mut buf = alloc::vec![0u8; n * stride];
        for i in 0..n {
            buf[i * stride..i * stride + own].copy_from_slice(&elems[i * own..(i + 1) * own]);
        }
        if !copy_out(q.buf as usize, &buf) { return Err(errno::EFAULT); }
    }
    q.count = count;
    q.gen = gen;
    q.elem_size = own as u32;
    let w = size.min(NcQuery::SIZE);
    if !copy_out(ptr, &q.as_bytes()[..w]) { return Err(errno::EFAULT); }
    Ok(n as i64)
}
