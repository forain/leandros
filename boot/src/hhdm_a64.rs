//! AArch64 HHDM walker and gap filler (4 KiB granule, TTBR1).
//!
//! The kernel keeps the bootloader's TTBR1 tables for its whole life, so any
//! RAM the bootloader's higher-half direct map leaves out is unreachable
//! through `phys + hhdm` -- the buddy's bitmap, its free-list headers and
//! `phys_to_virt` would fault on it. x86_64 has had the same check and fill
//! since lane fbmm (the HP with Fast Boot OFF: Limine's HHDM mapped nothing
//! above 4 GiB); this is the aarch64 equivalent.
//!
//! Pure table logic, no asm and no globals: the caller passes the root, the
//! VA size (from TCR_EL1) and a `table(phys) -> Option<*mut u64>` that
//! says whether a table page is readable (through the HHDM) and where. The
//! module is included verbatim by `boot/memmap-tests` so the host tests run
//! exactly this code.

/// Physical output-address bits of a 4 KiB-granule descriptor (no LPA2).
pub const OA_MASK: u64 = 0x0000_FFFF_FFFF_F000;
/// Leaf attribute bits worth copying from an existing HHDM leaf: lower
/// attributes [11:2] and upper [63:53] minus the contiguous hint (52) --
/// the copy is placed alone, never as part of a contiguous run.
pub const ATTR_MASK: u64 = 0xFFE0_0000_0000_0FFC;

/// Virtual-address bits of the TTBR1 region described by `tcr` (TCR_EL1), or
/// why the format is not one this walker handles (only TG1 = 4 KiB, DS = 0).
pub fn va_bits_of(tcr: u64) -> Result<u32, &'static str> {
    let t1sz = (tcr >> 16) & 0x3F;
    let tg1 = (tcr >> 30) & 3;
    if tg1 != 0b10 { return Err("TG1 is not 4 KiB"); }
    if (tcr >> 59) & 1 != 0 { return Err("TCR.DS (LPA2) set"); }
    let bits = 64 - t1sz;
    if !(22..=48).contains(&bits) { return Err("T1SZ out of range"); }
    Ok(bits as u32)
}

/// Level a walk of a `bits`-bit region starts at.
#[inline]
pub fn start_of(bits: u32) -> u8 { (4 - (bits - 12 + 8) / 9) as u8 }

#[inline]
fn shift(level: u8) -> u32 { 12 + 9 * (3 - level as u32) }

/// Size of a leaf at `level` (L1 1 GiB, L2 2 MiB, L3 4 KiB).
#[inline]
pub fn leaf_size(level: u8) -> u64 { 1u64 << shift(level) }

#[inline]
fn index(va: u64, level: u8, start: u8, bits: u32) -> usize {
    let s = shift(level);
    let width = if level == start { bits - s } else { 9 };
    ((va >> s) & ((1u64 << width) - 1)) as usize
}

/// One walk's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Walk {
    /// Leaf size if mapped, else 0.
    pub page: u64,
    /// Level the walk stopped at (the leaf, the invalid entry or the table it
    /// could not read).
    pub level: u8,
    /// The descriptor found there (0 when the table was unreadable).
    pub desc: u64,
    /// A table pointer was outside what `table` accepts.
    pub bad_table: bool,
}

impl Walk {
    /// Span of VA the stop point covers: the leaf, or the hole at `level`.
    pub fn span(&self) -> u64 { if self.page != 0 { self.page } else { leaf_size(self.level) } }
}

/// Walk `va` through the tables at `root`.
///
/// # Safety
/// Every pointer `table` returns must address 512 readable u64s.
pub unsafe fn walk(root: u64, bits: u32, va: u64, table: &dyn Fn(u64) -> Option<*mut u64>) -> Walk {
    let start = start_of(bits);
    let mut t = root & OA_MASK;
    let mut level = start;
    loop {
        let p = match table(t) {
            Some(p) => p,
            None => return Walk { page: 0, level, desc: 0, bad_table: true },
        };
        let d = core::ptr::read_volatile(p.add(index(va, level, start, bits)));
        if d & 1 == 0 { return Walk { page: 0, level, desc: d, bad_table: false }; }
        let is_table = d & 2 != 0 && level < 3;
        if !is_table {
            // A block at L0 or a 0b01 at L3 is reserved/invalid encoding.
            if level == 0 || (level == 3 && d & 2 == 0) {
                return Walk { page: 0, level, desc: d, bad_table: false };
            }
            return Walk { page: leaf_size(level), level, desc: d, bad_table: false };
        }
        t = d & OA_MASK;
        level += 1;
    }
}

/// What a fill did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FillStats {
    pub gib: u64,
    pub mib2: u64,
    pub kib4: u64,
    pub tables: u64,
    /// Bytes that could not be mapped (unreadable table, pool exhausted,
    /// or an invalid encoding in the way).
    pub failed: u64,
    /// Input ranges in which at least one entry was written.
    pub patched: u64,
}

