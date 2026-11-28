//! Address-space cloning for `fork()`, with real copy-on-write.
//!
//! Parent and child end up mapping the *same* physical pages read-only
//! wherever content could later diverge; a write fault then promotes just
//! that one 4 KiB page (see `AddressSpace::handle_user_page_fault` in
//! `vmm.rs`). `pageref` tracks how many owners a shared page has, so the
//! last one to let go of it is the one that actually frees it back to the
//! buddy allocator.
//!
//! `MAP_SHARED` regions are the one exception: both sides keep full original
//! permissions (writes are meant to be visible to every owner immediately),
//! refcounted only so teardown frees the frame exactly once.
//!
//! Device (MMIO) mappings — identified by the `file_cap == usize::MAX`
//! sentinel `map_device` uses — are *aliased* into the child: the same
//! physical range is mapped a second time, with the original permissions, and
//! nothing is copied or refcounted. See the branch below for why that is the
//! only correct answer for this VMA class.
//!
//! Pages that were never faulted in before the fork are left as absent on
//! both sides; the first touch by either sibling afterward demand-pages
//! independently, which is correct since nobody has read or written that
//! memory yet.

extern crate alloc;
use alloc::vec::Vec;
use crate::vmm::{AddressSpace, VmaRegion, MAP_SHARED};
use crate::paging::{map_page, tlb_flush_as, PageFlags};
use crate::buddy::PAGE_SIZE;
use crate::pageref;

/// Serializes every compound pageref transaction: `clone_as`'s inc+downgrade
/// sweep and `handle_user_page_fault`'s get→copy→dec promotion. The two run
/// under *different* per-address-space busy locks (parent vs child), so
/// without this a child promoting a shared frame races the parent's next
/// fork over the same refcounts — mis-deciding "sole owner, reuse in place"
/// leaves one frame writable in two processes. Lock order is always
/// own-AS-busy → COW_LOCK, so the two-lock combination cannot deadlock.
pub static COW_LOCK: spin::Mutex<()> = spin::Mutex::new(());

/// Pages the most recent `clone_as` shared copy-on-write / copied outright.
/// Advisory diagnostics for the `[FORK]` serial line in `sched::clone`.
pub static LAST_SHARED_PAGES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
pub static LAST_COPIED_PAGES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);
/// Of `LAST_SHARED_PAGES`, those in writable private VMAs — what the old
/// eager-copy fork duplicated up front.
pub static LAST_PRIVATE_RW_PAGES: core::sync::atomic::AtomicUsize = core::sync::atomic::AtomicUsize::new(0);

/// Clone `src` into a fresh `AddressSpace` rooted at `new_page_table_root`.
///
/// Takes `src` by mutable reference: sharing a page copy-on-write requires
/// downgrading the *parent's* existing mapping to read-only too, and marking
/// the parent's own `VmaRegion`s as CoW-tracked, not just the child's.
///
/// Returns `None` on out-of-memory.
/// State of a resumable clone (see [`clone_as_step`]). `dst` is consistent
/// after every step, so dropping a half-built cursor undoes it exactly.
pub struct CloneCursor {
    pub dst: AddressSpace,
    region: usize,
    page: usize,
    started: bool,
    shared_pages: usize,
    private_rw_pages: usize,
    new_root: usize,
    /// Number of `clone_as_step` calls so far (the `holds=` of `[FORK]`).
    pub steps: usize,
}

/// First part of a clone: the child AddressSpace shell around `new_page_table_root`.
pub fn clone_as_begin(src: &AddressSpace, new_page_table_root: usize) -> CloneCursor {
    let mut dst = AddressSpace::new(new_page_table_root);
    dst.heap_start = src.heap_start;
    dst.heap_end   = src.heap_end;
    dst.stack_top  = src.stack_top;
    dst.regions.resize(src.regions.len(), None);
    CloneCursor { dst, region: 0, page: 0, started: false, shared_pages: 0,
                  private_rw_pages: 0, new_root: new_page_table_root, steps: 0 }
}

