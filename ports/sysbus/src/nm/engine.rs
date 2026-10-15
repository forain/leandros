//! The one task that owns the backend and mutates the model.
//!
//! Each turn: take kernel events (all that are queued) or one command, let the
//! model reconcile, then `sync()` the bus with the model — export new objects,
//! emit PropertiesChanged for every property whose value changed (found by
//! diffing each object's GetAll against the last one sent; zbus proxies cache
//! properties, so a missed PropertiesChanged is a stale client), emit the
//! queued explicit signals, and unexport what has expired. A command replies
//! only after its sync, so a returned path is always readable.

use std::collections::{BTreeMap, HashMap};
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant};

use netctl_abi::profiles::{global_flags, Profile};
use netctl_abi::util::uuid_from_str;
use netctl_abi::{connect_flags, wifi_state, Event, NcConnect, MAX_PROFILES};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::sync::oneshot;
use zbus::object_server::{Interface, SignalEmitter};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::backend::{self, Backend, EventRx};
use super::dbus::{AccessPoint, ActiveConn, ConnIface, Device, Ip4Config, NmError, Root, SettingsIface, Wired, Wireless};
use super::model::{nm, ConnSrc, Shared, Sig};
use super::settings::{self, Kind, Parsed, Settings};
use super::{path, NAME, NM_PATH, SETTINGS_PATH};

type Reply<T> = oneshot::Sender<Result<T, NmError>>;

pub enum Cmd {
    Activate { conn: OwnedObjectPath, dev: OwnedObjectPath, spec: OwnedObjectPath, reply: Reply<OwnedObjectPath> },
    AddAndActivate { settings: Settings, dev: OwnedObjectPath, spec: OwnedObjectPath, reply: Reply<(OwnedObjectPath, OwnedObjectPath)> },
    Deactivate { ac: OwnedObjectPath, reply: Reply<()> },
    DevDisconnect { ifindex: u32, reply: Reply<()> },
    RequestScan { ifindex: u32, reply: oneshot::Sender<()> },
    SetWireless { on: bool, reply: Reply<()> },
    AddConnection { settings: Settings, save: bool, reply: Reply<OwnedObjectPath> },
    Update { n: u32, settings: Settings, save: bool, reply: Reply<()> },
    Delete { n: u32, reply: Reply<()> },
    GetSecrets { n: u32, reply: Reply<Settings> },
    ClearSecrets { n: u32, reply: Reply<()> },
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Obj {
    Root,
    Settings,
    Dev { ifindex: u32, wifi: bool },
    Ap(u32),
    Ac(u32),
    Ip4(u32),
    Conn(u32),
}

pub struct Engine {
    conn: zbus::Connection,
    sh: Arc<Shared>,
    be: Box<dyn Backend>,
    ev: EventRx,
    ev_open: bool,
    cmd: UnboundedReceiver<Cmd>,
    exported: BTreeMap<String, Obj>,
    last: HashMap<(String, &'static str), HashMap<String, OwnedValue>>,
    scan_waiters: Vec<(u32, Instant, oneshot::Sender<()>)>,
    named: bool,
}

fn io_err(e: io::Error) -> NmError {
    match e.raw_os_error() {
        Some(libc::EPERM) | Some(libc::EACCES) => NmError::PermissionDenied("not in group netdev".into()),
        Some(libc::ENOENT) => NmError::UnknownConnection("the kernel has no such profile".into()),
        Some(libc::ENODEV) => NmError::UnknownDevice("no such interface".into()),
        Some(libc::ENETDOWN) => NmError::Failed("the radio is off".into()),
        Some(libc::EOPNOTSUPP) => NmError::Failed("this network's security is not supported yet".into()),
        _ => NmError::Failed(e.to_string()),
    }
}

fn denied() -> NmError {
    NmError::PermissionDenied("not in group netdev: the network is read-only for this user".into())
}

impl Engine {
    pub fn new(conn: zbus::Connection, sh: Arc<Shared>, be: Box<dyn Backend>, ev: EventRx, cmd: UnboundedReceiver<Cmd>) -> Self {
        let mut exported = BTreeMap::new();
        exported.insert(NM_PATH.to_owned(), Obj::Root);
        exported.insert(SETTINGS_PATH.to_owned(), Obj::Settings);
        Engine { conn, sh, be, ev, ev_open: true, cmd, exported, last: HashMap::new(), scan_waiters: Vec::new(), named: false }
    }

