//! Virtual Memory Manager — per-process address space descriptors.
//!
//! Analogous to Linux's `mm_struct` / `vm_area_struct`.
//!
//! Demand paging
//! -------------
//! `map_lazy()` records a VMA without allocating or installing any page-table
//! entries.  On the first access the CPU takes a page fault; the fault handler
//! calls `handle_user_page_fault(fault_va)` which allocates exactly one 4 KiB
//! page, zeroes it, and maps it into the page table.  Each additional access
//! triggers its own fault.  Lazy VMAs are tracked with a heap-allocated Vec so
//! there is no per-VMA page-count limit.

extern crate alloc;
use alloc::vec::Vec;
use crate::paging::{PageFlags, map_page, unmap_page, tlb_shootdown_all};
use crate::buddy::{PAGE_SIZE, alloc as buddy_alloc, free as buddy_free};

// ── POSIX mmap/mprotect protection flags ─────────────────────────────────────
pub const PROT_NONE:  u32 = 0;
pub const PROT_READ:  u32 = 1 << 0;
pub const PROT_WRITE: u32 = 1 << 1;
pub const PROT_EXEC:  u32 = 1 << 2;

// ── POSIX mmap map flags ──────────────────────────────────────────────────────
pub const MAP_SHARED:    u32 = 1 << 0;
pub const MAP_PRIVATE:   u32 = 1 << 1;
pub const MAP_ANONYMOUS: u32 = 1 << 5;
pub const MAP_FIXED:     u32 = 1 << 4;

/// Represents a contiguous virtual memory region within an address space.
#[derive(Clone)]
pub struct VmaRegion {
    pub start: usize,
    pub end:   usize,   // exclusive
    /// For eager VMAs: physical base of the contiguous buddy allocation.
    /// For lazy VMAs: unused (see `lazy_pages`).
    pub phys:  usize,
    pub flags: PageFlags,
    /// True if physical pages are allocated lazily on first access.
    pub lazy:  bool,
    /// Per-page physical addresses for lazy VMAs (0 = not yet faulted in).
    /// Indexed by `(fault_va - start) / PAGE_SIZE`.  Grows on demand; no
    /// fixed upper bound on VMA size.
    pub lazy_pages: Vec<usize>,
    /// Number of faulted-in pages tracked in `lazy_pages`.
    pub lazy_count: usize,

    // ── POSIX fields added in Phase 0 ────────────────────────────────────────
    /// POSIX protection flags (PROT_READ | PROT_WRITE | PROT_EXEC).
    pub prot:      u32,
    /// mmap flags (MAP_SHARED | MAP_PRIVATE | MAP_ANONYMOUS).
    pub map_flags: u32,
    /// Capability token for file-backed VMAs (0 = anonymous).
    pub file_cap:  usize,
    /// Offset into the backing file (for file-backed VMAs).
    pub file_off:  u64,
    /// True if this VMA is a copy-on-write clone; write faults allocate a
    /// new page and copy the content before remapping writable.
    pub cow:       bool,
}

/// Per-process address space.
pub struct AddressSpace {
    pub page_table_root: usize,
    pub regions: [Option<VmaRegion>; 8],
    /// Virtual address where the heap begins (set by ELF loader; 0 = no heap).
    pub heap_start: usize,
    /// Current heap break (end of heap VMA).
    pub heap_end: usize,
}

impl Drop for AddressSpace {
    /// Unmap and free all VMAs, then release the page-table root page.
    ///
    /// Called automatically when the owning `Task` is dropped by the
    /// zombie-reaping path in `sched::run()`.  This is the authoritative
    /// cleanup path for per-process physical memory.
    fn drop(&mut self) {
        // Free all VMA backing pages.
        for slot in self.regions.iter_mut() {
            if let Some(region) = slot.take() {
                if region.lazy {
                    for phys in region.lazy_pages.iter().copied() {
                        if phys != 0 { buddy_free(phys, 0); }
                    }
                } else if region.phys != 0 {
                    let pages = (region.end - region.start) / PAGE_SIZE;
                    buddy_free(region.phys, pages_to_order(pages));
                }
            }
        }
        // Free the page-table root (PGD on AArch64, PML4 on x86-64).
        if self.page_table_root != 0 {
            buddy_free(self.page_table_root, 0);
        }
        // Flush stale TLB entries on all CPUs now that all mappings are gone.
        tlb_shootdown_all();
    }
}

impl AddressSpace {
    pub fn new(page_table_root: usize) -> Self {
        Self {
            page_table_root,
            regions: [None, None, None, None, None, None, None, None],
            heap_start: 0,
            heap_end: 0,
        }
    }

