//! System-memory allocation for the GPU (nv_alloc_pages & co) and kernel
//! mappings (os_map_kernel_space, nv_alloc_kernel_mapping).
//!
//! - Pages come from the buddy allocator. There is no IOMMU, so the DMA
//!   address RM gets in `pte_array` is the physical address. Until RM calls
//!   `nv_set_dma_address_size` the limit is 4 GiB (Linux's default 32-bit
//!   PCI DMA mask); afterwards it is the GPU's width (47 bits on GA10x),
//!   which on a desktop covers all RAM, so plain `buddy::alloc` is used.
//! - Contiguous requests take one naturally aligned buddy block and give the
//!   unused tail back in aligned pieces.
//! - Cache types: RM asks for UNCACHED / WRITECOMBINED system memory for
//!   buffers the GPU reads with non-snooped transactions. Like
//!   nvidia-haiku (vm_set_area_memory_type), the pages keep their WB HHDM
//!   alias, which RM never uses; the lines are flushed (clflush) at
//!   allocation, and every kernel mapping RM gets of them
//!   (nv_alloc_kernel_mapping) carries the requested type (mm::kva vmap).
//! - Everything is zeroed (RM passes `zeroed` when it needs it; zeroing
//!   always costs ~10 ms for the 63 MB radix3 copy and removes a variable).

use alloc::boxed::Box;
use alloc::vec::Vec;
use core::ffi::c_void;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use mm::kva::CacheMode;

use crate::{nvlog, NV_ERR_INVALID_ARGUMENT, NV_ERR_NOT_SUPPORTED, NV_ERR_NO_MEMORY, NV_OK};

const PAGE: usize = 4096;

/// nv_memory_type.h
pub const NV_MEMORY_CACHED: u32 = 0;
pub const NV_MEMORY_UNCACHED: u32 = 1;
pub const NV_MEMORY_WRITECOMBINED: u32 = 2;
pub const NV_MEMORY_WRITEBACK: u32 = 5;

/// Highest DMA address + 1 the device can reach.
static DMA_LIMIT: AtomicU64 = AtomicU64::new(1 << 32);
pub(crate) static PAGES_LIVE: AtomicU64 = AtomicU64::new(0);
pub(crate) static ALLOCS_LIVE: AtomicU64 = AtomicU64::new(0);
pub(crate) static ALLOC_FAILS: AtomicU32 = AtomicU32::new(0);
static MAP_LOG_BUDGET: AtomicU32 = AtomicU32::new(64);
static ALLOC_LOG_BUDGET: AtomicU32 = AtomicU32::new(48);

fn budget(b: &AtomicU32) -> bool {
    b.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1)).is_ok()
}

pub(crate) fn cache_mode(cache_type: u32) -> CacheMode {
    match cache_type {
        NV_MEMORY_CACHED | NV_MEMORY_WRITEBACK => CacheMode::WriteBack,
        NV_MEMORY_WRITECOMBINED => CacheMode::WriteCombining,
        _ => CacheMode::Uncached,
    }
}

fn cache_name(cache_type: u32) -> &'static str {
    match cache_type {
        0 => "C",
        1 => "UC",
        2 => "WC",
        5 => "WB",
        6 => "DEFAULT",
        7 => "UCW",
        _ => "?",
    }
}

pub(crate) fn set_dma_limit_bits(bits: u32) {
    let lim = if bits >= 63 { u64::MAX } else { 1u64 << bits };
    DMA_LIMIT.store(lim, Ordering::Relaxed);
}

pub(crate) fn dma_limit() -> u64 {
    DMA_LIMIT.load(Ordering::Relaxed)
}

/// One buddy block, below the DMA limit.
fn alloc_block(order: usize) -> Option<usize> {
    let limit = DMA_LIMIT.load(Ordering::Relaxed);
    if limit >= mm::buddy::phys_end() as u64 {
        mm::buddy::alloc(order)
    } else {
        mm::buddy::alloc_below(order, limit as usize)
    }
}

/// Split pages [from, to) of a naturally aligned block into the largest
/// naturally aligned power-of-two pieces: (page offset, order).
pub(crate) fn pieces(from: usize, to: usize, mut emit: impl FnMut(usize, usize)) {
    let mut off = from;
    while off < to {
        let mut k = 0;
        while k + 1 < mm::buddy::MAX_ORDER && off % (1 << (k + 1)) == 0 && off + (1 << (k + 1)) <= to {
            k += 1;
        }
        emit(off, k);
        off += 1 << k;
    }
}

