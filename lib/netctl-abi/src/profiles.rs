//! Saved connections: `/etc/leandros/network/profiles.{a,b}`.
//!
//! Two slot files, `root:netdev 0660`. Userland (a netdev member: the NM shim
//! or the `netctl` CLI) writes; the kernel only reads, at boot and on
//! `NETCTL_RELOAD_PROFILES`. Each slot is a complete, self-checking image:
//!
//! ```text
//! offset size  field (little-endian)
//!      0    8  magic "LNDNETP\x01"
//!      8    2  version (1)
//!     10    2  header_size (>= 64; entries start here)
//!     12    2  entry_size (>= 288; 320 written by v1)
//!     14    2  count (<= 64)
//!     16    8  seq (writer's generation; the larger valid one wins)
//!     24    4  global_flags (bit 0: Wi-Fi radio off at boot)
//!     28   36  reserved (zero)
//!     64    …  count × entry_size entries
//!      …    4  crc32 (CRC-32/ISO-HDLC of every byte before it)
//! ```
//!
//! A slot is **valid** when the magic and version match, the geometry is
//! sane, the file holds at least `header_size + count*entry_size + 4` bytes
//! and the CRC matches; bytes after the CRC are ignored. The kernel uses the
//! valid slot with the larger `seq` (A on a tie). A torn or corrupt write
//! therefore leaves the previous slot in charge.
//!
//! Entry (v1, 320 bytes; readers use the first 288 and skip the rest):
//!
//! ```text
//!   0  16  uuid (non-nil)
//!  16   1  kind (if_kind: 3 wifi; 2 ethernet reserved)
//!  17   1  akm
//!  18   1  autoconnect (0/1)
//!  19   1  flags (bit 0 HIDDEN)
//!  20   4  priority (i32, higher first)
//!  24   1  ssid_len (1..=32 for wifi)
//!  25   1  id_len (0..=64)
//!  26   2  secret_len (0..=128)
//!  28   1  pmf
//!  29   3  reserved
//!  32   8  timestamp_s (u64, last successful connect, 0 = never)
//!  40  16  iface (NUL-padded; empty = any)
//!  56   8  reserved
//!  64  32  ssid
//!  96  64  id (UTF-8 display name)
//! 160 128  secret (passphrase / 64-hex PSK / SAE password)
//! 288  32  reserved (v1 writers zero it)
//! ```
//!
//! An entry with an unknown kind or akm, out-of-range lengths, a nil uuid or
//! a uuid already seen earlier in the slot is skipped, not fatal (forward
//! compatibility; the CRC already proved the writer meant it).
//!
//! Writers: read both slots, take `seq = max(valid seqs) + 1`, write the
//! slot that is *not* the current winner (open O_WRONLY|O_TRUNC, write,
//! fsync, close), then issue `NETCTL_RELOAD_PROFILES`. See
//! [`write_target`].

use crate::types::{akm, if_kind, profile_flags, NcProfileMeta};
use crate::util::secret_valid_for;
use crate::{crc32, IFNAMSIZ, MAX_PROFILES, PROFILE_ID_MAX, SECRET_MAX, SSID_MAX};

pub const MAGIC: [u8; 8] = *b"LNDNETP\x01";
pub const VERSION: u16 = 1;
pub const HEADER_SIZE: usize = 64;
/// Entry size v1 writers use.
pub const ENTRY_SIZE: usize = 320;
/// Bytes of an entry a v1 reader interprets (entries must be at least this).
pub const ENTRY_MIN: usize = 288;
pub const CRC_SIZE: usize = 4;
/// Largest valid v1 file.
pub const FILE_MAX: usize = HEADER_SIZE + MAX_PROFILES * ENTRY_SIZE + CRC_SIZE;
/// Upper bound for readers that accept newer geometry (header/entry growth).
pub const FILE_READ_MAX: usize = 64 * 1024;

/// `global_flags` bits.
pub mod global_flags {
    /// Bring Wi-Fi up with the soft radio off (NM `WirelessEnabled=false`).
    pub const WIFI_RADIO_OFF: u32 = 1 << 0;
}

/// Which slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot { A, B }

impl Slot {
    pub fn path(self) -> &'static str {
        match self { Slot::A => crate::PROFILES_SLOT_A, Slot::B => crate::PROFILES_SLOT_B }
    }
    pub fn other(self) -> Slot { match self { Slot::A => Slot::B, Slot::B => Slot::A } }
    /// The `profiles_source` value.
    pub fn source(self) -> u8 {
        match self { Slot::A => crate::types::profiles_source::SLOT_A, Slot::B => crate::types::profiles_source::SLOT_B }
    }
}

