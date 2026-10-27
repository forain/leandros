//! `leandros-sysbus bluez`: org.bluez (BlueZ 5 D-Bus API) for the COSMIC
//! Bluetooth applet (bluer 0.17) and COSMIC Settings' Bluetooth page
//! (bluez-zbus), as a façade over btd (`/run/bluetooth/btd.sock`,
//! docs/servers/bluetooth.md). btd owns the controller, pairing and the link
//! keys; this process only maps BlueZ objects onto btd requests.
//!
//! Objects: `/org/bluez` (AgentManager1, ProfileManager1), `/org/bluez/hci0`
//! (Adapter1, exported while btd reports a ready controller) and
//! `/org/bluez/hci0/dev_AA_BB_CC_DD_EE_FF` (Device1), with an ObjectManager at
//! `/`. Like the NetworkManager shim, PropertiesChanged comes from one place:
//! after every turn the engine diffs GetAll of each object against what it
//! last published, so no mutation can forget its signal.
//!
//! Mapping: StartDiscovery = btd SCAN in 30 s rounds until StopDiscovery
//! (Discovering stays true across rounds: bluer ends its stream on false);
//! Pair = btd PAIR (SSP just-works; agents are accepted, never called);
//! Connect on an input device = the bthid hook (see `hid_hook`), otherwise an
//! SDP channel held open until Disconnect; Disconnect = btd DISCONNECT;
//! RemoveDevice = btd REMOVE (forgets the link key). State is re-read from btd
//! (INFO + DEVICES) every 1.5 s.

mod dbus;
pub mod test;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use bt_host::api::{op, DeviceInfo, FrameBuf, Msg};
use bt_host::Addr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;
use tokio::sync::{mpsc, oneshot};
use zbus::object_server::{Interface, SignalEmitter};
use zbus::zvariant::{OwnedValue, Value};

pub const NAME: &str = "org.bluez";
pub const ROOT_PATH: &str = "/org/bluez";
pub const ADAPTER_PATH: &str = "/org/bluez/hci0";
/// bthid's request spool (lane bthid): one `AA:BB:CC:DD:EE:FF` line asks the
/// HID host to remember and connect that device. The shim only appends to an
/// existing file; bthid owns its creation and mode.
pub const BTHID_REQ: &str = "/run/bluetooth/bthid.connect";

pub(crate) fn log(msg: std::fmt::Arguments<'_>) {
    crate::log("bluez", msg);
}

pub fn sock_path() -> String {
    std::env::var("LEANDROS_BTD_SOCK").ok().filter(|s| !s.is_empty()).unwrap_or_else(|| "/run/bluetooth/btd.sock".into())
}

pub fn dev_path(a: &Addr) -> String {
    format!("{ADAPTER_PATH}/dev_{}", a.to_string().replace(':', "_"))
}

pub fn path_addr(p: &str) -> Option<Addr> {
    Addr::parse(&p.strip_prefix(ADAPTER_PATH)?.strip_prefix("/dev_")?.replace('_', ":"))
}

// ---------------------------------------------------------------- btd client

/// One btd.sock connection (one per operation, so long requests such as a
/// scan or a held channel never block the others).
pub struct Btd {
    s: UnixStream,
    fb: FrameBuf,
}

impl Btd {
    pub async fn open() -> io::Result<Btd> {
        Ok(Btd { s: UnixStream::connect(sock_path()).await?, fb: FrameBuf::default() })
    }
    pub async fn send(&mut self, m: &Msg) -> io::Result<()> {
        let mut v = Vec::new();
        m.encode(&mut v);
        self.s.write_all(&v).await
    }
    pub async fn next(&mut self) -> io::Result<Msg> {
        loop {
            if let Some(f) = self.fb.next() {
                if let Some(m) = f {
                    return Ok(m);
                }
                continue;
            }
            let mut b = [0u8; 4096];
            let n = self.s.read(&mut b).await?;
            if n == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
            self.fb.push(&b[..n]);
        }
    }
    /// Sends `m`, waits for RESULT(`o`) and returns its status; DEVICE frames
    /// on the way go to `each`.
    pub async fn op(&mut self, m: &Msg, o: u8, mut each: impl FnMut(DeviceInfo)) -> io::Result<u8> {
        self.send(m).await?;
        loop {
            match self.next().await? {
                Msg::Result { op, status } if op == o => return Ok(status),
                Msg::Device(d) => each(d),
                _ => {}
            }
        }
    }
    pub async fn info(&mut self) -> io::Result<Msg> {
        self.send(&Msg::Info).await?;
        loop {
            let m = self.next().await?;
            if matches!(m, Msg::InfoRsp { .. }) {
                return Ok(m);
            }
        }
    }
}