    pub async fn run(mut self) {
        let started = Instant::now();
        loop {
            let now = Instant::now();
            let mut deadline = now + Duration::from_secs(1);
            if let Some(t) = self.sh.lock().next_deadline() {
                deadline = deadline.min(t.max(now));
            }
            if let Some(t) = self.scan_waiters.iter().map(|w| w.1).min() {
                deadline = deadline.min(t.max(now));
            }
            enum Wake {
                Ev(Option<Event>),
                Cmd(Option<Cmd>),
                Tick,
            }
            let w = tokio::select! {
                e = self.ev.recv(), if self.ev_open => Wake::Ev(e),
                c = self.cmd.recv() => Wake::Cmd(c),
                _ = tokio::time::sleep_until(deadline.into()) => Wake::Tick,
            };
            match w {
                Wake::Ev(Some(e)) => {
                    self.on_event(e);
                    while let Ok(e) = self.ev.try_recv() {
                        self.on_event(e);
                    }
                }
                Wake::Ev(None) => self.ev_open = false,
                Wake::Cmd(Some(c)) => self.on_cmd(c).await,
                Wake::Cmd(None) => return,
                Wake::Tick => {}
            }
            let in_snapshot = self.sh.lock().in_snapshot;
            if !in_snapshot {
                self.sync().await;
            }
            self.answer_scan_waiters();
            if !self.named && (self.sh.lock().snapshot_done || started.elapsed() > Duration::from_secs(3)) {
                // Objects first, then the name (see mod.rs).
                self.sync().await;
                match self.conn.request_name(NAME).await {
                    Ok(_) => super::log(format_args!("serving {NAME}")),
                    Err(e) => super::log(format_args!("cannot own {NAME}: {e}")),
                }
                self.named = true;
            }
        }
    }

    fn on_event(&mut self, e: Event) {
        let scan_done = match &e {
            Event::ScanDone(s) => Some(s.ifindex),
            _ => None,
        };
        self.sh.lock().apply(e, Instant::now());
        if let Some(ifindex) = scan_done {
            let mut i = 0;
            while i < self.scan_waiters.len() {
                if self.scan_waiters[i].0 == ifindex || ifindex == 0 {
                    let (_, _, tx) = self.scan_waiters.swap_remove(i);
                    let _ = tx.send(());
                } else {
                    i += 1;
                }
            }
        }
    }

    fn answer_scan_waiters(&mut self) {
        let now = Instant::now();
        let mut i = 0;
        while i < self.scan_waiters.len() {
            if now >= self.scan_waiters[i].1 {
                let (_, _, tx) = self.scan_waiters.swap_remove(i);
                let _ = tx.send(());
            } else {
                i += 1;
            }
        }
    }

    // ── commands ──────────────────────────────────────────────────────────

    async fn on_cmd(&mut self, c: Cmd) {
        match c {
            Cmd::Activate { conn, dev, spec, reply } => {
                let r = self.activate(&conn, &dev, &spec);
                self.sync().await;
                let _ = reply.send(r);
            }
            Cmd::AddAndActivate { settings, dev, spec, reply } => {
                let r = self.add_and_activate(&settings, &dev, &spec);
                self.sync().await;
                let _ = reply.send(r);
            }
            Cmd::Deactivate { ac, reply } => {
                let r = self.deactivate(&ac);
                self.sync().await;
                let _ = reply.send(r);
            }
            Cmd::DevDisconnect { ifindex, reply } => {
                let r = self.dev_disconnect(ifindex);
                self.sync().await;
                let _ = reply.send(r);
            }
            Cmd::RequestScan { ifindex, reply } => {
                // Never an error: the subscription crate drops every AP when
                // RequestScan fails, and nmrs only warns. A read-only user,
                // a radio that is off or a scan already running all just
                // keep the cached list.
                let r = self.be.scan(ifindex);
                let first = self.sh.lock().ifaces.get(&ifindex).is_some_and(|i| i.last_scan == 0);
                if r.is_ok() && first {
                    // The first ever scan: hold the reply until it is done
                    // (<= 6 s), so LastScan's first value is a real one.
                    self.scan_waiters.push((ifindex, Instant::now() + Duration::from_secs(6), reply));
                } else {
                    let _ = reply.send(());
                }
            }
            Cmd::SetWireless { on, reply } => {
                let r = self.set_wireless(on);
                self.sync().await;
                let _ = reply.send(r);
            }
            Cmd::AddConnection { settings, save, reply } => {
                let r = self.add_connection(&settings, save);
                self.sync().await;
                let _ = reply.send(r);
            }
            Cmd::Update { n, settings, save, reply } => {
                self.update(n, &settings, save);
                self.sync().await;
                let _ = reply.send(Ok(()));
            }
            Cmd::Delete { n, reply } => {
                let r = self.delete(n);
                self.sync().await;
                let _ = reply.send(r);
            }
            Cmd::GetSecrets { n, reply } => {
                let _ = reply.send(self.get_secrets(n));
            }
            Cmd::ClearSecrets { n, reply } => {
                let r = self.clear_secrets(n);
                self.sync().await;
                let _ = reply.send(r);
            }
        }
    }