/// Why a slot is invalid.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlotError {
    /// Shorter than a header.
    TooShort,
    BadMagic,
    BadVersion,
    /// header_size < 64, entry_size < 288, or count > MAX_PROFILES.
    BadGeometry,
    /// Fewer bytes than the header announces (torn write).
    Truncated,
    BadCrc,
}

/// Why an entry was skipped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryError {
    NilUuid,
    DuplicateUuid,
    UnknownKind,
    UnknownAkm,
    BadSsidLen,
    BadIdLen,
    BadSecret,
}

/// Why serialization failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SerError {
    TooMany,
    BufferTooSmall,
    Invalid(EntryError),
}

/// One saved connection, secret included. The secret is wiped on drop.
#[derive(Clone, PartialEq, Eq)]
pub struct Profile {
    pub uuid: [u8; 16],
    pub kind: u8,
    pub akm: u8,
    pub autoconnect: bool,
    /// Stored flags ([`profile_flags::STORED_MASK`]).
    pub flags: u8,
    pub priority: i32,
    pub pmf: u8,
    pub timestamp_s: u64,
    pub iface: [u8; IFNAMSIZ],
    pub ssid_len: u8,
    pub ssid: [u8; SSID_MAX],
    pub id_len: u8,
    pub id: [u8; PROFILE_ID_MAX],
    pub secret_len: u16,
    pub secret: [u8; SECRET_MAX],
}

impl core::fmt::Debug for Profile {
    // Never prints the secret.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Profile")
            .field("uuid", &self.uuid)
            .field("kind", &self.kind)
            .field("akm", &self.akm)
            .field("autoconnect", &self.autoconnect)
            .field("flags", &self.flags)
            .field("priority", &self.priority)
            .field("ssid", &self.ssid())
            .field("id", &self.id())
            .field("secret_len", &self.secret_len)
            .finish()
    }
}

impl Drop for Profile {
    fn drop(&mut self) { self.wipe_secret(); }
}

impl Profile {
    /// A Wi-Fi profile with defaults (autoconnect on, priority 0, id = SSID).
    /// `None` when the SSID or secret is out of range for `akm`.
    pub fn new_wifi(uuid: [u8; 16], ssid: &[u8], akm_: u8, secret: &[u8]) -> Option<Profile> {
        if ssid.is_empty() || ssid.len() > SSID_MAX || secret.len() > SECRET_MAX { return None; }
        let mut p = Profile {
            uuid, kind: if_kind::WIFI, akm: akm_, autoconnect: true, flags: 0, priority: 0,
            pmf: crate::types::pmf::DEFAULT, timestamp_s: 0, iface: [0; IFNAMSIZ],
            ssid_len: ssid.len() as u8, ssid: [0; SSID_MAX], id_len: 0, id: [0; PROFILE_ID_MAX],
            secret_len: secret.len() as u16, secret: [0; SECRET_MAX],
        };
        p.ssid[..ssid.len()].copy_from_slice(ssid);
        p.secret[..secret.len()].copy_from_slice(secret);
        let n = ssid.len().min(PROFILE_ID_MAX);
        p.id[..n].copy_from_slice(&ssid[..n]);
        p.id_len = n as u8;
        match p.validate() { Ok(()) => Some(p), Err(_) => None }
    }

    pub fn ssid(&self) -> &[u8] { &self.ssid[..(self.ssid_len as usize).min(SSID_MAX)] }
    pub fn id(&self) -> &[u8] { &self.id[..(self.id_len as usize).min(PROFILE_ID_MAX)] }
    pub fn secret(&self) -> &[u8] { &self.secret[..(self.secret_len as usize).min(SECRET_MAX)] }
    pub fn iface(&self) -> &[u8] { crate::util::cstr(&self.iface) }
    pub fn hidden(&self) -> bool { self.flags & profile_flags::HIDDEN != 0 }

    /// Set the display name (truncated to 64 bytes).
    pub fn set_id(&mut self, id: &[u8]) {
        let n = id.len().min(PROFILE_ID_MAX);
        self.id = [0; PROFILE_ID_MAX];
        self.id[..n].copy_from_slice(&id[..n]);
        self.id_len = n as u8;
    }

    /// Overwrite the secret bytes.
    pub fn wipe_secret(&mut self) {
        for b in self.secret.iter_mut() {
            // SAFETY: plain byte store; volatile so it is not elided.
            unsafe { core::ptr::write_volatile(b, 0) };
        }
        self.secret_len = 0;
    }

