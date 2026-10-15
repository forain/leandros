//! The `/dev/netctl` contract (ABI version 1).
//!
//! `/dev/netctl` is the one control node for network interfaces and Wi-Fi.
//! Commands are ioctls on #[repr(C)] structs ([`types`]); state arrives as a
//! stream of fixed 64-byte event records read(2) from the node ([`event`]);
//! saved connections live in an A/B-slotted file that userland writes and the
//! kernel reads ([`profiles`]). The prose specification, which this crate
//! implements byte for byte, is `docs/servers/netctl.md`.
//!
//! Wire rules shared by every type here:
//! * Native endianness. Both LeandrOS targets (x86_64, aarch64) are
//!   little-endian; the profiles *file* is explicitly little-endian.
//! * Every struct is `#[repr(C)]`, made only of integers and integer arrays,
//!   with every padding byte an explicit field, so any byte pattern is a valid
//!   value ([`Pod`]) and `as_bytes` never exposes uninitialised memory.
//! * Command structs start with `size: u32` (the caller's `size_of`). The
//!   kernel copies `min(size, its own)` and zero-fills the rest, so a struct
//!   may only ever grow at its end.
//! * Enumerations travel as plain integers (`u8`); unknown values must be
//!   tolerated by readers, never trusted.

#![no_std]

pub mod event;
pub mod profiles;
pub mod types;
pub mod util;

mod crc32;

pub use crc32::crc32;
pub use event::*;
pub use types::*;

/// ABI version reported by `NETCTL_GET_VERSION` and in `SNAPSHOT_BEGIN`.
/// Bumped only for incompatible changes; growth (new ioctls, new event kinds,
/// fields appended to structs) keeps the version.
pub const NETCTL_ABI_VERSION: u32 = 1;

// ── The node ────────────────────────────────────────────────────────────────

/// Path of the control node.
pub const NETCTL_PATH: &str = "/dev/netctl";
/// Character device major (Linux "misc").
pub const NETCTL_MAJOR: u32 = 10;
/// Character device minor (free in Linux's misc range).
pub const NETCTL_MINOR: u32 = 242;
/// Owner, group and mode of the node: `root:netdev 0664`.
pub const NETCTL_UID: u32 = 0;
pub const NETCTL_MODE: u32 = 0o664;

/// The `netdev` group: members (and root) may open the node for writing and
/// therefore manage the network. Everyone else gets the read-only view.
pub const GID_NETDEV: u32 = 108;
pub const NETDEV_GROUP_NAME: &str = "netdev";

// ── Interfaces and routing ─────────────────────────────────────────────────

/// ifindex of the loopback interface. Real interfaces start at 2 and are
/// numbered in registration order; an ifindex is never reused within a boot.
pub const IFINDEX_LO: u32 = 1;
/// Upper bound on interfaces (including `lo`) reported by the kernel.
pub const MAX_IFACES: usize = 8;
/// Interface name buffer (NUL-padded, Linux IFNAMSIZ).
pub const IFNAMSIZ: usize = 16;

/// Default-route metrics (NetworkManager's defaults): lower wins.
pub const METRIC_WIRED: u32 = 100;
pub const METRIC_WIFI: u32 = 600;

// ── Wi-Fi limits ────────────────────────────────────────────────────────────

/// 802.11 SSID: 0..=32 arbitrary bytes (not necessarily UTF-8).
pub const SSID_MAX: usize = 32;
/// Secret buffer: a WPA passphrase (8..=63 bytes), a raw PSK as 64 hex
/// digits, or (WPA3, later) an SAE password of up to 128 bytes.
pub const SECRET_MAX: usize = 128;
/// Profile display name (NM `connection.id`), UTF-8 bytes.
pub const PROFILE_ID_MAX: usize = 64;
/// Upper bound on cached BSS entries, kernel-wide (all Wi-Fi interfaces).
pub const MAX_BSS: usize = 128;
/// Upper bound on profiles (stored + legacy).
pub const MAX_PROFILES: usize = 64;
/// Directed-probe SSIDs per scan request.
pub const SCAN_MAX_SSIDS: usize = 4;

/// A BSS not seen by any scan for this long is aged out (`BSS_GONE`).
pub const BSS_AGE_OUT_MS: u32 = 120_000;
/// Scan requests within this window of the last completed scan are coalesced
/// (answered with the cached results and a `SCAN_DONE` flagged COALESCED).
pub const SCAN_COALESCE_MS: u32 = 10_000;
/// `DISCONNECT` must reach `WIFI_DISCONNECTED` well within this (nmrs waits
/// 10 s for it before every connect).
pub const DISCONNECT_DEADLINE_MS: u32 = 2_000;
/// Overall bound of one connect attempt up to `WIFI_CONNECTED` (auth, assoc,
/// 4-way); DHCP comes on top. NM clients give up after 30 s.
pub const CONNECT_DEADLINE_MS: u32 = 15_000;

