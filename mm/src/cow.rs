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
pub fn clone_as(src: &mut AddressSpace, new_page_table_root: usize) -> Option<AddressSpace> {
    let _cow_guard = COW_LOCK.lock();
    let mut shared_pages = 0usize;
    let copied_pages = 0usize; // nothing is copied at fork any more
    let mut private_rw_pages = 0usize;
    let src_root = src.root();
    let mut dst = AddressSpace::new(new_page_table_root);
    dst.heap_start = src.heap_start;
    dst.heap_end   = src.heap_end;

    dst.regions.resize(src.regions.len(), None);

    for (src_slot, dst_slot) in src.regions.iter_mut().zip(dst.regions.iter_mut()) {
        let region = match src_slot.as_mut() {
            Some(r) => r,
            None    => continue,
        };

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
            *dst_slot = Some(VmaRegion {
                start: region.start, end: region.end, phys: region.phys,
                flags: region.flags, lazy: false, lazy_pages: Vec::new(), lazy_count: 0,
                prot: region.prot, map_flags: region.map_flags,
                file_cap: region.file_cap, file_off: region.file_off,
                file_len: region.file_len, cow: false,
            });
            continue;
        }

        let downgraded = region.flags & !PageFlags::WRITABLE;
        let private_rw = !is_shared && region.flags.contains(PageFlags::WRITABLE);
        let shared_before = shared_pages;
        let mut dst_lazy_pages = Vec::new();
        let mut dst_lazy_count = 0usize;

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
            // promote individual pages independently of the other.
            let n_pages = (region.end - region.start) / PAGE_SIZE;
            dst_lazy_pages.resize(n_pages, 0);
            for i in 0..n_pages {
                let phys = region.phys + i * PAGE_SIZE;
                pageref::inc(phys);
                shared_pages += 1;
                dst_lazy_pages[i] = phys;
                dst_lazy_count += 1;
                let install_flags = if is_shared { region.flags } else { downgraded };
                unsafe {
                    map_page(new_page_table_root, region.start + i * PAGE_SIZE, phys, install_flags);
                    map_page(src_root, region.start + i * PAGE_SIZE, phys, install_flags);
                }
            }
            crate::vmm::free_eager_tail(region.phys, n_pages);
            region.lazy = true;
            region.phys = 0;
            region.lazy_pages = dst_lazy_pages.clone();
            region.lazy_count = dst_lazy_count;
            region.cow = !is_shared;
        } else {
            // Already per-page tracked (ordinary lazy mmap/heap, or a region
            // that went through this same conversion in an earlier fork).
            for (i, &phys) in region.lazy_pages.iter().enumerate() {
                if phys == 0 { continue; }
                pageref::inc(phys);
                shared_pages += 1;
                if dst_lazy_pages.len() <= i { dst_lazy_pages.resize(i + 1, 0); }
                dst_lazy_pages[i] = phys;
                dst_lazy_count += 1;
                let install_flags = if is_shared { region.flags } else { downgraded };
                unsafe {
                    map_page(new_page_table_root, region.start + i * PAGE_SIZE, phys, install_flags);
                    if !is_shared {
                        map_page(src_root, region.start + i * PAGE_SIZE, phys, install_flags);
                    }
                }
            }
            if !is_shared { region.cow = true; }
        }

        if private_rw { private_rw_pages += shared_pages - shared_before; }

        // The child VMA holds its own reference to any backing file: pages
        // still absent after the fork are demand-read by whichever side
        // touches them first, so the file must outlive both address spaces.
        if crate::vmm::is_file_backed(region.file_cap) {
            crate::vmm::file_retain(region.file_cap);
        }

        *dst_slot = Some(VmaRegion {
            start:      region.start,
            end:        region.end,
            phys:       0,
            flags:      region.flags,
            lazy:       true,
            lazy_pages: dst_lazy_pages,
            lazy_count: dst_lazy_count,
            prot:       region.prot,
            map_flags:  region.map_flags,
            file_cap:   region.file_cap,
            file_off:   region.file_off,
            file_len:   region.file_len,
            cow:        !is_shared,
        });
    }

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
    LAST_SHARED_PAGES.store(shared_pages, Ordering::Relaxed);
    LAST_COPIED_PAGES.store(copied_pages, Ordering::Relaxed);
    LAST_PRIVATE_RW_PAGES.store(private_rw_pages, Ordering::Relaxed);
    Some(dst)
}