    /// The rules every stored entry must meet (uuid uniqueness is checked
    /// per slot by the reader).
    pub fn validate(&self) -> Result<(), EntryError> {
        if crate::util::uuid_is_nil(&self.uuid) { return Err(EntryError::NilUuid); }
        if self.akm > akm::MAX { return Err(EntryError::UnknownAkm); }
        match self.kind {
            if_kind::WIFI => {
                if self.ssid_len == 0 || self.ssid_len as usize > SSID_MAX { return Err(EntryError::BadSsidLen); }
            }
            if_kind::ETHERNET => {
                if self.ssid_len as usize > SSID_MAX { return Err(EntryError::BadSsidLen); }
            }
            _ => return Err(EntryError::UnknownKind),
        }
        if self.id_len as usize > PROFILE_ID_MAX { return Err(EntryError::BadIdLen); }
        if self.secret_len as usize > SECRET_MAX || !secret_valid_for(self.akm, self.secret()) {
            return Err(EntryError::BadSecret);
        }
        Ok(())
    }

    /// Secret-free metadata as `/dev/netctl` reports it. `extra_flags` adds
    /// kernel-side flags (LEGACY, BLOCKED_WRONG_KEY).
    pub fn meta(&self, gen: u32, extra_flags: u8) -> NcProfileMeta {
        let mut m = NcProfileMeta::default();
        m.uuid = self.uuid;
        m.kind = self.kind;
        m.akm = self.akm;
        m.autoconnect = self.autoconnect as u8;
        m.flags = (self.flags & profile_flags::STORED_MASK) | extra_flags
            | if self.secret_len > 0 { profile_flags::HAS_SECRET } else { 0 };
        m.priority = self.priority;
        m.gen = gen;
        m.ssid_len = self.ssid_len;
        m.ssid = self.ssid;
        m.id_len = self.id_len;
        m.id = self.id;
        m.pmf = self.pmf;
        m.iface = self.iface;
        m.timestamp_s = self.timestamp_s;
        m
    }

    /// Decode one entry (the first ENTRY_MIN bytes of `e`). Duplicate uuids
    /// are not checked here.
    pub fn decode(e: &[u8]) -> Result<Profile, EntryError> {
        debug_assert!(e.len() >= ENTRY_MIN);
        let mut p = Profile {
            uuid: arr(&e[0..16]),
            kind: e[16],
            akm: e[17],
            autoconnect: e[18] != 0,
            flags: e[19] & profile_flags::STORED_MASK,
            priority: i32::from_le_bytes(arr(&e[20..24])),
            ssid_len: e[24],
            id_len: e[25],
            secret_len: u16::from_le_bytes(arr(&e[26..28])),
            pmf: e[28],
            timestamp_s: u64::from_le_bytes(arr(&e[32..40])),
            iface: arr(&e[40..56]),
            ssid: arr(&e[64..96]),
            id: arr(&e[96..160]),
            secret: [0; SECRET_MAX],
        };
        if p.secret_len as usize > SECRET_MAX {
            p.secret_len = 0;
            return Err(EntryError::BadSecret);
        }
        p.secret.copy_from_slice(&e[160..288]);
        p.validate()?;
        Ok(p)
    }

    /// Encode into an ENTRY_SIZE buffer (reserved bytes zero).
    pub fn encode(&self, e: &mut [u8]) {
        e[..ENTRY_SIZE].fill(0);
        e[0..16].copy_from_slice(&self.uuid);
        e[16] = self.kind;
        e[17] = self.akm;
        e[18] = self.autoconnect as u8;
        e[19] = self.flags & profile_flags::STORED_MASK;
        e[20..24].copy_from_slice(&self.priority.to_le_bytes());
        e[24] = self.ssid_len;
        e[25] = self.id_len;
        e[26..28].copy_from_slice(&self.secret_len.to_le_bytes());
        e[28] = self.pmf;
        e[32..40].copy_from_slice(&self.timestamp_s.to_le_bytes());
        e[40..56].copy_from_slice(&self.iface);
        e[64..64 + SSID_MAX].copy_from_slice(&self.ssid);
        // Only the used prefix of the variable fields is written, so stale
        // bytes behind a shortened value never reach the file.
        e[96..96 + self.id().len()].copy_from_slice(self.id());
        e[160..160 + self.secret().len()].copy_from_slice(self.secret());
        e[64 + self.ssid().len()..96].fill(0);
    }
}

fn arr<const N: usize>(s: &[u8]) -> [u8; N] {
    let mut a = [0u8; N];
    a.copy_from_slice(&s[..N]);
    a
}

