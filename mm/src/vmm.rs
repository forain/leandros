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
    /// Capability token for file-backed VMAs (0 = anonymous, usize::MAX =
    /// device mapping).  Any other value identifies a kernel-registered
    /// backing file; absent pages are populated through the file-read hook
    /// (see `set_file_backing_hooks`) instead of plain zero-fill.
    pub file_cap:  usize,
    /// Offset into the backing file (for file-backed VMAs).
    pub file_off:  u64,
    /// Number of bytes of file data backing this VMA, starting at `file_off`.
    /// Pages (or page tails) beyond this extent are zero-filled (BSS).
    pub file_len:  u64,
    /// True if this VMA is a copy-on-write clone; write faults allocate a
    /// new page and copy the content before remapping writable.
    pub cow:       bool,
}

// ── File-backed VMA hooks ─────────────────────────────────────────────────────
//
// The mm crate cannot call into the VFS/filesystem servers (they depend on
// mm, not the reverse), so the kernel registers three function pointers at
// boot.  All three run synchronously in the faulting task's context; the
// filesystem side services them with direct handler calls and polling I/O,
// so they never block or reschedule.

/// Read up to `len` bytes at byte `offset` of the backing file identified by
/// `file_cap` into `dst` (a kernel HHDM pointer).  Returns the number of
/// bytes read (short only at end of file) or a negative errno.
pub type FileReadFn = fn(file_cap: usize, offset: u64, dst: *mut u8, len: usize) -> isize;
/// Adjust the reference count of `file_cap` (one reference per live VMA).
pub type FileRefFn = fn(file_cap: usize);

use core::sync::atomic::{AtomicUsize, Ordering};
static FILE_READ_HOOK:    AtomicUsize = AtomicUsize::new(0);
static FILE_RETAIN_HOOK:  AtomicUsize = AtomicUsize::new(0);
static FILE_RELEASE_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register the kernel's file-backing callbacks.  Must be called once during
/// boot, before the first file-backed VMA is created.
pub fn set_file_backing_hooks(read: FileReadFn, retain: FileRefFn, release: FileRefFn) {
    FILE_READ_HOOK.store(read as usize, Ordering::Release);
    FILE_RETAIN_HOOK.store(retain as usize, Ordering::Release);
    FILE_RELEASE_HOOK.store(release as usize, Ordering::Release);
}

/// True for caps that name a registered backing file (not anonymous, not the
/// `usize::MAX` device-mapping sentinel).
#[inline]
pub fn is_file_backed(file_cap: usize) -> bool {
    file_cap != 0 && file_cap != usize::MAX
}

/// Read through the registered file hook (see [`FileReadFn`]). Public so
/// the scheduler's fault path can do the read with the address space
/// unlocked (see [`AddressSpace::plan_user_page_fault`]).
pub fn file_read(file_cap: usize, offset: u64, dst: *mut u8, len: usize) -> isize {
    let f = FILE_READ_HOOK.load(Ordering::Acquire);
    if f == 0 { return -5; }
    let f: FileReadFn = unsafe { core::mem::transmute(f) };
    f(file_cap, offset, dst, len)
}

/// Kernel-private `map_flags` bit (Linux never sets bit 30): the VMA is an
/// mmap(2) file mapping, so a page lying wholly past the file's *current*
/// end raises SIGBUS instead of being zero-filled, and the part of the last
/// page past EOF reads as zeros. ELF segments leave it clear: their span
/// past `p_filesz` is BSS and must zero-fill.
pub const MAP_EOF_SIGBUS: u32 = 1 << 30;

/// Outcome of a user page fault.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Fault {
    /// Resolved (or raced with a concurrent change): retry the access.
    Handled,
    /// Not mapped, or an access the mapping forbids: SIGSEGV.
    Segv,
    /// A file page past end of file, or a read error: SIGBUS.
    Bus,
}

/// A file-backed fault whose read the caller performs with the address
/// space *unlocked*, then hands to [`AddressSpace::install_file_fault`].
#[derive(Clone, Copy, Debug)]
pub struct FileFault {
    pub cap:     usize,
    /// Page-aligned faulting address.
    pub page_va: usize,
    /// File byte offset backing `page_va`.
    pub pos:     u64,
    /// Bytes to read at `pos` (0: the window is pure BSS).
    pub len:     usize,
    /// Pages in the fault-around window, starting at `page_va`.
    pub window:  usize,
    pub eof_sigbus: bool,
}

/// First half of a fault: either finished under the lock, or a file read to
/// do without it.
pub enum FaultPlan {
    Done(Fault),
    Read(FileFault),
}

pub fn file_retain(file_cap: usize) {
    let f = FILE_RETAIN_HOOK.load(Ordering::Acquire);
    if f == 0 { return; }
    let f: FileRefFn = unsafe { core::mem::transmute(f) };
    f(file_cap)
}

pub fn file_release(file_cap: usize) {
    let f = FILE_RELEASE_HOOK.load(Ordering::Acquire);
    if f == 0 { return; }
    let f: FileRefFn = unsafe { core::mem::transmute(f) };
    f(file_cap)
}

/// Pages a file-backed fault reads ahead in one go (64 KiB).
const FAULT_AROUND_PAGES: usize = 16;

