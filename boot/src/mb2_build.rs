//! Multiboot2 boot-information builder (kexec, x86_64).
//!
//! The inverse of [`crate::multiboot2::parse`]: describes the machine to the
//! next kernel (`kernel-direct`, entered at `_start_pvh` with EAX=0x36d76289
//! and EBX=MBI). Pure and allocation-free: everything is written into a
//! caller-provided byte buffer, scratch for the memory-map sweep lives in a
//! caller-provided [`Mb2Scratch`]. The MBI contains no self-pointers, so the
//! buffer may be filled anywhere and copied to its final physical page.
//!
//! Memory-map rules (kexec design B#2/B#8/B#9):
//!   * Raw Limine types are mapped to MB2 types: 0 usable, 5 bootloader
//!     reclaimable and 6 executable+modules become Available (the old kernel
//!     is dead after the jump); 2 -> ACPI reclaimable, 3 -> ACPI NVS,
//!     4 -> bad; 1, 7 and anything unknown -> Reserved.
//!   * RAM above 2 GiB (and 4 GiB) passes through: the direct-boot
//!     trampoline maps only 0-2 GiB, but `kernel/src/direct_hhdm.rs` extends
//!     the HHDM over every RAM range of this map before `mm` runs.
//!   * Below 1 MiB the loader's types pass through (the kernel's buddy never
//!     allocates there; SMP needs 0x7000 Available), page 0 is Reserved.
//!   * The initrd (page-rounded) and any caller `reserve` ranges are Reserved.
//!   * At most 128 entries (the parser's static array); if the sweep yields
//!     more, the smallest Available ranges are dropped (not reported as RAM).

use crate::memmap::{self, MemoryRegion, MemoryType};

/// Always reported Reserved: page 0 (real-mode IVT, BDA).
pub const MB2_LOW_PAGE0: u64 = 0x1000;
/// `boot::multiboot2::parse` keeps at most this many tag-6 entries.
pub const MB2_MAX_ENTRIES: usize = 128;
/// Largest raw input map accepted (Limine reports far fewer).
pub const MB2_MAX_RAW: usize = 512;
const CAP: usize = MB2_MAX_RAW + 32;

/// One raw Limine memmap entry (`type_` as in `limine.rs`: 0 usable,
/// 1 reserved, 2 ACPI reclaimable, 3 ACPI NVS, 4 bad, 5 bootloader
/// reclaimable, 6 executable+modules, 7 framebuffer).
#[derive(Clone, Copy, Debug)]
pub struct RawRegion {
    pub base: u64,
    pub length: u64,
    pub raw_type: u32,
}

/// Framebuffer tag (type 1 = direct RGB, XRGB8888 layout 16/8, 8/8, 0/8).
#[derive(Clone, Copy, Debug)]
pub struct Mb2Fb {
    pub addr: u64,
    pub pitch: u32,
    pub width: u32,
    pub height: u32,
    pub bpp: u8,
}

pub struct Mb2Input<'a> {
    /// Command line without the NUL (empty = no tag 1).
    pub cmdline: &'a [u8],
    /// Boot loader name for tag 2 (empty = no tag).
    pub loader_name: &'a [u8],
    /// Initrd `(phys start, length)`; must end below 4 GiB.
    pub initrd: Option<(u64, u64)>,
    /// The current kernel's memory map, raw Limine types.
    pub regions: &'a [RawRegion],
    /// Extra `(base, length)` ranges to report Reserved (new kernel image,
    /// the MBI itself if above 1 MiB, ...).
    pub reserve: &'a [(u64, u64)],
    pub framebuffer: Option<Mb2Fb>,
    /// RSDP bytes (20 for ACPI 1.0, 36 for 2.0+); copied into tag 14/15.
    pub rsdp: Option<&'a [u8]>,
    /// EFI system table physical address (tag 12), 0 = none.
    pub efi64: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mb2Error {
    BufferTooSmall,
    TooManyRegions,
    InitrdAbove4G,
}

/// Scratch for the memory-map sweep (~40 KiB: keep it off small stacks).
pub struct Mb2Scratch {
    work: [MemoryRegion; CAP],
    bounds: [u64; 2 * CAP],
    out: [MemoryRegion; CAP],
}

impl Mb2Scratch {
    /// Initialise a scratch in place at `p` without building it on the stack
    /// (a by-value `Mb2Scratch` is ~40 KiB; kernel stacks are small).
    ///
    /// # Safety
    /// `p` must be valid for writes of `size_of::<Mb2Scratch>()` bytes and aligned.
    pub unsafe fn init_at(p: *mut Mb2Scratch) {
        let w = core::ptr::addr_of_mut!((*p).work) as *mut MemoryRegion;
        for i in 0..CAP { w.add(i).write(MemoryRegion::EMPTY); }
        let b = core::ptr::addr_of_mut!((*p).bounds) as *mut u64;
        for i in 0..2 * CAP { b.add(i).write(0); }
        let o = core::ptr::addr_of_mut!((*p).out) as *mut MemoryRegion;
        for i in 0..CAP { o.add(i).write(MemoryRegion::EMPTY); }
    }