    /// The interface a method names: its device path, or for "/" the first
    /// interface of the wanted kind.
    fn resolve_dev(&self, dev: &str, kind: Option<Kind>) -> Result<u32, NmError> {
        let m = self.sh.lock();
        if dev != "/" {
            let i = path::dev_num(dev).filter(|i| m.ifaces.contains_key(i));
            return i.ok_or_else(|| NmError::UnknownDevice(format!("no device {dev}")));
        }
        let want_wifi = kind != Some(Kind::Ethernet);
        m.ifaces
            .iter()
            .find(|(_, i)| i.is_wifi() == want_wifi)
            .map(|(k, _)| *k)
            .ok_or_else(|| NmError::UnknownDevice(format!("no {} device", if want_wifi { "Wi-Fi" } else { "wired" })))
    }

    /// AP number, SSID and BSSID of a specific-object path.
    fn spec_ap(&self, spec: &str) -> Option<(u32, Vec<u8>, [u8; 6])> {
        let n = path::ap_num(spec)?;
        let m = self.sh.lock();
        let a = m.aps.get(&n)?;
        Some((n, a.bss.ssid().to_vec(), a.bss.bssid))
    }

    fn reload(&mut self) {
        if let Err(e) = self.be.reload_profiles() {
            super::log(format_args!("reload profiles: {e}"));
        }
    }

    /// Write a Wi-Fi profile (create or update by uuid) and reload. When the
    /// slots cannot be written, the connection lives in memory only and the
    /// returned profile carries what an inline connect needs.
    fn save_wifi(&mut self, p: &Parsed, uuid: [u8; 16], ssid: &[u8]) -> Result<(u32, Option<Profile>), NmError> {
        let uuid_s = super::model::uuid_string(&uuid);
        let res = backend::edit(self.be.store(), |list, _flags| {
            let idx = list.iter().position(|q| q.uuid == uuid);
            let prof = settings::to_profile(p, uuid, ssid, idx.map(|i| &list[i]))?;
            let s = settings::secret_free(&prof, &uuid_s);
            match idx {
                Some(i) => list[i] = prof,
                None if list.len() >= MAX_PROFILES => {
                    return Err(NmError::Failed(format!("at most {MAX_PROFILES} saved networks")));
                }
                None => list.push(prof),
            }
            Ok(s)
        });
        match res {
            Ok(Ok(s)) => {
                self.reload();
                let n = self.sh.lock().put_conn(&uuid_s, ConnSrc::Pending, s, false, ssid.to_vec());
                Ok((n, None))
            }
            Ok(Err(e)) => Err(e),
            Err(e) => {
                super::log(format_args!("profiles not writable ({e}); connection kept in memory"));
                let prof = settings::to_profile(p, uuid, ssid, None)?;
                let s = settings::secret_free(&prof, &uuid_s);
                let n = self.sh.lock().put_conn(&uuid_s, ConnSrc::Memory, s, true, ssid.to_vec());
                Ok((n, Some(prof)))
            }
        }
    }

    fn connect_wifi(
        &mut self,
        ifindex: u32,
        conn_n: u32,
        uuid: [u8; 16],
        ap: Option<(u32, Vec<u8>, [u8; 6])>,
        inline: Option<Profile>,
    ) -> Result<u32, NmError> {
        let mut req = NcConnect::new();
        req.ifindex = ifindex;
        req.profile_uuid = uuid;
        if let Some((_, _, bssid)) = &ap {
            req.flags |= connect_flags::BSSID;
            req.bssid = *bssid;
        }
        if let Some(p) = &inline {
            req.flags |= connect_flags::INLINE;
            if p.hidden() {
                req.flags |= connect_flags::HIDDEN;
            }
            req.ssid_len = p.ssid().len() as u8;
            req.ssid[..p.ssid().len()].copy_from_slice(p.ssid());
            req.akm = p.akm;
            req.pmf = p.pmf;
            req.secret_len = p.secret().len() as u16;
            req.secret[..p.secret().len()].copy_from_slice(p.secret());
        }
        let r = self.be.connect(&mut req);
        req.wipe_secret();
        let attempt = r.map_err(io_err)?;
        let now = Instant::now();
        let mut m = self.sh.lock();
        let ac = m.new_ac(ifindex, attempt, Some(conn_n), ap.map(|a| a.0), true, now);
        m.reconcile(now);
        Ok(ac)
    }