/// One-shot request on a fresh connection, with a deadline.
pub async fn btd_op(m: Msg, o: u8, secs: u64) -> io::Result<u8> {
    let fut = async {
        let mut b = Btd::open().await?;
        b.op(&m, o, |_| {}).await
    };
    tokio::time::timeout(Duration::from_secs(secs), fut).await.unwrap_or_else(|_| Err(io::ErrorKind::TimedOut.into()))
}

// ---------------------------------------------------------------- model

pub struct Dev {
    pub addr: Addr,
    pub name: String,
    pub alias: Option<String>,
    pub class: u32,
    pub rssi: Option<i16>,
    pub paired: bool,
    pub connected: bool,
    pub trusted: bool,
    pub blocked: bool,
    pub le: bool,
    pub random: bool,
    pub seen: Instant,
}

impl Dev {
    pub fn alias(&self) -> String {
        if let Some(a) = &self.alias {
            return a.clone();
        }
        if !self.name.is_empty() {
            return self.name.clone();
        }
        self.addr.to_string().replace(':', "-")
    }
    pub fn major(&self) -> u32 {
        (self.class >> 8) & 0x1f
    }
    /// An input device (keyboard, mouse, gamepad...): its Connect goes to the HID host.
    pub fn is_hid(&self) -> bool {
        self.major() == 0x05
    }
    /// BlueZ's class_to_icon().
    pub fn icon(&self) -> &'static str {
        let c = self.class;
        match self.major() {
            0x01 => "computer",
            0x02 => "phone",
            0x03 => "network-wireless",
            0x04 => match (c & 0xfc) >> 2 {
                0x01 | 0x02 => "audio-headset",
                0x06 => "audio-headphones",
                0x0b | 0x0c | 0x0d => "camera-video",
                _ => "audio-card",
            },
            0x05 => match (c & 0xc0) >> 6 {
                0x01 => "input-keyboard",
                0x02 => "input-mouse",
                _ => match (c & 0x1e) >> 2 {
                    0x01 | 0x02 => "input-gaming",
                    0x05 => "input-tablet",
                    _ => "input-keyboard",
                },
            },
            0x06 if c & 0x80 != 0 => "printer",
            0x06 if c & 0x20 != 0 => "camera-photo",
            _ => "bluetooth",
        }
    }
    pub fn uuids(&self) -> Vec<String> {
        let mut v = Vec::new();
        if self.is_hid() {
            v.push("00001124-0000-1000-8000-00805f9b34fb".into());
        }
        v
    }
}

pub struct Model {
    pub ready: bool,
    pub addr: Addr,
    pub name: String,
    pub alias: Option<String>,
    pub powered: bool,
    pub discoverable: bool,
    pub pairable: bool,
    pub discovering: bool,
    pub devs: BTreeMap<[u8; 6], Dev>,
    /// Removed with RemoveDevice: btd may still list them, so they stay
    /// hidden until a scan sees them again.
    pub hidden: HashSet<[u8; 6]>,
}

impl Model {
    fn upsert(&mut self, d: &DeviceInfo, from_scan: bool) {
        if from_scan {
            self.hidden.remove(&d.addr.0);
        } else if self.hidden.contains(&d.addr.0) {
            return;
        }
        let now = Instant::now();
        let e = self.devs.entry(d.addr.0).or_insert_with(|| Dev {
            addr: d.addr,
            name: String::new(),
            alias: None,
            class: 0,
            rssi: None,
            paired: false,
            connected: false,
            trusted: false,
            blocked: false,
            le: false,
            random: false,
            seen: now,
        });
        if !d.name.is_empty() {
            e.name = d.name.clone();
        }
        if d.class != 0 {
            e.class = d.class;
        }
        let paired = d.flags & bt_host::api::dev_flags::PAIRED != 0;
        if paired && !e.paired {
            e.trusted = true;
        }
        e.paired = paired;
        e.connected = d.flags & bt_host::api::dev_flags::CONNECTED != 0;
        e.le = d.flags & bt_host::api::dev_flags::LE != 0;
        e.random = d.flags & bt_host::api::dev_flags::LE_RANDOM != 0;
        if from_scan {
            e.seen = now;
            if d.rssi != 0 {
                e.rssi = Some(d.rssi as i16);
            }
        }
    }
}