/// Finish: flush the parent's TLB entries and publish the stats.
pub fn clone_as_finish(src: &AddressSpace, c: CloneCursor) -> AddressSpace {
    let src_root = src.root();
    // The downgrades above rewrote *live* PTEs of the calling (parent)
    // process from writable to read-only. arch_map_page does not invalidate
    // existing translations (its barrier reasoning covers invalid→valid
    // transitions only), so this CPU's TLB still holds stale writable
    // entries for the parent's pages — most critically its user stack. If
    // the parent resumes and writes through such an entry before its next
    // page-table switch, the write silently lands on the still-shared frame
    // (no fault, no copy) and the child later reads the corruption. Flush
    // now, while the parent's root is the active one, so the parent's first
    // post-fork write takes the CoW fault it must. Only CPUs with the parent's
    // root loaded can hold such entries (its other threads are quiesced).
    tlb_flush_as(src_root);

    use core::sync::atomic::Ordering;
    LAST_SHARED_PAGES.store(c.shared_pages, Ordering::Relaxed);
    LAST_COPIED_PAGES.store(0, Ordering::Relaxed);
    LAST_PRIVATE_RW_PAGES.store(c.private_rw_pages, Ordering::Relaxed);
    c.dst
}

/// Clone `src` into a fresh `AddressSpace` rooted at `new_page_table_root`.
///
/// Takes `src` by mutable reference: sharing a page copy-on-write requires
/// downgrading the *parent's* existing mapping to read-only too, and marking
/// the parent's own `VmaRegion`s as CoW-tracked, not just the child's.
///
/// Returns `None` if `src.regions` changed shape (cannot happen in a single call).
pub fn clone_as(src: &mut AddressSpace, new_page_table_root: usize) -> Option<AddressSpace> {
    let mut c = clone_as_begin(src, new_page_table_root);
    loop {
        match clone_as_step(src, &mut c, usize::MAX) {
            Ok(true) => break,
            Ok(false) => {}
            Err(()) => return None,
        }
    }
    Some(clone_as_finish(src, c))
}