    fn activate_wired(&mut self, ifindex: u32) -> Result<u32, NmError> {
        {
            let m = self.sh.lock();
            if let Some(cur) = m.ifaces.get(&ifindex).and_then(|i| i.cur_ac) {
                if m.acs.get(&cur).is_some_and(|a| a.live()) {
                    return Ok(cur);
                }
            }
            if !m.writable {
                return Err(denied());
            }
        }
        let mut req = NcConnect::new();
        req.ifindex = ifindex;
        self.be.connect(&mut req).map_err(io_err)?;
        let now = Instant::now();
        let mut m = self.sh.lock();
        let conn = m.conns.iter().find(|(_, c)| matches!(c.src, ConnSrc::Wired { ifindex: x } if x == ifindex)).map(|(k, _)| *k);
        let ac = m.new_ac(ifindex, 0, conn, None, true, now);
        m.reconcile(now);
        Ok(ac)
    }

    fn activate(&mut self, conn: &str, dev: &str, spec: &str) -> Result<OwnedObjectPath, NmError> {
        let n = path::conn_num(conn).ok_or_else(|| NmError::UnknownConnection(format!("no connection {conn}")))?;
        let (wired, uuid, unsaved, memory, ssid) = {
            let m = self.sh.lock();
            let c = m.conns.get(&n).ok_or_else(|| NmError::UnknownConnection(format!("no connection {conn}")))?;
            let wired = match c.src {
                ConnSrc::Wired { ifindex } => Some(ifindex),
                _ => None,
            };
            (wired, c.uuid.clone(), c.unsaved, matches!(c.src, ConnSrc::Memory), c.ssid.clone())
        };
        if let Some(ifindex) = wired {
            let ifindex = if dev == "/" { ifindex } else { self.resolve_dev(dev, Some(Kind::Ethernet))? };
            return self.activate_wired(ifindex).map(path::ac);
        }
        if !self.sh.lock().writable || unsaved {
            // A read-only user's overlay can be edited but never activated.
            return Err(denied());
        }
        if memory {
            return Err(NmError::Failed("this connection was never saved; add it again with its password".into()));
        }
        let ifindex = self.resolve_dev(dev, Some(Kind::Wifi))?;
        let mut ap = self.spec_ap(spec);
        if ap.as_ref().is_some_and(|a| a.1 != ssid) {
            ap = None;
        }
        let uuid_b = uuid_from_str(uuid.as_bytes()).unwrap_or_default();
        self.connect_wifi(ifindex, n, uuid_b, ap, None).map(path::ac)
    }

    fn add_and_activate(&mut self, s: &Settings, dev: &str, spec: &str) -> Result<(OwnedObjectPath, OwnedObjectPath), NmError> {
        let p = settings::parse(s)?;
        let ifindex = self.resolve_dev(dev, p.kind)?;
        let (is_wifi, writable) = {
            let m = self.sh.lock();
            let i = m.ifaces.get(&ifindex).ok_or_else(|| NmError::UnknownDevice("device went away".into()))?;
            (i.is_wifi(), m.writable)
        };
        if !is_wifi {
            let ac = self.activate_wired(ifindex)?;
            let m = self.sh.lock();
            let conn = m.conns.iter().find(|(_, c)| matches!(c.src, ConnSrc::Wired { ifindex: x } if x == ifindex)).map(|(k, _)| *k);
            return Ok((conn.map(path::conn).unwrap_or_else(path::root), path::ac(ac)));
        }
        if !writable {
            return Err(denied());
        }
        let ap = self.spec_ap(spec);
        let ssid = p.ssid.clone().or_else(|| ap.as_ref().map(|a| a.1.clone())).ok_or_else(|| {
            NmError::InvalidConnection("no SSID: give 802-11-wireless.ssid or an access point".into())
        })?;
        // One profile per SSID: nmrs re-adds a profile on every connect with a
        // password (its "RebuildFresh" path); real NM would pile up duplicates.
        let uuid = {
            let m = self.sh.lock();
            m.wifi_conn_by_ssid(&ssid)
                .and_then(|n| uuid_from_str(m.conns[&n].uuid.as_bytes()))
                .or(p.uuid)
                .unwrap_or_else(settings::random_uuid)
        };
        let (conn_n, inline) = self.save_wifi(&p, uuid, &ssid)?;
        let ap = ap.filter(|a| a.1 == ssid);
        let ac = self.connect_wifi(ifindex, conn_n, uuid, ap, inline)?;
        Ok((path::conn(conn_n), path::ac(ac)))
    }

