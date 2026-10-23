//! The virtio-gpu host-visible window's span table, with no kernel dependency:
//! `drm_device_interface` owns the lock, the device round-trips and the user
//! memory; this module only decides which window offsets are taken and when a
//! span may be given back. Host-tested in drivers/uapi-tests, which includes
//! this file verbatim.
//!
//! LIFETIME (L1.14). A span has two kinds of holder: the blob object it was
//! reserved for (until GEM_CLOSE / the open's handle sweep drops the last
//! reference), and every user VMA that maps part of it (`maps`, taken by
//! sys_mmap, +1 per fork alias, -1 per munmap / exit teardown). The span — and
//! the host resource behind it, whose RESOURCE_UNMAP_BLOB + UNREF are deferred
//! with it — is given back only when BOTH are gone. Before this, the object's
//! death freed the offset at once and the next blob was placed at the same
//! guest-physical address while an old mapping still pointed there: the old
//! process read and wrote the new blob's memory.
//!
//! A span whose owner is gone but which is still mapped is a *zombie*: it keeps
//! its offset, refuses new mmaps, and becomes *ready* when its last mapping
//! goes. Ready zombies are reaped by the driver (`take_ready` → host
//! unmap/unref with no lock held → `remove`), never from the VMA teardown
//! hook, which runs under the address space's busy flag.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub len: u64,
    /// Live user VMAs mapping (part of) this span.
    pub maps: u32,
    /// Host resource id of the dead owner; 0 while the owner lives.
    pub zombie_res: u32,
    /// Handed to the reaper by `take_ready`; it is removed after the host round-trip.
    pub reaping: bool,
}

pub struct SpanTable {
    pub spans: BTreeMap<u64, Span>,
    grain: u64,
    max_spans: usize,
}

/// What the caller must do after the owner of a span went away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OwnerGone {
    /// Not mapped: the span is already removed; unmap/unref the host resource now.
    FreeNow,
    /// Still mapped: the span became a zombie; leave the host resource alone.
    Deferred,
    /// No such span.
    Unknown,
}

impl SpanTable {
    pub const fn new(grain: u64, max_spans: usize) -> Self {
        SpanTable { spans: BTreeMap::new(), grain, max_spans }
    }

    /// First-fit reservation of `bytes` (rounded up to the grain) in a
    /// `window_len`-byte window.
    pub fn alloc(&mut self, bytes: u64, window_len: u64) -> Option<u64> {
        if bytes == 0 || window_len == 0 { return None; }
        let need = bytes.checked_add(self.grain - 1)? & !(self.grain - 1);
        if self.spans.len() >= self.max_spans { return None; }
        let mut cursor: u64 = 0;
        for (&off, s) in self.spans.iter() {
            if off.saturating_sub(cursor) >= need { break; }
            cursor = cursor.max(off.checked_add(s.len)?);
        }
        if cursor.checked_add(need)? > window_len { return None; }
        self.spans.insert(cursor, Span { len: need, maps: 0, zombie_res: 0, reaping: false });
        Some(cursor)
    }

    /// Rollback of a reservation nobody has mapped yet.
    pub fn remove(&mut self, off: u64) {
        self.spans.remove(&off);
    }

    /// The span containing window offset `rel`.
    fn containing(&mut self, rel: u64) -> Option<(u64, &mut Span)> {
        let (&off, s) = self.spans.range_mut(..=rel).next_back()?;
        if rel - off < s.len { Some((off, s)) } else { None }
    }

    /// A new mmap of `[rel, rel+len)`: allowed only inside one live (owned)
    /// span. Takes the mapping's reference on success.
    pub fn pin_map(&mut self, rel: u64, len: u64) -> bool {
        if len == 0 { return false; }
        match self.containing(rel) {
            Some((off, s)) if s.zombie_res == 0 && !s.reaping => {
                match rel.checked_add(len) {
                    Some(end) if end <= off + s.len => { s.maps += 1; true }
                    _ => false,
                }
            }
            _ => false,
        }
    }

    /// One more VMA aliasing a mapped range (fork). Zombies included: the
    /// parent's mapping already holds the span, the child's must as well.
    pub fn retain_map(&mut self, rel: u64) -> bool {
        match self.containing(rel) {
            Some((_, s)) => { s.maps += 1; true }
            None => false,
        }
    }

    /// A VMA mapping this span went away.
    pub fn release_map(&mut self, rel: u64) -> bool {
        match self.containing(rel) {
            Some((_, s)) if s.maps > 0 => { s.maps -= 1; true }
            _ => false,
        }
    }

    /// The owning object of span `off` (host resource `res`) is gone.
    pub fn owner_gone(&mut self, off: u64, res: u32) -> OwnerGone {
        let mapped = match self.spans.get_mut(&off) {
            None => return OwnerGone::Unknown,
            Some(s) => {
                if s.maps != 0 { s.zombie_res = res.max(1); }
                s.maps != 0
            }
        };
        if mapped { return OwnerGone::Deferred; }
        self.spans.remove(&off);
        OwnerGone::FreeNow
    }

    /// Zombies whose last mapping is gone: `(offset, host resource)`. Each is
    /// marked reaping and returned once; the caller tears the host side down
    /// and then calls `remove(offset)`.
    pub fn take_ready(&mut self) -> Vec<(u64, u32)> {
        let mut out = Vec::new();
        for (&off, s) in self.spans.iter_mut() {
            if s.zombie_res != 0 && s.maps == 0 && !s.reaping {
                s.reaping = true;
                out.push((off, s.zombie_res));
            }
        }
        out
    }
}