    /// Map `size` bytes (rounded up to pages) at virtual address `virt`,
    /// backed by freshly allocated physical pages.
    ///
    /// Returns `true` on success, `false` if OOM or the VMA table is full.
    pub fn map(&mut self, virt: usize, size: usize, flags: PageFlags) -> bool {
        if size == 0 { return false; }

        // Find a free VMA slot.
        let slot = match self.regions.iter().position(|r| r.is_none()) {
            Some(i) => i,
            None    => return false,
        };

        // Align virt down and size up to page granularity.
        let virt  = virt & !(PAGE_SIZE - 1);
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let end   = match virt.checked_add(pages * PAGE_SIZE) {
            Some(e) => e,
            None    => return false, // overflow → reject
        };

        // Reject if the new range overlaps any existing VMA.
        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if virt < r.end && end > r.start { return false; }
        }
        let order = pages_to_order(pages);

        let phys = match buddy_alloc(order) {
            Some(p) => p,
            None    => return false,
        };

        // Zero the backing memory via HHDM virtual address.
        unsafe { (crate::phys_to_virt(phys) as *mut u8).write_bytes(0, pages * PAGE_SIZE); }

        // Map each page.  If any individual mapping fails (OOM in page-table
        // node allocation), unmap the pages already installed, free the buddy
        // allocation, and report failure.
        for i in 0..pages {
            let ok = unsafe {
                map_page(
                    self.page_table_root,
                    virt + i * PAGE_SIZE,
                    phys + i * PAGE_SIZE,
                    flags,
                )
            };
            if !ok {
                // Roll back already-mapped pages.
                for j in 0..i {
                    unsafe { unmap_page(self.page_table_root, virt + j * PAGE_SIZE); }
                }
                buddy_free(phys, order);
                return false;
            }
        }

