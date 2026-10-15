//! The event stream: fixed 64-byte records read(2) from `/dev/netctl`.
//!
//! A *logical event* is one state record ([`NcIface`], [`NcBss`], …) plus a
//! kind. Records carry 48 payload bytes, so a logical event whose payload is
//! larger is split into `nparts` consecutive records (`part` = 0..nparts-1)
//! that share `kind`, `ifindex` and `seq`. The payload of the event is the
//! concatenation of the parts' payloads; a reader zero-extends it to the
//! struct it knows (a newer kernel may send more parts, an older one fewer).
//!
//! The kernel queues all parts of an event at once or none of them, so parts
//! are always adjacent in the stream. Only an overflow can cut an event: the
//! reader then sees `RESYNC` (part 0 of a new event) where it expected the
//! next part, drops the partial event and rebuilds from the snapshot that
//! follows. [`Reassembler`] implements exactly that.

use crate::types::*;
use crate::Pod;
use core::mem::size_of;

/// Bytes per record.
pub const EVENT_RECORD_SIZE: usize = 64;
/// Payload bytes per record.
pub const EVENT_PAYLOAD_SIZE: usize = 48;
/// Most parts one logical event may have in any ABI version (v1 uses <= 4).
pub const EVENT_MAX_PARTS: usize = 8;
/// Largest logical payload a reader must be able to buffer.
pub const EVENT_MAX_PAYLOAD: usize = EVENT_MAX_PARTS * EVENT_PAYLOAD_SIZE;

/// Event kinds (`NcEventRecord.kind`) and their payload types.
pub mod kind {
    /// [`NcSnapshotInfo`](crate::NcSnapshotInfo): a snapshot starts.
    pub const SNAPSHOT_BEGIN: u8 = 0x01;
    /// [`NcSnapshotInfo`](crate::NcSnapshotInfo): the snapshot is complete.
    pub const SNAPSHOT_END: u8 = 0x02;
    /// [`NcResync`](crate::NcResync): queue overflow; a snapshot follows.
    pub const RESYNC: u8 = 0x03;
    /// [`NcIface`](crate::NcIface)
    pub const IFACE_ADDED: u8 = 0x10;
    /// [`NcIface`](crate::NcIface): carrier, admin, radio, MAC, speed, oper…
    pub const IFACE_CHANGED: u8 = 0x11;
    /// [`NcIface`](crate::NcIface): last known state; the ifindex is dead.
    pub const IFACE_REMOVED: u8 = 0x12;
    /// [`NcIpv4`](crate::NcIpv4): any change of the interface's IPv4 config,
    /// including which interface holds the default route.
    pub const IPV4: u8 = 0x20;
    /// [`NcWifiState`](crate::NcWifiState): every state transition.
    pub const WIFI_STATE: u8 = 0x30;
    /// [`NcWifiSignal`](crate::NcWifiSignal)
    pub const WIFI_SIGNAL: u8 = 0x31;
    /// [`NcScanInfo`](crate::NcScanInfo)
    pub const SCAN_STARTED: u8 = 0x32;
    /// [`NcScanInfo`](crate::NcScanInfo): after the BSS/BSS_GONE records of
    /// that scan.
    pub const SCAN_DONE: u8 = 0x33;
    /// [`NcBss`](crate::NcBss): new, or changed materially.
    pub const BSS: u8 = 0x34;
    /// [`NcBss`](crate::NcBss): aged out or radio off.
    pub const BSS_GONE: u8 = 0x35;
    /// [`NcProfileMeta`](crate::NcProfileMeta): one profile of a batch.
    pub const PROFILE: u8 = 0x40;
    /// [`NcProfilesInfo`](crate::NcProfilesInfo): closes a PROFILE batch.
    pub const PROFILES_END: u8 = 0x41;
}

/// `NcEventRecord.flags`.
pub mod event_flags {
    /// The record belongs to a snapshot (between SNAPSHOT_BEGIN and
    /// SNAPSHOT_END inclusive) rather than reporting a change.
    pub const SNAPSHOT: u8 = 1 << 0;
}

