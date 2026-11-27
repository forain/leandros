//! AArch64 stage-2 translation table builder (4 KiB granule), pure logic.
//!
//! The builder owns the tables through a [`FrameAlloc`]; nothing here touches
//! system registers or TLBs. The caller (kernel glue) invalidates the TLB with
//! `arch_aarch64::kvm::tlb` after anything that removes or downgrades a
//! mapping, and unpins the frames reported by [`Stage2::unmap`].
//!
//! Geometry (4 KiB granule, IPA size `ipa_bits` in 32..=48):
//!   * start level 2 for ipa_bits <= 34, level 1 for <= 43, else level 0;
//!   * the root is `2^(ipa_bits - bits_covered_by_one_start_table)` concatenated
//!     4 KiB pages (1 when the IPA space fits one table), which must be
//!     physically contiguous and aligned to their own size
//!     (40 bits: level 1, two pages, T0SZ 24, SL0 = 1).
//!
//! Leaf descriptors: bits[1:0]=0b11, MemAttr[5:2] (0b1111 Normal WB, 0b0001
//! Device-nGnRE), S2AP[7:6], SH[9:8]=IS for Normal, AF[10]=1, XN[54].

pub const PAGE_SIZE: u64 = 4096;
const ENTRIES: usize = 512;
const ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

pub const DESC_VALID: u64 = 1 << 0;
pub const DESC_TABLE: u64 = 1 << 1; // with VALID: table (L0-L2) or page (L3)
pub const DESC_AF: u64 = 1 << 10;
pub const DESC_XN: u64 = 1 << 54;
pub const S2AP_SHIFT: u32 = 6;
pub const SH_SHIFT: u32 = 8;
pub const SH_INNER: u64 = 0b11;
pub const MEMATTR_SHIFT: u32 = 2;
pub const MEMATTR_NORMAL_WB: u64 = 0b1111;
pub const MEMATTR_DEVICE_NGNRE: u64 = 0b0001;

/// Access permission of a stage-2 leaf. `read` must be set for any mapping
/// (S2AP cannot express write-only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Perm {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
}

impl Perm {
    pub const RO: Perm = Perm { read: true, write: false, exec: false };
    pub const RW: Perm = Perm { read: true, write: true, exec: false };
    pub const RX: Perm = Perm { read: true, write: false, exec: true };
    pub const RWX: Perm = Perm { read: true, write: true, exec: true };