        self.regions[slot] = Some(VmaRegion {
            start: virt,
            end:   virt + pages * PAGE_SIZE,
            phys,
            flags,
            lazy: false,
            lazy_pages: Vec::new(),
            lazy_count: 0,
            prot:      PROT_READ | PROT_WRITE,
            map_flags: MAP_ANONYMOUS | MAP_PRIVATE,
            file_cap:  0,
            file_off:  0,
            cow:       false,
        });
        true
    }

    /// Reserve a virtual address range without allocating physical pages.
    ///
    /// Each page is allocated and mapped on the first access that faults into
    /// it.  Mirrors `mmap(PROT_…, MAP_ANONYMOUS | MAP_PRIVATE, …)` with no
    /// `MAP_POPULATE` flag.
    ///
    /// Returns `true` on success, `false` if the VMA table is full or the range
    /// overlaps an existing VMA.
    pub fn map_lazy(&mut self, virt: usize, size: usize, flags: PageFlags) -> bool {
        if size == 0 { return false; }

        let slot = match self.regions.iter().position(|r| r.is_none()) {
            Some(i) => i,
            None    => return false,
        };

        let virt  = virt & !(PAGE_SIZE - 1);
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let end   = match virt.checked_add(pages * PAGE_SIZE) {
            Some(e) => e,
            None    => return false, // overflow → reject
        };

        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if virt < r.end && end > r.start { return false; }
        }

        self.regions[slot] = Some(VmaRegion {
            start: virt,
            end,
            phys: 0,
            flags,
            lazy: true,
            lazy_pages: Vec::new(),
            lazy_count: 0,
            prot:      PROT_READ | PROT_WRITE,
            map_flags: MAP_ANONYMOUS | MAP_PRIVATE,
            file_cap:  0,
            file_off:  0,
            cow:       false,
        });
        true
    }

    /// Handle a user-mode page fault at `fault_va`.
    ///
    /// Looks up the VMA that contains `fault_va`.  If the VMA is lazy and the
    /// faulting page has not been backed yet, allocates one 4 KiB physical page,
    /// zeroes it, and maps it into `self.page_table_root`.
    ///
    /// Returns `true` if the fault was handled (execution can resume), or `false`
    /// if `fault_va` is not within any VMA (segmentation fault).
    pub fn handle_user_page_fault(&mut self, fault_va: usize) -> bool {
        let page_va = fault_va & !(PAGE_SIZE - 1);

        // Find the VMA that covers the faulting address.
        let region = match self.regions.iter_mut().filter_map(|r| r.as_mut()).find(
            |r| fault_va >= r.start && fault_va < r.end
        ) {
            Some(r) => r,
            None    => return false, // not mapped at all → segfault
        };

        if !region.lazy {
            // The page should already be present; this is not a demand-paging
            // fault — likely a protection fault.  Signal as unhandled.
            return false;
        }

        // Compute the page index within this VMA.
        let page_idx = (page_va - region.start) / PAGE_SIZE;

        // If this page was already faulted in, it is a protection fault.
        if region.lazy_pages.get(page_idx).copied().unwrap_or(0) != 0 {
            return false;
        }

        // Allocate one physical page for this fault.
        let phys = match buddy_alloc(0) {
            Some(p) => p,
            None    => return false, // OOM
        };
        unsafe { (crate::phys_to_virt(phys) as *mut u8).write_bytes(0, PAGE_SIZE); }

        // Map just the faulting page.
        let mapped = unsafe {
            map_page(self.page_table_root, page_va, phys, region.flags)
        };
        if !mapped {
            buddy_free(phys, 0);
            return false;
        }

        // Grow the tracking Vec to cover page_idx, then record the physical page.
        if region.lazy_pages.len() <= page_idx {
            region.lazy_pages.resize(page_idx + 1, 0);
        }
        region.lazy_pages[page_idx] = phys;
        region.lazy_count += 1;

        true
    }

    /// Demand-page all unmapped pages in `[addr, addr+len)` so the kernel can
    /// safely write to user buffers without taking a kernel-mode page fault.
    pub fn prefault_range(&mut self, addr: usize, len: usize) {
        if len == 0 { return; }
        let page_start = addr & !(PAGE_SIZE - 1);
        let page_end   = (addr + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let mut va = page_start;
        while va < page_end {
            if self.virt_to_phys(va).is_none() {
                self.handle_user_page_fault(va);
            }
            va += PAGE_SIZE;
        }
    }

    /// Unmap a virtual address range `[virt, virt+len)`, freeing any backing pages.
    ///
    /// Handles full removal, front-trim, and back-trim for each overlapping VMA.
    /// Middle splits (where neither end of the unmap aligns with the VMA boundary)
    /// truncate to the left portion; the right portion is leaked — this is a known
    /// Phase 6 limitation that Phase 7's VMO refcount migration will resolve.
    pub fn unmap_range(&mut self, virt: usize, len: usize) {
        if len == 0 { return; }
        let virt = virt & !(PAGE_SIZE - 1);
        let len  = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let end  = match virt.checked_add(len) { Some(e) => e, None => return };

        let pt = self.page_table_root;
        let mut did_unmap = false;

        for slot in self.regions.iter_mut() {
            let region = match slot {
                Some(r) if r.start < end && r.end > virt => r,
                _ => continue,
            };

            let r_start = region.start;
            let r_end   = region.end;
            let clip_s  = virt.max(r_start);
            let clip_e  = end.min(r_end);

            // ── Free physical pages in the clipped range ──────────────────────
            if region.lazy {
                let pg_first = (clip_s - r_start) / PAGE_SIZE;
                let pg_last  = (clip_e - r_start + PAGE_SIZE - 1) / PAGE_SIZE;
                for i in pg_first..pg_last.min(region.lazy_pages.len()) {
                    if region.lazy_pages[i] != 0 {
                        unsafe { unmap_page(pt, r_start + i * PAGE_SIZE); }
                        buddy_free(region.lazy_pages[i], 0);
                        region.lazy_pages[i] = 0;
                        region.lazy_count = region.lazy_count.saturating_sub(1);
                    }
                }
            } else {
                // Eager VMA: unmap each page in the overlap.
                let n = (clip_e - clip_s) / PAGE_SIZE;
                for i in 0..n {
                    unsafe { unmap_page(pt, clip_s + i * PAGE_SIZE); }
                }
            }

            // ── Reshape the VMA ───────────────────────────────────────────────
            if clip_s == r_start && clip_e == r_end {
                // Whole VMA removed.
                if !region.lazy && region.phys != 0 {
                    buddy_free(region.phys, pages_to_order((r_end - r_start) / PAGE_SIZE));
                }
                *slot = None;
            } else if clip_s == r_start {
                // Front trim: VMA shrinks to [clip_e, r_end).
                if region.lazy {
                    // Drain the entries for the removed prefix so index 0 aligns with the new start.
                    let shift = (clip_e - r_start) / PAGE_SIZE;
                    if shift < region.lazy_pages.len() {
                        region.lazy_pages.drain(0..shift);
                    } else {
                        region.lazy_pages.clear();
                    }
                } else if region.phys != 0 {
                    region.phys += clip_e - r_start;
                }
                region.start = clip_e;
            } else {
                // Back trim (or middle → leave left part, accept right leak for eager).
                region.end = clip_s;
            }

            did_unmap = true;
        }

        if did_unmap { tlb_shootdown_all(); }
    }

    /// Unmap `size` bytes starting at `virt` and free the backing pages.
    ///
    /// Delegates to [`unmap_range`]; kept for compatibility with existing call sites.
    pub fn unmap(&mut self, virt: usize, size: usize) {
        self.unmap_range(virt, size);
    }

    /// Look up the VmaRegion that contains `virt`, if any.
    pub fn find(&self, virt: usize) -> Option<&VmaRegion> {
        self.regions.iter()
            .filter_map(|r| r.as_ref())
            .find(|r| virt >= r.start && virt < r.end)
    }

    /// Translate a user virtual address to the physical address of its backing byte.
    ///
    /// For eager VMAs the backing memory is contiguous: `phys = vma.phys + (virt - vma.start)`.
    /// For lazy VMAs each faulted-in page is stored separately in `lazy_pages[]`.
    ///
    /// Returns `None` if:
    /// - no VMA covers `virt`, or
    /// - the containing VMA is lazy and the page hasn't been faulted in yet.
    pub fn virt_to_phys(&self, virt: usize) -> Option<usize> {
        let vma = self.find(virt)?;
        if vma.lazy {
            let offset     = virt - vma.start;
            let page_index = offset / PAGE_SIZE;
            let page_off   = offset % PAGE_SIZE;
            let phys_page  = vma.lazy_pages.get(page_index).copied().unwrap_or(0);
            if phys_page == 0 { return None; } // not yet faulted in
            Some(phys_page + page_off)
        } else {
            Some(vma.phys + (virt - vma.start))
        }
    }

    /// Read data from user virtual memory into a kernel buffer.
    pub fn read_user_buf(&self, user_va: usize, dest: &mut [u8]) -> bool {
        let mut offset = 0;
        while offset < dest.len() {
            let va = user_va + offset;
            let phys = match self.virt_to_phys(va) {
                Some(p) => p,
                None => return false,
            };
            
            // Calculate how many bytes we can read from this page
            let page_off = va % PAGE_SIZE;
            let avail = PAGE_SIZE - page_off;
            let chunk = usize::min(avail, dest.len() - offset);
            
            unsafe {
                let src_ptr = crate::phys_to_virt(phys) as *const u8;
                core::ptr::copy_nonoverlapping(src_ptr, dest.as_mut_ptr().add(offset), chunk);
            }
            offset += chunk;
        }
        true
    }

    /// Write data from a kernel buffer into user virtual memory.
    pub fn write_user_buf(&self, user_va: usize, src: &[u8]) -> bool {
        let mut offset = 0;
        while offset < src.len() {
            let va = user_va + offset;
            let phys = match self.virt_to_phys(va) {
                Some(p) => p,
                None => return false,
            };
            
            let page_off = va % PAGE_SIZE;
            let avail = PAGE_SIZE - page_off;
            let chunk = usize::min(avail, src.len() - offset);
            
            unsafe {
                let dest_ptr = crate::phys_to_virt(phys) as *mut u8;
                core::ptr::copy_nonoverlapping(src.as_ptr().add(offset), dest_ptr, chunk);
            }
            offset += chunk;
        }
        true
    }

    /// Change protection flags on `[addr, addr+len)`.
    ///
    /// Translates POSIX `prot` flags to `PageFlags` and remaps every already-
    /// faulted page in the affected VMAs.  W^X is enforced: PROT_WRITE and
    /// PROT_EXEC together return `false`.
    ///
    /// Returns `true` on success, `false` if the range is invalid or W^X
    /// would be violated.
    pub fn mprotect(&mut self, addr: usize, len: usize, prot: u32) -> bool {
        if prot & PROT_WRITE != 0 && prot & PROT_EXEC != 0 { return false; }

        let addr = addr & !(PAGE_SIZE - 1);
        let end  = match addr.checked_add((len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)) {
            Some(e) => e,
            None    => return false,
        };

        // Build the new PageFlags from the POSIX prot bits.
        let mut new_flags = PageFlags::PRESENT | PageFlags::USER;
        if prot & PROT_WRITE != 0 { new_flags |= PageFlags::WRITABLE; }
        if prot & PROT_EXEC  != 0 { new_flags |= PageFlags::EXECUTE; }

        let mut changed = false;
        for slot in self.regions.iter_mut() {
            let region = match slot.as_mut() {
                Some(r) if r.start < end && r.end > addr => r,
                _ => continue,
            };

            region.prot  = prot;
            region.flags = new_flags;

            // Remap pages that are already backed (lazy pages that have been faulted in).
            if region.lazy {
                for (i, &phys) in region.lazy_pages.iter().enumerate() {
                    if phys != 0 {
                        let page_va = region.start + i * PAGE_SIZE;
                        if page_va >= addr && page_va < end {
                            unsafe { map_page(self.page_table_root, page_va, phys, new_flags); }
                        }
                    }
                }
            } else if region.phys != 0 {
                let n_pages = (region.end - region.start) / PAGE_SIZE;
                for i in 0..n_pages {
                    let page_va = region.start + i * PAGE_SIZE;
                    if page_va >= addr && page_va < end {
                        unsafe { map_page(self.page_table_root, page_va, region.phys + i * PAGE_SIZE, new_flags); }
                    }
                }
            }
            changed = true;
        }

        if changed { tlb_shootdown_all(); }
        changed
    }

    /// Adjust the heap break (program break) for this address space.
    ///
    /// The heap VMA is identified as the one starting at `self.heap_start`
    /// (set by the ELF loader in Phase 1; zero for kernel tasks).
    ///
    /// Follows Linux `brk(2)` semantics:
    ///   - `new_end == 0` → query: return the current break without modifying anything.
    ///   - Success        → return the new break.
    ///   - Failure (OOM, overlap) → return the **current** break unchanged.
    ///     (musl detects failure by comparing the return value to the requested value,
    ///     NOT by checking for a negative return.)
    /// Return the page table root physical address.
    pub fn root(&self) -> usize {
        self.page_table_root
    }

    pub fn brk(&mut self, new_end: usize) -> isize {
        let current_break = if self.heap_end != 0 { self.heap_end } else { self.heap_start };

        // Query: return the current break without any modification.
        if new_end == 0 { return current_break as isize; }

        if self.heap_start == 0 { return current_break as isize; } // kernel task, no heap
        let new_end = (new_end + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);

        // Find the heap VMA (lazily created on first brk call after execve).
        let idx = match self.regions.iter().position(|r| {
            r.as_ref().map(|r| r.start == self.heap_start).unwrap_or(false)
        }) {
            Some(i) => i,
            None    => {
                // No heap VMA yet — create one on the first upward brk call.
                if new_end <= self.heap_start { return current_break as isize; }
                let flags = PageFlags::PRESENT | PageFlags::USER | PageFlags::WRITABLE;
                if self.map_lazy(self.heap_start, new_end - self.heap_start, flags) {
                    self.heap_end = new_end;
                    return new_end as isize;
                }
                return current_break as isize; // OOM: return unchanged break
            }
        };

        let region = self.regions[idx].as_mut().unwrap();
        if new_end == region.end {
            return new_end as isize; // no-op
        }

        if new_end > region.end {
            // Grow: check for overlap with other VMAs first.
            let old_end = region.end;
            for (i, slot) in self.regions.iter().enumerate() {
                if i == idx { continue; }
                if let Some(r) = slot {
                    if r.start < new_end && r.end > old_end {
                        return current_break as isize; // overlap: return unchanged
                    }
                }
            }
            self.regions[idx].as_mut().unwrap().end = new_end;
        } else {
            // Shrink: unmap and free pages from new_end to old_end.
            let region = self.regions[idx].as_mut().unwrap();
            let heap_start = region.start; // = self.heap_start
            let old_end    = region.end;
            region.end     = new_end;

            // Page indices are relative to the VMA start (heap_start).
            let first_idx = (new_end - heap_start) / PAGE_SIZE;
            let last_idx  = (old_end  - heap_start + PAGE_SIZE - 1) / PAGE_SIZE;
            for i in first_idx..last_idx.min(region.lazy_pages.len()) {
                if region.lazy_pages[i] != 0 {
                    let page_va = heap_start + i * PAGE_SIZE;
                    unsafe { unmap_page(self.page_table_root, page_va); }
                    buddy_free(region.lazy_pages[i], 0);
                    region.lazy_pages[i] = 0;
                    region.lazy_count = region.lazy_count.saturating_sub(1);
                }
            }
            tlb_shootdown_all();
        }

        self.heap_end = new_end;
        new_end as isize
    }
}

fn pages_to_order(pages: usize) -> usize {
    let mut order = 0;
    let mut cap   = 1usize;
    while cap < pages { cap <<= 1; order += 1; }
    order
}