/// Records each open's event queue holds before it overflows (64 KiB). It is
/// larger than the biggest possible snapshot (see docs/servers/netctl.md).
pub const EVENT_QUEUE_RECORDS: usize = 1024;

// ── Paths ───────────────────────────────────────────────────────────────────

/// Directory of the profile slots, `root:netdev 0750`.
pub const PROFILES_DIR: &str = "/etc/leandros/network";
/// Slot files, `root:netdev 0660`. See [`profiles`].
pub const PROFILES_SLOT_A: &str = "/etc/leandros/network/profiles.a";
pub const PROFILES_SLOT_B: &str = "/etc/leandros/network/profiles.b";
/// Legacy single-network credentials (still honoured, read-only).
pub const LEGACY_WIFI_CONF: &str = "/etc/leandros/wifi.conf";

// ── ioctl numbers ───────────────────────────────────────────────────────────

/// ioctl "type" byte (`_IOC_TYPE`).
pub const NETCTL_IOC_TYPE: u8 = b'N';

pub const IOC_NONE: u32 = 0;
pub const IOC_WRITE: u32 = 1;
pub const IOC_READ: u32 = 2;

/// Linux's generic `_IOC(dir, type, nr, size)` (identical on x86_64 and
/// arm64): nr bits 0-7, type 8-15, size 16-29, direction 30-31.
pub const fn ioc(dir: u32, nr: u8, size: usize) -> u32 {
    (dir << 30) | (((size as u32) & 0x3FFF) << 16) | ((NETCTL_IOC_TYPE as u32) << 8) | nr as u32
}
/// `_IOC_NR`.
pub const fn ioc_nr(cmd: u32) -> u8 { cmd as u8 }
/// `_IOC_TYPE`.
pub const fn ioc_type(cmd: u32) -> u8 { (cmd >> 8) as u8 }
/// `_IOC_SIZE`.
pub const fn ioc_size(cmd: u32) -> usize { ((cmd >> 16) & 0x3FFF) as usize }
/// `_IOC_DIR`.
pub const fn ioc_dir(cmd: u32) -> u32 { cmd >> 30 }

use core::mem::size_of;

/// `_IOR('N', 0x00, NcVersion)` — ABI version and limits. Any open.
pub const NETCTL_GET_VERSION: u32 = ioc(IOC_READ, 0x00, size_of::<NcVersion>());
/// `_IOWR('N', 0x01, NcQuery)` — interface list → `NcIface[]`. Any open.
pub const NETCTL_GET_IFACES: u32 = ioc(IOC_READ | IOC_WRITE, 0x01, size_of::<NcQuery>());
/// `_IOWR('N', 0x02, NcQuery)` — IPv4 configuration → `NcIpv4[]`. Any open.
pub const NETCTL_GET_IPV4: u32 = ioc(IOC_READ | IOC_WRITE, 0x02, size_of::<NcQuery>());
/// `_IOWR('N', 0x03, NcQuery)` — Wi-Fi link state → `NcWifiState[]`. Any open.
pub const NETCTL_GET_WIFI: u32 = ioc(IOC_READ | IOC_WRITE, 0x03, size_of::<NcQuery>());
/// `_IOWR('N', 0x04, NcQuery)` — cached scan results → `NcBss[]`. Any open.
pub const NETCTL_GET_BSS: u32 = ioc(IOC_READ | IOC_WRITE, 0x04, size_of::<NcQuery>());
/// `_IOWR('N', 0x05, NcQuery)` — profile metadata (no secrets) →
/// `NcProfileMeta[]`. Any open.
pub const NETCTL_GET_PROFILES: u32 = ioc(IOC_READ | IOC_WRITE, 0x05, size_of::<NcQuery>());
/// `_IO('N', 0x06)` — queue a fresh snapshot (reason REQUESTED) on this open
/// only. Any open.
pub const NETCTL_REQUEST_SNAPSHOT: u32 = ioc(IOC_NONE, 0x06, 0);

/// First ioctl nr that mutates state. Every nr >= this needs a write-capable
/// open and answers `EPERM` on a read-only one.
pub const NETCTL_NR_FIRST_MUTATING: u8 = 0x20;