    fn s2ap(self) -> u64 {
        (self.read as u64) | ((self.write as u64) << 1)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attr {
    Normal,
    Device,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum S2Err {
    /// `ipa_bits` outside 32..=48.
    BadIpaBits,
    /// ipa/pa/len not page aligned.
    Unaligned,
    /// ipa beyond the configured IPA size.
    IpaRange,
    /// pa beyond 48 bits.
    PaRange,
    /// Perm without read, or exec on Device memory.
    BadPerm,
    /// Frame allocator exhausted.
    NoMem,
    /// A different frame is already mapped at this IPA (unmap it first).
    Exists,
    /// Allocator returned a root that is not aligned to its own size.
    RootAlign,
}

/// Table frame allocator. `ptr(pa)` must give a writable pointer to a frame
/// previously returned by `alloc_zeroed`/`alloc_zeroed_pages`.
pub trait FrameAlloc {
    /// One zeroed 4 KiB frame: (physical address, pointer to its 512 entries).
    fn alloc_zeroed(&mut self) -> Option<(u64, *mut u64)>;
    fn free(&mut self, pa: u64);
    /// Pointer to the entries of table frame `pa`.
    fn ptr(&self, pa: u64) -> *mut u64;
    /// `n` (power of two) physically contiguous zeroed frames aligned to
    /// `n * 4 KiB`. Only needed for concatenated roots (n > 1).
    fn alloc_zeroed_pages(&mut self, n: usize) -> Option<(u64, *mut u64)> {
        if n == 1 { self.alloc_zeroed() } else { None }
    }
    fn free_pages(&mut self, pa: u64, n: usize) {
        if n == 1 { self.free(pa) }
    }
}

/// Geometry derived from the IPA size.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Geometry {
    pub ipa_bits: u32,
    /// First level walked (0, 1 or 2).
    pub start_level: u32,
    /// Concatenated root pages (power of two, 1..=16).
    pub root_pages: usize,
}

impl Geometry {
    pub fn new(ipa_bits: u32) -> Result<Geometry, S2Err> {
        if !(32..=48).contains(&ipa_bits) {
            return Err(S2Err::BadIpaBits);
        }
        let start_level = if ipa_bits <= 34 { 2 } else if ipa_bits <= 43 { 1 } else { 0 };
        let covered = 12 + 9 * (4 - start_level); // bits one start table spans
        let root_pages = if ipa_bits > covered { 1usize << (ipa_bits - covered) } else { 1 };
        Ok(Geometry { ipa_bits, start_level, root_pages })
    }

    /// VTCR_EL2.SL0 encoding for the 4 KiB granule.
    pub fn sl0(&self) -> u64 {
        match self.start_level { 2 => 0b00, 1 => 0b01, _ => 0b10 }
    }
}

/// Size in bytes of the range one entry of `level` covers.
const fn level_span(level: u32) -> u64 {
    1u64 << (12 + 9 * (3 - level))
}

pub struct Stage2<A: FrameAlloc> {
    alloc: A,
    geo: Geometry,
    root_pa: u64,
    root: *mut u64,
    /// Number of valid leaf entries (for tests and `is_empty`).
    leaves: usize,
}

// The raw pointers refer to frames owned by `alloc`.
unsafe impl<A: FrameAlloc + Send> Send for Stage2<A> {}

impl<A: FrameAlloc> Stage2<A> {
    pub fn new(mut alloc: A, ipa_bits: u32) -> Result<Self, (S2Err, A)> {
        let geo = match Geometry::new(ipa_bits) { Ok(g) => g, Err(e) => return Err((e, alloc)) };
        let (pa, p) = match alloc.alloc_zeroed_pages(geo.root_pages) {
            Some(x) => x,
            None => return Err((S2Err::NoMem, alloc)),
        };
        if pa % (geo.root_pages as u64 * PAGE_SIZE) != 0 {
            alloc.free_pages(pa, geo.root_pages);
            return Err((S2Err::RootAlign, alloc));
        }
        Ok(Stage2 { alloc, geo, root_pa: pa, root: p, leaves: 0 })
    }

    pub fn geometry(&self) -> Geometry { self.geo }
    pub fn leaf_count(&self) -> usize { self.leaves }
    pub fn allocator(&self) -> &A { &self.alloc }
    pub fn allocator_mut(&mut self) -> &mut A { &mut self.alloc }

    /// Physical address of the root table, for VTTBR_EL2.BADDR.
    pub fn vttbr_baddr(&self) -> u64 { self.root_pa }

    /// VTTBR_EL2 value for `vmid` (16-bit VMID field when VTCR.VS is set,
    /// 8-bit otherwise; the caller guarantees the vmid fits).
    pub fn vttbr(&self, vmid: u16) -> u64 { self.root_pa | ((vmid as u64) << 48) }

    /// VTCR_EL2 for this geometry. `parange` is ID_AA64MMFR0_EL1.PARange
    /// (0..=6); `vmid16` sets VS (needs ID_AA64MMFR1_EL1.VMIDBits == 2).
    pub fn vtcr_with(&self, parange: u64, vmid16: bool) -> u64 {
        let t0sz = (64 - self.geo.ipa_bits) as u64;
        t0sz
            | (self.geo.sl0() << 6)
            | (0b01 << 8)   // IRGN0 WB RA WA
            | (0b01 << 10)  // ORGN0 WB RA WA
            | (0b11 << 12)  // SH0 inner shareable
            | (0b00 << 14)  // TG0 4 KiB
            | (parange.min(6) << 16) // PS
            | ((vmid16 as u64) << 19) // VS
            | (1u64 << 31)  // RES1
    }

    /// [`vtcr_with`](Self::vtcr_with) with an 8-bit VMID.
    pub fn vtcr(&self, parange: u64) -> u64 { self.vtcr_with(parange, false) }

    fn root_index(&self, ipa: u64) -> usize {
        let shift = 12 + 9 * (3 - self.geo.start_level);
        ((ipa >> shift) as usize) & (self.geo.root_pages * ENTRIES - 1)
    }

    fn leaf_desc(pa: u64, perm: Perm, attr: Attr) -> u64 {
        let mut d = (pa & ADDR_MASK) | DESC_VALID | DESC_TABLE | DESC_AF | (perm.s2ap() << S2AP_SHIFT);
        match attr {
            Attr::Normal => {
                d |= (MEMATTR_NORMAL_WB << MEMATTR_SHIFT) | (SH_INNER << SH_SHIFT);
            }
            Attr::Device => d |= MEMATTR_DEVICE_NGNRE << MEMATTR_SHIFT,
        }
        if !perm.exec { d |= DESC_XN; }
        d
    }

    /// Map one 4 KiB page. Mapping the same `pa` again updates permission and
    /// attribute (a downgrade needs a TLB invalidation by the caller); a
    /// different `pa` at a valid IPA is [`S2Err::Exists`].
    pub fn map(&mut self, ipa: u64, pa: u64, perm: Perm, attr: Attr) -> Result<(), S2Err> {
        if ipa % PAGE_SIZE != 0 || pa % PAGE_SIZE != 0 { return Err(S2Err::Unaligned); }
        if ipa >> self.geo.ipa_bits != 0 { return Err(S2Err::IpaRange); }
        if pa >> 48 != 0 { return Err(S2Err::PaRange); }
        if !perm.read || (perm.exec && attr == Attr::Device) { return Err(S2Err::BadPerm); }

        let mut table = self.root;
        let mut idx = self.root_index(ipa);
        for level in self.geo.start_level..3 {
            let e = unsafe { table.add(idx).read_volatile() };
            let next = if e & DESC_VALID != 0 {
                self.alloc.ptr(e & ADDR_MASK)
            } else {
                let (npa, np) = self.alloc.alloc_zeroed().ok_or(S2Err::NoMem)?;
                unsafe { table.add(idx).write_volatile(npa | DESC_VALID | DESC_TABLE); }
                np
            };
            table = next;
            idx = ((ipa >> (12 + 9 * (2 - level))) as usize) & (ENTRIES - 1);
        }
        let old = unsafe { table.add(idx).read_volatile() };
        if old & DESC_VALID != 0 {
            if old & ADDR_MASK != pa { return Err(S2Err::Exists); }
        } else {
            self.leaves += 1;
        }
        unsafe { table.add(idx).write_volatile(Self::leaf_desc(pa, perm, attr)); }
        Ok(())
    }

    /// Raw leaf descriptor for `ipa`, if mapped.
    pub fn walk_desc(&self, ipa: u64) -> Option<u64> {
        if ipa >> self.geo.ipa_bits != 0 { return None; }
        let mut table = self.root;
        let mut idx = self.root_index(ipa);
        for level in self.geo.start_level..3 {
            let e = unsafe { table.add(idx).read_volatile() };
            if e & DESC_VALID == 0 { return None; }
            table = self.alloc.ptr(e & ADDR_MASK);
            idx = ((ipa >> (12 + 9 * (2 - level))) as usize) & (ENTRIES - 1);
        }
        let e = unsafe { table.add(idx).read_volatile() };
        if e & DESC_VALID != 0 { Some(e) } else { None }
    }

    /// Translate `ipa` (any byte): the page's PA plus the in-page offset, and
    /// its permission.
    pub fn walk(&self, ipa: u64) -> Option<(u64, Perm)> {
        let d = self.walk_desc(ipa)?;
        let s2ap = (d >> S2AP_SHIFT) & 0b11;
        let perm = Perm { read: s2ap & 1 != 0, write: s2ap & 2 != 0, exec: d & DESC_XN == 0 };
        Some(((d & ADDR_MASK) | (ipa & (PAGE_SIZE - 1)), perm))
    }

    /// Change the permission of an existing leaf (e.g. grant X lazily).
    /// Returns false when `ipa` is unmapped or exec is requested on Device.
    /// Removing rights needs a TLB invalidation by the caller.
    pub fn protect(&mut self, ipa: u64, perm: Perm) -> bool {
        if !perm.read { return false; }
        let d = match self.walk_desc(ipa) { Some(d) => d, None => return false };
        let device = (d >> MEMATTR_SHIFT) & 0xF == MEMATTR_DEVICE_NGNRE;
        if device && perm.exec { return false; }
        let nd = (d & !((0b11 << S2AP_SHIFT) | DESC_XN))
            | (perm.s2ap() << S2AP_SHIFT)
            | if perm.exec { 0 } else { DESC_XN };
        // Re-walk to the slot (walk_desc returned the value only).
        let mut table = self.root;
        let mut idx = self.root_index(ipa);
        for level in self.geo.start_level..3 {
            let e = unsafe { table.add(idx).read_volatile() };
            table = self.alloc.ptr(e & ADDR_MASK);
            idx = ((ipa >> (12 + 9 * (2 - level))) as usize) & (ENTRIES - 1);
        }
        unsafe { table.add(idx).write_volatile(nd); }
        true
    }

    /// Unmap `[ipa, ipa+len)` (page aligned; clamped to the IPA space),
    /// calling `f(pa)` for every page that was mapped, then freeing table
    /// frames that became empty (never the root). Returns the page count.
    /// The caller must invalidate the TLB for the range before reusing or
    /// freeing the frames reported to `f`.
    pub fn unmap(&mut self, ipa: u64, len: u64, mut f: impl FnMut(u64)) -> usize {
        if len == 0 || ipa % PAGE_SIZE != 0 { return 0; }
        let limit = 1u64 << self.geo.ipa_bits;
        if ipa >= limit { return 0; }
        let end = ipa.saturating_add(len).min(limit);
        let end = (end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let mut count = 0usize;
        let sl = self.geo.start_level;
        let root_span = level_span(sl);
        let n = self.geo.root_pages * ENTRIES;
        let mut i = self.root_index(ipa);
        let last = self.root_index(end - 1);
        while i <= last && i < n {
            let base = i as u64 * root_span;
            let e = unsafe { self.root.add(i).read_volatile() };
            if e & DESC_VALID != 0 {
                let child_pa = e & ADDR_MASK;
                let empty = self.unmap_table(child_pa, sl + 1, base, ipa, end, &mut f, &mut count);
                if empty {
                    unsafe { self.root.add(i).write_volatile(0); }
                    self.alloc.free(child_pa);
                }
            }
            i += 1;
        }
        self.leaves -= count;
        count
    }

    /// Returns true when the table at `pa` is entirely empty afterwards.
    fn unmap_table(&mut self, pa: u64, level: u32, base: u64, start: u64, end: u64,
                   f: &mut impl FnMut(u64), count: &mut usize) -> bool {
        let table = self.alloc.ptr(pa);
        let span = level_span(level);
        let mut any_left = false;
        for i in 0..ENTRIES {
            let eb = base + i as u64 * span;
            let e = unsafe { table.add(i).read_volatile() };
            if e & DESC_VALID == 0 { continue; }
            if eb + span <= start || eb >= end { any_left = true; continue; }
            if level == 3 {
                unsafe { table.add(i).write_volatile(0); }
                f(e & ADDR_MASK);
                *count += 1;
            } else {
                let child = e & ADDR_MASK;
                if self.unmap_table(child, level + 1, eb, start, end, f, count) {
                    unsafe { table.add(i).write_volatile(0); }
                    self.alloc.free(child);
                } else {
                    any_left = true;
                }
            }
        }
        !any_left
    }

    /// Unmap everything (VM destroy), reporting every mapped page to `f`.
    pub fn unmap_all(&mut self, f: impl FnMut(u64)) -> usize {
        self.unmap(0, 1u64 << self.geo.ipa_bits, f)
    }
}

impl<A: FrameAlloc> Drop for Stage2<A> {
    fn drop(&mut self) {
        // Frees every table; pages still mapped are NOT reported (the caller
        // is expected to have run `unmap_all` to unpin them first).
        self.unmap_all(|_| {});
        self.alloc.free_pages(self.root_pa, self.geo.root_pages);
    }
}