/// Per-process address space.
pub struct AddressSpace {
    pub page_table_root: usize,
    pub regions: Vec<Option<VmaRegion>>,
    /// Virtual address where the heap begins (set by ELF loader; 0 = no heap).
    pub heap_start: usize,
    /// Current heap break (end of heap VMA).
    pub heap_end: usize,
    /// Exclusive-access flag, held (CAS true → work → store false) around
    /// every mutation of this address space — page-fault service, mmap/
    /// munmap/mprotect/brk, fork's CoW clone — *instead of* the global
    /// run-queue lock.  Address-space work allocates, copies whole pages,
    /// and waits on TLB-shootdown acknowledgements; doing that under the
    /// scheduler lock stalls every other CPU (see
    /// `sched::lock_leader_address_space`).
    pub busy: core::sync::atomic::AtomicBool,
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
                        if phys != 0 { crate::pageref::unref_or_free(phys, 0); }
                    }
                } else if region.phys != 0 && region.file_cap != usize::MAX {
                    let pages = (region.end - region.start) / PAGE_SIZE;
                    buddy_free(region.phys, pages_to_order(pages));
                }
                if is_file_backed(region.file_cap) {
                    file_release(region.file_cap);
                }
            }
        }
        // Free the page-table tree: every intermediate node `map_page`
        // allocated on the way down, then the root (PGD on AArch64, PML4 on
        // x86-64). Every level below the root was allocated by this address
        // space's own mappings (fork maps page by page into a fresh root;
        // nothing shares tables between roots), so the whole user tree goes
        // with it. Until 2026-09-18 only the root was returned, and every
        // PDPT/PD/PT page a process ever touched leaked for the rest of the
        // boot — ~50 pages per `brush -c true`, ~1000 per greeter chain.
        // The leaves were unmapped or discarded above, so the walk frees
        // nodes only (see `arch_free_user_page_tables`).
        if self.page_table_root != 0 {
            unsafe { crate::paging::free_user_page_tables(self.page_table_root); }
            buddy_free(self.page_table_root, 0);
        }
        // Flush stale TLB entries on all CPUs now that all mappings are gone.
        tlb_shootdown_all();
    }
}

impl AddressSpace {
    pub fn new(page_table_root: usize) -> Self {
        const NONE: Option<VmaRegion> = None;
        Self {
            page_table_root,
            regions: alloc::vec![NONE; 128],
            heap_start: 0,
            heap_end: 0,
            busy: core::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Map `size` bytes (rounded up to pages) at virtual address `virt`,
    /// backed by freshly allocated physical pages.
    ///
    /// Returns `true` on success, `false` if OOM or the VMA table is full.
    pub fn map(&mut self, virt: usize, size: usize, flags: PageFlags) -> bool {
        if size == 0 { return note_fail(FAIL_ZERO_SIZE); }

        // Find a free VMA slot.
        let slot = match self.regions.iter().position(|r| r.is_none()) {
            Some(i) => i,
            None    => {
                self.regions.push(None);
                self.regions.len() - 1
            }
        };

        // Align virt down and size up to page granularity.
        let virt  = virt & !(PAGE_SIZE - 1);
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let end   = match virt.checked_add(pages * PAGE_SIZE) {
            Some(e) => e,
            None    => return note_fail(FAIL_VA_OVERFLOW),
        };

        // Reject if the new range overlaps any existing VMA.
        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if virt < r.end && end > r.start { return note_fail(FAIL_VMA_OVERLAP); }
        }
        let order = pages_to_order(pages);

        let phys = match buddy_alloc(order) {
            Some(p) => p,
            None    => return note_fail(FAIL_BUDDY),
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
                return note_fail(FAIL_PTE_INSTALL);
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
            file_len:  0,
            cow:       false,
        });

        true
        }

        /// Map `size` bytes (rounded up to pages) at virtual address `virt`,
        /// backed by an existing physical address (e.g., a hardware framebuffer).
        ///
        /// Returns `true` on success, `false` if the VMA table is full or mapping fails.
        pub fn map_device(&mut self, virt: usize, phys: usize, size: usize, flags: PageFlags) -> bool {
        if size == 0 { return note_fail(FAIL_ZERO_SIZE); }

        // Find a free VMA slot.
        let slot = match self.regions.iter().position(|r| r.is_none()) {
            Some(i) => i,
            None    => {
                self.regions.push(None);
                self.regions.len() - 1
            }
        };

        // Align virt/phys down and size up to page granularity.
        let virt  = virt & !(PAGE_SIZE - 1);
        let phys  = phys & !(PAGE_SIZE - 1);
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let end   = match virt.checked_add(pages * PAGE_SIZE) {
            Some(e) => e,
            None    => return note_fail(FAIL_VA_OVERFLOW), // overflow → reject
        };

        // Reject if the new range overlaps any existing VMA.
        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if virt < r.end && end > r.start { return note_fail(FAIL_VMA_OVERLAP); }
        }

        // Map each page to the specified physical address.
        for i in 0..pages {
            let v = virt + i * PAGE_SIZE;
            let p = phys + i * PAGE_SIZE;
            unsafe {
                if !crate::paging::map_page(self.page_table_root, v, p, flags) {
                    for j in 0..i {
                        crate::paging::unmap_page(self.page_table_root, virt + j * PAGE_SIZE);
                    }
                    return note_fail(FAIL_PTE_INSTALL);
                }
            }
        }

        self.regions[slot] = Some(VmaRegion {
            start: virt,
            end,
            phys,
            flags,
            lazy: false,
            lazy_pages: Vec::new(),
            lazy_count: 0,
            prot:      PROT_READ | PROT_WRITE,
            map_flags: MAP_SHARED, // Devices are shared
            file_cap:  usize::MAX, // Special marker for device mappings (do not free)
            file_off:  0,
            file_len:  0,
            cow:       false,
        });