/// `_IOW('N', 0x20, NcScan)` — start (or join, or coalesce) a scan. W.
pub const NETCTL_SCAN: u32 = ioc(IOC_WRITE, 0x20, size_of::<NcScan>());
/// `_IOWR('N', 0x21, NcConnect)` — connect; returns the attempt id. W.
pub const NETCTL_CONNECT: u32 = ioc(IOC_READ | IOC_WRITE, 0x21, size_of::<NcConnect>());
/// `_IOW('N', 0x22, NcIfReq)` — disconnect (Wi-Fi) / unmanage (wired). W.
pub const NETCTL_DISCONNECT: u32 = ioc(IOC_WRITE, 0x22, size_of::<NcIfReq>());
/// `_IOW('N', 0x23, NcRadio)` — soft radio on/off. W.
pub const NETCTL_SET_RADIO: u32 = ioc(IOC_WRITE, 0x23, size_of::<NcRadio>());
/// `_IOWR('N', 0x24, NcReload)` — re-read the profile slots, synchronously. W.
pub const NETCTL_RELOAD_PROFILES: u32 = ioc(IOC_READ | IOC_WRITE, 0x24, size_of::<NcReload>());

/// True for the ioctls a read-only open may not issue. The kernel dispatches
/// on (type, nr) only; the size and direction bits are informational, the
/// struct's own `size` field governs the copy.
pub const fn is_mutating(cmd: u32) -> bool {
    ioc_type(cmd) == NETCTL_IOC_TYPE && ioc_nr(cmd) >= NETCTL_NR_FIRST_MUTATING
}

/// True when `cmd` is one of ours (type 'N').
pub const fn is_netctl_ioctl(cmd: u32) -> bool { ioc_type(cmd) == NETCTL_IOC_TYPE }

// ── errno values the node answers (Linux numbers, positive) ────────────────

pub mod errno {
    /// Mutating ioctl on a read-only open.
    pub const EPERM: i32 = 1;
    /// Profile uuid not loaded.
    pub const ENOENT: i32 = 2;
    /// Bad user pointer.
    pub const EFAULT: i32 = 14;
    /// Unknown ifindex, or the interface is of the wrong kind for the command.
    pub const ENODEV: i32 = 19;
    /// Malformed struct (size too small, lengths out of range, bad enum).
    pub const EINVAL: i32 = 22;
    /// Unknown ioctl.
    pub const ENOTTY: i32 = 25;
    /// Empty queue on a non-blocking read.
    pub const EAGAIN: i32 = 11;
    /// open(O_WRONLY/O_RDWR) without root or netdev.
    pub const EACCES: i32 = 13;
    /// AKM or feature this hardware/build cannot do (e.g. SAE before WPA3).
    pub const EOPNOTSUPP: i32 = 95;
    /// Radio off or firmware not ready.
    pub const ENETDOWN: i32 = 100;
}

// ── Plain-old-data helpers ─────────────────────────────────────────────────

/// A type whose every byte pattern is a valid value and which has no
/// implicit padding: integers and integer arrays in `#[repr(C)]`, with every
/// gap spelled out as a field.
///
/// # Safety
/// Implement only for such types. Every type in this crate is checked by a
/// size assertion and by the offset tests in `tests.rs`.
pub unsafe trait Pod: Copy + 'static {
    /// The all-zero value.
    fn zeroed() -> Self {
        // SAFETY: Pod types accept every byte pattern, including all zeros.
        unsafe { core::mem::zeroed() }
    }
    /// The value's bytes, exactly as they travel.
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: no padding, so every byte is initialised.
        unsafe { core::slice::from_raw_parts(self as *const Self as *const u8, size_of::<Self>()) }
    }
    /// Mutable view of the value's bytes.
    fn as_bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and every byte pattern written is a valid value.
        unsafe { core::slice::from_raw_parts_mut(self as *mut Self as *mut u8, size_of::<Self>()) }
    }
    /// Read a value from the first `size_of::<Self>()` bytes; `None` if short.
    fn read_from(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < size_of::<Self>() { return None; }
        // SAFETY: length checked; unaligned read of a Pod type.
        Some(unsafe { core::ptr::read_unaligned(bytes.as_ptr() as *const Self) })
    }
    /// Read a value from a prefix of any length: missing bytes are zero,
    /// extra bytes ignored. This is the "copy min(size, ours), zero-fill the
    /// rest" rule for growing structs.
    fn read_prefix(bytes: &[u8]) -> Self {
        let mut v = Self::zeroed();
        let n = bytes.len().min(size_of::<Self>());
        v.as_bytes_mut()[..n].copy_from_slice(&bytes[..n]);
        v
    }
}

#[cfg(test)]
mod tests;