/// One 64-byte record as read(2) returns it.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NcEventRecord {
    /// 0: [`kind`].
    pub kind: u8,
    /// 1: [`event_flags`].
    pub flags: u8,
    /// 2: index of this part, 0..nparts.
    pub part: u8,
    /// 3: parts of this logical event (>= 1).
    pub nparts: u8,
    /// 4: interface the event is about; 0 for global events.
    pub ifindex: u32,
    /// 8: per-open logical event sequence number (all parts share it).
    /// Starts at 1 with the open's SNAPSHOT_BEGIN, +1 per logical event
    /// queued *or dropped*, wraps. Diagnostic: a gap is always explained by
    /// a RESYNC.
    pub seq: u32,
    /// 12: boot-time milliseconds when the event was generated (wraps after
    /// 49 days).
    pub time_ms: u32,
    /// 16
    pub payload: [u8; EVENT_PAYLOAD_SIZE],
}
// SAFETY: integer-only #[repr(C)] struct, no implicit padding.
unsafe impl Pod for NcEventRecord {}
const _: () = assert!(size_of::<NcEventRecord>() == EVENT_RECORD_SIZE);

impl Default for NcEventRecord {
    fn default() -> Self { <Self as Pod>::zeroed() }
}

/// Header fields of a logical event (everything but the payload).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct EventMeta {
    pub kind: u8,
    pub flags: u8,
    pub ifindex: u32,
    pub seq: u32,
    pub time_ms: u32,
}

impl EventMeta {
    pub fn is_snapshot(&self) -> bool { self.flags & event_flags::SNAPSHOT != 0 }
}

/// A decoded logical event.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Event {
    SnapshotBegin(NcSnapshotInfo),
    SnapshotEnd(NcSnapshotInfo),
    Resync(NcResync),
    IfaceAdded(NcIface),
    IfaceChanged(NcIface),
    IfaceRemoved(NcIface),
    Ipv4(NcIpv4),
    WifiState(NcWifiState),
    WifiSignal(NcWifiSignal),
    ScanStarted(NcScanInfo),
    ScanDone(NcScanInfo),
    Bss(NcBss),
    BssGone(NcBss),
    Profile(NcProfileMeta),
    ProfilesEnd(NcProfilesInfo),
    /// A kind this reader does not know (newer kernel): skip it.
    Unknown { kind: u8 },
}

/// Parts needed for a payload of `size` bytes.
pub const fn parts_for(size: usize) -> usize {
    if size == 0 { 1 } else { size.div_ceil(EVENT_PAYLOAD_SIZE) }
}

/// Parts the ABI-1 payload of `kind` occupies, `None` for unknown kinds.
pub const fn kind_parts(k: u8) -> Option<usize> {
    let size = match k {
        kind::SNAPSHOT_BEGIN | kind::SNAPSHOT_END => size_of::<NcSnapshotInfo>(),
        kind::RESYNC => size_of::<NcResync>(),
        kind::IFACE_ADDED | kind::IFACE_CHANGED | kind::IFACE_REMOVED => size_of::<NcIface>(),
        kind::IPV4 => size_of::<NcIpv4>(),
        kind::WIFI_STATE => size_of::<NcWifiState>(),
        kind::WIFI_SIGNAL => size_of::<NcWifiSignal>(),
        kind::SCAN_STARTED | kind::SCAN_DONE => size_of::<NcScanInfo>(),
        kind::BSS | kind::BSS_GONE => size_of::<NcBss>(),
        kind::PROFILE => size_of::<NcProfileMeta>(),
        kind::PROFILES_END => size_of::<NcProfilesInfo>(),
        _ => return None,
    };
    Some(parts_for(size))
}