pub struct Shared {
    m: Mutex<Model>,
    kick: mpsc::UnboundedSender<()>,
    /// Devices connected through a held SDP channel: dropping the sender closes it.
    held: Mutex<HashMap<[u8; 6], oneshot::Sender<()>>>,
    scan_stop: Mutex<Option<oneshot::Sender<()>>>,
}

pub type Sh = Arc<Shared>;

impl Shared {
    pub fn lock(&self) -> MutexGuard<'_, Model> {
        self.m.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub fn kick(&self) {
        let _ = self.kick.send(());
    }
    pub fn stop_scan(&self) {
        if let Some(tx) = self.scan_stop.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(());
        }
    }
    pub fn drop_held(&self, a: &Addr) -> bool {
        self.held.lock().unwrap_or_else(|e| e.into_inner()).remove(&a.0).is_some()
    }
}

/// Starts the scan task (no-op when one runs). Discovering stays true until
/// StopDiscovery, a power-off, or the 10-minute cap (a client that died
/// without StopDiscovery must not keep the radio inquiring forever).
pub fn start_scan(sh: &Sh) {
    let (tx, mut rx) = oneshot::channel();
    {
        let mut s = sh.scan_stop.lock().unwrap_or_else(|e| e.into_inner());
        if s.is_some() {
            return;
        }
        *s = Some(tx);
    }
    sh.lock().discovering = true;
    sh.kick();
    let sh = sh.clone();
    tokio::spawn(async move {
        let t0 = Instant::now();
        'rounds: while t0.elapsed() < Duration::from_secs(600) {
            let mut b = match Btd::open().await {
                Ok(b) => b,
                Err(e) => {
                    log(format_args!("scan: btd: {e}"));
                    break;
                }
            };
            if b.send(&Msg::Scan { secs: 30 }).await.is_err() {
                break;
            }
            loop {
                tokio::select! {
                    _ = &mut rx => {
                        let _ = b.send(&Msg::Scan { secs: 0 }).await;
                        break 'rounds;
                    }
                    m = b.next() => match m {
                        Ok(Msg::Device(d)) => { sh.lock().upsert(&d, true); sh.kick(); }
                        Ok(Msg::Result { op: op::SCAN, status }) => {
                            if status != 0 {
                                log(format_args!("scan: btd status {status}"));
                                tokio::time::sleep(Duration::from_secs(1)).await;
                            }
                            continue 'rounds;
                        }
                        Ok(_) => {}
                        Err(e) => { log(format_args!("scan: {e}")); break 'rounds; }
                    }
                }
            }
        }
        sh.scan_stop.lock().unwrap_or_else(|e| e.into_inner()).take();
        let mut m = sh.lock();
        m.discovering = false;
        for d in m.devs.values_mut() {
            d.rssi = None;
        }
        drop(m);
        sh.kick();
    });
}