/// Free `n` pages starting at block-aligned `base` (the buddy allocator
/// accepts naturally aligned sub-blocks).
fn free_run(base: usize, n: usize) {
    pieces(0, n, |off, k| mm::buddy::free(base + off * PAGE, k));
}

/// Self-check of `pieces` (testdev): exact cover, natural alignment.
pub(crate) fn pieces_ok() -> bool {
    for total_order in 0..12usize {
        let total = 1usize << total_order;
        for n in 0..=total {
            let (mut sum, mut ok, mut next) = (0usize, true, n);
            pieces(n, total, |off, k| {
                ok &= off == next && off % (1 << k) == 0;
                next = off + (1 << k);
                sum += 1 << k;
            });
            if !ok || sum != total - n {
                return false;
            }
        }
    }
    true
}

fn order_for(n: usize) -> usize {
    let mut o = 0;
    while (1usize << o) < n {
        o += 1;
    }
    o
}

#[cfg(target_arch = "x86_64")]
fn flush_lines(va: usize, len: usize) {
    let mut p = va & !63;
    while p < va + len {
        unsafe { core::arch::asm!("clflush [{}]", in(reg) p, options(nostack)) };
        p += 64;
    }
    unsafe { core::arch::asm!("mfence", options(nostack)) };
}

#[cfg(not(target_arch = "x86_64"))]
fn flush_lines(_va: usize, _len: usize) {}

/// RM's view of one nv_alloc_pages allocation (`priv_data`).
pub(crate) struct NvAlloc {
    /// Physical address of every 4 KiB page, in order.
    pub pages: Vec<u64>,
    /// Buddy runs to free: (phys, pages).
    runs: Vec<(usize, usize)>,
    pub contiguous: bool,
    pub cache_type: u32,
}