    pub const fn new() -> Self {
        Mb2Scratch {
            work: [MemoryRegion::EMPTY; CAP],
            bounds: [0; 2 * CAP],
            out: [MemoryRegion::EMPTY; CAP],
        }
    }
}

/// Map a raw Limine type to the MB2 memory type.
pub fn mb2_kind(raw: u32) -> MemoryType {
    match raw {
        0 | 5 | 6 => MemoryType::Available,
        2 => MemoryType::AcpiReclaimable,
        3 => MemoryType::AcpiNvs,
        4 => MemoryType::BadMemory,
        _ => MemoryType::Reserved,
    }
}

fn push(work: &mut [MemoryRegion], n: &mut usize, base: u64, length: u64, kind: MemoryType)
    -> Result<(), Mb2Error>
{
    if length == 0 { return Ok(()); }
    if *n >= work.len() { return Err(Mb2Error::TooManyRegions); }
    work[*n] = MemoryRegion { base, length, kind };
    *n += 1;
    Ok(())
}

fn page_round(b: u64, l: u64) -> (u64, u64) {
    let start = b & !0xfff;
    let end = b.saturating_add(l).saturating_add(0xfff) & !0xfff;
    (start, end.saturating_sub(start))
}

/// Build the MB2 memory map (sorted, non-overlapping, <= 128 entries) into
/// `scratch.out`; returns the entry count.
fn build_map(input: &Mb2Input, s: &mut Mb2Scratch) -> Result<usize, Mb2Error> {
    if input.regions.len() > MB2_MAX_RAW { return Err(Mb2Error::TooManyRegions); }
    let mut n = 0usize;
    for r in input.regions {
        push(&mut s.work, &mut n, r.base, r.length, mb2_kind(r.raw_type))?;
    }
    // Below 1 MiB the loader's types pass through, page 0 (IVT/BDA) aside:
    // the kernel's buddy never allocates below 1 MiB, the AP trampoline
    // needs 0x7000-0x7FFF listed Available (arch/x86_64/src/lib.rs), and a
    // chained kexec needs the true low map to place its own MBI there.
    push(&mut s.work, &mut n, 0, MB2_LOW_PAGE0, MemoryType::Reserved)?;
    if let Some((b, l)) = input.initrd {
        let (b, l) = page_round(b, l);
        push(&mut s.work, &mut n, b, l, MemoryType::Reserved)?;
    }
    for &(b, l) in input.reserve {
        let (b, l) = page_round(b, l);
        push(&mut s.work, &mut n, b, l, MemoryType::Reserved)?;
    }
    let st = memmap::sanitize(&mut s.work[..n], &mut s.bounds, &mut s.out);
    if st.out_truncated != 0 { return Err(Mb2Error::TooManyRegions); }
    let mut cnt = st.output;
    while cnt > MB2_MAX_ENTRIES {
        // Drop the smallest Available range.
        let mut best: Option<usize> = None;
        for i in 0..cnt {
            if s.out[i].kind == MemoryType::Available
                && best.map_or(true, |b| s.out[i].length < s.out[b].length)
            { best = Some(i); }
        }
        let Some(b) = best else { return Err(Mb2Error::TooManyRegions) };
        // An absent range is not RAM to the parser: same effect as Reserved,
        // without needing a neighbour to merge with.
        for i in b..cnt - 1 { s.out[i] = s.out[i + 1]; }
        cnt -= 1;
    }
    Ok(cnt)
}

struct W<'a> { buf: &'a mut [u8], off: usize, err: bool }

impl<'a> W<'a> {
    fn bytes(&mut self, b: &[u8]) {
        if self.off + b.len() > self.buf.len() { self.err = true; return; }
        self.buf[self.off..self.off + b.len()].copy_from_slice(b);
        self.off += b.len();
    }
    fn u8(&mut self, v: u8) { self.bytes(&[v]); }
    fn u16(&mut self, v: u16) { self.bytes(&v.to_le_bytes()); }
    fn u32(&mut self, v: u32) { self.bytes(&v.to_le_bytes()); }
    fn u64(&mut self, v: u64) { self.bytes(&v.to_le_bytes()); }
    /// Start a tag; returns its offset for `end_tag`.
    fn tag(&mut self, typ: u32) -> usize {
        let at = self.off;
        self.u32(typ);
        self.u32(0);
        at
    }
    /// Patch the size and pad to 8 bytes.
    fn end_tag(&mut self, at: usize) {
        if self.err { return; }
        let size = (self.off - at) as u32;
        self.buf[at + 4..at + 8].copy_from_slice(&size.to_le_bytes());
        while self.off % 8 != 0 && !self.err { self.u8(0); }
    }
}

