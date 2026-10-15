//! The real backend: `/dev/netctl` (docs/servers/netctl.md, lib/netctl-abi).
//!
//! Opened O_RDWR with the session user's credentials; the kernel refuses that
//! (EACCES) unless the user is root or in group netdev, and then the shim
//! falls back to O_RDONLY and reports itself read-only. Commands are ioctls on
//! the engine's thread. Events are 64-byte records read(2) by a dedicated
//! thread, reassembled into logical events and handed to the engine; every
//! open starts with a full snapshot, and an overflow (RESYNC) is followed by
//! another one, so the engine never has to poll.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use netctl_abi::event::{records, Reassembler};
use netctl_abi::{NcConnect, NcIfReq, NcRadio, NcReload, NcScan, NcVersion, Pod, EVENT_RECORD_SIZE};

use super::backend::{Backend, DirStore, EventTx, ProfileStore};

pub struct NetctlBackend {
    fd: OwnedFd,
    writable: bool,
    features: u32,
    store: DirStore,
}

fn cerr(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

fn open_node(flags: libc::c_int) -> io::Result<OwnedFd> {
    let path = std::ffi::CString::new(netctl_abi::NETCTL_PATH).expect("no NUL");
    // SAFETY: valid NUL-terminated path; the fd is owned from here on.
    let fd = cerr(unsafe { libc::open(path.as_ptr(), flags | libc::O_CLOEXEC) })?;
    // SAFETY: fd was just returned by open(2) and nothing else owns it.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

impl NetctlBackend {
    pub fn open(tx: EventTx) -> io::Result<Self> {
        let (fd, writable) = match open_node(libc::O_RDWR) {
            Ok(fd) => (fd, true),
            Err(e) if matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) => {
                (open_node(libc::O_RDONLY)?, false)
            }
            Err(e) => return Err(e),
        };
        let mut b = NetctlBackend {
            fd,
            writable,
            features: 0,
            store: DirStore { dir: netctl_abi::PROFILES_DIR.into(), create: false },
        };
        let mut v = NcVersion::new();
        b.ioctl(netctl_abi::NETCTL_GET_VERSION, &mut v)?;
        if v.abi_version != netctl_abi::NETCTL_ABI_VERSION {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                format!("netctl ABI {} (shim speaks {})", v.abi_version, netctl_abi::NETCTL_ABI_VERSION),
            ));
        }
        b.features = v.features;
        // The reader gets its own descriptor of the same open: the kernel
        // queues events per open, and the snapshot it queued at open(2) is
        // for this one.
        let rfd = b.fd.try_clone()?;
        std::thread::Builder::new()
            .name("netctl-events".into())
            .spawn(move || reader(rfd, tx))?;
        Ok(b)
    }

    fn ioctl<T: Pod>(&self, cmd: u32, arg: &mut T) -> io::Result<libc::c_int> {
        // SAFETY: `arg` is a valid, exclusively borrowed #[repr(C)] struct of
        // exactly the size encoded in `cmd`.
        cerr(unsafe { libc::ioctl(self.fd.as_raw_fd(), cmd as _, arg as *mut T) })
    }
}

/// Blocking reader: records -> logical events -> engine. Ends when the node
/// errors out (the engine keeps serving its last state).
fn reader(fd: OwnedFd, tx: EventTx) {
    let mut re = Reassembler::new();
    let mut buf = vec![0u8; EVENT_RECORD_SIZE * 64];
    loop {
        // SAFETY: buf is valid for buf.len() bytes.
        let n = unsafe { libc::read(fd.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                Some(libc::EAGAIN) => {
                    let mut p = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                    // SAFETY: one valid pollfd.
                    unsafe { libc::poll(&mut p, 1, 1000) };
                    continue;
                }
                _ => {
                    super::log(format_args!("netctl read: {e}; event stream ended"));
                    return;
                }
            }
        }
        if n == 0 {
            super::log(format_args!("netctl read: EOF; event stream ended"));
            return;
        }
        for rec in records(&buf[..n as usize]) {
            match re.push(&rec) {
                Ok(Some((_meta, ev))) => {
                    if tx.send(ev).is_err() {
                        return;
                    }
                }
                Ok(None) => {}
                Err(e) => super::log(format_args!("netctl: dropped record ({e:?})")),
            }
        }
    }
}

impl Backend for NetctlBackend {
    fn name(&self) -> &'static str {
        "netctl"
    }
    fn writable(&self) -> bool {
        self.writable
    }
    fn features(&self) -> u32 {
        self.features
    }
    fn scan(&mut self, ifindex: u32) -> io::Result<()> {
        let mut s = NcScan::new();
        s.ifindex = ifindex;
        self.ioctl(netctl_abi::NETCTL_SCAN, &mut s).map(drop)
    }
    fn connect(&mut self, req: &mut NcConnect) -> io::Result<u32> {
        req.size = NcConnect::SIZE as u32;
        self.ioctl(netctl_abi::NETCTL_CONNECT, req)?;
        Ok(req.attempt)
    }
    fn disconnect(&mut self, ifindex: u32) -> io::Result<()> {
        let mut r = NcIfReq::new();
        r.ifindex = ifindex;
        self.ioctl(netctl_abi::NETCTL_DISCONNECT, &mut r).map(drop)
    }
    fn set_radio(&mut self, ifindex: u32, on: bool) -> io::Result<()> {
        let mut r = NcRadio::new();
        r.ifindex = ifindex;
        r.on = on as u32;
        self.ioctl(netctl_abi::NETCTL_SET_RADIO, &mut r).map(drop)
    }
    fn reload_profiles(&mut self) -> io::Result<NcReload> {
        let mut r = NcReload::new();
        self.ioctl(netctl_abi::NETCTL_RELOAD_PROFILES, &mut r)?;
        Ok(r)
    }
    fn store(&self) -> &dyn ProfileStore {
        &self.store
    }
}