impl Drop for NvAlloc {
    fn drop(&mut self) {
        for &(p, n) in &self.runs {
            free_run(p, n);
        }
        PAGES_LIVE.fetch_sub(self.pages.len() as u64, Ordering::Relaxed);
        ALLOCS_LIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Allocate `page_count` 4 KiB pages. `chunk_pages` = pages per physically
/// contiguous chunk for a non-contiguous request (RM's `page_size`).
pub(crate) fn alloc_pages(page_count: usize, chunk_pages: usize, contiguous: bool, cache_type: u32) -> Option<Box<NvAlloc>> {
    let mut a = Box::new(NvAlloc { pages: Vec::new(), runs: Vec::new(), contiguous, cache_type });
    if a.pages.try_reserve_exact(page_count).is_err() {
        return None;
    }
    ALLOCS_LIVE.fetch_add(1, Ordering::Relaxed);
    let grab = |n: usize, a: &mut NvAlloc| -> bool {
        let order = order_for(n);
        let base = match alloc_block(order) {
            Some(b) => b,
            None => return false,
        };
        let total = 1usize << order;
        if total > n {
            free_run_tail(base, n, total);
        }
        a.runs.push((base, n));
        for i in 0..n {
            a.pages.push((base + i * PAGE) as u64);
        }
        PAGES_LIVE.fetch_add(n as u64, Ordering::Relaxed);
        true
    };
    let ok = if contiguous {
        grab(page_count, &mut a)
    } else {
        let chunk = chunk_pages.max(1);
        let mut left = page_count;
        let mut ok = true;
        while left > 0 {
            let n = chunk.min(left);
            if !grab(n, &mut a) {
                ok = false;
                break;
            }
            left -= n;
        }
        ok
    };
    if !ok {
        ALLOC_FAILS.fetch_add(1, Ordering::Relaxed);
        return None; // Drop frees what was taken.
    }
    for &(p, n) in &a.runs {
        let va = mm::phys_to_virt(p);
        unsafe { core::ptr::write_bytes(va as *mut u8, 0, n * PAGE) };
        if !matches!(cache_mode(cache_type), CacheMode::WriteBack) {
            flush_lines(va, n * PAGE);
        }
    }
    Some(a)
}

/// Give back pages [n, total) of a fresh block in aligned pieces.
fn free_run_tail(base: usize, n: usize, total: usize) {
    pieces(n, total, |off, k| mm::buddy::free(base + off * PAGE, k));
}

/// C: `NV_STATUS nv_alloc_pages(nv_state_t *, NvU32 page_count, NvU64
/// page_size, NvBool contiguous, NvU32 cache_type, NvBool zeroed, NvBool
/// unencrypted, NvS32 node_id, NvU64 *pte_array, void **priv_data)`.
#[no_mangle]
pub unsafe extern "C" fn nv_alloc_pages(
    _nv: *mut c_void, page_count: u32, page_size: u64, contiguous: u8, cache_type: u32,
    _zeroed: u8, unencrypted: u8, node_id: i32, pte_array: *mut u64, priv_data: *mut *mut c_void,
) -> u32 {
    cb!("nv_alloc_pages");
    let contiguous = contiguous != 0;
    if page_count == 0 || pte_array.is_null() || priv_data.is_null() {
        return NV_ERR_INVALID_ARGUMENT;
    }
    if !contiguous && page_size == 0 {
        return NV_ERR_INVALID_ARGUMENT;
    }
    let chunk = ((page_size as usize).max(PAGE) / PAGE).next_power_of_two();
    let t0 = sched::monotonic_ns();
    let a = match alloc_pages(page_count as usize, chunk, contiguous, cache_type) {
        Some(a) => a,
        None => {
            nvlog!("[nvrm] +{}ms nv_alloc_pages: OUT OF MEMORY ({} pages, contig={}, page_size={:#x}, limit={:#x})\n",
                   crate::ms(), page_count, contiguous as u8, page_size, dma_limit());
            return NV_ERR_NO_MEMORY;
        }
    };
    if budget(&ALLOC_LOG_BUDGET) || page_count >= 256 {
        nvlog!("[nvrm] +{}ms nv_alloc_pages {} pages ({} KiB) contig={} pgsz={:#x} {} unenc={} node={} -> {:#x} ({} runs, {} us)\n",
               crate::ms(), page_count, page_count as u64 * 4, contiguous as u8, page_size,
               cache_name(cache_type), unencrypted, node_id, a.pages[0], a.runs.len(),
               (sched::monotonic_ns() - t0) / 1000);
    }
    let n = if contiguous { 1 } else { page_count as usize };
    for i in 0..n {
        *pte_array.add(i) = a.pages[i];
    }
    *priv_data = Box::into_raw(a) as *mut c_void;
    NV_OK
}

#[no_mangle]
pub unsafe extern "C" fn nv_free_pages(_nv: *mut c_void, _page_count: u32, _contiguous: u8, _cache_type: u32, priv_data: *mut c_void) -> u32 {
    cb!("nv_free_pages");
    if !priv_data.is_null() {
        drop(Box::from_raw(priv_data as *mut NvAlloc));
    }
    NV_OK
}

/// C: `void *nv_alloc_kernel_mapping(nv_state_t *, void *pAllocPrivate,
/// NvU64 pageIndex, NvU32 pageOffset, NvU64 size, void **pPrivate)`.
/// WB memory: the HHDM alias when the range is physically contiguous;
/// otherwise (or for UC/WC) a vmap carrying the allocation's cache type,
/// remembered in `*pPrivate` for nv_free_kernel_mapping.
#[no_mangle]
pub unsafe extern "C" fn nv_alloc_kernel_mapping(
    _nv: *mut c_void, alloc_priv: *mut c_void, page_index: u64, page_offset: u32, size: u64, private: *mut *mut c_void,
) -> *mut c_void {
    cb!("nv_alloc_kernel_mapping");
    if alloc_priv.is_null() || private.is_null() {
        return core::ptr::null_mut();
    }
    let a = &*(alloc_priv as *const NvAlloc);
    let idx = page_index as usize;
    let span = size as usize + page_offset as usize;
    let npages = (span + PAGE - 1) / PAGE;
    if idx >= a.pages.len() || idx + npages.max(1) > a.pages.len() {
        nvlog!("[nvrm] nv_alloc_kernel_mapping: range {}+{} beyond {} pages\n", idx, npages, a.pages.len());
        return core::ptr::null_mut();
    }
    let mode = cache_mode(a.cache_type);
    let contiguous_range = a.contiguous || npages <= 1
        || (0..npages).all(|i| a.pages[idx + i] == a.pages[idx] + (i * PAGE) as u64);
    if matches!(mode, CacheMode::WriteBack) && contiguous_range {
        *private = core::ptr::null_mut();
        return (mm::phys_to_virt(a.pages[idx] as usize) + page_offset as usize) as *mut c_void;
    }
    match mm::kva::vmap(&a.pages[idx..idx + npages.max(1)], mode) {
        Some(va) => {
            *private = va as *mut c_void;
            (va + page_offset as usize) as *mut c_void
        }
        None => {
            nvlog!("[nvrm] nv_alloc_kernel_mapping: vmap of {} pages failed\n", npages);
            core::ptr::null_mut()
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn nv_free_kernel_mapping(_nv: *mut c_void, _alloc_priv: *mut c_void, _address: *mut c_void, private: *mut c_void) -> u32 {
    cb!("nv_free_kernel_mapping");
    if !private.is_null() {
        mm::kva::vunmap(private as usize);
    }
    NV_OK
}

/// C: `void *os_map_kernel_space(NvU64 start, NvU64 size, NvU32 mode)`.
/// Always a fresh mapping in the kva window with the requested cache type
/// (RAM included: a WB alias of RAM is harmless, and an MMIO hole below the
/// top of RAM can never be mistaken for RAM). Every call is logged (first
/// 64): these are the BAR maps.
#[no_mangle]
pub unsafe extern "C" fn os_map_kernel_space(start: u64, size: u64, mode: u32) -> *mut c_void {
    cb!("os_map_kernel_space");
    let cmode = cache_mode(mode);
    let va = mm::kva::ioremap(start, size as usize, cmode).unwrap_or(0);
    if budget(&MAP_LOG_BUDGET) || va == 0 {
        nvlog!("[nvrm] +{}ms os_map_kernel_space phys={:#x} size={:#x} {} -> {:#x}\n",
               crate::ms(), start, size, cache_name(mode), va);
    }
    va as *mut c_void
}

#[no_mangle]
pub unsafe extern "C" fn os_unmap_kernel_space(addr: *mut c_void, size: u64) {
    cb!("os_unmap_kernel_space");
    if budget(&MAP_LOG_BUDGET) {
        nvlog!("[nvrm] +{}ms os_unmap_kernel_space {:p} size={:#x}\n", crate::ms(), addr, size);
    }
    if mm::kva::in_window(addr as usize) {
        mm::kva::iounmap(addr as usize);
    }
}

#[no_mangle]
pub unsafe extern "C" fn nv_requires_dma_remap(_nv: *mut c_void) -> u8 {
    cb!("nv_requires_dma_remap");
    0
}

#[no_mangle]
pub unsafe extern "C" fn nv_set_dma_address_size(_nv: *mut c_void, phys_addr_bits: u32) {
    cb!("nv_set_dma_address_size");
    nvlog!("[nvrm] +{}ms nv_set_dma_address_size: {} bits\n", crate::ms(), phys_addr_bits);
    set_dma_limit_bits(phys_addr_bits);
}

#[no_mangle]
pub unsafe extern "C" fn nv_get_device_memory_config(
    _nv: *mut c_void, _a: *mut u64, _b: *mut u64, _c: *mut u64, _d: *mut u32, _e: *mut i32,
) -> u32 {
    cb!("nv_get_device_memory_config");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn os_get_max_user_va() -> u64 {
    cb!("os_get_max_user_va");
    0x0000_7FFF_FFFF_F000
}

/// x86 is coherent for PCIe snooped traffic; nvidia-haiku returns this.
#[no_mangle]
pub unsafe extern "C" fn os_flush_cpu_cache_all() -> u32 {
    cb!("os_flush_cpu_cache_all");
    NV_ERR_NOT_SUPPORTED
}

#[no_mangle]
pub unsafe extern "C" fn os_flush_user_cache() -> u32 {
    cb!("os_flush_user_cache");
    NV_ERR_NOT_SUPPORTED
}

/// Linux: wmb(); drains write-combining buffers.
#[no_mangle]
pub unsafe extern "C" fn os_flush_cpu_write_combine_buffer() {
    cb!("os_flush_cpu_write_combine_buffer");
    #[cfg(target_arch = "x86_64")]
    core::arch::asm!("sfence", options(nostack));
}

/// x86: a cache-coherent platform. Nothing to do.
#[no_mangle]
pub unsafe extern "C" fn nv_dma_cache_invalidate(_dev: *mut c_void, _priv: *mut c_void) {
    cb!("nv_dma_cache_invalidate");
}

#[no_mangle]
pub unsafe extern "C" fn nv_flush_coherent_cpu_cache_range(_nv: *mut c_void, _start: u64, _end: u64) {
    cb!("nv_flush_coherent_cpu_cache_range");
}

/// Copies from/to user memory (RM ioctls; not on the init path).
#[no_mangle]
pub unsafe extern "C" fn os_memcpy_from_user(to: *mut c_void, from: *const c_void, n: u32) -> u32 {
    cb!("os_memcpy_from_user");
    if sched::uaccess::copy_from_user(to as *mut u8, from as usize, n as usize) == 0 { NV_OK } else { crate::NV_ERR_INVALID_ADDRESS }
}

#[no_mangle]
pub unsafe extern "C" fn os_memcpy_to_user(to: *mut c_void, from: *const c_void, n: u32) -> u32 {
    cb!("os_memcpy_to_user");
    if sched::uaccess::copy_to_user(to as usize, from as *const u8, n as usize) == 0 { NV_OK } else { crate::NV_ERR_INVALID_ADDRESS }
}

