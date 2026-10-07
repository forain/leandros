//! Kernel virtual address allocator for device and scattered-page mappings:
//! `ioremap` / `iounmap` and `vmap` / `vunmap`.
//!
//! The HHDM is a fixed alias of physical memory, often built from 1 GiB / 2 MiB
//! pages, and this kernel refuses to split those (x86_64 `ensure_table`
//! fails on a huge entry). It also cannot give one physical range two cache
//! types, and cannot make scattered pages look contiguous. So device and
//! driver mappings get their own window of kernel VA, mapped with 4 KiB PTEs:
//!
//! * x86_64 and aarch64: `KVA_BASE = 0xffff_f000_0000_0000`, `KVA_SIZE` =
//!   256 GiB (top-level slot 480 of the kernel half; HHDM starts at slot 256
//!   on x86_64 / slot 0 of TTBR1 on aarch64, the kernel image is in slot 511).
//! * x86_64 subtlety: every user PML4 *copies* the kernel half at creation, so
//!   the window's PML4 entry must exist before the first user process. `init`
//!   (called from `kernel_main` before the init task) creates it by mapping
//!   and unmapping a probe page; every table below it is then shared.
//! * Allocation is next-fit with a one-page unmapped guard after every
//!   mapping. Freed ranges are reused only after the cursor wraps (256 GiB of
//!   allocations), so a CPU that missed a TLB shootdown ack (x86 waits are
//!   bounded) cannot be handed a stale translation of a recycled range.
//! * `iounmap` clears the PTEs and flushes every CPU (x86_64: CR3 reload
//!   broadcast — these PTEs are never global; aarch64: `tlbi vaae1is`).
//!
//! Task context only (takes a spin lock and allocates page tables); not
//! callable from IRQ handlers. Mappings are 4 KiB-granular (no huge pages:
//! a 256 MiB BAR costs 512 KiB of page tables).

extern crate alloc;
use alloc::vec::Vec;
use spin::Mutex;
use crate::paging::PageFlags;

const PAGE: usize = 4096;

pub const KVA_BASE: usize = 0xffff_f000_0000_0000;
pub const KVA_SIZE: usize = 256 << 30;

/// Cache attribute of a mapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum CacheMode {
    /// Strongly ordered, uncached: MMIO registers. x86_64 PCD (PAT UC-,
    /// which MTRR UC for MMIO makes UC); aarch64 Device-nGnRnE.
    Uncached = 0,
    /// Write-combining: framebuffers / apertures written in bulk. x86_64 PAT
    /// entry 5 (WC; falls back to UC- if PAT could not be programmed);
    /// aarch64 Normal Non-cacheable.
    WriteCombining = 1,
    /// Normal cached memory (RAM, e.g. scattered DMA pages on coherent
    /// platforms).
    WriteBack = 2,
}

impl CacheMode {
    fn flags(self) -> PageFlags {
        let base = PageFlags::PRESENT | PageFlags::WRITABLE;
        match self {
            CacheMode::Uncached => base | PageFlags::MMIO | PageFlags::NOCACHE,
            CacheMode::WriteCombining => base | PageFlags::WRITECOMBINE,
            CacheMode::WriteBack => base,
        }
    }
    pub fn from_u32(v: u32) -> CacheMode {
        match v { 1 => CacheMode::WriteCombining, 2 => CacheMode::WriteBack, _ => CacheMode::Uncached }
    }
}

struct Region { va: usize, pages: usize }

struct State {
    ready: bool,
    /// Next-fit cursor (offset from KVA_BASE).
    cursor: usize,
    /// Live mappings (va of first page, pages mapped, excluding the guard).
    live: Vec<Region>,
    /// Freed ranges (offset, pages incl. guard), reused after the cursor wraps.
    free: Vec<(usize, usize)>,
    mapped_pages: usize,
}

static STATE: Mutex<State> = Mutex::new(State {
    ready: false, cursor: 0, live: Vec::new(), free: Vec::new(), mapped_pages: 0,
});

/// Create the window's top-level table entry in the kernel root. Call once,
/// after paging is up and before any user address space exists.
pub fn init() -> bool {
    let root = crate::paging::get_kernel_root();
    // Probe: map one page at the base (allocates the intermediate tables in
    // the kernel root), then remove the leaf. The tables stay.
    let probe = match crate::buddy::alloc(0) { Some(p) => p, None => return false };
    let ok = unsafe { crate::paging::map_page(root, KVA_BASE, probe, PageFlags::PRESENT | PageFlags::WRITABLE) };
    if ok {
        unsafe { crate::paging::unmap_page(root, KVA_BASE); }
        crate::paging::tlb_flush_local_page(KVA_BASE);
    }
    crate::buddy::free(probe, 0);
    let mut st = STATE.lock();
    st.ready = ok;
    // Start one page in, so a NULL-ish offset bug never lands on a mapping.
    st.cursor = PAGE;
    ok
}