    fn deactivate(&mut self, ac: &str) -> Result<(), NmError> {
        let n = path::ac_num(ac).ok_or_else(|| NmError::ConnectionNotActive(format!("{ac} is not active")))?;
        let (ifindex, idle) = {
            let m = self.sh.lock();
            let a = m.acs.get(&n).filter(|a| a.live()).ok_or_else(|| NmError::ConnectionNotActive(format!("{ac} is not active")))?;
            if !m.writable {
                return Err(denied());
            }
            let idle = m.ifaces.get(&a.ifindex).and_then(|i| i.wifi).is_some_and(|w| {
                !matches!(w.state, wifi_state::CONNECTING..=wifi_state::CONNECTED | wifi_state::DISCONNECTING)
            });
            (a.ifindex, idle)
        };
        let r = self.be.disconnect(ifindex);
        let now = Instant::now();
        let mut m = self.sh.lock();
        if idle {
            m.finish_ac(n, nm::ACR_USER_DISCONNECTED, now);
        } else {
            m.set_ac_state(n, nm::AC_DEACTIVATING, nm::ACR_USER_DISCONNECTED);
        }
        m.reconcile(now);
        r.map_err(io_err)
    }

    fn dev_disconnect(&mut self, ifindex: u32) -> Result<(), NmError> {
        let (cur, idle, state) = {
            let mut m = self.sh.lock();
            if !m.writable {
                return Err(denied());
            }
            let i = m.ifaces.get_mut(&ifindex).ok_or_else(|| NmError::UnknownDevice("no such device".into()))?;
            // A FAILED being shown is over once the client asks.
            i.fail_hold = None;
            let idle = i.wifi.is_some_and(|w| {
                !matches!(w.state, wifi_state::CONNECTING..=wifi_state::CONNECTED | wifi_state::DISCONNECTING)
            });
            (i.cur_ac, idle, i.dev_state)
        };
        let now = Instant::now();
        if idle {
            // Nothing to tell the kernel: end what the shim still shows
            // (a pending activation the kernel never started, a FAILED hold).
            let mut m = self.sh.lock();
            if let Some(n) = cur {
                m.finish_ac(n, nm::ACR_USER_DISCONNECTED, now);
            }
            m.reconcile(now);
            return Ok(());
        }
        if state == nm::DEV_DISCONNECTED || state == nm::DEV_UNAVAILABLE {
            return Ok(());
        }
        let r = self.be.disconnect(ifindex);
        let mut m = self.sh.lock();
        if let Some(n) = cur {
            m.set_ac_state(n, nm::AC_DEACTIVATING, nm::ACR_USER_DISCONNECTED);
        }
        m.reconcile(now);
        r.map_err(io_err)
    }

    fn set_wireless(&mut self, on: bool) -> Result<(), NmError> {
        let wifi: Vec<u32> = {
            let mut m = self.sh.lock();
            if !m.writable {
                return Err(denied());
            }
            m.wireless_enabled_wanted = on;
            m.ifaces.iter().filter(|(_, i)| i.is_wifi()).map(|(k, _)| *k).collect()
        };
        for ifindex in wifi {
            self.be.set_radio(ifindex, on).map_err(io_err)?;
        }
        // Remembered across boots, as NM's NetworkManager.state does.
        let r = backend::edit(self.be.store(), |_, flags| {
            if on {
                *flags &= !global_flags::WIFI_RADIO_OFF;
            } else {
                *flags |= global_flags::WIFI_RADIO_OFF;
            }
            Ok::<(), ()>(())
        });
        if let Err(e) = r {
            super::log(format_args!("could not persist WirelessEnabled={on}: {e}"));
        }
        Ok(())
    }

    fn add_connection(&mut self, s: &Settings, save: bool) -> Result<OwnedObjectPath, NmError> {
        let p = settings::parse(s)?;
        if p.kind == Some(Kind::Ethernet) {
            let m = self.sh.lock();
            let n = m.conns.iter().find(|(_, c)| matches!(c.src, ConnSrc::Wired { .. })).map(|(k, _)| *k);
            return n.map(path::conn).ok_or_else(|| NmError::UnknownDevice("no wired device".into()));
        }
        let ssid = p.ssid.clone().ok_or_else(|| NmError::InvalidConnection("802-11-wireless.ssid is required".into()))?;
        let existing = {
            let m = self.sh.lock();
            m.wifi_conn_by_ssid(&ssid).and_then(|n| uuid_from_str(m.conns[&n].uuid.as_bytes()))
        };
        let uuid = existing.or(p.uuid).unwrap_or_else(settings::random_uuid);
        if save {
            if !self.sh.lock().writable {
                return Err(denied());
            }
            return self.save_wifi(&p, uuid, &ssid).map(|(n, _)| path::conn(n));
        }
        let uuid_s = super::model::uuid_string(&uuid);
        let mut stripped = settings::strip_secrets(s);
        stripped.entry("connection".into()).or_default().insert("uuid".into(), settings::ov(uuid_s.as_str()));
        let n = self.sh.lock().put_conn(&uuid_s, ConnSrc::Memory, stripped, true, ssid);
        Ok(path::conn(n))
    }

