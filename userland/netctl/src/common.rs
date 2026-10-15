//! Shared by `netctl` and `netctltest`: raw syscalls, a stdout formatter,
//! a thin `/dev/netctl` client over `netctl-abi`, and the profile-slot
//! writer protocol (docs/servers/netctl.md, "Profiles file").

#![allow(dead_code)]

use core::fmt::{self, Write};
use leandros_libc::syscall::{nr, syscall3, syscall4, syscall5};
use netctl_abi::profiles::{self as pf, Profile};
use netctl_abi::*;

// ── syscalls (raw: negative errno, never the libc errno global) ────────────

#[cfg(target_arch = "aarch64")] pub const SYS_PPOLL: usize = 73;
#[cfg(target_arch = "x86_64")]  pub const SYS_PPOLL: usize = 271;
#[cfg(target_arch = "aarch64")] pub const SYS_FSYNC: usize = 82;
#[cfg(target_arch = "x86_64")]  pub const SYS_FSYNC: usize = 74;
#[cfg(target_arch = "aarch64")] pub const SYS_GETRANDOM: usize = 278;
#[cfg(target_arch = "x86_64")]  pub const SYS_GETRANDOM: usize = 318;

pub const O_RDONLY: i32 = 0;
pub const O_WRONLY: i32 = 1;
pub const O_RDWR: i32 = 2;
pub const O_TRUNC: i32 = 0o1000;
pub const O_NONBLOCK: i32 = 0o4000;
pub const O_CLOEXEC: i32 = 0o2000000;
pub const AT_FDCWD: isize = -100;

pub const EPERM: i32 = 1;
pub const ENOENT: i32 = 2;
pub const EAGAIN: i32 = 11;
pub const EACCES: i32 = 13;
pub const ENODEV: i32 = 19;
pub const EINVAL: i32 = 22;
pub const ENOTTY: i32 = 25;
pub const EOPNOTSUPP: i32 = 95;

fn res(r: isize) -> Result<usize, i32> { if r < 0 { Err((-r) as i32) } else { Ok(r as usize) } }

/// `path` must end with a NUL.
pub fn sys_open(path: &[u8], flags: i32) -> Result<i32, i32> {
    debug_assert!(path.last() == Some(&0));
    res(unsafe { syscall4(nr::OPENAT, AT_FDCWD as usize, path.as_ptr() as usize, flags as usize, 0) }).map(|v| v as i32)
}
pub fn sys_close(fd: i32) { unsafe { syscall3(nr::CLOSE, fd as usize, 0, 0); } }
pub fn sys_read(fd: i32, buf: &mut [u8]) -> Result<usize, i32> {
    res(unsafe { syscall3(nr::READ, fd as usize, buf.as_mut_ptr() as usize, buf.len()) })
}
pub fn sys_write(fd: i32, buf: &[u8]) -> Result<usize, i32> {
    res(unsafe { syscall3(nr::WRITE, fd as usize, buf.as_ptr() as usize, buf.len()) })
}
pub fn sys_fsync(fd: i32) -> Result<usize, i32> { res(unsafe { syscall3(SYS_FSYNC, fd as usize, 0, 0) }) }
pub fn sys_ioctl(fd: i32, cmd: u32, arg: usize) -> Result<usize, i32> {
    res(unsafe { syscall3(nr::IOCTL, fd as usize, cmd as usize, arg) })
}

#[repr(C)]
pub struct PollFd { pub fd: i32, pub events: i16, pub revents: i16 }
pub const POLLIN: i16 = 1;

/// poll(2) on one fd; `timeout_ms` < 0 waits forever. Returns revents.
pub fn poll1(fd: i32, timeout_ms: i64) -> Result<i16, i32> {
    let mut p = PollFd { fd, events: POLLIN, revents: 0 };
    let ts = [timeout_ms.max(0) / 1000, (timeout_ms.max(0) % 1000) * 1_000_000];
    let tsp = if timeout_ms < 0 { 0 } else { ts.as_ptr() as usize };
    let r = res(unsafe { syscall5(SYS_PPOLL, &mut p as *mut PollFd as usize, 1, tsp, 0, 8) })?;
    Ok(if r == 0 { 0 } else { p.revents })
}

pub fn getrandom(buf: &mut [u8]) -> bool {
    unsafe { syscall3(SYS_GETRANDOM, buf.as_mut_ptr() as usize, buf.len(), 0) == buf.len() as isize }
}

pub fn now_ms() -> u64 {
    let mut ts = leandros_libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { leandros_libc::clock_gettime(1, &mut ts); }
    ts.tv_sec as u64 * 1000 + ts.tv_nsec as u64 / 1_000_000
}

