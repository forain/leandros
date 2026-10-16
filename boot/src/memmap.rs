//! Physical memory map types and the sanitizer every boot path runs its map
//! through before the memory manager sees it (lane fbmm, 2026-10-16).
//!
//! Self-contained (no crate dependencies, no allocation) so the host test
//! crate `boot/memmap-tests` can include this file verbatim.
//!
//! Why: the buddy allocator (`mm/src/buddy.rs`) writes a free-list header into
//! the first bytes of every block it is handed, and the free-head bitmap into
//! a carved range, all through the HHDM. A map entry that overlaps another,
//! is unsorted, has zero length, is not page-aligned, or wraps past 2^64 used
//! to go straight to it: an overlapped "usable" range that is really ACPI NVS
//! or firmware-reserved would be written into (silent corruption of memory
//! the firmware still owns), and a wrapped `base + length` produced garbage
//! bounds. The Limine parser also kept only the first 256 entries and dropped
//! the rest silently. A full-init firmware map (HP Fast Boot OFF) is the
//! obvious way to get a longer, more fragmented map than any QEMU run shows.
//!
//! What `sanitize` guarantees about its output:
//!   * sorted by base, non-overlapping, no zero-length entries;
//!   * adjacent entries of the same kind merged;
//!   * where input entries overlap, the MOST restrictive kind wins
//!     (BadMemory > Reserved > AcpiNvs > AcpiReclaimable > Available), so a
//!     byte is reported usable only if every input entry covering it says so;
//!   * `Available` entries are page-aligned inward (4 KiB) and lie below
//!     `MAX_PHYS`;
//!   * nothing wraps: every `base + length` fits in a u64.

/// Physical memory region types (multiboot2 §3.6.8 / UEFI MemoryType).
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MemoryType {
    Available       = 1,
    Reserved        = 2,
    AcpiReclaimable = 3,
    AcpiNvs         = 4,
    BadMemory       = 5,
}

/// A single entry in the physical memory map.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct MemoryRegion {
    pub base:   u64,
    pub length: u64,
    pub kind:   MemoryType,
}

impl MemoryRegion {
    pub const EMPTY: MemoryRegion = MemoryRegion { base: 0, length: 0, kind: MemoryType::Reserved };
    /// One past the last byte (saturating; sanitized entries never saturate).
    #[inline] pub fn end(&self) -> u64 { self.base.saturating_add(self.length) }
}

/// Input entries the parsers keep (Limine's own limit is far lower).
pub const MAX_RAW: usize = 1024;
/// Output capacity. Clipping overlaps can split an entry, so allow 2x.
pub const MAX_SANE: usize = 2 * MAX_RAW;
/// Usable RAM at or above this is dropped: it cannot be a real DIMM on any
/// CPU this kernel runs on, and it would not fit the 4-level HHDM.
pub const MAX_PHYS: u64 = 1 << 46;
const PAGE: u64 = 4096;

/// What `sanitize` found and changed. All counters are entry counts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Entries handed in.
    pub input: usize,
    /// Zero-length entries dropped.
    pub zero_len: usize,
    /// Entries whose `base + length` wrapped past 2^64 (clamped).
    pub wrapped: usize,
    /// True if the input was not sorted by base.
    pub unsorted: bool,
    /// Elementary intervals covered by more than one input entry.
    pub overlaps: usize,
    /// ... of which the covering entries disagreed on the kind.
    pub conflicts: usize,
    /// Usable entries that were not page-aligned (trimmed inward).
    pub unaligned: usize,
    /// Usable entries (or parts) dropped: smaller than a page after
    /// alignment, or above `MAX_PHYS`.
    pub dropped_usable: usize,
    /// Output entries that did not fit `out` (lost; should be 0).
    pub out_truncated: usize,
    /// Entries written to `out`.
    pub output: usize,
}

#[inline]
fn prio(k: MemoryType) -> u8 {
    match k {
        MemoryType::Available       => 0,
        MemoryType::AcpiReclaimable => 1,
        MemoryType::AcpiNvs         => 2,
        MemoryType::Reserved        => 3,
        MemoryType::BadMemory       => 4,
    }
}

/// In-place heapsort of `v[..n]` by `key` (no allocation, O(n log n) worst
/// case, so a hostile 1024-entry map costs the same as a friendly one).
fn heapsort_by<T: Copy, K: Ord>(v: &mut [T], key: impl Fn(&T) -> K) {
    let n = v.len();
    if n < 2 { return; }
    let sift = |v: &mut [T], mut root: usize, end: usize| {
        loop {
            let mut child = 2 * root + 1;
            if child >= end { break; }
            if child + 1 < end && key(&v[child]) < key(&v[child + 1]) { child += 1; }
            if key(&v[root]) >= key(&v[child]) { break; }
            v.swap(root, child);
            root = child;
        }
    };
    let mut start = n / 2;
    while start > 0 { start -= 1; sift(v, start, n); }
    let mut end = n;
    while end > 1 { end -= 1; v.swap(0, end); sift(v, 0, end); }
}

