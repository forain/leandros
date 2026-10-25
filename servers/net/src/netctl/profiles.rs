//! Loading saved connections: `/etc/leandros/network/profiles.{a,b}` plus
//! the legacy `/etc/leandros/wifi.conf` / `wifi.ssid=` credentials.
//!
//! The kernel only reads. Userland (a `netdev` member) writes a slot and
//! then issues `NETCTL_RELOAD_PROFILES`. The format, the slot selection and
//! the legacy rules are `netctl_abi::profiles` (docs/servers/netctl.md,
//! "Profiles file"); this file only does the I/O and the merge.
//!
//! Reads go through `drivers::firmware` (the kernel's raw read of the real
//! root), in the caller's task: at boot that is the `[netctl]` kthread (root),
//! on reload the ioctl caller (root or a netdev member, who may read the
//! `root:netdev 0660` slots). Secrets are never logged.

use alloc::vec::Vec;
use drivers::firmware::{request_firmware, FwError};
use netctl_abi::profiles::{self as pf, Profile};
use netctl_abi::*;

/// `global_flags` bit: Wi-Fi soft-off at boot.
pub const GLOBAL_RADIO_OFF: u32 = pf::global_flags::WIFI_RADIO_OFF;

/// What one load produced.
pub struct Loaded {
    /// Stored profiles then the legacy one (if any), secrets included.
    pub list: Vec<Profile>,
    /// How many of `list` are legacy (always at the end).
    pub n_legacy: usize,
    pub source: u8,
    pub status: u8,
    pub global_flags: u32,
    pub seq: u64,
}

enum Read {
    Missing,
    Data(Vec<u8>),
    Error,
}

fn read_file(path: &str, max: usize) -> Read {
    match request_firmware(path, max) {
        Ok(f) => match f.to_vec() { Some(v) => Read::Data(v), None => Read::Error },
        Err(FwError::Fs(-2)) => Read::Missing,
        // No root filesystem (yet, or at all): nothing stored to read.
        Err(FwError::NoRoot) => Read::Missing,
        // A slot larger than any valid one is corrupt, not an I/O problem.
        Err(FwError::TooBig) => Read::Data(Vec::new()),
        Err(_) => Read::Error,
    }
}

/// `%XX` escapes (so a cmdline value can carry spaces), as iwlwifi accepts.
fn percent_decode(s: &str) -> Vec<u8> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let hex = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push((h << 4) | l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    out
}

fn cmdline_val<'a>(cmdline: &'a str, key: &str) -> Option<&'a str> {
    cmdline.split_ascii_whitespace()
        .filter_map(|t| t.strip_prefix(key).and_then(|r| r.strip_prefix('=')))
        .last()
}

/// The legacy profile: `/etc/leandros/wifi.conf` wins over the cmdline, as
/// before netctl. `None` when neither names a usable network.
fn legacy(cmdline: &str) -> Option<Profile> {
    if let Read::Data(v) = read_file(LEGACY_WIFI_CONF, 64 * 1024) {
        if let Some((ssid, psk)) = pf::parse_legacy_wifi_conf(&v) {
            return pf::legacy_profile(ssid, psk);
        }
    }
    let ssid = percent_decode(cmdline_val(cmdline, "wifi.ssid")?);
    // The kernel scrubs wifi.psk= from the stored cmdline at boot and parks
    // it in drivers::cmdline_secrets (the same source iwlwifi uses).
    let psk = match drivers::cmdline_secrets::peek("wifi.psk") {
        Some(v) => percent_decode(core::str::from_utf8(&v).unwrap_or("")),
        None => cmdline_val(cmdline, "wifi.psk").map(percent_decode).unwrap_or_default(),
    };
    let p = pf::legacy_profile(&ssid, &psk);
    // Our copies of the secret: wipe them (the profile keeps its own).
    let mut psk = psk;
    for b in psk.iter_mut() { unsafe { core::ptr::write_volatile(b, 0) }; }
    p
}

/// Read both slots and the legacy credentials. `Err(())` on an I/O error
/// (the caller keeps its previous list and reports `IO_ERROR`).
pub fn load(cmdline: &str) -> Result<Loaded, ()> {
    let a = read_file(PROFILES_SLOT_A, pf::FILE_READ_MAX);
    let b = read_file(PROFILES_SLOT_B, pf::FILE_READ_MAX);
    if matches!(a, Read::Error) || matches!(b, Read::Error) { return Err(()); }
    let da = if let Read::Data(ref v) = a { Some(v.as_slice()) } else { None };
    let db = if let Read::Data(ref v) = b { Some(v.as_slice()) } else { None };

    let mut list: Vec<Profile> = Vec::new();
    let (source, status, global_flags, seq) = match pf::select(da, db) {
        Some((slot, view)) => {
            for p in view.profiles() {
                if list.len() >= MAX_PROFILES { break; }
                list.push(p);
            }
            (slot.source(), profiles_status::OK, view.global_flags, view.seq)
        }
        None if da.is_none() && db.is_none() => (profiles_source::NONE, profiles_status::NO_FILE, 0, 0),
        // An empty pre-created slot (mkfs) is "no profiles yet", not corrupt.
        None if da.map_or(true, |d| d.is_empty()) && db.map_or(true, |d| d.is_empty()) =>
            (profiles_source::NONE, profiles_status::NO_FILE, 0, 0),
        None => (profiles_source::NONE, profiles_status::CORRUPT, 0, 0),
    };
    // Wipe the raw slot images: they hold every secret.
    for v in [a, b] {
        if let Read::Data(mut v) = v {
            for x in v.iter_mut() { unsafe { core::ptr::write_volatile(x, 0) }; }
        }
    }

    let mut n_legacy = 0;
    if list.len() < MAX_PROFILES {
        if let Some(l) = legacy(cmdline) {
            if !list.iter().any(|p| p.kind == if_kind::WIFI && p.ssid() == l.ssid()) {
                list.push(l);
                n_legacy = 1;
            }
        }
    }
    Ok(Loaded { list, n_legacy, source, status, global_flags, seq })
}