// Every v1 payload is a whole number of parts, so nothing is ever padded.
const _: () = {
    assert!(size_of::<NcSnapshotInfo>().is_multiple_of(EVENT_PAYLOAD_SIZE));
    assert!(size_of::<NcResync>().is_multiple_of(EVENT_PAYLOAD_SIZE));
    assert!(size_of::<NcIface>().is_multiple_of(EVENT_PAYLOAD_SIZE));
    assert!(size_of::<NcIpv4>().is_multiple_of(EVENT_PAYLOAD_SIZE));
    assert!(size_of::<NcWifiState>().is_multiple_of(EVENT_PAYLOAD_SIZE));
    assert!(size_of::<NcWifiSignal>().is_multiple_of(EVENT_PAYLOAD_SIZE));
    assert!(size_of::<NcScanInfo>().is_multiple_of(EVENT_PAYLOAD_SIZE));
    assert!(size_of::<NcBss>().is_multiple_of(EVENT_PAYLOAD_SIZE));
    assert!(size_of::<NcProfileMeta>().is_multiple_of(EVENT_PAYLOAD_SIZE));
    assert!(size_of::<NcProfilesInfo>().is_multiple_of(EVENT_PAYLOAD_SIZE));
    assert!(size_of::<NcProfileMeta>() <= EVENT_MAX_PAYLOAD);
};

impl Event {
    /// The event's kind byte.
    pub fn kind(&self) -> u8 {
        match self {
            Event::SnapshotBegin(_) => kind::SNAPSHOT_BEGIN,
            Event::SnapshotEnd(_) => kind::SNAPSHOT_END,
            Event::Resync(_) => kind::RESYNC,
            Event::IfaceAdded(_) => kind::IFACE_ADDED,
            Event::IfaceChanged(_) => kind::IFACE_CHANGED,
            Event::IfaceRemoved(_) => kind::IFACE_REMOVED,
            Event::Ipv4(_) => kind::IPV4,
            Event::WifiState(_) => kind::WIFI_STATE,
            Event::WifiSignal(_) => kind::WIFI_SIGNAL,
            Event::ScanStarted(_) => kind::SCAN_STARTED,
            Event::ScanDone(_) => kind::SCAN_DONE,
            Event::Bss(_) => kind::BSS,
            Event::BssGone(_) => kind::BSS_GONE,
            Event::Profile(_) => kind::PROFILE,
            Event::ProfilesEnd(_) => kind::PROFILES_END,
            Event::Unknown { kind } => *kind,
        }
    }

    /// The interface the event is about (header `ifindex`), 0 for global.
    pub fn ifindex(&self) -> u32 {
        match self {
            Event::IfaceAdded(p) | Event::IfaceChanged(p) | Event::IfaceRemoved(p) => p.ifindex,
            Event::Ipv4(p) => p.ifindex,
            Event::WifiState(p) => p.ifindex,
            Event::WifiSignal(p) => p.ifindex,
            Event::ScanStarted(p) | Event::ScanDone(p) => p.ifindex,
            Event::Bss(p) | Event::BssGone(p) => p.ifindex,
            _ => 0,
        }
    }

    /// The payload bytes (empty for `Unknown`).
    pub fn payload_bytes(&self) -> &[u8] {
        match self {
            Event::SnapshotBegin(p) | Event::SnapshotEnd(p) => p.as_bytes(),
            Event::Resync(p) => p.as_bytes(),
            Event::IfaceAdded(p) | Event::IfaceChanged(p) | Event::IfaceRemoved(p) => p.as_bytes(),
            Event::Ipv4(p) => p.as_bytes(),
            Event::WifiState(p) => p.as_bytes(),
            Event::WifiSignal(p) => p.as_bytes(),
            Event::ScanStarted(p) | Event::ScanDone(p) => p.as_bytes(),
            Event::Bss(p) | Event::BssGone(p) => p.as_bytes(),
            Event::Profile(p) => p.as_bytes(),
            Event::ProfilesEnd(p) => p.as_bytes(),
            Event::Unknown { .. } => &[],
        }
    }