fn alloc_range(st: &mut State, pages: usize) -> Option<usize> {
    let need = pages + 1; // + guard
    let total = KVA_SIZE / PAGE;
    if st.cursor / PAGE + need <= total {
        let off = st.cursor;
        st.cursor += need * PAGE;
        return Some(KVA_BASE + off);
    }
    // Wrapped: first fit in the free list.
    for i in 0..st.free.len() {
        let (off, n) = st.free[i];
        if n >= need {
            if n == need { st.free.swap_remove(i); } else { st.free[i] = (off + need * PAGE, n - need); }
            return Some(KVA_BASE + off);
        }
    }
    None
}

fn flush_kernel_range(va: usize, pages: usize) {
    #[cfg(target_arch = "x86_64")]
    { let _ = (va, pages); crate::paging::tlb_shootdown_all(); }
    #[cfg(not(target_arch = "x86_64"))]
    { crate::paging::tlb_flush_range(crate::paging::get_kernel_root(), va, pages); }
}

fn unmap_pages(va: usize, pages: usize) {
    let root = crate::paging::get_kernel_root();
    for i in 0..pages {
        unsafe { crate::paging::unmap_page(root, va + i * PAGE); }
    }
    flush_kernel_range(va, pages);
}

/// Map `pages` physical frames (each 4 KiB-aligned) contiguously.
fn map_frames(frames: &mut dyn Iterator<Item = u64>, pages: usize, mode: CacheMode) -> Option<usize> {
    if pages == 0 { return None; }
    let mut st = STATE.lock();
    if !st.ready { return None; }
    let va = alloc_range(&mut st, pages)?;
    let root = crate::paging::get_kernel_root();
    let flags = mode.flags();
    for i in 0..pages {
        let pa = match frames.next() { Some(p) => p, None => { unmap_pages(va, i); return None; } };
        if !unsafe { crate::paging::map_page(root, va + i * PAGE, pa as usize & !(PAGE - 1), flags) } {
            unmap_pages(va, i);
            st.free.push((va - KVA_BASE, pages + 1));
            return None;
        }
    }
    st.live.push(Region { va, pages });
    st.mapped_pages += pages;
    Some(va)
}

/// Map the physical range `[phys, phys + len)` with cache attribute `mode`
/// into the KVA window. Returns the virtual address corresponding to `phys`
/// (same offset within the page). Task context.
pub fn ioremap(phys: u64, len: usize, mode: CacheMode) -> Option<usize> {
    if len == 0 { return None; }
    let start = phys & !(PAGE as u64 - 1);
    let end = phys.checked_add(len as u64)?.checked_add(PAGE as u64 - 1)? & !(PAGE as u64 - 1);
    let pages = ((end - start) / PAGE as u64) as usize;
    let mut it = (0..pages as u64).map(|i| start + i * PAGE as u64);
    let va = map_frames(&mut it, pages, mode)?;
    Some(va + (phys - start) as usize)
}

/// Map a list of (4 KiB-aligned) physical pages into one contiguous kernel VA
/// range, in order. Task context.
pub fn vmap(pages: &[u64], mode: CacheMode) -> Option<usize> {
    let mut it = pages.iter().copied();
    map_frames(&mut it, pages.len(), mode)
}

/// Remove the mapping that contains `va` (any address inside it, e.g. what
/// `ioremap` returned). Returns false if `va` is not a live mapping.
pub fn iounmap(va: usize) -> bool {
    let mut st = STATE.lock();
    let idx = match st.live.iter().position(|r| va >= r.va && va < r.va + r.pages * PAGE) {
        Some(i) => i,
        None => return false,
    };
    let r = st.live.swap_remove(idx);
    unmap_pages(r.va, r.pages);
    st.mapped_pages -= r.pages;
    st.free.push((r.va - KVA_BASE, r.pages + 1));
    true
}

/// Same as [`iounmap`].
pub fn vunmap(va: usize) -> bool { iounmap(va) }

/// True if `va` lies inside the KVA window.
pub fn in_window(va: usize) -> bool { va >= KVA_BASE && va < KVA_BASE + KVA_SIZE }

/// (live mappings, mapped pages, cursor bytes used).
pub fn stats() -> (usize, usize, usize) {
    let st = STATE.lock();
    (st.live.len(), st.mapped_pages, st.cursor)
}

// ── C ABI ────────────────────────────────────────────────────────────────────

/// `mode`: 0 = uncached, 1 = write-combining, 2 = write-back. NULL on failure.
#[no_mangle]
pub extern "C" fn kfacil_ioremap(phys: u64, len: usize, mode: u32) -> *mut u8 {
    ioremap(phys, len, CacheMode::from_u32(mode)).map_or(core::ptr::null_mut(), |v| v as *mut u8)
}
#[no_mangle]
pub extern "C" fn kfacil_iounmap(va: *mut u8) -> bool { iounmap(va as usize) }
/// Map `n` physical page addresses from `pages` contiguously.
#[no_mangle]
pub unsafe extern "C" fn kfacil_vmap(pages: *const u64, n: usize, mode: u32) -> *mut u8 {
    if pages.is_null() || n == 0 { return core::ptr::null_mut(); }
    let s = core::slice::from_raw_parts(pages, n);
    vmap(s, CacheMode::from_u32(mode)).map_or(core::ptr::null_mut(), |v| v as *mut u8)
}
