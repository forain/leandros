//! Kernel-module window: a 256 MiB range of kernel virtual address space,
//! mapped page by page (4 KiB, kernel root, never USER) for loadable modules.
//! The constants live in `kmod_abi::layout` and are only re-exported here.
//!
//! `init` creates the intermediate tables in the kernel root once (the same
//! probe pattern as `kva::init`, so every user PML4 created afterwards copies
//! them) and proves the window is otherwise empty. Slots are `SLOT_ALIGN`
//! (256 KiB) units tracked in a bitmap.
//!
//! `release` is for a load that failed before any code ran, or whose init
//! returned an error with no registrations (nothing can reference the slot).
//!
//! Note: the HHDM alias of the backing frames is RW and executable on both
//! direct kernels (2 MiB / 1 GiB blocks without NX/PXN); the "never W+X"
//! property holds for the window only.

use crate::paging::{self, PageFlags};
use spin::Mutex;
use kmod_abi::layout;

pub const BASE: usize = layout::BASE as usize;
pub const SIZE: usize = layout::SIZE as usize;
pub const SLOT_ALIGN: usize = layout::SLOT_ALIGN as usize;
pub const TEST_SLOT_OFF: usize = layout::TEST_SLOT_OFF as usize;
const NSLOTS: usize = SIZE / SLOT_ALIGN;
const PAGE: usize = 4096;
const L2: usize = 2 << 20;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Perm { R, RX, RW }

static SLOTS: Mutex<[u64; NSLOTS / 64]> = Mutex::new([0; NSLOTS / 64]);

#[cfg(target_arch = "aarch64")]
fn check_tcr() -> bool {
    let tcr: u64;
    unsafe { core::arch::asm!("mrs {}, tcr_el1", out(reg) tcr, options(nomem, nostack)); }
    // T1SZ (bits 21:16) must be <= 16 so that the window (top 16 VA bits set) translates.
    ((tcr >> 16) & 0x3f) <= 16
}
#[cfg(not(target_arch = "aarch64"))]
fn check_tcr() -> bool { true }

fn in_window(va: usize) -> bool { va >= BASE && va - BASE < SIZE }

/// Create the window's intermediate tables and verify it is empty.
/// Returns false (window unusable) on OOM, a bad TCR, or any pre-existing leaf.
pub fn init() -> bool {
    if !check_tcr() { return false; }
    let root = paging::get_kernel_root();
    // A pre-existing block (or page) anywhere in the window means it is not ours.
    let mut off = 0;
    while off < SIZE {
        if paging::leaf_pte(root, BASE + off) != 0 { return false; }
        off += L2;
    }
    let probe = match crate::buddy::alloc(0) { Some(p) => p, None => return false };
    let mut ok = true;
    let mut off = 0;
    while off < SIZE {
        let va = BASE + off;
        if unsafe { paging::map_page(root, va, probe, PageFlags::PRESENT | PageFlags::WRITABLE) } {
            unsafe { paging::unmap_page(root, va); }
            paging::tlb_flush_local_page(va);
        } else { ok = false; break; }
        off += L2;
    }
    crate::buddy::free(probe, 0);
    ok && present_pages() == 0
}

/// Number of present 4 KiB leaves (or blocks) in the window.
pub fn present_pages() -> usize {
    let root = paging::get_kernel_root();
    let mut n = 0;
    let mut va = BASE;
    while va < BASE + SIZE {
        if paging::leaf_pte(root, va) != 0 { n += 1; }
        va += PAGE;
    }
    n
}

fn slot_range(base: usize, size: usize) -> Option<(usize, usize)> {
    if size == 0 || base % SLOT_ALIGN != 0 || !in_window(base) { return None; }
    let n = (size + SLOT_ALIGN - 1) / SLOT_ALIGN;
    let first = (base - BASE) / SLOT_ALIGN;
    if first + n > NSLOTS { return None; }
    Some((first, n))
}

/// Claim a slot range (SLOT_ALIGN aligned, inside the window, not overlapping).
pub fn reserve(base: usize, size: usize) -> bool {
    let (first, n) = match slot_range(base, size) { Some(r) => r, None => return false };
    let mut m = SLOTS.lock();
    if (first..first + n).any(|i| m[i / 64] & (1 << (i % 64)) != 0) { return false; }
    for i in first..first + n { m[i / 64] |= 1 << (i % 64); }
    true
}

/// Return a slot range claimed by `reserve`.
pub fn release(base: usize, size: usize) {
    if let Some((first, n)) = slot_range(base, size) {
        let mut m = SLOTS.lock();
        for i in first..first + n { m[i / 64] &= !(1 << (i % 64)); }
    }
}

/// Map one 4 KiB page of the window. Never USER; R and RX are read-only.
/// RX is kernel-only executable (UXN on aarch64).
pub unsafe fn map(va: usize, phys: usize, perm: Perm) -> bool {
    if !in_window(va) || va % PAGE != 0 { return false; }
    let f = match perm {
        Perm::R => PageFlags::PRESENT,
        Perm::RX => PageFlags::PRESENT | PageFlags::EXECUTE | PageFlags::KERNEL_ONLY_X,
        Perm::RW => PageFlags::PRESENT | PageFlags::WRITABLE,
    };
    let root = paging::get_kernel_root();
    if paging::leaf_pte(root, va) != 0 { return false; }
    paging::map_page(root, va, phys, f)
}

/// Unmap `pages` pages starting at `va` and flush them on every CPU.
pub unsafe fn unmap(va: usize, pages: usize) {
    if !in_window(va) { return; }
    let root = paging::get_kernel_root();
    for i in 0..pages {
        let v = va + i * PAGE;
        if in_window(v) { paging::unmap_page(root, v); }
    }
    paging::tlb_flush_range(root, va, pages);
    // x86_64: the range flush above only reaches CPUs that have the kernel root loaded; a CPU
    // running a process root still caches the shared kernel-half window. The window PTEs are
    // not Global (map_page never sets G), so a CR3 reload on every CPU drops them. Needed for
    // unload-then-reload at the same VA. aarch64: broadcast TLBI, harmless and rare.
    paging::tlb_shootdown_all();
}