/// Holds an SDP channel to `a` open (keeps the ACL link up) until the sender
/// stored in `held` is dropped or the link goes away.
pub async fn hold_link(sh: &Sh, a: Addr) -> Result<(), u8> {
    let mut b = Btd::open().await.map_err(|_| bt_host::api::status::NOT_READY)?;
    b.send(&Msg::Connect { req: 1, addr: a, psm: 1 }).await.map_err(|_| bt_host::api::status::NOT_READY)?;
    let cid = loop {
        match tokio::time::timeout(Duration::from_secs(30), b.next()).await {
            Ok(Ok(Msg::ConnectResult { req: 1, status: 0, cid, .. })) => break cid,
            Ok(Ok(Msg::ConnectResult { req: 1, status, .. })) => return Err(status),
            Ok(Ok(_)) => {}
            Ok(Err(_)) => return Err(bt_host::api::status::NOT_READY),
            Err(_) => return Err(bt_host::api::status::TIMEOUT),
        }
    };
    let (tx, mut rx) = oneshot::channel::<()>();
    sh.held.lock().unwrap_or_else(|e| e.into_inner()).insert(a.0, tx);
    let sh2 = sh.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut rx => { let _ = b.send(&Msg::Close { cid }).await; break; }
                m = b.next() => match m {
                    Ok(Msg::Closed { cid: c, .. }) if c == cid => break,
                    Ok(_) => {}
                    Err(_) => break,
                }
            }
        }
        sh2.held.lock().unwrap_or_else(|e| e.into_inner()).remove(&a.0);
        sh2.kick();
    });
    Ok(())
}

/// The bthid hook: asks the HID host to connect `a` (control + interrupt
/// channels, uinput nodes). False when no HID host takes requests.
pub fn hid_hook(a: &Addr) -> bool {
    use std::io::Write;
    match std::fs::OpenOptions::new().append(true).open(BTHID_REQ) {
        Ok(mut f) => f.write_all(format!("{a}\n").as_bytes()).is_ok(),
        Err(e) => {
            log(format_args!("no HID host request spool {BTHID_REQ} ({e}); generic connect"));
            false
        }
    }
}