        true
        }


    /// Reserve a virtual address range without allocating physical pages.
    ///
    /// Each page is allocated and mapped on the first access that faults into
    /// it.  Mirrors `mmap(PROT_…, MAP_ANONYMOUS | MAP_PRIVATE | MAP_SHARED, …)`
    /// with no `MAP_POPULATE` flag; `is_shared` records which of
    /// `MAP_PRIVATE`/`MAP_SHARED` the caller actually requested so a later
    /// `fork()` knows whether to CoW-protect this region or share it with
    /// full permissions (see `mm::cow::clone_as`).
    ///
    /// Returns `true` on success, `false` if the VMA table is full or the range
    /// overlaps an existing VMA.
    pub fn map_lazy(&mut self, virt: usize, size: usize, flags: PageFlags, is_shared: bool) -> bool {
        if size == 0 { return note_fail(FAIL_ZERO_SIZE); }

        let slot = match self.regions.iter().position(|r| r.is_none()) {
            Some(i) => i,
            None    => {
                self.regions.push(None);
                self.regions.len() - 1
            }
        };

        let virt  = virt & !(PAGE_SIZE - 1);
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let end   = match virt.checked_add(pages * PAGE_SIZE) {
            Some(e) => e,
            None    => return note_fail(FAIL_VA_OVERFLOW),
        };

        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if virt < r.end && end > r.start {
                return note_fail(FAIL_VMA_OVERLAP);
            }
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
            map_flags: MAP_ANONYMOUS | if is_shared { MAP_SHARED } else { MAP_PRIVATE },
            file_cap:  0,
            file_off:  0,
            file_len:  0,
            cow:       false,
        });
        true
    }

    /// Map a run of already-allocated physical frames as a `MAP_SHARED`
    /// region, aliasing the frames rather than copying them.
    ///
    /// This is the K1 shared-VMO primitive: the frames belong to a tmpfs/memfd
    /// VMO (see `servers/vfs`), and the caller has **already** taken one
    /// `pageref` reference per frame (pin-before-publish, under the VMO lock).
    /// The VMA is shaped exactly like an already-faulted anonymous
    /// `MAP_SHARED` lazy region — `lazy = true` with `lazy_pages` pre-filled,
    /// `map_flags = MAP_SHARED`, `file_cap = 0`, `cow = false`, PTEs installed
    /// eagerly — so every existing fork/munmap/exit path handles it unchanged:
    /// `clone_as`'s `MAP_SHARED` branch pageref-incs and shares the frames,
    /// `unmap_range`/`AddressSpace::drop` `unref_or_free` them.
    ///
    /// The transferred +1 per frame becomes the VMA's reference, released by
    /// munmap/exit. This method therefore does **not** call `pageref::inc`.
    /// On any failure it unmaps whatever it installed and `unref_or_free`s
    /// **all** passed frames (dropping the caller's pins), returning `false`.
    pub fn map_shared_frames(&mut self, virt: usize, frames: &[usize], flags: PageFlags) -> bool {
        if frames.is_empty() {
            return note_fail(FAIL_ZERO_SIZE);
        }

        let release_all = || {
            for &phys in frames {
                if phys != 0 { crate::pageref::unref_or_free(phys, 0); }
            }
        };

        let slot = match self.regions.iter().position(|r| r.is_none()) {
            Some(i) => i,
            None    => {
                self.regions.push(None);
                self.regions.len() - 1
            }
        };

        let virt = virt & !(PAGE_SIZE - 1);
        let pages = frames.len();
        let end = match virt.checked_add(pages * PAGE_SIZE) {
            Some(e) => e,
            None    => { release_all(); return note_fail(FAIL_VA_OVERFLOW); }
        };

        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if virt < r.end && end > r.start { release_all(); return note_fail(FAIL_VMA_OVERLAP); }
        }

        // Install a PTE for each frame. On failure, roll back the PTEs already
        // installed, then release every pin.
        for i in 0..pages {
            let ok = unsafe {
                map_page(self.page_table_root, virt + i * PAGE_SIZE, frames[i], flags)
            };
            if !ok {
                for j in 0..i {
                    unsafe { unmap_page(self.page_table_root, virt + j * PAGE_SIZE); }
                }
                release_all();
                return note_fail(FAIL_PTE_INSTALL);
            }
        }

        self.regions[slot] = Some(VmaRegion {
            start: virt,
            end,
            phys: 0,
            flags,
            lazy: true,
            lazy_pages: frames.to_vec(),
            lazy_count: pages,
            prot:      {
                let mut p = PROT_READ;
                if flags.contains(PageFlags::WRITABLE) { p |= PROT_WRITE; }
                if flags.contains(PageFlags::EXECUTE)  { p |= PROT_EXEC; }
                p
            },
            map_flags: MAP_SHARED,
            file_cap:  0,
            file_off:  0,
            file_len:  0,
            cow:       false,
        });
        true
    }

    /// Reserve a file-backed virtual range without reading any data.
    ///
    /// The first access to each page faults; `handle_user_page_fault` then
    /// allocates the page and populates it from the backing file identified
    /// by `file_cap` (bytes `file_off .. file_off + file_len`; anything past
    /// that extent within the VMA is zero-filled — ELF BSS).  The mapping is
    /// private: pages diverge from the file once written and fork CoW-shares
    /// them like anonymous memory.
    ///
    /// Takes one reference on `file_cap` (released when the VMA is destroyed).
    pub fn map_lazy_file(
        &mut self,
        virt: usize,
        size: usize,
        flags: PageFlags,
        file_cap: usize,
        file_off: u64,
        file_len: u64,
    ) -> bool {
        self.map_lazy_file_with(virt, size, flags, file_cap, file_off, file_len, MAP_PRIVATE)
    }

    /// `mmap(2)` of a regular file, `MAP_PRIVATE`: demand-paged like an exec
    /// segment, but with Linux end-of-file semantics ([`MAP_EOF_SIGBUS`]):
    /// the whole VMA may be read from the file, and what the file no longer
    /// covers at fault time is SIGBUS (whole pages) or zeros (last page).
    /// Pages are private copies from the first fault, so a write needs no
    /// copy-on-write of its own; fork shares them CoW like anonymous memory.
    pub fn map_private_file(
        &mut self,
        virt: usize,
        size: usize,
        flags: PageFlags,
        file_cap: usize,
        file_off: u64,
    ) -> bool {
        let span = ((size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)) as u64;
        self.map_lazy_file_with(virt, size, flags, file_cap, file_off, span,
                                MAP_PRIVATE | MAP_EOF_SIGBUS)
    }

    fn map_lazy_file_with(
        &mut self,
        virt: usize,
        size: usize,
        flags: PageFlags,
        file_cap: usize,
        file_off: u64,
        file_len: u64,
        map_flags: u32,
    ) -> bool {
        if size == 0 { return note_fail(FAIL_ZERO_SIZE); }
        if !is_file_backed(file_cap) { return note_fail(FAIL_BAD_FILECAP); }

        let slot = match self.regions.iter().position(|r| r.is_none()) {
            Some(i) => i,
            None    => {
                self.regions.push(None);
                self.regions.len() - 1
            }
        };

        let virt  = virt & !(PAGE_SIZE - 1);
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let end   = match virt.checked_add(pages * PAGE_SIZE) {
            Some(e) => e,
            None    => return note_fail(FAIL_VA_OVERFLOW),
        };

        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if virt < r.end && end > r.start {
                return note_fail(FAIL_VMA_OVERLAP);
            }
        }

        let mut prot = PROT_READ;
        if flags.contains(PageFlags::WRITABLE) { prot |= PROT_WRITE; }
        if flags.contains(PageFlags::EXECUTE)  { prot |= PROT_EXEC; }

        file_retain(file_cap);
        self.regions[slot] = Some(VmaRegion {
            start: virt,
            end,
            phys: 0,
            flags,
            lazy: true,
            lazy_pages: Vec::new(),
            lazy_count: 0,
            prot,
            map_flags,
            file_cap,
            file_off,
            file_len,
            cow:       false,
        });
        true
    }

    /// Handle a user-mode page fault at `fault_va`.
    ///
    /// Looks up the VMA that contains `fault_va`.  Three cases:
    ///   - Not backed yet (lazy VMA, page never faulted in): allocate one
    ///     4 KiB physical page, zero it, map it.
    ///   - Backed, write fault, `region.cow`: promote — copy the page (or
    ///     reuse it in place if we're already the sole remaining owner) and
    ///     remap it writable in this address space only.
    ///   - Backed, anything else: a real protection violation.
    ///
    /// Returns `true` if the fault was handled (execution can resume), or `false`
    /// if `fault_va` is not within any VMA, or it's a genuine protection
    /// violation (segmentation fault either way).
    pub fn handle_user_page_fault(&mut self, fault_va: usize, is_write: bool) -> bool {
        self.user_page_fault(fault_va, is_write) == Fault::Handled
    }

    /// [`handle_user_page_fault`] with the SIGSEGV/SIGBUS distinction, doing
    /// any file read *while this address space is held*. Only for callers
    /// that already hold it (prefault, the ELF loader); the scheduler's
    /// fault entry splits the work with [`plan_user_page_fault`] instead.
    ///
    /// [`handle_user_page_fault`]: Self::handle_user_page_fault
    /// [`plan_user_page_fault`]: Self::plan_user_page_fault
    pub fn user_page_fault(&mut self, fault_va: usize, is_write: bool) -> Fault {
        match self.plan_user_page_fault(fault_va, is_write) {
            FaultPlan::Done(f) => f,
            FaultPlan::Read(p) => {
                let mut bounce: Vec<u8> = alloc::vec![0u8; p.len];
                let got = if p.len == 0 { 0 } else {
                    file_read(p.cap, p.pos, bounce.as_mut_ptr(), p.len)
                };
                self.install_file_fault(&p, &bounce, got)
            }
        }
    }

    /// First half of a user page fault.
    ///
    /// Looks up the VMA that contains `fault_va`:
    ///   - Not backed yet, anonymous: allocate one zeroed page and map it.
    ///   - Not backed yet, file-backed: return the read to perform
    ///     ([`FaultPlan::Read`]). The caller reads with the address space
    ///     *unlocked* — file I/O is milliseconds, and holding `busy` across
    ///     it stalls every sibling thread's fault and mm syscall — then
    ///     calls [`install_file_fault`](Self::install_file_fault).
    ///   - Backed, write fault, `region.cow`: promote — copy the page (or
    ///     reuse it in place if we're already the sole remaining owner) and
    ///     remap it writable in this address space only.
    ///   - Backed, anything else: a real protection violation.
    pub fn plan_user_page_fault(&mut self, fault_va: usize, is_write: bool) -> FaultPlan {
        let page_va = fault_va & !(PAGE_SIZE - 1);
        let page_table_root = self.page_table_root;

        // Find the VMA that covers the faulting address.
        let region = match self.regions.iter_mut().filter_map(|r| r.as_mut()).find(
            |r| fault_va >= r.start && fault_va < r.end
        ) {
            Some(r) => r,
            None    => return FaultPlan::Done(Fault::Segv), // not mapped at all
        };

        if !region.lazy {
            return FaultPlan::Done(Fault::Segv);
        }

        // Compute the page index within this VMA.
        let page_idx = (page_va - region.start) / PAGE_SIZE;

        let lazy_phys = region.lazy_pages.get(page_idx).copied().unwrap_or(0);
        if lazy_phys != 0 {
            // Page already present. A write to a CoW-shared page needs a
            // promotion (below). Any other fault on a present page is most
            // likely a *concurrent* fault: a sibling thread touched the same
            // fresh page, lost the race on the per-AS fault lock, and by the
            // time it got here the winner had already mapped the page. If
            // the region's protections allow the access, resume — the retry
            // will succeed. Only an access the region forbids is a real
            // protection violation.
            if !(is_write && region.cow) {
                return FaultPlan::Done(if !is_write || (region.prot & PROT_WRITE) != 0 { Fault::Handled } else { Fault::Segv });
            }

            // Serialize the get→copy→dec promotion against clone_as and
            // against promotions in the sibling address space — see
            // cow::COW_LOCK's doc comment.
            let _cow_guard = crate::cow::COW_LOCK.lock();
            let refcount = crate::pageref::get(lazy_phys);
            let new_phys = if refcount <= 1 {
                lazy_phys // sole remaining owner: no copy needed
            } else {
                let np = match buddy_alloc(0) {
                    Some(p) => p,
                    None    => return FaultPlan::Done(Fault::Segv), // OOM
                };
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        crate::phys_to_virt(lazy_phys) as *const u8,
                        crate::phys_to_virt(np)        as *mut u8,
                        PAGE_SIZE,
                    );
                }
                crate::pageref::dec(lazy_phys);
                np
            };

            if new_phys != lazy_phys {
                // Break-before-make: the PTE's output address changes, which
                // AArch64 only permits through an invalid entry + TLB flush.
                unsafe { unmap_page(page_table_root, page_va); }
                tlb_shootdown_all();
            }
            let mapped = unsafe { map_page(page_table_root, page_va, new_phys, region.flags) };
            if !mapped {
                if new_phys != lazy_phys { buddy_free(new_phys, 0); }
                return FaultPlan::Done(Fault::Segv);
            }
            region.lazy_pages[page_idx] = new_phys;
            // A *copy* promotion rewrote a live PTE to point at a different
            // frame (old shared → fresh copy). `map_page` (arch_map_page) issues
            // only a local store barrier, never a TLB invalidation — its barrier
            // reasoning covers invalid→valid transitions only. The threads of a
            // multithreaded process share this page table across CPUs, so a
            // sibling on another CPU would otherwise keep a stale TLB entry
            // pointing at the OLD frame. Broadcast an inner-shareable shootdown
            // to drop those stale entries, exactly as clone_as does after its
            // own downgrades. (Reuse-in-place keeps the same frame, so only the
            // frame-changing copy path needs this.)
            if new_phys != lazy_phys {
                tlb_shootdown_all();
            }
            return FaultPlan::Done(Fault::Handled);
        }

        // ── Absent page ───────────────────────────────────────────────────────
        //
        // File-backed VMAs (demand-paged exec images and private mmap(2) of
        // a file) fault around: a run of following absent pages is read in
        // the same fault, so one gathered file read (which the filesystem
        // turns into few multi-block device requests) replaces up to
        // FAULT_AROUND_PAGES separate fault round trips.
        if is_file_backed(region.file_cap) {
            let region_pages = (region.end - region.start) / PAGE_SIZE;
            let mut n = 1usize;
            while n < FAULT_AROUND_PAGES
                && page_idx + n < region_pages
                && region.lazy_pages.get(page_idx + n).copied().unwrap_or(0) == 0
            {
                n += 1;
            }
            // Window tails past the file extent are BSS and stay zero.
            let win_off = (page_idx * PAGE_SIZE) as u64;
            let len = if win_off < region.file_len {
                ((region.file_len - win_off) as usize).min(n * PAGE_SIZE)
            } else { 0 };
            return FaultPlan::Read(FileFault {
                cap: region.file_cap,
                page_va,
                pos: region.file_off + win_off,
                len,
                window: n,
                eof_sigbus: region.map_flags & MAP_EOF_SIGBUS != 0,
            });
        }

        // Anonymous: one zeroed page.
        let phys = match buddy_alloc(0) {
            Some(p) => p,
            None    => return FaultPlan::Done(Fault::Segv), // OOM
        };
        unsafe { (crate::phys_to_virt(phys) as *mut u8).write_bytes(0, PAGE_SIZE); }
        if !unsafe { map_page(page_table_root, page_va, phys, region.flags) } {
            buddy_free(phys, 0);
            return FaultPlan::Done(Fault::Segv);
        }
        if region.lazy_pages.len() <= page_idx {
            region.lazy_pages.resize(page_idx + 1, 0);
        }
        region.lazy_pages[page_idx] = phys;
        region.lazy_count += 1;
        FaultPlan::Done(Fault::Handled)
    }

    /// Second half of a file-backed fault: install the pages read for `p`.
    ///
    /// `data` holds the bytes read at `p.pos` and `got` is the read's result
    /// (bytes, or a negative errno). The address space was unlocked during
    /// the read, so everything is re-validated: if the VMA at `p.page_va` is
    /// gone or no longer maps the same file bytes (munmap, MAP_FIXED
    /// overlay, split with a different offset), nothing is installed and the
    /// access simply retries against the new state. Pages a concurrent fault
    /// populated meanwhile are left alone.
    pub fn install_file_fault(&mut self, p: &FileFault, data: &[u8], got: isize) -> Fault {
        let page_table_root = self.page_table_root;
        let region = match self.regions.iter_mut().filter_map(|r| r.as_mut()).find(
            |r| p.page_va >= r.start && p.page_va < r.end
        ) {
            Some(r) => r,
            None    => return Fault::Handled, // unmapped meanwhile: the retry decides
        };
        let page_idx = (p.page_va - region.start) / PAGE_SIZE;
        if !region.lazy
            || region.file_cap != p.cap
            || region.file_off + (page_idx * PAGE_SIZE) as u64 != p.pos
            || (region.map_flags & MAP_EOF_SIGBUS != 0) != p.eof_sigbus
        {
            return Fault::Handled;
        }
        if region.lazy_pages.get(page_idx).copied().unwrap_or(0) != 0 {
            return Fault::Handled; // a sibling's fault got there first
        }

        // How many bytes are valid, and how many pages to populate.
        let (valid, pages) = if p.eof_sigbus {
            if got < 0 { return Fault::Bus; } // I/O error: SIGBUS, as on Linux
            let valid = (got as usize).min(p.len).min(data.len());
            if valid == 0 { return Fault::Bus; } // page wholly past EOF
            // Pages wholly past the end stay absent, so touching them later
            // raises SIGBUS (or reads data the file has grown into).
            (valid, ((valid + PAGE_SIZE - 1) / PAGE_SIZE).min(p.window))
        } else {
            // ELF segment: the file extent is exact; a short read is an error.
            if p.len != 0 && got != p.len as isize { return Fault::Segv; }
            (p.len.min(data.len()), p.window)
        };

        let region_pages = (region.end - region.start) / PAGE_SIZE;
        if region.lazy_pages.len() < (page_idx + pages).min(region_pages) {
            region.lazy_pages.resize((page_idx + pages).min(region_pages), 0);
        }

        for i in 0..pages {
            let idx = page_idx + i;
            if idx >= region_pages { break; }
            if region.lazy_pages[idx] != 0 { continue; }
            let phys = match buddy_alloc(0) {
                Some(p) => p,
                // OOM on a fault-around page is not a failure as long as the
                // faulting page itself (i == 0) was populated.
                None => return if i > 0 { Fault::Handled } else { Fault::Segv },
            };
            let dst = crate::phys_to_virt(phys) as *mut u8;
            unsafe { dst.write_bytes(0, PAGE_SIZE); }
            let copy_start = i * PAGE_SIZE;
            if copy_start < valid {
                let n = (valid - copy_start).min(PAGE_SIZE);
                unsafe {
                    core::ptr::copy_nonoverlapping(data.as_ptr().add(copy_start), dst, n);
                }
            }
            // AArch64: clean the D-cache for executable pages so the I-cache
            // invalidate below refetches the freshly written bytes.
            #[cfg(target_arch = "aarch64")]
            if region.flags.contains(PageFlags::EXECUTE) {
                unsafe {
                    let mut line = dst as usize & !63;
                    let end_a = dst as usize + PAGE_SIZE;
                    while line < end_a {
                        core::arch::asm!("dc cvac, {}", in(reg) line);
                        line += 64;
                    }
                }
            }
            let mapped = unsafe {
                map_page(page_table_root, region.start + idx * PAGE_SIZE, phys, region.flags)
            };
            if !mapped {
                buddy_free(phys, 0);
                return if i > 0 { Fault::Handled } else { Fault::Segv };
            }
            region.lazy_pages[idx] = phys;
            region.lazy_count += 1;
        }

        #[cfg(target_arch = "aarch64")]
        if region.flags.contains(PageFlags::EXECUTE) {
            unsafe {
                core::arch::asm!("ic iallu");
                core::arch::asm!("isb");
            }
        }

        Fault::Handled
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
                self.handle_user_page_fault(va, false);
            } else {
                // A kernel store into a writable page still shared
                // copy-on-write would fault (the PTE is read-only for the
                // kernel too) — possibly under a lock the fault path cannot
                // take. Prefaulting promises no fault afterwards, so give
                // this side its private copy now.
                let _ = self.unshare_cow_page(va, true);
            }
            va += PAGE_SIZE;
        }
    }

    /// If the page at `va` is still shared copy-on-write with another address
    /// space, promote it to a private copy here (as a user write fault would).
    ///
    /// Required before any store the kernel makes through the HHDM
    /// (`write_user_buf`): that path bypasses the read-only PTE, so without
    /// this the store lands in the frame the fork sibling still maps.
    /// Returns false only when the private copy could not be made (OOM).
    /// `only_writable` restricts it to VMAs userspace may write (the
    /// prefault case, which also covers buffers the kernel only reads).
    pub fn unshare_cow_page(&mut self, va: usize, only_writable: bool) -> bool {
        let shared = match self.regions.iter().filter_map(|r| r.as_ref())
            .find(|r| va >= r.start && va < r.end)
        {
            Some(r) if r.lazy && r.cow
                && (!only_writable || r.flags.contains(PageFlags::WRITABLE)) =>
            {
                let idx = ((va & !(PAGE_SIZE - 1)) - r.start) / PAGE_SIZE;
                let phys = r.lazy_pages.get(idx).copied().unwrap_or(0);
                // Refcount 1 cannot rise under us: only a fork of an address
                // space mapping this frame could raise it, and the only such
                // space is this one, whose busy lock the caller holds.
                phys != 0 && crate::pageref::get(phys) > 1
            }
            _ => false,
        };
        // false only when a needed private copy could not be made (OOM).
        !shared || self.handle_user_page_fault(va, true)
    }

    /// Split the VMA that *strictly* contains the page-aligned `boundary` into
    /// two adjacent VMAs `[start, boundary)` (kept in the original slot) and
    /// `[boundary, end)` (moved to a fresh slot).  Returns `true` if a split
    /// occurred.
    ///
    /// No-op (`false`) when `boundary` coincides with a VMA edge or falls in a
    /// hole (nothing to split), or when the containing VMA is a device mapping
    /// (`file_cap == usize::MAX`) — device ranges are mapped and torn down
    /// whole and are never sub-divided.
    ///
    /// The physical↔virtual mapping is untouched; only the VMA bookkeeping is
    /// repartitioned, so no PTE is rewritten here.  An *eager* (contiguous
    /// buddy-backed) VMA is first converted to per-page lazy tracking — exactly
    /// the transformation `mm::cow::clone_as` performs — so the two halves each
    /// own and free their own pages independently.  The original buddy block is
    /// reconstructed by the allocator's coalescing as its pages are freed one
    /// order-0 page at a time.
    ///
    /// This is the shared primitive behind `unmap_range`'s middle-punch and
    /// `mprotect`'s sub-range: the dynamic loader (`ld.so`) relies on both when
    /// it `MAP_FIXED`-overlays library segments and applies RELRO.
    pub fn split_at(&mut self, boundary: usize) -> bool {
        let boundary = boundary & !(PAGE_SIZE - 1);

        // Locate the VMA whose interior the boundary lands in.
        let idx = match self.regions.iter().position(
            |slot| matches!(slot, Some(r) if r.start < boundary && boundary < r.end)
        ) {
            Some(i) => i,
            None    => return false,
        };
        // Device mappings are never split.
        if matches!(self.regions[idx], Some(ref r) if r.file_cap == usize::MAX) {
            return false;
        }

        // Convert eager → per-page lazy in place, then peel the tail pages off.
        let (r_end, tail_pages, flags, prot, map_flags,
             file_cap, right_off, right_len, cow) = {
            let r = self.regions[idx].as_mut().unwrap();
            if !r.lazy {
                let n = (r.end - r.start) / PAGE_SIZE;
                let mut v = Vec::with_capacity(n);
                for i in 0..n { v.push(r.phys + i * PAGE_SIZE); }
                free_eager_tail(r.phys, n);
                r.lazy = true;
                r.phys = 0;
                r.lazy_pages = v;
                r.lazy_count = n;
            }

            let split_idx   = (boundary - r.start) / PAGE_SIZE;
            let split_bytes = (split_idx * PAGE_SIZE) as u64;
            let orig_len    = r.file_len;

            let tail = if split_idx < r.lazy_pages.len() {
                r.lazy_pages.split_off(split_idx)
            } else {
                Vec::new()
            };
            let r_end = r.end;

            // Left half: ends at boundary, keeps its file offset but its file
            // extent is clipped to what precedes the boundary.
            r.end = boundary;
            r.lazy_count = r.lazy_pages.iter().filter(|&&p| p != 0).count();
            let right_off = r.file_off + split_bytes;
            let right_len = orig_len.saturating_sub(split_bytes);
            if is_file_backed(r.file_cap) && r.file_len > split_bytes {
                r.file_len = split_bytes;
            }

            (r_end, tail, r.flags, r.prot, r.map_flags,
             r.file_cap, right_off, right_len, r.cow)
        };

        // Each surviving VMA holds its own reference on the backing file.
        if is_file_backed(file_cap) { file_retain(file_cap); }

        let right_count = tail_pages.iter().filter(|&&p| p != 0).count();
        let new_region = VmaRegion {
            start: boundary,
            end:   r_end,
            phys:  0,
            flags,
            lazy:  true,
            lazy_pages: tail_pages,
            lazy_count: right_count,
            prot,
            map_flags,
            file_cap,
            file_off: right_off,
            file_len: right_len,
            cow,
        };

        match self.regions.iter().position(|r| r.is_none()) {
            Some(i) => self.regions[i] = Some(new_region),
            None    => self.regions.push(Some(new_region)),
        }
        true
    }

    /// Unmap a virtual address range `[virt, virt+len)`, freeing any backing pages.
    ///
    /// Splits any VMA straddling either end of the range (see [`split_at`]) so
    /// that every overlap becomes a wholly-contained VMA, then removes those
    /// VMAs in full.  This makes front-, back-, and middle-punches uniform and
    /// leak-free — including the middle-punch the dynamic loader triggers with
    /// its `MAP_FIXED` segment overlays.
    ///
    /// [`split_at`]: Self::split_at
    pub fn unmap_range(&mut self, virt: usize, len: usize) {
        if len == 0 { return; }
        let virt = virt & !(PAGE_SIZE - 1);
        let len  = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let end  = match virt.checked_add(len) { Some(e) => e, None => return };

        // After splitting at both boundaries, any VMA overlapping [virt,end) is
        // fully contained within it.
        self.split_at(virt);
        self.split_at(end);

        let pt = self.page_table_root;
        let mut did_unmap = false;

        for slot in self.regions.iter_mut() {
            let region = match slot {
                Some(r) if r.start >= virt && r.end <= end && r.start < r.end => r,
                _ => continue,
            };

            let r_start = region.start;
            let r_end   = region.end;
            let n_pages = (r_end - r_start) / PAGE_SIZE;

            if region.lazy {
                for i in 0..region.lazy_pages.len() {
                    let phys = region.lazy_pages[i];
                    if phys != 0 {
                        unsafe { unmap_page(pt, r_start + i * PAGE_SIZE); }
                        crate::pageref::unref_or_free(phys, 0);
                        did_unmap = true;
                    }
                }
            } else if region.file_cap == usize::MAX {
                // Device mapping: drop the PTEs but never free the phys range.
                for i in 0..n_pages { unsafe { unmap_page(pt, r_start + i * PAGE_SIZE); } }
                did_unmap = true;
            } else {
                // Eager, contiguous buddy-backed block: unmap and free whole.
                for i in 0..n_pages { unsafe { unmap_page(pt, r_start + i * PAGE_SIZE); } }
                if region.phys != 0 {
                    buddy_free(region.phys, pages_to_order(n_pages));
                }
                did_unmap = true;
            }

            if is_file_backed(region.file_cap) {
                file_release(region.file_cap);
            }
            *slot = None;
        }

        // Only a PTE that was present can be cached in a TLB (an absent
        // lazy page never had one; every path that clears a lazy page's PTE
        // shoots down itself). So a VMA that was never touched — ld.so's
        // whole-library reservation it then MAP_FIXED-overlays with the
        // segments, now that file mappings are demand-paged — goes without
        // the all-CPU shootdown, which cost 0.5-1.2 s per overlay on
        // x86_64/TCG.
        if did_unmap { tlb_shootdown_all(); }
    }

    /// Unmap `size` bytes starting at `virt` and free the backing pages.
    ///
    /// Delegates to [`unmap_range`]; kept for compatibility with existing call sites.
    pub fn unmap(&mut self, virt: usize, size: usize) {
        self.unmap_range(virt, size);
    }

    /// Lowest page in `[from, end)` that belongs to a file-backed VMA and is
    /// not populated yet — what a kernel prefault must read in (unlocked,
    /// through the real fault path) before its locked `prefault_range` walk.
    pub fn first_absent_file_page(&self, from: usize, end: usize) -> Option<usize> {
        let from = from & !(PAGE_SIZE - 1);
        let mut best: Option<usize> = None;
        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if !r.lazy || !is_file_backed(r.file_cap) || r.end <= from || r.start >= end {
                continue;
            }
            let mut va = from.max(r.start);
            let stop = end.min(r.end);
            while va < stop && best.map_or(true, |b| va < b) {
                let idx = (va - r.start) / PAGE_SIZE;
                if r.lazy_pages.get(idx).copied().unwrap_or(0) == 0 {
                    best = Some(va);
                    break;
                }
                va += PAGE_SIZE;
            }
        }
        best
    }

    /// Record the POSIX protection of the VMA starting at `virt` (used by
    /// mmap's eager file copy, which installs the final page flags directly
    /// through `map`, whose VMA would otherwise claim PROT_READ|PROT_WRITE).
    pub fn set_prot(&mut self, virt: usize, prot: u32) {
        let virt = virt & !(PAGE_SIZE - 1);
        if let Some(r) = self.regions.iter_mut().filter_map(|r| r.as_mut())
            .find(|r| r.start == virt)
        {
            r.prot = prot;
        }
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
    ///
    /// The store goes through the HHDM, not the user mapping, so it ignores
    /// the PTE's write protection: a page still shared copy-on-write with a
    /// fork sibling is unshared first, or the sibling would see the write.
    pub fn write_user_buf(&mut self, user_va: usize, src: &[u8]) -> bool {
        let mut offset = 0;
        while offset < src.len() {
            let va = user_va + offset;
            if !self.unshare_cow_page(va, false) { return false; }
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

        // Split VMAs straddling either boundary so a sub-range mprotect changes
        // only the pages it names, never the flags recorded for the writable
        // remainder of the VMA.  This is what makes the dynamic loader's RELRO
        // pass (`mprotect(relro_start, len, PROT_READ)` over part of the RW LOAD
        // segment) correct: without the split the whole segment's recorded
        // flags would flip read-only and a later fault would re-install live
        // `.data` pages read-only.
        self.split_at(addr);
        self.split_at(end);

        // Build the new PageFlags from the POSIX prot bits.
        let mut new_flags = PageFlags::PRESENT | PageFlags::USER;
        if prot & PROT_WRITE != 0 { new_flags |= PageFlags::WRITABLE; }
        if prot & PROT_EXEC  != 0 { new_flags |= PageFlags::EXECUTE; }

        let mut changed = false;
        for slot in self.regions.iter_mut() {
            let region = match slot.as_mut() {
                Some(r) if r.start >= addr && r.end <= end && r.start < r.end => r,
                _ => continue,
            };

            region.prot  = prot;
            region.flags = new_flags;

            // Remap every already-backed page of this now wholly-contained VMA.
            if region.lazy {
                let is_cow = region.cow;
                for (i, &phys) in region.lazy_pages.iter().enumerate() {
                    if phys != 0 {
                        let page_va = region.start + i * PAGE_SIZE;
                        // A page still shared with another address space must
                        // stay read-only at the PTE level regardless of the
                        // requested prot — region.flags (set above) already
                        // records the real target permission, so the CoW
                        // promotion fault handler applies it in full once this
                        // page is no longer shared.
                        let install = if is_cow && crate::pageref::get(phys) > 1 {
                            new_flags & !PageFlags::WRITABLE
                        } else {
                            new_flags
                        };
                        unsafe { map_page(self.page_table_root, page_va, phys, install); }
                    }
                }
            } else if region.phys != 0 {
                let n_pages = (region.end - region.start) / PAGE_SIZE;
                for i in 0..n_pages {
                    let page_va = region.start + i * PAGE_SIZE;
                    unsafe { map_page(self.page_table_root, page_va, region.phys + i * PAGE_SIZE, new_flags); }
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

        if self.heap_start == 0 {
            note_fail(FAIL_NO_HEAP);
            return current_break as isize; // kernel task, no heap
        }
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
                if self.map_lazy(self.heap_start, new_end - self.heap_start, flags, false) {
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
                        note_fail(FAIL_VMA_OVERLAP);
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
                    crate::pageref::unref_or_free(region.lazy_pages[i], 0);
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

// ── Why the last mapping call refused ────────────────────────────────────────
//
// `map`, `map_lazy`, `map_device`, `map_shared_frames`, `map_lazy_file` and
// `brk` all answer `false` (or an unchanged break), and every caller turns that
// into the single errno 12. A VMA overlap, an address-space overflow and an
// exhausted buddy order are three different bugs wearing one number. This cell
// keeps the last reason so the syscall layer can name it; it is advisory
// diagnostics only -- nothing branches on it.

static LAST_MAP_FAIL: AtomicUsize = AtomicUsize::new(0);

const FAIL_NONE:        usize = 0;
const FAIL_ZERO_SIZE:   usize = 1;
const FAIL_VMA_OVERLAP: usize = 2;
const FAIL_VA_OVERFLOW: usize = 3;
const FAIL_BUDDY:       usize = 4;
const FAIL_PTE_INSTALL: usize = 5;
const FAIL_NO_HEAP:     usize = 6;
const FAIL_BAD_FILECAP: usize = 7;

fn note_fail(code: usize) -> bool {
    LAST_MAP_FAIL.store(code, Ordering::Relaxed);
    false
}

/// Reason for the most recent mapping refusal, as a stable short token.
///
/// `buddy-order-unavailable` is the one that looks like "out of RAM" and is
/// not: `map` and `map_device` ask the buddy allocator for `2^order`
/// *physically contiguous* pages, which a fragmented heap can refuse with
/// gigabytes free.
pub fn last_map_fail() -> &'static str {
    match LAST_MAP_FAIL.load(Ordering::Relaxed) {
        FAIL_NONE        => "none-recorded",
        FAIL_ZERO_SIZE   => "zero-size",
        FAIL_VMA_OVERLAP => "vma-overlap",
        FAIL_VA_OVERFLOW => "va-overflow",
        FAIL_BUDDY       => "buddy-order-unavailable",
        FAIL_PTE_INSTALL => "pte-install",
        FAIL_NO_HEAP     => "no-heap-vma",
        FAIL_BAD_FILECAP => "bad-file-cap",
        _                => "unrecorded",
    }
}

/// Free the rounding tail of an eager VMA's buddy block.
///
/// `map` backs an `n`-page VMA with one naturally aligned block of
/// `2^pages_to_order(n)` pages and maps only the first `n`; the rest is
/// reachable solely through that order, which is how `Drop` and
/// `unmap_range` give the whole block back. The moment a VMA goes from eager
/// to per-page tracking (`split_at`, fork's `clone_as`) the order is
/// forgotten and each side frees exactly its `n` pages — so the tail, never
/// mapped and owned by nobody, must be returned right there. Until
/// 2026-09-24 it was not: a 5 MiB file mmap (8 MiB block) that `ld.so` then
/// MAP_FIXED-overlaid or RELRO-mprotected leaked 3 MiB for the rest of the
/// boot, ~14k pages (~55 MiB) per greeter-chain death on x86_64.
///
/// Freed as the largest aligned sub-blocks (the block is aligned to its
/// order, so every sub-block is aligned to its own), not page by page.
pub(crate) fn free_eager_tail(phys: usize, n: usize) {
    if phys == 0 || n == 0 { return; }
    let total = 1usize << pages_to_order(n);
    let mut i = n;
    while i < total {
        // Largest k with i aligned to 2^k and i + 2^k <= total.
        let mut k = i.trailing_zeros() as usize;
        while i + (1usize << k) > total { k -= 1; }
        buddy_free(phys + i * PAGE_SIZE, k);
        i += 1usize << k;
    }
}

fn pages_to_order(pages: usize) -> usize {
    let mut order = 0;
    let mut cap   = 1usize;
    while cap < pages { cap <<= 1; order += 1; }
    order
}