/// Build the MBI into `buf` (which should be 8-byte aligned wherever it is
/// finally placed) and return its length in bytes.
pub fn build_mbi(input: &Mb2Input, scratch: &mut Mb2Scratch, buf: &mut [u8])
    -> Result<usize, Mb2Error>
{
    if let Some((b, l)) = input.initrd {
        if b.saturating_add(l) > (1u64 << 32) { return Err(Mb2Error::InitrdAbove4G); }
    }
    let cnt = build_map(input, scratch)?;
    let mut w = W { buf, off: 0, err: false };
    w.u32(0); // total_size, patched below
    w.u32(0);

    if !input.cmdline.is_empty() {
        let t = w.tag(1);
        w.bytes(input.cmdline);
        w.u8(0);
        w.end_tag(t);
    }
    if !input.loader_name.is_empty() {
        let t = w.tag(2);
        w.bytes(input.loader_name);
        w.u8(0);
        w.end_tag(t);
    }
    if let Some((b, l)) = input.initrd {
        let t = w.tag(3);
        w.u32(b as u32);
        w.u32((b + l) as u32);
        w.bytes(b"initrd\0");
        w.end_tag(t);
    }
    {
        let t = w.tag(6);
        w.u32(24); // entry_size
        w.u32(0);  // entry_version
        for e in &scratch.out[..cnt] {
            w.u64(e.base);
            w.u64(e.length);
            w.u32(e.kind as u32);
            w.u32(0);
        }
        w.end_tag(t);
    }
    if let Some(fb) = input.framebuffer {
        let t = w.tag(8);
        w.u64(fb.addr);
        w.u32(fb.pitch);
        w.u32(fb.width);
        w.u32(fb.height);
        w.u8(fb.bpp);
        w.u8(1); // direct RGB
        w.u16(0);
        w.bytes(&[16, 8, 8, 8, 0, 8]); // red, green, blue pos/size
        w.end_tag(t);
    }
    if let Some(r) = input.rsdp {
        if r.len() >= 36 && r[15] >= 2 {
            let t = w.tag(15);
            w.bytes(&r[..36]);
            w.end_tag(t);
        } else if r.len() >= 20 {
            let t = w.tag(14);
            w.bytes(&r[..20]);
            w.end_tag(t);
        }
    }
    if input.efi64 != 0 {
        let t = w.tag(12);
        w.u64(input.efi64);
        w.end_tag(t);
    }
    let t = w.tag(0);
    w.end_tag(t);
    if w.err { return Err(Mb2Error::BufferTooSmall); }
    let total = w.off;
    w.buf[0..4].copy_from_slice(&(total as u32).to_le_bytes());
    Ok(total)
}

// ── Physical placement below 1 MiB ───────────────────────────────────────────

/// Low physical ranges `[start, end)` that the direct-boot kernel
/// (`entry_x86_64.s`, `arch/x86_64/src/smp.rs`) or the PC itself uses, and
/// that an MBI must therefore avoid:
///   * `0x0000-0x0FFF`   real-mode IVT, BDA
///   * `0x7000-0x7FFF`   AP startup trampoline (+ GDT/params at 0x7E00-0x7F1F)
///   * `0x10000-0x10FFF` boot GDT written by `_start_pvh` (24 bytes)
///   * `0x20000-0x27FFF` early page tables (`rep stosd` clears 32 KiB)
///   * `0x8F000-0x90FFF` early stack (ESP=0x90000, grows down)
///   * `0x9F000-0xFFFFF` EBDA, video, option ROMs, BIOS
pub const MB2_LOW_USED: [(u32, u32); 6] = [
    (0x0000, 0x1000),
    (0x7000, 0x8000),
    (0x10000, 0x11000),
    (0x20000, 0x28000),
    (0x8F000, 0x91000),
    (0x9F000, 0x100000),
];

/// First 4 KiB-aligned physical address below 1 MiB where `size` bytes fit
/// clear of [`MB2_LOW_USED`] and `extra_avoid`. The search starts at 0x28000
/// (the large clean gap above the page tables).
pub fn pick_mbi_phys(size: usize, extra_avoid: &[(u32, u32)]) -> Option<u32> {
    let size = size as u64;
    let mut cand: u64 = 0x28000;
    while cand + size <= 0x9F000 {
        let end = cand + size;
        let mut next: Option<u64> = None;
        for &(s, e) in MB2_LOW_USED.iter().chain(extra_avoid.iter()) {
            if cand < e as u64 && (s as u64) < end {
                next = Some(next.map_or(e as u64, |h| h.max(e as u64)));
            }
        }
        match next {
            None => return Some(cand as u32),
            Some(e) => cand = (e + 0xfff) & !0xfff,
        }
    }
    None
}