pub fn sleep_ms(ms: u32) { unsafe { leandros_libc::usleep(ms.saturating_mul(1000)); } }

// ── output ──────────────────────────────────────────────────────────────────

pub struct Out;
impl Write for Out {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let mut b = s.as_bytes();
        while !b.is_empty() {
            match sys_write(1, b) { Ok(n) if n > 0 => b = &b[n..], _ => return Err(fmt::Error) }
        }
        Ok(())
    }
}
pub struct Err2;
impl Write for Err2 {
    fn write_str(&mut self, s: &str) -> fmt::Result { let _ = sys_write(2, s.as_bytes()); Ok(()) }
}

#[macro_export]
macro_rules! say { ($($t:tt)*) => {{ use core::fmt::Write; let _ = write!($crate::common::Out, $($t)*); }} }
#[macro_export]
macro_rules! sayln { ($($t:tt)*) => {{ use core::fmt::Write; let _ = writeln!($crate::common::Out, $($t)*); }} }
#[macro_export]
macro_rules! eprintln { ($($t:tt)*) => {{ use core::fmt::Write; let _ = writeln!($crate::common::Err2, $($t)*); }} }

pub fn errname(e: i32) -> &'static str {
    match e {
        1 => "EPERM", 2 => "ENOENT", 5 => "EIO", 9 => "EBADF", 11 => "EAGAIN", 13 => "EACCES",
        14 => "EFAULT", 16 => "EBUSY", 19 => "ENODEV", 22 => "EINVAL", 23 => "ENFILE",
        25 => "ENOTTY", 95 => "EOPNOTSUPP", 100 => "ENETDOWN", _ => "E?",
    }
}

pub struct Mac(pub [u8; 6]);
impl fmt::Display for Mac {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let m = self.0;
        write!(f, "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5])
    }
}
pub struct Ip(pub [u8; 4]);
impl fmt::Display for Ip {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result { write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3]) }
}
/// Bytes as text: printable ASCII as is, everything else `\xNN`.
pub struct Esc<'a>(pub &'a [u8]);
impl fmt::Display for Esc<'_> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        for &c in self.0 {
            if (0x20..0x7f).contains(&c) && c != b'\\' { f.write_char(c as char)?; } else { write!(f, "\\x{:02x}", c)?; }
        }
        Ok(())
    }
}
pub struct Uuid(pub [u8; 16]);
impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        let s = util::uuid_to_str(&self.0);
        f.write_str(core::str::from_utf8(&s).unwrap_or("?"))
    }
}

pub fn kind_name(k: u8) -> &'static str {
    match k { if_kind::LOOPBACK => "loopback", if_kind::ETHERNET => "ethernet", if_kind::WIFI => "wifi", _ => "unknown" }
}
pub fn oper_name(o: u8) -> &'static str {
    match o { oper::DOWN => "down", oper::LOWER_DOWN => "lower-down", oper::DORMANT => "dormant", oper::UP => "up", _ => "unknown" }
}
pub fn ip_state_name(s: u8) -> &'static str {
    match s { ip_state::NONE => "none", ip_state::REQUESTING => "requesting", ip_state::BOUND => "bound", ip_state::STATIC => "static", _ => "?" }
}
pub fn wifi_state_name(s: u8) -> &'static str {
    use wifi_state::*;
    match s {
        UNAVAILABLE => "unavailable", RADIO_OFF => "radio-off", DISCONNECTED => "disconnected",
        CONNECTING => "connecting", AUTHENTICATING => "authenticating", ASSOCIATING => "associating",
        HANDSHAKE => "handshake", CONNECTED => "connected", DISCONNECTING => "disconnecting", FAILED => "failed",
        _ => "?",
    }
}
pub fn reason_name(r: u8) -> &'static str {
    use reason::*;
    match r {
        NONE => "none", WRONG_KEY => "wrong-key", NO_SECRET => "no-secret", NO_AP => "no-ap",
        AUTH_REJECTED => "auth-rejected", ASSOC_REJECTED => "assoc-rejected", TIMEOUT => "timeout",
        FW_ERROR => "fw-error", UNSUPPORTED_SECURITY => "unsupported-security", DEAUTH_BY_AP => "deauth-by-ap",
        BEACON_LOSS => "beacon-loss", USER => "user", RADIO_OFF => "radio-off", RFKILL => "rfkill",
        SUPERSEDED => "superseded", _ => "?",
    }
}
pub fn akm_name(a: u8) -> &'static str {
    match a { akm::AUTO => "auto", akm::NONE => "none", akm::PSK => "psk", akm::PSK_SHA256 => "psk-sha256", akm::SAE => "sae", _ => "?" }
}
pub fn kind_ev_name(k: u8) -> &'static str {
    use netctl_abi::event::kind::*;
    match k {
        SNAPSHOT_BEGIN => "SNAPSHOT_BEGIN", SNAPSHOT_END => "SNAPSHOT_END", RESYNC => "RESYNC",
        IFACE_ADDED => "IFACE_ADDED", IFACE_CHANGED => "IFACE_CHANGED", IFACE_REMOVED => "IFACE_REMOVED",
        IPV4 => "IPV4", WIFI_STATE => "WIFI_STATE", WIFI_SIGNAL => "WIFI_SIGNAL",
        SCAN_STARTED => "SCAN_STARTED", SCAN_DONE => "SCAN_DONE", BSS => "BSS", BSS_GONE => "BSS_GONE",
        PROFILE => "PROFILE", PROFILES_END => "PROFILES_END", _ => "UNKNOWN",
    }
}

