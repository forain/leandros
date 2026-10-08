//! VM_BIND range bookkeeping for the nouveau-UAPI layer: which GPU VA ranges
//! of one DRM file are mapped to which buffer, which are sparse regions, and
//! what RM operations a MAP / UNMAP / SPARSE request turns into. No kernel or
//! RM dependency — the RM side is behind [`VmOps`] — so drivers/uapi-tests
//! includes this file verbatim and checks it on the host.
//!
//! THE SEMANTICS (Linux `drm_gpuvm` as nouveau's uvmm uses it):
//!
//! * `MAP (bo, addr, range, bo_offset, kind)` binds `[addr, addr+range)`.
//!   Anything already mapped there is replaced: an overlapped mapping is cut
//!   back to the parts outside the new range ("split"), exactly what
//!   `drm_gpuvm_sm_map` produces.
//! * `UNMAP (addr, range)` unbinds whatever is mapped in the range, splitting
//!   mappings that straddle its ends. Inside a sparse region the hole goes
//!   back to reading zeros.
//! * `MAP | SPARSE (addr, range)` creates a sparse region; it may not overlap
//!   another region. `UNMAP | SPARSE` destroys the region that starts at
//!   `addr` with exactly that size, unmapping whatever is still in it first.
//! * A mapping lies entirely inside one sparse region or entirely outside
//!   every region (uvmm refuses a mapping that crosses a region boundary).
//!
//! HOW IT MAPS ONTO RM (see `uapi.rs`): every mapping outside a region is its
//! own NV50_MEMORY_VIRTUAL range allocated at the fixed address (so its PTE
//! kind is the range's) plus one NV04_MAP_MEMORY_DMA; a mapping inside a
//! region is an NV04_MAP_MEMORY_DMA into the region's sparse range. A split
//! therefore is "unmap the old mapping, map the pieces that survive" — RM
//! cannot shrink a mapping in place.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

/// GPU page size every bind is aligned to (NVK's bind_align: the OS page).
pub const PAGE: u64 = 4096;

/// errno values (positive) returned to userspace.
pub const EINVAL: i32 = 22;
pub const ENOENT: i32 = 2;

/// What a mapping is backed by.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Backing {
    /// Buffer object id (the layer's lifetime id, not a GEM handle).
    pub bo: u32,
    /// Byte offset into the buffer of the mapping's first byte.
    pub offset: u64,
    /// PTE kind (NVK's `pte_kind`; 0 = pitch/generic).
    pub kind: u8,
}

/// The RM side of the bookkeeping.
pub trait VmOps {
    /// One RM mapping (handles needed to undo it).
    type Map;
    /// One RM sparse range.
    type Sparse;
    /// Map `[start, start+len)` to `b`, inside `sparse` if given.
    fn map(&mut self, start: u64, len: u64, b: Backing, sparse: Option<&Self::Sparse>) -> Result<Self::Map, i32>;
    /// Undo a mapping made by `map` with the same `sparse`.
    fn unmap(&mut self, m: Self::Map, sparse: Option<&Self::Sparse>);
    fn sparse_new(&mut self, start: u64, len: u64) -> Result<Self::Sparse, i32>;
    fn sparse_free(&mut self, s: Self::Sparse);
}

#[derive(Debug)]
pub struct Mapping<M> {
    pub end: u64,
    pub back: Backing,
    pub tok: M,
}

#[derive(Debug)]
pub struct Region<S> {
    pub end: u64,
    pub tok: S,
}

/// One DRM file's VA bookkeeping. `lo..hi` is the range binds may use; the
/// kernel-managed window `[kern_lo, kern_hi)` (VM_INIT) is refused.
pub struct VmState<M, S> {
    pub maps: BTreeMap<u64, Mapping<M>>,
    pub regions: BTreeMap<u64, Region<S>>,
    pub lo: u64,
    pub hi: u64,
    pub kern_lo: u64,
    pub kern_hi: u64,
}

impl<M, S> VmState<M, S> {
    pub fn new(lo: u64, hi: u64, kern_lo: u64, kern_hi: u64) -> Self {
        VmState { maps: BTreeMap::new(), regions: BTreeMap::new(), lo, hi, kern_lo, kern_hi }
    }

    /// Validate a user range: page aligned, non-empty, inside `lo..hi`, and
    /// clear of the kernel window.
    pub fn check_range(&self, start: u64, len: u64) -> Result<u64, i32> {
        if len == 0 || start % PAGE != 0 || len % PAGE != 0 {
            return Err(EINVAL);
        }
        let end = start.checked_add(len).ok_or(EINVAL)?;
        if start < self.lo || end > self.hi {
            return Err(EINVAL);
        }
        if start < self.kern_hi && end > self.kern_lo {
            return Err(EINVAL);
        }
        Ok(end)
    }

    /// The sparse region (start) wholly containing `[start, end)`; `Ok(None)`
    /// if the range touches no region; `Err` if it straddles a boundary.
    pub fn region_of(&self, start: u64, end: u64) -> Result<Option<u64>, i32> {
        // A region starting at or before `start`.
        if let Some((&rs, r)) = self.regions.range(..=start).next_back() {
            if start < r.end {
                return if end <= r.end { Ok(Some(rs)) } else { Err(EINVAL) };
            }
        }
        // A region starting inside the range.
        if self.regions.range(start..end).next().is_some() {
            return Err(EINVAL);
        }
        Ok(None)
    }

    /// The region a mapping starting at `start` lives in (a mapping never
    /// crosses a region boundary, so its start decides).
    fn region_at(&self, start: u64) -> Option<u64> {
        let (&rs, r) = self.regions.range(..=start).next_back()?;
        if start < r.end { Some(rs) } else { None }
    }