/// A validated slot, borrowing the file bytes.
#[derive(Clone, Copy, Debug)]
pub struct SlotView<'a> {
    pub seq: u64,
    pub global_flags: u32,
    pub count: usize,
    entry_size: usize,
    entries: &'a [u8],
}

/// Validate a slot image.
pub fn parse_slot(buf: &[u8]) -> Result<SlotView<'_>, SlotError> {
    if buf.len() < HEADER_SIZE + CRC_SIZE { return Err(SlotError::TooShort); }
    if buf[0..8] != MAGIC { return Err(SlotError::BadMagic); }
    if u16::from_le_bytes(arr(&buf[8..10])) != VERSION { return Err(SlotError::BadVersion); }
    let header_size = u16::from_le_bytes(arr(&buf[10..12])) as usize;
    let entry_size = u16::from_le_bytes(arr(&buf[12..14])) as usize;
    let count = u16::from_le_bytes(arr(&buf[14..16])) as usize;
    if header_size < HEADER_SIZE || entry_size < ENTRY_MIN || count > MAX_PROFILES {
        return Err(SlotError::BadGeometry);
    }
    let body = header_size + count * entry_size;
    if buf.len() < body + CRC_SIZE { return Err(SlotError::Truncated); }
    let want = u32::from_le_bytes(arr(&buf[body..body + CRC_SIZE]));
    if crc32(&buf[..body]) != want { return Err(SlotError::BadCrc); }
    Ok(SlotView {
        seq: u64::from_le_bytes(arr(&buf[16..24])),
        global_flags: u32::from_le_bytes(arr(&buf[24..28])),
        count,
        entry_size,
        entries: &buf[header_size..body],
    })
}

impl<'a> SlotView<'a> {
    fn raw(&self, i: usize) -> &'a [u8] { &self.entries[i * self.entry_size..(i + 1) * self.entry_size] }

    /// Every entry in file order, decoded or with the reason it is skipped.
    /// A uuid that already appeared earlier is `DuplicateUuid` (first wins).
    pub fn entries(&self) -> impl Iterator<Item = Result<Profile, EntryError>> + 'a {
        let v = *self;
        (0..v.count).map(move |i| {
            let e = v.raw(i);
            let p = Profile::decode(e)?;
            if (0..i).any(|j| v.raw(j)[0..16] == e[0..16] && Profile::decode(v.raw(j)).is_ok()) {
                return Err(EntryError::DuplicateUuid);
            }
            Ok(p)
        })
    }

    /// Only the usable entries.
    pub fn profiles(&self) -> impl Iterator<Item = Profile> + 'a {
        self.entries().filter_map(Result::ok)
    }
}

/// Pick the slot to load: the valid one with the larger seq (A on a tie).
/// `None` when neither is valid. Pass `None` for a slot that could not be
/// read.
pub fn select<'a>(a: Option<&'a [u8]>, b: Option<&'a [u8]>) -> Option<(Slot, SlotView<'a>)> {
    let va = a.and_then(|d| parse_slot(d).ok());
    let vb = b.and_then(|d| parse_slot(d).ok());
    match (va, vb) {
        (Some(x), Some(y)) => if y.seq > x.seq { Some((Slot::B, y)) } else { Some((Slot::A, x)) },
        (Some(x), None) => Some((Slot::A, x)),
        (None, Some(y)) => Some((Slot::B, y)),
        (None, None) => None,
    }
}

/// Where a writer puts its next image and with which seq: the slot that is
/// not the current winner, seq = winner's + 1 (1 when there is none).
pub fn write_target(a: Option<&[u8]>, b: Option<&[u8]>) -> (Slot, u64) {
    match select(a, b) {
        Some((s, v)) => (s.other(), v.seq.wrapping_add(1).max(1)),
        None => (Slot::A, 1),
    }
}

/// Bytes a slot with `n` v1 entries takes.
pub const fn serialized_len(n: usize) -> usize { HEADER_SIZE + n * ENTRY_SIZE + CRC_SIZE }