pub fn name_of(i: &NcIface) -> &str { core::str::from_utf8(i.name()).unwrap_or("?") }

// ── /dev/netctl client ─────────────────────────────────────────────────────

pub struct Netctl { pub fd: i32 }

impl Drop for Netctl {
    fn drop(&mut self) { sys_close(self.fd); }
}

impl Netctl {
    pub fn open(flags: i32) -> Result<Netctl, i32> {
        sys_open(b"/dev/netctl\0", flags | O_CLOEXEC).map(|fd| Netctl { fd })
    }

    pub fn ioctl<T: Pod>(&self, cmd: u32, v: &mut T) -> Result<usize, i32> {
        sys_ioctl(self.fd, cmd, v as *mut T as usize)
    }

    pub fn version(&self) -> Result<NcVersion, i32> {
        let mut v = NcVersion::new();
        self.ioctl(NETCTL_GET_VERSION, &mut v)?;
        Ok(v)
    }

    /// A list query into `out`; returns (written, count, gen).
    pub fn query<T: Pod>(&self, cmd: u32, ifindex: u32, out: &mut [T]) -> Result<(usize, u32, u32), i32> {
        let mut q = NcQuery::new();
        q.ifindex = ifindex;
        q.elem_size = core::mem::size_of::<T>() as u32;
        q.cap = out.len() as u32;
        q.buf = out.as_mut_ptr() as u64;
        let n = self.ioctl(cmd, &mut q)?;
        Ok((n, q.count, q.gen))
    }

    pub fn ifaces(&self, out: &mut [NcIface; MAX_IFACES]) -> Result<usize, i32> {
        self.query(NETCTL_GET_IFACES, 0, &mut out[..]).map(|r| r.0)
    }

    pub fn reload(&self) -> Result<NcReload, i32> {
        let mut r = NcReload::new();
        self.ioctl(NETCTL_RELOAD_PROFILES, &mut r)?;
        Ok(r)
    }

    /// Resolve an interface argument: a name ("wifi0") or a number.
    pub fn resolve_if(&self, arg: &[u8]) -> Result<u32, i32> {
        if !arg.is_empty() && arg.iter().all(|c| c.is_ascii_digit()) {
            let mut v = 0u32;
            for c in arg { v = v * 10 + (c - b'0') as u32; }
            return Ok(v);
        }
        let mut l = [NcIface::default(); MAX_IFACES];
        let n = self.ifaces(&mut l)?;
        l[..n].iter().find(|i| i.name() == arg).map(|i| i.ifindex).ok_or(ENODEV)
    }

    /// Read records (blocking unless the fd is O_NONBLOCK).
    pub fn read_records(&self, buf: &mut [u8]) -> Result<usize, i32> {
        sys_read(self.fd, buf).map(|n| n / EVENT_RECORD_SIZE)
    }
}

/// Feed every record of `buf[..n*64]` to `r`, calling `f` per completed event.
pub fn for_events(r: &mut Reassembler, buf: &[u8], n: usize, mut f: impl FnMut(EventMeta, Event)) -> u32 {
    let mut errs = 0;
    for rec in records(&buf[..n * EVENT_RECORD_SIZE]) {
        match r.push(&rec) {
            Ok(Some((m, e))) => f(m, e),
            Ok(None) => {}
            Err(_) => errs += 1,
        }
    }
    errs
}

// ── Profile slots (writer protocol) ─────────────────────────────────────────

static mut SLOT_A: [u8; pf::FILE_READ_MAX] = [0; pf::FILE_READ_MAX];
static mut SLOT_B: [u8; pf::FILE_READ_MAX] = [0; pf::FILE_READ_MAX];
static mut SLOT_OUT: [u8; pf::FILE_MAX] = [0; pf::FILE_MAX];