    /// Mappings overlapping `[start, end)`, by start address.
    fn overlapping(&self, start: u64, end: u64) -> Vec<u64> {
        let mut v = Vec::new();
        if let Some((&ms, m)) = self.maps.range(..start).next_back() {
            if m.end > start {
                v.push(ms);
            }
        }
        for (&ms, _) in self.maps.range(start..end) {
            v.push(ms);
        }
        v
    }

    /// Unmap `[start, end)` (no validation): cut back or remove every
    /// overlapping mapping, re-mapping the parts that survive.
    fn unmap_range<O: VmOps<Map = M, Sparse = S>>(&mut self, ops: &mut O, start: u64, end: u64) -> Result<(), i32> {
        let mut first_err = Ok(());
        for ms in self.overlapping(start, end) {
            let m = self.maps.remove(&ms).expect("overlapping mapping");
            let me = m.end;
            let back = m.back;
            let reg = self.region_at(ms);
            ops.unmap(m.tok, reg.and_then(|r| self.regions.get(&r)).map(|r| &r.tok));
            // The piece before the range.
            if ms < start {
                let piece = Backing { offset: back.offset, ..back };
                match ops.map(ms, start - ms, piece, reg.and_then(|r| self.regions.get(&r)).map(|r| &r.tok)) {
                    Ok(tok) => {
                        self.maps.insert(ms, Mapping { end: start, back: piece, tok });
                    }
                    Err(e) => first_err = first_err.and(Err(e)),
                }
            }
            // The piece after it.
            if me > end {
                let piece = Backing { offset: back.offset + (end - ms), ..back };
                match ops.map(end, me - end, piece, reg.and_then(|r| self.regions.get(&r)).map(|r| &r.tok)) {
                    Ok(tok) => {
                        self.maps.insert(end, Mapping { end: me, back: piece, tok });
                    }
                    Err(e) => first_err = first_err.and(Err(e)),
                }
            }
        }
        first_err
    }

    /// `DRM_NOUVEAU_VM_BIND_OP_MAP` of a buffer.
    pub fn op_map<O: VmOps<Map = M, Sparse = S>>(&mut self, ops: &mut O, start: u64, len: u64, back: Backing) -> Result<(), i32> {
        let end = self.check_range(start, len)?;
        if back.offset % PAGE != 0 {
            return Err(EINVAL);
        }
        let reg = self.region_of(start, end)?;
        self.unmap_range(ops, start, end)?;
        let tok = ops.map(start, len, back, reg.and_then(|r| self.regions.get(&r)).map(|r| &r.tok))?;
        self.maps.insert(start, Mapping { end, back, tok });
        Ok(())
    }

    /// `DRM_NOUVEAU_VM_BIND_OP_UNMAP` (not sparse).
    pub fn op_unmap<O: VmOps<Map = M, Sparse = S>>(&mut self, ops: &mut O, start: u64, len: u64) -> Result<(), i32> {
        let end = self.check_range(start, len)?;
        self.unmap_range(ops, start, end)
    }

    /// `MAP | SPARSE`: a new sparse region.
    pub fn op_sparse_map<O: VmOps<Map = M, Sparse = S>>(&mut self, ops: &mut O, start: u64, len: u64) -> Result<(), i32> {
        let end = self.check_range(start, len)?;
        if let Some((_, r)) = self.regions.range(..end).next_back() {
            if r.end > start {
                return Err(EINVAL); // overlaps an existing region
            }
        }
        // Mappings already in the range would straddle the new boundary or
        // change meaning; uvmm refuses a region over live mappings.
        if !self.overlapping(start, end).is_empty() {
            return Err(EINVAL);
        }
        let tok = ops.sparse_new(start, len)?;
        self.regions.insert(start, Region { end, tok });
        Ok(())
    }

    /// `UNMAP | SPARSE`: destroy the region `[start, start+len)` exactly.
    pub fn op_sparse_unmap<O: VmOps<Map = M, Sparse = S>>(&mut self, ops: &mut O, start: u64, len: u64) -> Result<(), i32> {
        let end = self.check_range(start, len)?;
        match self.regions.get(&start) {
            Some(r) if r.end == end => {}
            _ => return Err(ENOENT),
        }
        let r = self.unmap_range(ops, start, end);
        let reg = self.regions.remove(&start).expect("region");
        ops.sparse_free(reg.tok);
        r
    }

    /// Drop every mapping of buffer `bo` (it is being freed).
    pub fn forget_bo<O: VmOps<Map = M, Sparse = S>>(&mut self, ops: &mut O, bo: u32) {
        let dead: Vec<u64> = self.maps.iter().filter(|(_, m)| m.back.bo == bo).map(|(s, _)| *s).collect();
        for ms in dead {
            let m = self.maps.remove(&ms).expect("mapping");
            let reg = self.region_at(ms);
            ops.unmap(m.tok, reg.and_then(|r| self.regions.get(&r)).map(|r| &r.tok));
        }
    }

    /// Tear everything down (`uapi` instead frees the whole RM client and
    /// drops the bookkeeping; kept for the host tests and future callers).
    #[allow(dead_code)]
    pub fn clear<O: VmOps<Map = M, Sparse = S>>(&mut self, ops: &mut O) {
        let starts: Vec<u64> = self.maps.keys().copied().collect();
        for ms in starts {
            let m = self.maps.remove(&ms).expect("mapping");
            let reg = self.region_at(ms);
            ops.unmap(m.tok, reg.and_then(|r| self.regions.get(&r)).map(|r| &r.tok));
        }
        while let Some((_, r)) = self.regions.pop_first() {
            ops.sparse_free(r.tok);
        }
    }

    /// Bytes mapped (for the log).
    pub fn mapped_bytes(&self) -> u64 {
        self.maps.iter().map(|(s, m)| m.end - s).sum()
    }
}