/// Map `[s, t)` of every range at `hhdm + phys` into the empty slots of the
/// tables at `root`; every valid entry is left exactly as it is. Leaves get
/// `attrs` (see [`ATTR_MASK`]) plus the descriptor type; 1 GiB blocks only if
/// `gib`. New table pages come from `alloc` (a physical page that `table`
/// accepts; it is zeroed here). Ranges must be 4 KiB aligned.
///
/// # Safety
/// The tables must be the live ones only if the caller then issues the
/// barriers (`dsb ishst; isb`); `table` as for [`walk`].
pub unsafe fn fill(root: u64, bits: u32, hhdm: u64, ranges: &[(u64, u64)], attrs: u64, gib: bool,
                   table: &dyn Fn(u64) -> Option<*mut u64>,
                   alloc: &mut dyn FnMut() -> Option<u64>) -> FillStats {
    let start = start_of(bits);
    let attrs = attrs & ATTR_MASK;
    let mut st = FillStats::default();
    for &(s, t) in ranges {
        let mut wrote = false;
        let mut a = s;
        'range: while a < t {
            let va = hhdm.wrapping_add(a);
            let mut tp = root & OA_MASK;
            let mut level = start;
            loop {
                let size = leaf_size(level);
                let next = ((a & !(size - 1)) + size).min(t);
                let p = match table(tp) {
                    Some(p) => p,
                    None => { st.failed += next - a; a = next; continue 'range; }
                };
                let slot = p.add(index(va, level, start, bits));
                let d = core::ptr::read_volatile(slot);
                if d & 1 != 0 {
                    if d & 2 != 0 && level < 3 { tp = d & OA_MASK; level += 1; continue; }
                    if level == 0 || (level == 3 && d & 2 == 0) { st.failed += next - a; }
                    a = next;
                    continue 'range;
                }
                // Empty slot: a leaf if this level allows one and it fits.
                let aligned = a & (size - 1) == 0 && va & (size - 1) == 0 && a + size <= t;
                let leaf_ok = match level { 1 => gib, 2 | 3 => true, _ => false };
                if leaf_ok && aligned {
                    let ty = if level == 3 { 0b11 } else { 0b01 };
                    core::ptr::write_volatile(slot, (a & OA_MASK) | attrs | ty);
                    match level { 1 => st.gib += 1, 2 => st.mib2 += 1, _ => st.kib4 += 1 }
                    wrote = true;
                    a += size;
                    continue 'range;
                }
                if level == 3 { st.failed += next - a; a = next; continue 'range; }
                let np = match alloc() {
                    Some(np) => np,
                    None => { st.failed += next - a; a = next; continue 'range; }
                };
                match table(np) {
                    Some(q) => for i in 0..512 { core::ptr::write_volatile(q.add(i), 0); },
                    None => { st.failed += next - a; a = next; continue 'range; }
                }
                core::ptr::write_volatile(slot, (np & OA_MASK) | 0b11);
                st.tables += 1;
                wrote = true;
                tp = np;
                level += 1;
            }
        }
        if wrote { st.patched += 1; }
    }
    st
}

/// Walk `[s, t)` and call `hole(base, end, walk)` for each maximal unmapped
/// run (adjacent holes merged). Returns the number of holes.
///
/// # Safety
/// As for [`walk`].
pub unsafe fn holes(root: u64, bits: u32, hhdm: u64, s: u64, t: u64,
                    table: &dyn Fn(u64) -> Option<*mut u64>,
                    hole: &mut dyn FnMut(u64, u64, Walk)) -> usize {
    let mut n = 0usize;
    let mut cur: Option<(u64, u64, Walk)> = None;
    let mut a = s;
    while a < t {
        let w = walk(root, bits, hhdm.wrapping_add(a), table);
        let span = w.span();
        let next = ((a & !(span - 1)) + span).min(t);
        if w.page == 0 {
            cur = match cur {
                Some((b, e, w0)) if e == a => Some((b, next, w0)),
                Some((b, e, w0)) => { hole(b, e, w0); n += 1; Some((a, next, w)) }
                None => Some((a, next, w)),
            };
        }
        a = next;
    }
    if let Some((b, e, w0)) = cur { hole(b, e, w0); n += 1; }
    n
}

/// Test hook: zero the valid entry at `level` on the walk of `va` (an L1
/// entry drops a whole GiB of the HHDM). Returns whether one was cleared.
/// The caller invalidates the TLB.
///
/// # Safety
/// Destroys live mappings; as for [`walk`].
pub unsafe fn clear_at(root: u64, bits: u32, va: u64, level: u8,
                       table: &dyn Fn(u64) -> Option<*mut u64>) -> bool {
    let start = start_of(bits);
    let mut t = root & OA_MASK;
    let mut l = start;
    loop {
        let p = match table(t) { Some(p) => p, None => return false };
        let slot = p.add(index(va, l, start, bits));
        let d = core::ptr::read_volatile(slot);
        if d & 1 == 0 { return false; }
        if l == level { core::ptr::write_volatile(slot, 0); return true; }
        if d & 2 == 0 || l >= 3 { return false; }
        t = d & OA_MASK;
        l += 1;
    }
}