    /// Decode a reassembled payload. Short payloads are zero-extended and
    /// long ones truncated (struct growth in either direction).
    pub fn decode(k: u8, payload: &[u8]) -> Event {
        match k {
            kind::SNAPSHOT_BEGIN => Event::SnapshotBegin(Pod::read_prefix(payload)),
            kind::SNAPSHOT_END => Event::SnapshotEnd(Pod::read_prefix(payload)),
            kind::RESYNC => Event::Resync(Pod::read_prefix(payload)),
            kind::IFACE_ADDED => Event::IfaceAdded(Pod::read_prefix(payload)),
            kind::IFACE_CHANGED => Event::IfaceChanged(Pod::read_prefix(payload)),
            kind::IFACE_REMOVED => Event::IfaceRemoved(Pod::read_prefix(payload)),
            kind::IPV4 => Event::Ipv4(Pod::read_prefix(payload)),
            kind::WIFI_STATE => Event::WifiState(Pod::read_prefix(payload)),
            kind::WIFI_SIGNAL => Event::WifiSignal(Pod::read_prefix(payload)),
            kind::SCAN_STARTED => Event::ScanStarted(Pod::read_prefix(payload)),
            kind::SCAN_DONE => Event::ScanDone(Pod::read_prefix(payload)),
            kind::BSS => Event::Bss(Pod::read_prefix(payload)),
            kind::BSS_GONE => Event::BssGone(Pod::read_prefix(payload)),
            kind::PROFILE => Event::Profile(Pod::read_prefix(payload)),
            kind::PROFILES_END => Event::ProfilesEnd(Pod::read_prefix(payload)),
            _ => Event::Unknown { kind: k },
        }
    }

    /// Split the event into its records. `flags` is [`event_flags`]; the
    /// header `ifindex` is taken from the payload.
    pub fn encode(&self, flags: u8, seq: u32, time_ms: u32) -> EncodedEvent {
        encode_raw(self.kind(), flags, self.ifindex(), seq, time_ms, self.payload_bytes())
    }
}

/// The records of one logical event, in order.
#[derive(Clone, Copy, Debug)]
pub struct EncodedEvent {
    recs: [NcEventRecord; EVENT_MAX_PARTS],
    n: usize,
}

impl EncodedEvent {
    pub fn records(&self) -> &[NcEventRecord] { &self.recs[..self.n] }
    pub fn len(&self) -> usize { self.n }
    pub fn is_empty(&self) -> bool { self.n == 0 }
    /// Copy the records' bytes into `out`; returns the bytes written
    /// (`len() * 64`), or `None` when `out` is too small.
    pub fn write_to(&self, out: &mut [u8]) -> Option<usize> {
        let total = self.n * EVENT_RECORD_SIZE;
        if out.len() < total { return None; }
        for (i, r) in self.records().iter().enumerate() {
            out[i * EVENT_RECORD_SIZE..(i + 1) * EVENT_RECORD_SIZE].copy_from_slice(r.as_bytes());
        }
        Some(total)
    }
}

/// Split an arbitrary payload into records (payload longer than
/// EVENT_MAX_PAYLOAD is truncated; callers never do that).
pub fn encode_raw(k: u8, flags: u8, ifindex: u32, seq: u32, time_ms: u32, payload: &[u8]) -> EncodedEvent {
    let payload = &payload[..payload.len().min(EVENT_MAX_PAYLOAD)];
    let n = parts_for(payload.len());
    let mut recs = [NcEventRecord::default(); EVENT_MAX_PARTS];
    for (i, rec) in recs.iter_mut().enumerate().take(n) {
        rec.kind = k;
        rec.flags = flags;
        rec.part = i as u8;
        rec.nparts = n as u8;
        rec.ifindex = ifindex;
        rec.seq = seq;
        rec.time_ms = time_ms;
        let lo = i * EVENT_PAYLOAD_SIZE;
        let hi = (lo + EVENT_PAYLOAD_SIZE).min(payload.len());
        if lo < hi { rec.payload[..hi - lo].copy_from_slice(&payload[lo..hi]); }
    }
    EncodedEvent { recs, n }
}

