//! What the shim needs from "the kernel": a stream of netctl events, a handful
//! of commands, and the profile slots. Two implementations talk to something
//! real or simulated (netctl.rs, mock.rs); a third (here) reports nothing,
//! for a kernel without /dev/netctl.
//!
//! Both real and mock speak the netctl-abi types, so the engine and model are
//! exercised identically by either.

use std::io;
use std::path::PathBuf;

use netctl_abi::profiles::{self, Profile, Slot};
use netctl_abi::{Event, NcConnect, NcReload, NcSnapshotInfo};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

pub type EventTx = UnboundedSender<Event>;
pub type EventRx = UnboundedReceiver<Event>;

/// The kernel side, as the engine drives it. Every command is synchronous and
/// cheap (an ioctl, or a timer armed in the mock); outcomes arrive as events.
pub trait Backend: Send + Sync {
    fn name(&self) -> &'static str;
    /// This open may issue mutating commands (root or group netdev).
    fn writable(&self) -> bool;
    /// `netctl_abi::features` bits.
    fn features(&self) -> u32;
    fn scan(&mut self, ifindex: u32) -> io::Result<()>;
    /// Start a connect (Wi-Fi) or admin-up (wired); returns the attempt id.
    fn connect(&mut self, req: &mut NcConnect) -> io::Result<u32>;
    /// Wi-Fi: leave the network. Wired: admin down.
    fn disconnect(&mut self, ifindex: u32) -> io::Result<()>;
    fn set_radio(&mut self, ifindex: u32, on: bool) -> io::Result<()>;
    /// Make the kernel re-read the profile slots (synchronous).
    fn reload_profiles(&mut self) -> io::Result<NcReload>;
    fn store(&self) -> &dyn ProfileStore;
}

/// The two A/B slot files.
pub trait ProfileStore: Send + Sync {
    /// Ok(None) when the slot does not exist; Err when it exists but cannot be
    /// read (a writer must then not guess at the sequence number).
    fn read(&self, slot: Slot) -> io::Result<Option<Vec<u8>>>;
    fn write(&self, slot: Slot, data: &[u8]) -> io::Result<()>;
}

/// Slots as files in a directory. The real one is /etc/leandros/network,
/// pre-created root:netdev 0660 by mkfs; writers truncate in place, so no
/// create/rename/chown is needed there. The mock's directory is its own.
pub struct DirStore {
    pub dir: PathBuf,
    pub create: bool,
}

impl DirStore {
    fn path(&self, slot: Slot) -> PathBuf {
        let name = std::path::Path::new(slot.path()).file_name().expect("slot file name").to_owned();
        self.dir.join(name)
    }
}