/// Polls btd until `a` reports connected (or not), up to `secs`.
pub async fn wait_connected(sh: &Sh, a: &Addr, want: bool, secs: u64) -> bool {
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_secs(secs) {
        poll(sh).await;
        if sh.lock().devs.get(&a.0).map(|d| d.connected) == Some(want) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    false
}

/// Re-reads INFO + DEVICES from btd into the model.
pub async fn poll(sh: &Sh) {
    let fut = async {
        let mut b = Btd::open().await?;
        let info = b.info().await?;
        let mut list = Vec::new();
        b.op(&Msg::Devices, op::DEVICES, |d| list.push(d)).await?;
        Ok::<_, io::Error>((info, list))
    };
    let r = tokio::time::timeout(Duration::from_secs(3), fut).await;
    let mut m = sh.lock();
    match r {
        Ok(Ok((Msg::InfoRsp { addr, state, name, .. }, list))) => {
            if !m.ready && state == 1 {
                log(format_args!("adapter {addr} \"{name}\" ready"));
            }
            m.ready = state == 1;
            m.addr = addr;
            m.name = name;
            let listed: HashSet<[u8; 6]> = list.iter().map(|d| d.addr.0).collect();
            for d in &list {
                m.upsert(d, false);
            }
            let discovering = m.discovering;
            m.devs.retain(|k, d| {
                if listed.contains(k) {
                    return true;
                }
                d.paired = false;
                d.connected = false;
                // An unpaired device btd no longer lists lingers 3 min after
                // it was last seen while not discovering (BlueZ's temporary devices).
                discovering || d.seen.elapsed() < Duration::from_secs(180)
            });
        }
        _ => {
            if m.ready {
                log(format_args!("btd unreachable at {}; adapter gone", sock_path()));
            }
            m.ready = false;
        }
    }
    let held: Vec<[u8; 6]> = sh.held.lock().unwrap_or_else(|e| e.into_inner()).keys().copied().collect();
    for k in held {
        if let Some(d) = m.devs.get_mut(&k) {
            d.connected = true;
        }
    }
}

// ---------------------------------------------------------------- engine

struct Engine {
    conn: zbus::Connection,
    sh: Sh,
    exported: HashSet<String>,
    adapter: bool,
    last: HashMap<(String, &'static str), HashMap<String, OwnedValue>>,
}

impl Engine {
    async fn sync(&mut self) {
        let (ready, devs): (bool, Vec<Addr>) = {
            let m = self.sh.lock();
            (m.ready, if m.ready { m.devs.values().map(|d| d.addr).collect() } else { Vec::new() })
        };
        let server = self.conn.object_server();
        let want: HashSet<String> = devs.iter().map(dev_path).collect();
        let gone: Vec<String> = self.exported.difference(&want).cloned().collect();
        for p in gone {
            let _ = server.remove::<dbus::Device, _>(p.as_str()).await;
            self.last.remove(&(p.clone(), "org.bluez.Device1"));
            self.exported.remove(&p);
        }
        if self.adapter && !ready {
            let _ = server.remove::<dbus::Adapter, _>(ADAPTER_PATH).await;
            self.last.remove(&(ADAPTER_PATH.to_string(), "org.bluez.Adapter1"));
            self.adapter = false;
        }
        if !self.adapter && ready {
            if let Err(e) = server.at(ADAPTER_PATH, dbus::Adapter(self.sh.clone())).await {
                log(format_args!("export adapter: {e}"));
            }
            self.adapter = true;
        }
        for a in &devs {
            let p = dev_path(a);
            if !self.exported.contains(&p) {
                if let Err(e) = server.at(p.as_str(), dbus::Device { sh: self.sh.clone(), addr: *a }).await {
                    log(format_args!("export {p}: {e}"));
                }
                self.exported.insert(p);
            }
        }
        if self.adapter {
            self.diff(ADAPTER_PATH, "org.bluez.Adapter1", dbus::Adapter(self.sh.clone())).await;
        }
        for a in &devs {
            self.diff(&dev_path(a), "org.bluez.Device1", dbus::Device { sh: self.sh.clone(), addr: *a }).await;
        }
    }

    async fn diff<I: Interface>(&mut self, p: &str, name: &'static str, iface: I) {
        let Ok(emitter) = SignalEmitter::new(&self.conn, p.to_owned()) else { return };
        let server = self.conn.object_server();
        let Ok(now) = iface.get_all(&server, &self.conn, None, &emitter).await else { return };
        let key = (p.to_owned(), name);
        if let Some(prev) = self.last.get(&key) {
            let changed: HashMap<&str, Value<'_>> = now
                .iter()
                .filter(|(k, v)| prev.get(*k) != Some(*v))
                .filter_map(|(k, v)| v.try_clone().ok().map(|v| (k.as_str(), Value::from(v))))
                .collect();
            if !changed.is_empty() {
                let iname = zbus::names::InterfaceName::from_static_str_unchecked(name);
                if let Err(e) = zbus::fdo::Properties::properties_changed(&emitter, iname, changed, (&[][..]).into()).await {
                    log(format_args!("PropertiesChanged {p}: {e}"));
                }
            }
        }
        self.last.insert(key, now);
    }
}

pub async fn serve(builder: zbus::connection::Builder<'_>) -> zbus::Result<zbus::Connection> {
    let (tx, mut rx) = mpsc::unbounded_channel();
    let sh: Sh = Arc::new(Shared {
        m: Mutex::new(Model {
            ready: false,
            addr: Addr([0; 6]),
            name: String::new(),
            alias: None,
            powered: true,
            discoverable: false,
            pairable: true,
            discovering: false,
            devs: BTreeMap::new(),
            hidden: HashSet::new(),
        }),
        kick: tx,
        held: Mutex::new(HashMap::new()),
        scan_stop: Mutex::new(None),
    });
    log(format_args!("btd at {}", sock_path()));
    // The adapter is exported before the name is taken when btd is already
    // up: COSMIC Settings never picks up an adapter that appears later.
    poll(&sh).await;
    let conn = builder
        .serve_at("/", zbus::fdo::ObjectManager)?
        .serve_at(ROOT_PATH, dbus::AgentManager)?
        .serve_at(ROOT_PATH, dbus::ProfileManager)?
        .build()
        .await?;
    let mut eng = Engine { conn: conn.clone(), sh: sh.clone(), exported: HashSet::new(), adapter: false, last: HashMap::new() };
    eng.sync().await;
    // The name last: whoever got us activated sees a complete object tree.
    conn.request_name(NAME).await?;
    tokio::spawn(async move {
        let mut iv = tokio::time::interval(Duration::from_millis(1500));
        loop {
            tokio::select! {
                _ = iv.tick() => poll(&eng.sh).await,
                k = rx.recv() => if k.is_none() { break },
            }
            while rx.try_recv().is_ok() {}
            eng.sync().await;
        }
    });
    Ok(conn)
}