/// Sanitize `scratch[..n]` (reordered in place) into `out`, using `bounds`
/// (at least `2 * n` long) as scratch. Returns the stats; the output length
/// is `stats.output`.
pub fn sanitize(scratch: &mut [MemoryRegion], bounds: &mut [u64], out: &mut [MemoryRegion]) -> Stats {
    let mut st = Stats { input: scratch.len(), ..Stats::default() };

    // 1. Drop zero-length entries, clamp wrapping ones. Lengths become exact
    //    so every later `base + length` is safe.
    let mut n = 0usize;
    for i in 0..scratch.len() {
        let mut r = scratch[i];
        if r.length == 0 { st.zero_len += 1; continue; }
        if r.base.checked_add(r.length).is_none() {
            st.wrapped += 1;
            r.length = u64::MAX - r.base;
            if r.length == 0 { st.zero_len += 1; continue; }
        }
        scratch[n] = r;
        n += 1;
    }
    let ents = &mut scratch[..n];

    // 2. Sort by base.
    for i in 1..n { if ents[i - 1].base > ents[i].base { st.unsorted = true; break; } }
    if st.unsorted { heapsort_by(ents, |r| r.base); }

    // 3. Every start and end is an interval boundary; sort and dedup them.
    let nb_cap = bounds.len().min(2 * n);
    let mut nb = 0usize;
    for r in ents.iter() {
        if nb + 2 > nb_cap { break; }
        bounds[nb] = r.base; bounds[nb + 1] = r.base + r.length; nb += 2;
    }
    let bnd = &mut bounds[..nb];
    heapsort_by(bnd, |&b| b);
    let mut m = 0usize;
    for i in 0..nb {
        if m == 0 || bnd[m - 1] != bnd[i] { bnd[m] = bnd[i]; m += 1; }
    }

    // 4. For each elementary interval, the most restrictive covering kind.
    //    Entries are sorted by base, so the scan stops at the first entry
    //    starting at or after the interval's end; `lo` skips the prefix of
    //    entries that ended before the interval (it only ever moves forward
    //    past entries that end at or before every later interval start, and
    //    a sorted-by-base prefix whose ends are all <= a is exactly that).
    let mut no = 0usize;
    let mut lo = 0usize;
    for w in 0..m.saturating_sub(1) {
        let (a, b) = (bnd[w], bnd[w + 1]);
        while lo < n && ents[lo].base + ents[lo].length <= a && ents[lo].base <= a { lo += 1; }
        let mut kind: Option<MemoryType> = None;
        let mut cover = 0usize;
        let mut differ = false;
        let mut j = lo;
        while j < n && ents[j].base < b {
            let r = ents[j];
            if r.base <= a && r.base + r.length >= b {
                cover += 1;
                match kind {
                    None => kind = Some(r.kind),
                    Some(k) => {
                        if k != r.kind { differ = true; }
                        if prio(r.kind) > prio(k) { kind = Some(r.kind); }
                    }
                }
            }
            j += 1;
        }
        if cover > 1 { st.overlaps += 1; if differ { st.conflicts += 1; } }
        let kind = match kind { Some(k) => k, None => continue }; // a hole
        if no > 0 {
            let last = &mut out[no - 1];
            if last.kind == kind && last.base + last.length == a {
                last.length += b - a;
                continue;
            }
        }
        if no < out.len() {
            out[no] = MemoryRegion { base: a, length: b - a, kind };
            no += 1;
        } else {
            st.out_truncated += 1;
        }
    }

    // 5. Usable entries: page-align inward, clip at MAX_PHYS.
    let mut k = 0usize;
    for i in 0..no {
        let mut r = out[i];
        if r.kind == MemoryType::Available {
            let end = r.base + r.length;
            let s = r.base.checked_add(PAGE - 1).map_or(u64::MAX, |v| v & !(PAGE - 1));
            let mut e = end & !(PAGE - 1);
            if s != r.base || e != end { st.unaligned += 1; }
            if e > MAX_PHYS { e = MAX_PHYS; }
            if s >= e { st.dropped_usable += 1; continue; }
            if e != end && end > MAX_PHYS { st.dropped_usable += 1; }
            r.base = s;
            r.length = e - s;
        }
        out[k] = r;
        k += 1;
    }
    st.output = k;
    st
}

/// The raw Limine memory-map type as a short (<= 6 char) tag for the early
/// screen dump.
pub fn limine_type_tag(t: u64) -> &'static str {
    match t {
        0 => "usable",
        1 => "resvd",
        2 => "acpiR",
        3 => "acpiN",
        4 => "BAD",
        5 => "bootR",
        6 => "kernel",
        7 => "fb",
        8 => "mapRes",
        _ => "?",
    }
}

/// Limine memory-map type to the kernel's kinds. Everything the kernel must
/// not hand out — bootloader-reclaimable (Limine's page tables, the stack it
/// entered us on, the responses), kernel+modules, framebuffer — is Reserved.
pub fn limine_kind(t: u64) -> MemoryType {
    match t {
        0 => MemoryType::Available,
        2 => MemoryType::AcpiReclaimable,
        3 => MemoryType::AcpiNvs,
        4 => MemoryType::BadMemory,
        _ => MemoryType::Reserved,
    }
}