/// Why a record did not fit the event being reassembled. The reassembler has
/// already recovered (dropped the partial event) when it reports one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecodeError {
    /// `nparts` is 0 or above EVENT_MAX_PARTS, or `part >= nparts`.
    BadFraming,
    /// A continuation part arrived without its part 0 (e.g. after a resync).
    OrphanPart,
}

/// Turns the record stream back into logical events.
pub struct Reassembler {
    /// Partial events discarded so far (a new part 0 arrived first, or a
    /// framing error). Only an overflow (RESYNC) causes this with a correct
    /// kernel.
    pub dropped_partials: u32,
    buf: [u8; EVENT_MAX_PAYLOAD],
    meta: EventMeta,
    nparts: usize,
    next: usize,
}

impl Default for Reassembler {
    fn default() -> Self { Self::new() }
}

impl Reassembler {
    pub const fn new() -> Self {
        Reassembler {
            dropped_partials: 0,
            buf: [0; EVENT_MAX_PAYLOAD],
            meta: EventMeta { kind: 0, flags: 0, ifindex: 0, seq: 0, time_ms: 0 },
            nparts: 0,
            next: 0,
        }
    }

    /// True while a multi-part event is incomplete.
    pub fn in_progress(&self) -> bool { self.next != 0 }

    /// Forget a partial event.
    pub fn reset(&mut self) { self.next = 0; self.nparts = 0; }

    fn drop_partial(&mut self) {
        if self.in_progress() { self.dropped_partials = self.dropped_partials.saturating_add(1); }
        self.reset();
    }

    /// Feed one record. `Ok(Some(..))` when it completed an event, `Ok(None)`
    /// when more parts are needed. A part 0 that arrives while an event is
    /// incomplete starts the new event (the partial one is dropped and
    /// counted in `dropped_partials`). `Err` means this record was unusable
    /// and is skipped; the reassembler is ready for the next part 0.
    pub fn push(&mut self, rec: &NcEventRecord) -> Result<Option<(EventMeta, Event)>, DecodeError> {
        let n = rec.nparts as usize;
        let p = rec.part as usize;
        if n == 0 || n > EVENT_MAX_PARTS || p >= n {
            self.drop_partial();
            return Err(DecodeError::BadFraming);
        }
        if self.in_progress() {
            let same = p == self.next && n == self.nparts && rec.kind == self.meta.kind
                && rec.seq == self.meta.seq && rec.ifindex == self.meta.ifindex;
            if !same {
                self.drop_partial();
                if p != 0 { return Err(DecodeError::OrphanPart); }
            }
        } else if p != 0 {
            return Err(DecodeError::OrphanPart);
        }
        if p == 0 {
            self.meta = EventMeta { kind: rec.kind, flags: rec.flags, ifindex: rec.ifindex, seq: rec.seq, time_ms: rec.time_ms };
            self.nparts = n;
            self.buf = [0; EVENT_MAX_PAYLOAD];
        }
        let lo = p * EVENT_PAYLOAD_SIZE;
        self.buf[lo..lo + EVENT_PAYLOAD_SIZE].copy_from_slice(&rec.payload);
        self.next = p + 1;
        if self.next == self.nparts {
            let len = self.nparts * EVENT_PAYLOAD_SIZE;
            let ev = Event::decode(self.meta.kind, &self.buf[..len]);
            self.reset();
            Ok(Some((self.meta, ev)))
        } else {
            Ok(None)
        }
    }
}

/// Iterate the records in a read(2) buffer (trailing partial bytes ignored;
/// the kernel never returns them).
pub fn records(buf: &[u8]) -> impl Iterator<Item = NcEventRecord> + '_ {
    buf.chunks_exact(EVENT_RECORD_SIZE).map(|c| NcEventRecord::read_from(c).unwrap_or_default())
}
