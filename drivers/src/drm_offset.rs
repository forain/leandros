//! Opaque mmap offsets for DRM buffer objects (Linux `drm_vma_offset_manager`).
//!
//! Every token a DRM ioctl hands userspace as "the offset to pass to mmap()"
//! (MAP_DUMB, VIRTGPU_MAP, V3D_MMAP_BO, i915 MMAP_OFFSET, nouveau GEM_NEW's
//! `map_handle`, the legacy 0x1002 CREATE_FB slot) used to be the object's
//! physical address, or its place in the virtio-gpu shared-memory window. That
//! told any client where its buffers live in physical memory. This module
//! replaces those values with offsets from a private window that has no
//! relation to physical memory: `offset_for(real, size)` allocates (or reuses)
//! a page-aligned span for an object's real token, and the device's mmap entry
//! point (`handle_ioctl_mmap`, ioctl 0x1007) turns a fake offset back into the
//! real token with `resolve` BEFORE its per-open ownership check runs.
//!
//! Spans are keyed by the real token, so one object keeps one offset no matter
//! how often, or through which handle (a PRIME import alias of a dumb buffer),
//! it is mapped. `release(real)` frees the span when the object is freed. A
//! missed release is a bounded leak, never an access hole: the resolved real
//! token still has to pass the ownership check, and a later object at the same
//! real token reuses (or, with a different size, replaces) the stale span.
//!
//! LOCKING: `OFFSETS` is a leaf. Nothing else is taken while it is held.

use alloc::collections::BTreeMap;
use spin::Mutex;

/// First fake offset. 16 TiB: above any guest-physical or BAR address this OS
/// meets (QEMU virt's high PCIe window ends at 1 TiB; the laptop/PC CPUs have
/// 39-46 physical address bits but firmware places nothing near the top), so
/// a fake offset is never confused with a physical one when reading logs.
pub const FAKE_BASE: u64 = 1 << 44;
/// End of the fake window (exclusive): 16 TiB of offset space.
pub const FAKE_END: u64 = 1 << 45;
const PAGE: u64 = 4096;
/// A second, 32-bit window for the one consumer that carries its offset in a
/// u32: the legacy 0x1002 CREATE_FB reply (`fb_data[5]`, read by DOOM's SDL
/// backend). 256 MiB just below the x86 LAPIC/IOAPIC/flash hole, never RAM.
pub const FAKE32_BASE: u64 = 0xE000_0000;
pub const FAKE32_END: u64 = 0xF000_0000;

struct Span {
    size: u64,
    real: u64,
}

struct Offsets {
    by_fake: BTreeMap<u64, Span>,
    by_real: BTreeMap<u64, u64>,
    /// Next-fit cursor per window (64-bit, 32-bit).
    next: [u64; 2],
}

/// First address >= `from` where `size` bytes plus a guard page fit before the
/// next span (spans are kept in `lo..hi`), or `None` if none fits below `hi`.
fn gap(spans: &BTreeMap<u64, Span>, from: u64, hi: u64, size: u64) -> Option<u64> {
    let mut cursor = from;
    // A span starting below `from` may still extend past it.
    if let Some((&b, sp)) = spans.range(..from).next_back() {
        cursor = cursor.max(b + sp.size + PAGE);
    }
    for (&base, sp) in spans.range(from..hi) {
        if base >= cursor && base - cursor >= size + PAGE { break; }
        cursor = cursor.max(base + sp.size + PAGE);
    }
    if cursor.checked_add(size)? > hi { None } else { Some(cursor) }
}

static OFFSETS: Mutex<Offsets> = Mutex::new(Offsets {
    by_fake: BTreeMap::new(),
    by_real: BTreeMap::new(),
    next: [FAKE_BASE, FAKE32_BASE],
});

/// Is `off` inside the fake window (whether or not a span is allocated there)?
pub fn is_fake(off: u64) -> bool {
    (off >= FAKE_BASE && off < FAKE_END) || (off >= FAKE32_BASE && off < FAKE32_END)
}

/// The fake offset for the object whose real token is `real`, allocating a
/// span of `size` bytes (rounded up to pages, at least one page) on first use.
/// `None` only for `real == 0` or an exhausted window.
pub fn offset_for(real: u64, size: u64) -> Option<u64> {
    offset_in(real, size, FAKE_BASE, FAKE_END)
}

/// `offset_for` in the 32-bit window (see `FAKE32_BASE`). An object already
/// holding a 64-bit span keeps that span only if it lies in this window too,
/// so an object mapped both ways is re-homed here (both callers then agree).
pub fn offset_for32(real: u64, size: u64) -> Option<u32> {
    offset_in(real, size, FAKE32_BASE, FAKE32_END).map(|o| o as u32)
}

fn offset_in(real: u64, size: u64, lo: u64, hi: u64) -> Option<u64> {
    if real == 0 { return None; }
    let size = size.max(PAGE).checked_add(PAGE - 1)? & !(PAGE - 1);
    let mut o = OFFSETS.lock();
    if let Some(&fake) = o.by_real.get(&real) {
        if fake >= lo && fake < hi && o.by_fake.get(&fake).map_or(false, |s| s.size == size) {
            return Some(fake);
        }
        // Same real token, different object size: the old object is gone
        // without a release. Replace its span rather than resolve past it.
        o.by_fake.remove(&fake);
        o.by_real.remove(&real);
    }
    // Next fit from where the last allocation ended, wrapping once: a freed
    // offset is not handed out again until the window has cycled, so a client
    // still holding a closed BO's offset does not find a new BO behind it (the
    // ownership check would bound that to its own BOs, but it is still wrong).
    // One guard page between spans so an offset just past one object's end
    // never resolves into its neighbour.
    let slot = if lo == FAKE_BASE { 0 } else { 1 };
    let start = o.next[slot].clamp(lo, hi);
    let cursor = match gap(&o.by_fake, start, hi, size).or_else(|| gap(&o.by_fake, lo, hi, size)) {
        Some(c) => c,
        None => return None,
    };
    o.next[slot] = cursor + size + PAGE;
    o.by_fake.insert(cursor, Span { size, real });
    o.by_real.insert(real, cursor);
    Some(cursor)
}

/// The real token behind fake offset `off` (keeping `off`'s position inside
/// its span, so a partial map of a large object resolves to the same place in
/// it). `None` if `off` names no live span.
pub fn resolve(off: u64) -> Option<u64> {
    if !is_fake(off) { return None; }
    let o = OFFSETS.lock();
    let (&base, s) = o.by_fake.range(..=off).next_back()?;
    if off - base >= s.size { return None; }
    Some(s.real + (off - base))
}

/// Free the span of the object whose real token is `real` (no-op if none).
pub fn release(real: u64) {
    if real == 0 { return; }
    let mut o = OFFSETS.lock();
    if let Some(fake) = o.by_real.remove(&real) {
        o.by_fake.remove(&fake);
    }
}

/// Number of live spans (diagnostics).
pub fn live() -> usize {
    OFFSETS.lock().by_fake.len()
}