    /// Never fails (cosmic-settings unwraps it): anything that cannot be
    /// saved becomes an unsaved overlay of this connection.
    fn update(&mut self, n: u32, s: &Settings, save: bool) {
        let (is_wifi, uuid, ssid, writable) = {
            let m = self.sh.lock();
            let Some(c) = m.conns.get(&n) else { return };
            (c.is_wifi(), c.uuid.clone(), c.ssid.clone(), m.writable)
        };
        if is_wifi && save && writable {
            if let Ok(p) = settings::parse(s) {
                let ssid = p.ssid.clone().unwrap_or(ssid);
                if let Some(u) = uuid_from_str(uuid.as_bytes()) {
                    if !ssid.is_empty() && self.save_wifi(&p, u, &ssid).is_ok() {
                        return;
                    }
                }
            }
        }
        let mut m = self.sh.lock();
        if let Some(c) = m.conns.get_mut(&n) {
            let mut o = settings::strip_secrets(s);
            let conn_sec = o.entry("connection".into()).or_default();
            conn_sec.insert("uuid".into(), settings::ov(uuid.as_str()));
            if let Some(t) = settings::get_str(&c.settings, "connection", "type") {
                conn_sec.entry("type".into()).or_insert_with(|| settings::ov(t.as_str()));
            }
            c.settings = o;
            c.unsaved = true;
            m.signals.push(Sig::ConnUpdated(n));
        }
    }

    fn delete(&mut self, n: u32) -> Result<(), NmError> {
        let (src_kind, uuid) = {
            let m = self.sh.lock();
            let c = m.conns.get(&n).ok_or_else(|| NmError::UnknownConnection("no such connection".into()))?;
            if !m.writable {
                return Err(denied());
            }
            let k = match &c.src {
                ConnSrc::Wired { .. } => 0,
                ConnSrc::Memory => 1,
                _ if c.legacy() => 2,
                _ => 3,
            };
            (k, c.uuid.clone())
        };
        match src_kind {
            0 => Err(NmError::Failed("the wired connection is built in".into())),
            2 => Err(NmError::Failed("this network comes from /etc/leandros/wifi.conf; edit that file".into())),
            1 => {
                self.sh.lock().remove_conn(n);
                Ok(())
            }
            _ => {
                let u = uuid_from_str(uuid.as_bytes()).unwrap_or_default();
                let r = backend::edit(self.be.store(), |list, _| {
                    list.retain(|p| p.uuid != u);
                    Ok::<(), ()>(())
                });
                match r {
                    Ok(_) => {
                        self.reload();
                        self.sh.lock().remove_conn(n);
                        Ok(())
                    }
                    Err(e) => Err(io_err(e)),
                }
            }
        }
    }

    fn get_secrets(&mut self, n: u32) -> Result<Settings, NmError> {
        let uuid = {
            let m = self.sh.lock();
            let c = m.conns.get(&n).ok_or_else(|| NmError::UnknownConnection("no such connection".into()))?;
            if !m.writable {
                return Err(denied());
            }
            uuid_from_str(c.uuid.as_bytes()).unwrap_or_default()
        };
        let loaded = backend::load(self.be.store()).map_err(io_err)?;
        let mut out = Settings::new();
        if let Some(p) = loaded.profiles.iter().find(|p| p.uuid == uuid) {
            if !p.secret().is_empty() {
                let psk = String::from_utf8_lossy(p.secret()).into_owned();
                out.entry("802-11-wireless-security".into()).or_default().insert("psk".into(), settings::ov(psk));
            }
        }
        Ok(out)
    }

    fn clear_secrets(&mut self, n: u32) -> Result<(), NmError> {
        let uuid = {
            let m = self.sh.lock();
            let c = m.conns.get(&n).ok_or_else(|| NmError::UnknownConnection("no such connection".into()))?;
            if !m.writable {
                return Err(denied());
            }
            uuid_from_str(c.uuid.as_bytes()).unwrap_or_default()
        };
        backend::edit(self.be.store(), |list, _| {
            for p in list.iter_mut().filter(|p| p.uuid == uuid) {
                p.wipe_secret();
            }
            Ok::<(), ()>(())
        })
        .map_err(io_err)?
        .ok();
        self.reload();
        Ok(())
    }

    // ── bus sync ──────────────────────────────────────────────────────────