/// Clone up to `budget_pages` more pages. `Ok(true)` = done, `Ok(false)` = budget
/// spent (the caller may drop the AS lock, then call again with the lock
/// re-taken and `lock_epoch` unchanged), `Err(())` = `src.regions` changed shape.
///
/// `COW_LOCK` is taken per step: it guards the per-frame inc+downgrade
/// transaction, and each page's is complete inside one step. Lazy regions are
/// installed in `dst` up front (retain, `cow`, zeroed page vector) and filled in
/// place, so `dst` is always droppable.
pub fn clone_as_step(src: &mut AddressSpace, c: &mut CloneCursor, budget_pages: usize) -> Result<bool, ()> {
    let _cow_guard = COW_LOCK.lock();
    if src.regions.len() != c.dst.regions.len() { return Err(()); }
    c.steps += 1;
    let src_root = src.root();
    let new_page_table_root = c.new_root;
    let mut spent = 0usize;

    while c.region < src.regions.len() {
        let ri = c.region;
        let region = match src.regions[ri].as_mut() {
            Some(r) => r,
            None    => { c.region += 1; c.page = 0; continue; }
        };
        // MADV_DONTFORK: the child gets nothing here, and the parent's
        // frames (hypervisor guest RAM) are never downgraded to CoW.
        if region.map_flags & crate::vmm::MAP_DONTFORK != 0 {
            c.region += 1; c.page = 0;
            continue;
        }
        // A forked-from page becomes read-only: a hypervisor mapping it
        // writable in stage 2 must drop it first. The address-space lock is dropped
        // between steps (a hypervisor can map new pages of the remaining range
        // meanwhile), so every step notifies the range it has still to process.
        {
            let from = (region.start + c.page * 4096).min(region.end);
            crate::vmm::notify_range_invalidate(src_root, from, region.end - from);
        }
        let dst_slot = &mut c.dst.regions[ri];

        let is_shared = region.map_flags & MAP_SHARED != 0;
        let is_device = region.file_cap == usize::MAX;

        if !region.lazy && is_device {
            // ── Device (MMIO) mapping: ALIAS, never copy ─────────────────────
            //
            // The discriminator is deliberately `file_cap == usize::MAX` — "is
            // this a device VMA" — and not "is this MAP_SHARED" or "does this
            // physical range fall outside RAM":
            //
            //   * MAP_SHARED is far too broad. Shared tmpfs/memfd VMOs and
            //     shared anonymous memory are also MAP_SHARED, but they own
            //     real, refcounted RAM frames and must keep going through the
            //     `pageref` path below — that is the machinery the whole
            //     Wayland stack runs on.
            //   * "outside RAM" is too narrow, and untestable besides. It would
            //     leave the copy in place for a dumb buffer or a guest-backed
            //     blob, where copying is just as wrong (a child that inherits
            //     the scanout must write to *the* scanout, not to a snapshot of
            //     it), and it would make fork behavior depend on where the host
            //     happened to place a BAR — a distinction no test on a machine
            //     without a host-visible window can ever exercise.
            //
            // `file_cap == usize::MAX` is set in exactly one place,
            // `AddressSpace::map_device`, and it already means "this range is
            // not ours to own": both teardown paths (`Drop for AddressSpace`
            // and `unmap_range`) drop the PTEs and deliberately never free the
            // frames, and `split_at` refuses to subdivide such a VMA. Every
            // such VMA is created MAP_SHARED and eager, so aliasing preserves
            // the ownership rules already in force rather than inventing new
            // ones: no `pageref` entry is needed (nobody frees these pages), no
            // permission is downgraded (a device mapping must never take a CoW
            // fault — promoting a BAR page to a private RAM copy would silently
            // disconnect the child from the hardware), and the parent is left
            // untouched.
            //
            // The copy this replaces was a live panic, not just a semantic
            // wart. For a host-visible virtio-gpu blob `region.phys` is a PCI
            // BAR address, and `phys_to_virt` on it yields an HHDM address the
            // HHDM does not map, so the `copy_nonoverlapping` faulted in
            // kernel context (`memcpy+0xe`, CR2 = HHDM base + the shared-memory
            // window offset) and took the boot down. It also leaked: the fresh
            // buddy block handed to the child carried `file_cap == usize::MAX`,
            // so neither teardown path would ever free it.
            let n_pages = (region.end - region.start) / PAGE_SIZE;
            unsafe {
                for i in 0..n_pages {
                    map_page(new_page_table_root, region.start + i * PAGE_SIZE,
                             region.phys + i * PAGE_SIZE, region.flags);
                }
            }
            // The child's alias is one more mapping of the device range.
            crate::vmm::device_vma_retain(region.phys, region.end - region.start);
            *dst_slot = Some(VmaRegion {
                start: region.start, end: region.end, phys: region.phys,
                flags: region.flags, lazy: false, lazy_pages: crate::pagevec::PageVec::new(), lazy_count: 0,
                prot: region.prot, map_flags: region.map_flags,
                file_cap: region.file_cap, file_off: region.file_off,
                file_len: region.file_len, cow: false, written: Vec::new(),
                shm: None, shm_pgoff: 0, vmo: region.vmo,
            });
            spent += n_pages;
            c.region += 1; c.page = 0;
            if spent >= budget_pages && c.region < src.regions.len() { return Ok(false); }
            continue;
        }

        let downgraded = region.flags & !PageFlags::WRITABLE;
        // PROT_NONE regions keep their frames but never have PTEs (see
        // `AddressSpace::mprotect`); the frames are still shared CoW.
        let no_access = region.prot & crate::vmm::PROT_ACCESS == 0;
        let private_rw = !is_shared && region.flags.contains(PageFlags::WRITABLE);

        // Writable private regions (stack, heap, .data/.bss, RW mmaps) are
        // shared copy-on-write exactly like read-only ones: both sides map
        // the frame read-only and the first writer takes a private copy.
        //
        // Until 2026-09-24 they were copied eagerly here (e9510cb), which
        // cost a ~200 MiB transient per cosmic-comp spawn and tripped
        // `[BUDDY] Allocation failed` on a depleted guest. The corruption
        // that motivated the eager copy (brush/tokio: std's `Process.pidfd`
        // reading 0) has two causes, both closed elsewhere now:
        //   * a sibling thread writing through a stale writable TLB entry
        //     into the frame the child now shares — closed by the
        //     stop-the-world quiesce around this clone
        //     (`sched::quiesce_thread_group`) plus the final shootdown below;
        //   * the kernel storing into user memory through the HHDM
        //     (`write_user_buf`: wait status, signal frames, read(2) into a
        //     buffer) — which bypasses the page-table write protection and
        //     would land in the frame both sides still share. `write_user_buf`
        //     and `prefault_range` now break the sharing first
        //     (`AddressSpace::unshare_cow_page`), and direct kernel stores
        //     through a user pointer take the ordinary EL1/ring-0 write fault
        //     (the PTE is read-only for the kernel too: AP[2] / CR0.WP).

        if !region.lazy {
            // Still-contiguous, never-forked eager block: convert both
            // parent and child to per-page tracking so each side can later
            // promote individual pages independently of the other. An eager
            // region is processed whole (bounded by MAP_MAX_BYTES).
            let n_pages = (region.end - region.start) / PAGE_SIZE;
            let mut dst_lazy_pages = crate::pagevec::PageVec::new();
            let mut dst_lazy_count = 0usize;
            dst_lazy_pages.resize(n_pages, 0);
            for i in 0..n_pages {
                let phys = region.phys + i * PAGE_SIZE;
                pageref::inc(phys);
                dst_lazy_pages[i] = phys;
                dst_lazy_count += 1;
                let install_flags = if is_shared { region.flags } else { downgraded };
                if !no_access { unsafe {
                    map_page(new_page_table_root, region.start + i * PAGE_SIZE, phys, install_flags);
                    map_page(src_root, region.start + i * PAGE_SIZE, phys, install_flags);
                } }
            }
            c.shared_pages += n_pages;
            if private_rw { c.private_rw_pages += n_pages; }
            crate::vmm::free_eager_tail(region.phys, n_pages);
            region.lazy = true;
            region.phys = 0;
            region.lazy_pages = dst_lazy_pages.clone();
            region.lazy_count = dst_lazy_count;
            region.cow = !is_shared;
            if crate::vmm::is_file_backed(region.file_cap) {
                crate::vmm::file_retain(region.file_cap);
            }
            crate::vmm::vmo_retain(region.vmo);
            *dst_slot = Some(VmaRegion {
                start: region.start, end: region.end, phys: 0, flags: region.flags,
                lazy: true, lazy_pages: dst_lazy_pages, lazy_count: dst_lazy_count,
                prot: region.prot, map_flags: region.map_flags,
                file_cap: region.file_cap, file_off: region.file_off,
                file_len: region.file_len, cow: !is_shared,
                written: region.written.clone(),
                shm: region.shm.clone(), shm_pgoff: region.shm_pgoff, vmo: region.vmo,
            });
            spent += n_pages;
        } else {
            // Already per-page tracked (ordinary lazy mmap/heap, or a region
            // that went through this same conversion in an earlier fork).
            if !c.started {
                // Per-region bookkeeping, once. The child VMA holds its own
                // reference to any backing file: pages still absent after the
                // fork are demand-read by whichever side touches them first,
                // so the file must outlive both address spaces.
                if crate::vmm::is_file_backed(region.file_cap) {
                    crate::vmm::file_retain(region.file_cap);
                }
                crate::vmm::vmo_retain(region.vmo);
                if !is_shared { region.cow = true; }
                let mut dst_lazy_pages = crate::pagevec::PageVec::new();
                dst_lazy_pages.resize(region.lazy_pages.len(), 0);
                *dst_slot = Some(VmaRegion {
                    start:      region.start,
                    end:        region.end,
                    phys:       0,
                    flags:      region.flags,
                    lazy:       true,
                    lazy_pages: dst_lazy_pages,
                    lazy_count: 0,
                    prot:       region.prot,
                    map_flags:  region.map_flags,
                    file_cap:   region.file_cap,
                    file_off:   region.file_off,
                    file_len:   region.file_len,
                    cow:        !is_shared,
                    written:    region.written.clone(),
                    // Shared anonymous memory: the child names the same object, so
                    // a page either side first touches after the fork is still one
                    // page (see `crate::shmem`).
                    shm:        region.shm.clone(),
                    shm_pgoff:  region.shm_pgoff,
                    vmo:        region.vmo,
                });
                c.started = true;
            }
            let dst_region = dst_slot.as_mut().unwrap();
            for (i, phys) in region.lazy_pages.present_from(c.page) {
                if spent >= budget_pages { c.page = i; return Ok(false); }
                pageref::inc(phys);
                c.shared_pages += 1;
                if private_rw { c.private_rw_pages += 1; }
                dst_region.lazy_pages[i] = phys;
                dst_region.lazy_count += 1;
                spent += 1;
                // A shared file mapping's clean page stays read-only in the
                // child too: its first write must fault to mark the cached
                // page dirty (see `map_shared_file`).
                let install_flags = if !is_shared { downgraded }
                    else if crate::vmm::is_file_backed(region.file_cap) { region.install_flags(i, region.flags) }
                    else { region.flags };
                if !no_access { unsafe {
                    map_page(new_page_table_root, region.start + i * PAGE_SIZE, phys, install_flags);
                    if !is_shared {
                        map_page(src_root, region.start + i * PAGE_SIZE, phys, install_flags);
                    }
                } }
            }
        }
        c.region += 1; c.page = 0; c.started = false;
        if spent >= budget_pages && c.region < src.regions.len() { return Ok(false); }
    }
    Ok(true)
}