fn read_whole(path: &[u8], buf: &mut [u8]) -> Result<Option<usize>, i32> {
    let fd = match sys_open(path, O_RDONLY | O_CLOEXEC) {
        Ok(fd) => fd,
        Err(ENOENT) => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut n = 0;
    loop {
        if n == buf.len() { break; }
        match sys_read(fd, &mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) => { sys_close(fd); return Err(e); }
        }
    }
    sys_close(fd);
    Ok(Some(n))
}

pub fn slot_path(s: pf::Slot) -> &'static [u8] {
    match s { pf::Slot::A => b"/etc/leandros/network/profiles.a\0", pf::Slot::B => b"/etc/leandros/network/profiles.b\0" }
}

/// Run `f` on the currently loaded (winning) slot's profiles; returns the
/// winner's global flags. Ok(()) with an empty list when there is none.
pub fn with_current<R>(f: impl FnOnce(&mut alloc::vec::Vec<Profile>, u32) -> R) -> Result<R, i32> {
    let (a, b) = unsafe { (&mut *core::ptr::addr_of_mut!(SLOT_A), &mut *core::ptr::addr_of_mut!(SLOT_B)) };
    let na = read_whole(slot_path(pf::Slot::A), a)?;
    let nb = read_whole(slot_path(pf::Slot::B), b)?;
    let da = na.map(|n| &a[..n]);
    let db = nb.map(|n| &b[..n]);
    let mut list = alloc::vec::Vec::new();
    let mut gflags = 0;
    if let Some((_, v)) = pf::select(da, db) {
        gflags = v.global_flags;
        for p in v.profiles() { list.push(p); }
    }
    let r = f(&mut list, gflags);
    wipe(a); wipe(b);
    Ok(r)
}

fn wipe(b: &mut [u8]) { for x in b.iter_mut() { unsafe { core::ptr::write_volatile(x, 0) }; } }

/// Writer protocol: read both slots, seq = winner + 1, write the loser slot
/// (O_WRONLY|O_TRUNC, write, fsync). Returns the slot written and its seq.
/// The caller then issues `NETCTL_RELOAD_PROFILES`.
pub fn write_profiles(list: &[Profile], global_flags: u32) -> Result<(pf::Slot, u64), i32> {
    let (a, b, out) = unsafe {
        (&mut *core::ptr::addr_of_mut!(SLOT_A), &mut *core::ptr::addr_of_mut!(SLOT_B), &mut *core::ptr::addr_of_mut!(SLOT_OUT))
    };
    let na = read_whole(slot_path(pf::Slot::A), a)?;
    let nb = read_whole(slot_path(pf::Slot::B), b)?;
    let (slot, seq) = pf::write_target(na.map(|n| &a[..n]), nb.map(|n| &b[..n]));
    wipe(a); wipe(b);
    let len = pf::serialize(seq, global_flags, list, out).map_err(|_| EINVAL)?;
    let r = write_file(slot_path(slot), &out[..len]);
    wipe(out);
    r.map(|_| (slot, seq))
}

/// Overwrite a slot file with `data` (empty truncates).
pub fn write_file(path: &[u8], data: &[u8]) -> Result<(), i32> {
    let fd = sys_open(path, O_WRONLY | O_TRUNC | O_CLOEXEC)?;
    let mut off = 0;
    while off < data.len() {
        match sys_write(fd, &data[off..]) {
            Ok(0) => { sys_close(fd); return Err(5); }
            Ok(n) => off += n,
            Err(e) => { sys_close(fd); return Err(e); }
        }
    }
    let r = sys_fsync(fd);
    sys_close(fd);
    r.map(|_| ())
}

/// A random (version 4) UUID.
pub fn new_uuid() -> [u8; 16] {
    let mut u = [0u8; 16];
    if !getrandom(&mut u) {
        let t = now_ms();
        for (i, b) in u.iter_mut().enumerate() { *b = (t >> ((i % 8) * 8)) as u8 ^ (i as u8).wrapping_mul(37); }
    }
    u[6] = (u[6] & 0x0F) | 0x40;
    u[8] = (u[8] & 0x3F) | 0x80;
    u
}

/// NUL-terminated argv[i] as a slice.
pub unsafe fn arg<'a>(argv: *const *const u8, i: usize) -> &'a [u8] {
    let p = *argv.add(i);
    let mut n = 0;
    while *p.add(n) != 0 { n += 1; }
    core::slice::from_raw_parts(p, n)
}