    async fn sync(&mut self) {
        let now = Instant::now();
        let (desired, signals) = {
            let mut m = self.sh.lock();
            m.reconcile(now);
            let mut d = BTreeMap::new();
            d.insert(NM_PATH.to_owned(), Obj::Root);
            d.insert(SETTINGS_PATH.to_owned(), Obj::Settings);
            for (&k, i) in &m.ifaces {
                d.insert(path::dev(k).to_string(), Obj::Dev { ifindex: k, wifi: i.is_wifi() });
            }
            for &k in m.aps.keys() {
                d.insert(path::ap(k).to_string(), Obj::Ap(k));
            }
            for &k in m.acs.keys() {
                d.insert(path::ac(k).to_string(), Obj::Ac(k));
                d.insert(path::ip4(k).to_string(), Obj::Ip4(k));
            }
            for &k in m.conns.keys() {
                d.insert(path::conn(k).to_string(), Obj::Conn(k));
            }
            (d, std::mem::take(&mut m.signals))
        };
        for (p, o) in &desired {
            if self.exported.get(p) != Some(o) {
                if let Some(old) = self.exported.remove(p) {
                    self.unexport(p, old).await;
                }
                self.export(p, *o).await;
                self.exported.insert(p.clone(), *o);
            }
        }
        for (p, o) in &desired {
            self.diff(p, *o).await;
        }
        for s in signals {
            if let Err(e) = self.emit(s).await {
                super::log(format_args!("signal: {e}"));
            }
        }
        let stale: Vec<(String, Obj)> =
            self.exported.iter().filter(|(p, _)| !desired.contains_key(*p)).map(|(p, o)| (p.clone(), *o)).collect();
        for (p, o) in stale {
            self.unexport(&p, o).await;
            self.exported.remove(&p);
        }
    }

    async fn export(&self, p: &str, o: Obj) {
        let s = self.conn.object_server();
        let sh = self.sh.clone();
        let r = match o {
            Obj::Root | Obj::Settings => Ok(true),
            Obj::Dev { ifindex, wifi } => {
                let r = s.at(p, Device { sh: sh.clone(), ifindex }).await;
                if wifi {
                    s.at(p, Wireless { sh, ifindex }).await.and(r)
                } else {
                    s.at(p, Wired { sh, ifindex }).await.and(r)
                }
            }
            Obj::Ap(n) => s.at(p, AccessPoint { sh, n }).await,
            Obj::Ac(n) => s.at(p, ActiveConn { sh, n }).await,
            Obj::Ip4(n) => s.at(p, Ip4Config { sh, n }).await,
            Obj::Conn(n) => s.at(p, ConnIface { sh, n }).await,
        };
        if let Err(e) = r {
            super::log(format_args!("export {p}: {e}"));
        }
    }

    async fn unexport(&mut self, p: &str, o: Obj) {
        let s = self.conn.object_server();
        let _ = match o {
            Obj::Root | Obj::Settings => Ok(true),
            Obj::Dev { wifi, .. } => {
                let _ = s.remove::<Device, _>(p).await;
                if wifi { s.remove::<Wireless, _>(p).await } else { s.remove::<Wired, _>(p).await }
            }
            Obj::Ap(_) => s.remove::<AccessPoint, _>(p).await,
            Obj::Ac(_) => s.remove::<ActiveConn, _>(p).await,
            Obj::Ip4(_) => s.remove::<Ip4Config, _>(p).await,
            Obj::Conn(_) => s.remove::<ConnIface, _>(p).await,
        };
        self.last.retain(|(lp, _), _| lp != p);
    }

    async fn diff(&mut self, p: &str, o: Obj) {
        let sh = self.sh.clone();
        match o {
            Obj::Root => self.diff_iface(p, Root(sh)).await,
            Obj::Settings => self.diff_iface(p, SettingsIface(sh)).await,
            Obj::Dev { ifindex, wifi } => {
                self.diff_iface(p, Device { sh: sh.clone(), ifindex }).await;
                if wifi {
                    self.diff_iface(p, Wireless { sh, ifindex }).await
                } else {
                    self.diff_iface(p, Wired { sh, ifindex }).await
                }
            }
            Obj::Ap(n) => self.diff_iface(p, AccessPoint { sh, n }).await,
            Obj::Ac(n) => self.diff_iface(p, ActiveConn { sh, n }).await,
            Obj::Ip4(n) => self.diff_iface(p, Ip4Config { sh, n }).await,
            Obj::Conn(n) => self.diff_iface(p, ConnIface { sh, n }).await,
        }
    }