/// Write a complete slot image into `out`; returns its length. Entries are
/// validated; duplicate uuids are rejected as `Invalid(DuplicateUuid)`.
pub fn serialize(seq: u64, global_flags: u32, profiles: &[Profile], out: &mut [u8]) -> Result<usize, SerError> {
    if profiles.len() > MAX_PROFILES { return Err(SerError::TooMany); }
    let len = serialized_len(profiles.len());
    if out.len() < len { return Err(SerError::BufferTooSmall); }
    for (i, p) in profiles.iter().enumerate() {
        p.validate().map_err(SerError::Invalid)?;
        if profiles[..i].iter().any(|q| q.uuid == p.uuid) {
            return Err(SerError::Invalid(EntryError::DuplicateUuid));
        }
    }
    let out = &mut out[..len];
    out.fill(0);
    out[0..8].copy_from_slice(&MAGIC);
    out[8..10].copy_from_slice(&VERSION.to_le_bytes());
    out[10..12].copy_from_slice(&(HEADER_SIZE as u16).to_le_bytes());
    out[12..14].copy_from_slice(&(ENTRY_SIZE as u16).to_le_bytes());
    out[14..16].copy_from_slice(&(profiles.len() as u16).to_le_bytes());
    out[16..24].copy_from_slice(&seq.to_le_bytes());
    out[24..28].copy_from_slice(&global_flags.to_le_bytes());
    for (i, p) in profiles.iter().enumerate() {
        let o = HEADER_SIZE + i * ENTRY_SIZE;
        p.encode(&mut out[o..o + ENTRY_SIZE]);
    }
    let body = len - CRC_SIZE;
    let c = crc32(&out[..body]);
    out[body..].copy_from_slice(&c.to_le_bytes());
    Ok(len)
}

/// Parse the legacy `/etc/leandros/wifi.conf` (`ssid=` / `psk=` lines,
/// optional double quotes, `#` comments). Returns (ssid, psk) slices.
pub fn parse_legacy_wifi_conf(text: &[u8]) -> Option<(&[u8], &[u8])> {
    let mut ssid = None;
    let mut psk: &[u8] = &[];
    for line in text.split(|&c| c == b'\n') {
        let line = trim(line);
        if line.first() == Some(&b'#') { continue; }
        if let Some(v) = line.strip_prefix(b"ssid=") { ssid = Some(unquote(trim(v))); }
        else if let Some(v) = line.strip_prefix(b"psk=") { psk = unquote(trim(v)); }
    }
    let ssid = ssid?;
    if ssid.is_empty() || ssid.len() > SSID_MAX { return None; }
    Some((ssid, psk))
}

/// The profile the kernel synthesises from legacy credentials (wifi.conf or
/// `wifi.ssid=`/`wifi.psk=`): uuid derived from the SSID, akm AUTO (NONE
/// without a psk; a psk that is neither 8..=63 printable nor 64 hex is
/// refused, as before), autoconnect on, priority 0 like every profile
/// NetworkManager creates (it used to be -1000, which made any saved network
/// win over it and autoconnect roam away from it). Reported with flag
/// LEGACY; dropped when a stored profile has the same SSID.
pub fn legacy_profile(ssid: &[u8], psk: &[u8]) -> Option<Profile> {
    let uuid = crate::util::derive_uuid(crate::util::UUID_DOMAIN_LEGACY, ssid);
    // Same acceptance as the pre-netctl driver: a WPA passphrase or 64 hex.
    if !psk.is_empty() && !crate::util::secret_valid_for(akm::PSK, psk) { return None; }
    let a = if psk.is_empty() { akm::NONE } else { akm::AUTO };
    Profile::new_wifi(uuid, ssid, a, psk)
}

fn trim(s: &[u8]) -> &[u8] {
    let mut a = 0;
    let mut b = s.len();
    while a < b && (s[a] == b' ' || s[a] == b'\t' || s[a] == b'\r') { a += 1; }
    while b > a && (s[b - 1] == b' ' || s[b - 1] == b'\t' || s[b - 1] == b'\r') { b -= 1; }
    &s[a..b]
}

fn unquote(s: &[u8]) -> &[u8] {
    if s.len() >= 2 && s[0] == b'"' && s[s.len() - 1] == b'"' { &s[1..s.len() - 1] } else { s }
}

/// Seconds since the epoch below which the clock is not set (2020-01-01).
pub const STAMP_MIN_EPOCH_S: u64 = 1_577_836_800;
/// Re-stamp a profile only when its stored value is older than this (flash wear).
pub const STAMP_REFRESH_S: u64 = 6 * 3600;

/// Record a successful connect in `p.timestamp_s`. With no clock yet
/// (`now_s` before 2020) the value 1 means "proven, time unknown". Returns
/// false when nothing changed, so the caller skips the slot write.
pub fn stamp(p: &mut Profile, now_s: u64) -> bool {
    let old = p.timestamp_s;
    let new = if now_s < STAMP_MIN_EPOCH_S {
        if old == 0 { 1 } else { return false }
    } else if old < STAMP_MIN_EPOCH_S || now_s.saturating_sub(old) > STAMP_REFRESH_S {
        now_s
    } else {
        return false;
    };
    p.timestamp_s = new;
    true
}