impl ProfileStore for DirStore {
    fn read(&self, slot: Slot) -> io::Result<Option<Vec<u8>>> {
        match std::fs::read(self.path(slot)) {
            Ok(mut d) => {
                d.truncate(profiles::FILE_READ_MAX);
                Ok(Some(d))
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
    fn write(&self, slot: Slot, data: &[u8]) -> io::Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .truncate(true)
            .create(self.create)
            .mode(0o600)
            .open(self.path(slot))?;
        f.write_all(data)?;
        f.sync_all()
    }
}

/// The loaded image: profiles of the winning slot and its global flags.
pub struct Loaded {
    pub profiles: Vec<Profile>,
    pub global_flags: u32,
}

/// Read both slots and decode the winner (empty when neither is valid).
pub fn load(store: &dyn ProfileStore) -> io::Result<Loaded> {
    let a = store.read(Slot::A)?;
    let b = store.read(Slot::B)?;
    Ok(match profiles::select(a.as_deref(), b.as_deref()) {
        Some((_, v)) => Loaded { profiles: v.profiles().collect(), global_flags: v.global_flags },
        None => Loaded { profiles: Vec::new(), global_flags: 0 },
    })
}

/// Read-modify-write the profiles: decode the winning slot, let `f` edit the
/// list, write the result into the other slot with seq+1. The kernel picks it
/// up on the next RELOAD_PROFILES. A torn write leaves the old slot in charge.
/// When `f` fails nothing is written.
pub fn edit<R, E>(
    store: &dyn ProfileStore,
    f: impl FnOnce(&mut Vec<Profile>, &mut u32) -> Result<R, E>,
) -> io::Result<Result<R, E>> {
    let a = store.read(Slot::A)?;
    let b = store.read(Slot::B)?;
    let (mut list, mut flags) = match profiles::select(a.as_deref(), b.as_deref()) {
        Some((_, v)) => (v.profiles().collect::<Vec<_>>(), v.global_flags),
        None => (Vec::new(), 0),
    };
    let r = match f(&mut list, &mut flags) {
        Ok(r) => r,
        Err(e) => return Ok(Err(e)),
    };
    let (slot, seq) = profiles::write_target(a.as_deref(), b.as_deref());
    let mut buf = vec![0u8; profiles::serialized_len(list.len())];
    let n = profiles::serialize(seq, flags, &list, &mut buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("profile image: {e:?}")))?;
    let res = store.write(slot, &buf[..n]);
    // The image holds secrets; do not leave it in freed memory.
    for b in buf.iter_mut() {
        // SAFETY: plain byte store; volatile so it is not elided.
        unsafe { core::ptr::write_volatile(b, 0) };
    }
    res.map(|()| Ok(r))
}

/// No /dev/netctl (a kernel before the netctl lanes): one snapshot with no
/// interfaces, read-only. The applet then shows no devices instead of an error.
pub struct NullBackend {
    store: DirStore,
}

impl Backend for NullBackend {
    fn name(&self) -> &'static str {
        "none (no /dev/netctl)"
    }
    fn writable(&self) -> bool {
        false
    }
    fn features(&self) -> u32 {
        0
    }
    fn scan(&mut self, _: u32) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENODEV))
    }
    fn connect(&mut self, _: &mut NcConnect) -> io::Result<u32> {
        Err(io::Error::from_raw_os_error(libc::ENODEV))
    }
    fn disconnect(&mut self, _: u32) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENODEV))
    }
    fn set_radio(&mut self, _: u32, _: bool) -> io::Result<()> {
        Err(io::Error::from_raw_os_error(libc::ENODEV))
    }
    fn reload_profiles(&mut self) -> io::Result<NcReload> {
        Err(io::Error::from_raw_os_error(libc::ENODEV))
    }
    fn store(&self) -> &dyn ProfileStore {
        &self.store
    }
}

/// Which backend: LEANDROS_NM_BACKEND=mock|netctl|none wins; otherwise the
/// mock when asked for (LEANDROS_NM_MOCK=<scenario|1>, the file
/// /etc/leandros/nm-mock, or `nmshim.mock` on the kernel command line), else
/// /dev/netctl, else the null backend.
pub fn open() -> (Box<dyn Backend>, EventRx) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let forced = std::env::var("LEANDROS_NM_BACKEND").unwrap_or_default();
    let mock_scenario = mock_requested();
    if forced == "mock" || (forced.is_empty() && mock_scenario.is_some()) {
        return (Box::new(super::mock::MockBackend::start(mock_scenario.flatten(), tx)), rx);
    }
    if forced != "none" {
        match super::netctl::NetctlBackend::open(tx.clone()) {
            Ok(b) => return (Box::new(b), rx),
            Err(e) => super::log(format_args!("{}: {e}; reporting no devices", netctl_abi::NETCTL_PATH)),
        }
    }
    let empty = NcSnapshotInfo { reason: netctl_abi::snapshot_reason::OPEN, abi_version: 1, ..Default::default() };
    let _ = tx.send(Event::SnapshotBegin(empty));
    let _ = tx.send(Event::SnapshotEnd(empty));
    (Box::new(NullBackend { store: DirStore { dir: netctl_abi::PROFILES_DIR.into(), create: false } }), rx)
}

/// Some(Some(path)) = mock with a scenario file; Some(None) = built-in scenario.
fn mock_requested() -> Option<Option<PathBuf>> {
    if let Ok(v) = std::env::var("LEANDROS_NM_MOCK") {
        if !v.is_empty() && v != "0" {
            return Some(if v == "1" { None } else { Some(v.into()) });
        }
    }
    const FLAG: &str = "/etc/leandros/nm-mock";
    if let Ok(md) = std::fs::metadata(FLAG) {
        return Some(if md.len() > 0 { Some(FLAG.into()) } else { None });
    }
    if let Ok(c) = std::fs::read_to_string("/proc/cmdline") {
        if c.split_whitespace().any(|w| w == "nmshim.mock" || w == "nmshim.mock=1") {
            return Some(None);
        }
    }
    None
}