    /// PropertiesChanged for every property of `iface` at `p` whose value
    /// differs from the last one published (nothing on the first look: the
    /// object was just exported and clients read it fresh).
    async fn diff_iface<I: Interface>(&mut self, p: &str, iface: I) {
        let Ok(emitter) = SignalEmitter::new(&self.conn, p.to_owned()) else { return };
        let server = self.conn.object_server();
        let Ok(now) = iface.get_all(&server, &self.conn, None, &emitter).await else { return };
        let name = I::name();
        let key = (p.to_owned(), leak_name(name.as_str()));
        match self.last.get(&key) {
            None => {
                self.last.insert(key, now);
            }
            Some(prev) => {
                let changed: HashMap<&str, Value<'_>> = now
                    .iter()
                    .filter(|(k, v)| prev.get(*k) != Some(*v))
                    .filter_map(|(k, v)| v.try_clone().ok().map(|v| (k.as_str(), Value::from(v))))
                    .collect();
                if !changed.is_empty() {
                    let r = zbus::fdo::Properties::properties_changed(&emitter, name.clone(), changed, (&[][..]).into()).await;
                    if let Err(e) = r {
                        super::log(format_args!("PropertiesChanged {p}: {e}"));
                    }
                }
                self.last.insert(key, now);
            }
        }
    }

    async fn emit(&self, s: Sig) -> zbus::Result<()> {
        const NMI: &str = "org.freedesktop.NetworkManager";
        let c = &self.conn;
        match s {
            Sig::NmStateChanged(st) => c.emit_signal(None::<()>, NM_PATH, NMI, "StateChanged", &(st,)).await,
            Sig::DeviceAdded(i) => c.emit_signal(None::<()>, NM_PATH, NMI, "DeviceAdded", &(path::dev(i),)).await,
            Sig::DeviceRemoved(i) => c.emit_signal(None::<()>, NM_PATH, NMI, "DeviceRemoved", &(path::dev(i),)).await,
            Sig::DevStateChanged { ifindex, new, old, reason } => {
                c.emit_signal(None::<()>, path::dev(ifindex).as_str(), "org.freedesktop.NetworkManager.Device", "StateChanged", &(new, old, reason))
                    .await
            }
            Sig::ApAdded { ifindex, ap } => {
                c.emit_signal(
                    None::<()>,
                    path::dev(ifindex).as_str(),
                    "org.freedesktop.NetworkManager.Device.Wireless",
                    "AccessPointAdded",
                    &(path::ap(ap),),
                )
                .await
            }
            Sig::ApRemoved { ifindex, ap } => {
                c.emit_signal(
                    None::<()>,
                    path::dev(ifindex).as_str(),
                    "org.freedesktop.NetworkManager.Device.Wireless",
                    "AccessPointRemoved",
                    &(path::ap(ap),),
                )
                .await
            }
            Sig::AcStateChanged { ac, state, reason } => {
                c.emit_signal(None::<()>, path::ac(ac).as_str(), "org.freedesktop.NetworkManager.Connection.Active", "StateChanged", &(state, reason))
                    .await
            }
            Sig::NewConnection(n) => {
                c.emit_signal(None::<()>, SETTINGS_PATH, "org.freedesktop.NetworkManager.Settings", "NewConnection", &(path::conn(n),)).await
            }
            Sig::ConnectionRemoved(n) => {
                c.emit_signal(None::<()>, SETTINGS_PATH, "org.freedesktop.NetworkManager.Settings", "ConnectionRemoved", &(path::conn(n),))
                    .await
            }
            Sig::ConnUpdated(n) => {
                c.emit_signal(None::<()>, path::conn(n).as_str(), "org.freedesktop.NetworkManager.Settings.Connection", "Updated", &()).await
            }
            Sig::ConnRemoved(n) => {
                c.emit_signal(None::<()>, path::conn(n).as_str(), "org.freedesktop.NetworkManager.Settings.Connection", "Removed", &()).await
            }
        }
    }
}

/// Interface names are a small fixed set; intern them as &'static str keys.
fn leak_name(s: &str) -> &'static str {
    const KNOWN: &[&str] = &[
        "org.freedesktop.NetworkManager",
        "org.freedesktop.NetworkManager.Settings",
        "org.freedesktop.NetworkManager.Device",
        "org.freedesktop.NetworkManager.Device.Wireless",
        "org.freedesktop.NetworkManager.Device.Wired",
        "org.freedesktop.NetworkManager.AccessPoint",
        "org.freedesktop.NetworkManager.Connection.Active",
        "org.freedesktop.NetworkManager.IP4Config",
        "org.freedesktop.NetworkManager.Settings.Connection",
    ];
    KNOWN.iter().find(|k| **k == s).copied().unwrap_or("other")
}
