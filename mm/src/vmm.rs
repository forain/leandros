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
use crate::paging::{PageFlags, map_page, unmap_page, tlb_flush_range, tlb_flush_range_start, tlb_flush_as, tlb_flush_local_page};
use crate::buddy::{PAGE_SIZE, alloc as buddy_alloc, free as buddy_free};

// ── POSIX mmap/mprotect protection flags ─────────────────────────────────────
pub const PROT_NONE:  u32 = 0;
pub const PROT_READ:  u32 = 1 << 0;
pub const PROT_WRITE: u32 = 1 << 1;
pub const PROT_EXEC:  u32 = 1 << 2;
/// Any access bit; a VMA whose `prot` has none of them is PROT_NONE.
pub const PROT_ACCESS: u32 = PROT_READ | PROT_WRITE | PROT_EXEC;

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
    pub lazy_pages: crate::pagevec::PageVec,
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
    /// Private file-backed VMAs only: bit `i` set once page `i` (inside the
    /// file extent) has been written — it is then an anonymous copy, counted
    /// `RssAnon` like Linux's CoW'd private file page, not `RssFile`. A clean
    /// such page is mapped read-only even in a writable VMA, so its first
    /// write faults here (`plan_user_page_fault`) and sets the bit. Only
    /// meaningful where `lazy_pages[i] != 0`: every install clears it.
    pub written:   Vec<u64>,
    /// Shared anonymous VMAs only: the object every mapping of this memory
    /// (across fork and VMA splits) resolves absent pages through, and the
    /// object page of this VMA's first page. See [`crate::shmem`].
    pub shm:       Option<alloc::sync::Arc<crate::shmem::ShmObj>>,
    pub shm_pgoff: usize,
    /// tmpfs/memfd `MAP_SHARED` VMAs only (`map_shared_frames` + [`AddressSpace::set_vmo`]):
    /// the VFS's identity of the VMO whose frames this VMA aliases (0 = none),
    /// with `file_off` the byte offset of the VMA's first page in it. Used to
    /// find every mapping of the object when the file shrinks, to grow the
    /// mapping (`mremap`), and to name it in `/proc/<pid>/maps`. An absent
    /// page of such a VMA (zapped by truncation) raises SIGBUS.
    pub vmo:       u64,
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

// ── Deferred frame release behind an asynchronous TLB shootdown ─────────────
//
// Frames, page-table pages and device ranges a flush protects may only be
// freed once every remote CPU acknowledged it. Waiting for that with the
// address-space lock held convoys every sibling thread behind the slowest
// acknowledger, so the unmap paths start the flush, queue what it protects
// here, and the wait happens after the lock is dropped ([`drain_deferred`]).
// A timed-out wait never frees: the entry stays queued until the ack arrives.
struct Deferred {
    tickets: Vec<crate::paging::FlushTicket>,
    frames: Vec<usize>,
    blocks: Vec<(usize, usize)>,
    dev: Vec<(usize, usize)>,
    tables: Vec<usize>,
}

static DEFERRED: spin::Mutex<Vec<Deferred>> = spin::Mutex::new(Vec::new());
static DEFERRED_LEN: AtomicUsize = AtomicUsize::new(0);
/// Frees postponed because a shootdown ack was not yet in.
pub static DEFERRED_FREES: AtomicUsize = AtomicUsize::new(0);
/// Waits that timed out (entry kept, nothing freed).
pub static DEFERRED_TIMEOUTS: AtomicUsize = AtomicUsize::new(0);

fn release_deferred(d: Deferred) {
    for phys in d.frames { crate::pageref::unref_or_free(phys, 0); }
    for (phys, order) in d.blocks { buddy_free(phys, order); }
    for (phys, len) in d.dev { device_vma_release(phys, len); }
    if !d.tables.is_empty() {
        PT_PAGES_PRUNED.fetch_add(d.tables.len(), Ordering::Relaxed);
        for phys in d.tables { buddy_free(phys, 0); }
    }
}

/// Free `d` now if every flush it waits for is already acknowledged, else queue it.
fn free_after_flush(mut d: Deferred) {
    d.tickets.retain_mut(|t| !crate::paging::flush_poll(t));
    if d.tickets.is_empty() { release_deferred(d); return; }
    DEFERRED_FREES.fetch_add(1, Ordering::Relaxed);
    DEFERRED_LEN.fetch_add(1, Ordering::Relaxed);
    DEFERRED.lock().push(d);
}

/// Release queued entries whose shootdowns completed. `wait`: bounded wait
/// for the others (call with no address-space lock held).
pub fn drain_deferred(wait: bool) {
    if DEFERRED_LEN.load(Ordering::Relaxed) == 0 { return; }
    let list = core::mem::take(&mut *DEFERRED.lock());
    DEFERRED_LEN.fetch_sub(list.len(), Ordering::Relaxed);
    let mut keep: Vec<Deferred> = Vec::new();
    for mut d in list {
        d.tickets.retain_mut(|t| {
            if wait { !crate::paging::flush_wait(t) } else { !crate::paging::flush_poll(t) }
        });
        if d.tickets.is_empty() { release_deferred(d); } else {
            if wait { DEFERRED_TIMEOUTS.fetch_add(1, Ordering::Relaxed); }
            keep.push(d);
        }
    }
    if !keep.is_empty() {
        DEFERRED_LEN.fetch_add(keep.len(), Ordering::Relaxed);
        DEFERRED.lock().extend(keep);
    }
}

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

/// Reference a device range that a VMA maps (`phys`, `len` bytes). Called once
/// per device VMA that comes into being by aliasing (fork), and the release
/// once per device VMA that goes away (munmap, exit/exec teardown). The first
/// reference of a fresh `map_device` VMA is taken by its creator (sys_mmap),
/// not here. Lets a driver keep a device range (a virtio-gpu host-visible
/// window span) reserved until the last user mapping of it is gone.
pub type DeviceRefFn = fn(phys: usize, len: usize);
static DEVICE_RETAIN_HOOK:  AtomicUsize = AtomicUsize::new(0);
static DEVICE_RELEASE_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register the device-VMA reference callbacks (once, at boot).
pub fn set_device_vma_hooks(retain: DeviceRefFn, release: DeviceRefFn) {
    DEVICE_RETAIN_HOOK.store(retain as usize, Ordering::Release);
    DEVICE_RELEASE_HOOK.store(release as usize, Ordering::Release);
}

pub fn device_vma_retain(phys: usize, len: usize) {
    let f = DEVICE_RETAIN_HOOK.load(Ordering::Acquire);
    if f == 0 { return; }
    let f: DeviceRefFn = unsafe { core::mem::transmute(f) };
    f(phys, len)
}

pub fn device_vma_release(phys: usize, len: usize) {
    let f = DEVICE_RELEASE_HOOK.load(Ordering::Acquire);
    if f == 0 { return; }
    let f: DeviceRefFn = unsafe { core::mem::transmute(f) };
    f(phys, len)
}

/// Page-cache key of a backing file cap: a stable identity of the file
/// (mount + inode), 0 when its pages must not be shared through
/// [`crate::pagecache`].
pub type FileKeyFn = fn(file_cap: usize) -> u64;
static FILE_KEY_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register the kernel's cap → page-cache key callback (see [`FileKeyFn`]).
/// Write the path of the backing file `file_cap` into `out`; returns its
/// length (0: unknown). For `/proc/<pid>/maps`; never called under `busy`.
pub type FileNameFn = fn(file_cap: usize, out: &mut [u8]) -> usize;
static FILE_NAME_HOOK: AtomicUsize = AtomicUsize::new(0);

pub fn set_file_name_hook(f: FileNameFn) {
    FILE_NAME_HOOK.store(f as usize, Ordering::Release);
}

/// The path `file_cap` was mapped through (see [`FileNameFn`]).
pub fn file_name(file_cap: usize, out: &mut [u8]) -> usize {
    let f = FILE_NAME_HOOK.load(Ordering::Acquire);
    if f == 0 || !is_file_backed(file_cap) { return 0; }
    let f: FileNameFn = unsafe { core::mem::transmute(f) };
    f(file_cap, out)
}

pub fn set_file_key_hook(f: FileKeyFn) {
    FILE_KEY_HOOK.store(f as usize, Ordering::Release);
}

/// Page-cache key of `file_cap` (0: not cacheable).
pub fn file_key(file_cap: usize) -> u64 {
    if !is_file_backed(file_cap) { return 0; }
    let f = FILE_KEY_HOOK.load(Ordering::Acquire);
    if f == 0 { return 0; }
    let f: FileKeyFn = unsafe { core::mem::transmute(f) };
    f(file_cap)
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

/// Bumped (before any address space is visited) by every truncate/hole-punch
/// zap. A fault that read its backing with the address space unlocked
/// installs only if no zap started meanwhile: the bytes it read may predate
/// the shrink, and the zap may already have passed this address space. A
/// stale fault simply retries. Global: shrinks of mapped objects are rare,
/// so a false positive costs one re-read.
static BACKING_SHRINK_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Note that an object's backing is about to be zapped (see
/// [`BACKING_SHRINK_SEQ`]). Call before visiting any address space.
pub fn backing_shrink_begin() {
    BACKING_SHRINK_SEQ.fetch_add(1, Ordering::SeqCst);
}

fn backing_seq() -> u64 { BACKING_SHRINK_SEQ.load(Ordering::SeqCst) }

/// One referenced frame of tmpfs/memfd object `vmo` holding byte `off`
/// (allocated zeroed if it is a hole), or None if `off` lies past the
/// object's end (or it is gone). Takes the VFS's tmpfs locks, so it is only
/// ever called with the address space unlocked.
pub type VmoPageFn = fn(vmo: u64, off: u64, out: &mut [usize]) -> usize;
/// Pages a VMO fault maps around the faulting one (aligned window).
pub const VMO_FAULT_AROUND: usize = 32;
static VMO_PAGE_HOOK: AtomicUsize = AtomicUsize::new(0);

/// Register the VFS's tmpfs/memfd page lookup (see [`VmoPageFn`]).
pub fn set_vmo_page_hook(f: VmoPageFn) {
    VMO_PAGE_HOOK.store(f as usize, Ordering::Release);
}

/// Acquire the frames of pages `off/4096 ..` into `out` through the
/// registered hook in one call (see [`VmoPageFn`]); returns how many
/// consecutive pages were acquired (0: none, the first is past the end).
pub fn vmo_pages(vmo: u64, off: u64, out: &mut [usize]) -> usize {
    let f = VMO_PAGE_HOOK.load(Ordering::Acquire);
    if f == 0 { return 0; }
    let f: VmoPageFn = unsafe { core::mem::transmute(f) };
    f(vmo, off, out)
}

/// Kernel-private `map_flags` bit (Linux never sets bit 30): the VMA is an
/// mmap(2) file mapping, so a page lying wholly past the file's *current*
/// end raises SIGBUS instead of being zero-filled, and the part of the last
/// page past EOF reads as zeros. ELF segments leave it clear: their span
/// past `p_filesz` is BSS and must zero-fill.
pub const MAP_EOF_SIGBUS: u32 = 1 << 30;
/// `map_flags` bit (kernel-private): file mapping from a noexec mount.
pub const MAP_NOEXEC_MOUNT: u32 = 1 << 28;

/// What [`AddressSpace::futex_backing`] found at an address.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FutexBacking {
    /// No mapping covers the address.
    Unmapped,
    /// A private mapping: the futex is keyed by (address space, address).
    Private,
    /// A shared mapping: (kind, object, offset) names the futex in every
    /// address space that maps the same memory.
    Shared(u8, u64, u64),
    /// Shared, keyed physically, but the page is not faulted in yet.
    NotPresent,
}

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
    /// Page-cache key of the file (0: pages of this fault are not shared).
    pub key:     u64,
    /// The access was a write (a shared mapping then installs it writable).
    pub write:   bool,
    /// [`BACKING_SHRINK_SEQ`] when the fault was planned.
    pub seq:     u64,
}

/// An absent page of a MAP_SHARED tmpfs/memfd VMA: the object's frame is
/// looked up with the address space unlocked (the tmpfs locks never nest
/// under `busy`), then [`AddressSpace::install_vmo_fault`] maps it.
pub struct VmoFault {
    pub vmo:     u64,
    /// Page-aligned faulting address.
    pub page_va: usize,
    /// Byte offset in the object backing `page_va`.
    pub off:     u64,
    /// First page of the fault-around window (<= `page_va`) and its length
    /// in pages, clamped to the VMA.
    pub win_va:  usize,
    pub win_n:   usize,
    /// [`BACKING_SHRINK_SEQ`] when the fault was planned.
    pub seq:     u64,
}

/// First half of a fault: either finished under the lock, or a file read to
/// do without it.
pub enum FaultPlan {
    Done(Fault),
    Read(FileFault),
    Vmo(VmoFault),
}

/// VFS reference counting of the tmpfs/memfd object a VMA names, so the object
/// outlives its last descriptor while a lazily-mapped VMA may still fault.
pub type VmoRefFn = fn(vmo: u64);
static VMO_RETAIN_HOOK:  AtomicUsize = AtomicUsize::new(0);
static VMO_RELEASE_HOOK: AtomicUsize = AtomicUsize::new(0);

pub fn set_vmo_ref_hooks(retain: VmoRefFn, release: VmoRefFn) {
    VMO_RETAIN_HOOK.store(retain as usize, Ordering::Release);
    VMO_RELEASE_HOOK.store(release as usize, Ordering::Release);
}

pub fn vmo_retain(vmo: u64) {
    if vmo == 0 { return; }
    let f = VMO_RETAIN_HOOK.load(Ordering::Acquire);
    if f == 0 { return; }
    let f: VmoRefFn = unsafe { core::mem::transmute(f) };
    f(vmo)
}

pub fn vmo_release(vmo: u64) {
    if vmo == 0 { return; }
    let f = VMO_RELEASE_HOOK.load(Ordering::Acquire);
    if f == 0 { return; }
    let f: VmoRefFn = unsafe { core::mem::transmute(f) };
    f(vmo)
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

/// Bottom of the window mmap places unhinted, non-MAP_FIXED mappings in (the
/// old bump cursor's start: 256 MiB above the dynamic loader at 768 MiB).
pub const MMAP_BASE: usize = 0x4000_0000;
/// Exclusive top of that window, well below the main stack at the top of the
/// user half (hints and MAP_FIXED may still go anywhere).
pub const MMAP_TOP: usize = 0x7F00_0000_0000;
/// Exclusive end of the user half (48-bit VA).
const USER_VA_END: usize = 0x0000_8000_0000_0000;

/// What `mremap` learns about its source range ([`AddressSpace::mremap_source`]).
#[derive(Clone, Copy, Debug)]
pub struct MremapSource {
    /// tmpfs/memfd object the range aliases (0: none). Growing such a
    /// mapping needs the object's frames for the new pages, which the caller
    /// acquires from the VFS before it takes the address space.
    pub vmo: u64,
    /// Byte offset of the range's first page in that object.
    pub vmo_off: u64,
    /// A device aperture (never split or grown).
    pub device: bool,
    /// Private anonymous memory (what MREMAP_DONTUNMAP accepts).
    pub private_anon: bool,
    /// The range ends its VMA (growing in place is possible).
    pub ends_vma: bool,
}

/// Room kept free above the brk break when the heap lies in the window.
const BRK_ROOM: usize = 256 << 20;

/// Page-table pages `unmap_range` has returned (diagnostics).
pub static PT_PAGES_PRUNED: AtomicUsize = AtomicUsize::new(0);

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
    /// Bumped on every release of `busy`; a chunked operation compares it
    /// across its own unlock/relock to detect a foreign holder in between.
    pub lock_epoch: core::sync::atomic::AtomicU64,
    /// One past the top of the main thread's stack VMA (0 = unknown), so
    /// `/proc/<pid>/status` can report `VmStk` and keep the stack out of
    /// `VmData`, as Linux's VM_STACK does. Set by whoever maps the stack.
    pub stack_top: usize,
    /// Peak resident pages (`VmHWM`) and peak mapped pages (`VmPeak`), as of
    /// the last time either could have gone down. Linux's `hiwater_rss` /
    /// `hiwater_vm` scheme: resident and mapped totals only ever drop in
    /// `unmap_range` and `brk` shrink, so snapshotting there (plus the current
    /// value at read time) yields the exact peak with nothing on the fault
    /// path. See [`AddressSpace::mem_counters`].
    pub hiwater_rss: usize,
    pub hiwater_vm: usize,
    /// Page faults serviced without / with file I/O (`minflt` / `majflt` of
    /// `/proc/<pid>/stat`, `getrusage`). Counted by `sched::page_fault`
    /// while it holds `busy`; atomics only so readers need not take it.
    pub min_flt: core::sync::atomic::AtomicU64,
    pub maj_flt: core::sync::atomic::AtomicU64,
    /// Index into `regions` of the last slot [`find`](Self::find) hit. Only a
    /// hint: `find` re-checks the slot it names, so a stale value (slot freed,
    /// reused, split, Vec shrunk) costs one fallback scan and is never wrong.
    /// Atomic because `find` takes `&self`; Relaxed is enough because the
    /// hint is never trusted.
    pub find_hint: AtomicUsize,
    /// Thread-group id of the process this space belongs to (0 = not yet
    /// attached to a Task). Set where the space gets its Task (fork, exec,
    /// clone); read only by the teardown ring (`/proc/leandros_asfree`).
    pub owner_tgid: AtomicUsize,
}

/// Entries of the address-space teardown ring.
pub const ASFREE_RING: usize = 256;
struct AsFreeSlot {
    /// Entry sequence number + 1 (0 = never written); stored last.
    seq: core::sync::atomic::AtomicU64,
    tgid: AtomicUsize,
    pages: AtomicUsize,
    pt_pages: AtomicUsize,
}
#[allow(clippy::declare_interior_mutable_const)]
const ASFREE_SLOT0: AsFreeSlot = AsFreeSlot {
    seq: core::sync::atomic::AtomicU64::new(0),
    tgid: AtomicUsize::new(0), pages: AtomicUsize::new(0), pt_pages: AtomicUsize::new(0),
};
static ASFREE_SLOTS: [AsFreeSlot; ASFREE_RING] = [ASFREE_SLOT0; ASFREE_RING];
/// Teardowns recorded so far (the next entry's sequence number).
static ASFREE_SEQ: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// One teardown ring entry: `(seq, owner tgid, pages whose refcount reached
/// zero, page-table pages freed)`.
pub type AsFreeEntry = (u64, usize, usize, usize);

fn asfree_record(tgid: usize, pages: usize, pt_pages: usize) {
    let n = ASFREE_SEQ.fetch_add(1, Ordering::Relaxed);
    let s = &ASFREE_SLOTS[(n as usize) % ASFREE_RING];
    s.seq.store(0, Ordering::Release);
    s.tgid.store(tgid, Ordering::Relaxed);
    s.pages.store(pages, Ordering::Relaxed);
    s.pt_pages.store(pt_pages, Ordering::Relaxed);
    s.seq.store(n + 1, Ordering::Release);
}

/// Snapshot the teardown ring, oldest first. Returns the total number of
/// teardowns recorded since boot (more than [`ASFREE_RING`] means older
/// entries were overwritten).
pub fn asfree_snapshot(out: &mut Vec<AsFreeEntry>) -> u64 {
    let total = ASFREE_SEQ.load(Ordering::Acquire);
    let first = total.saturating_sub(ASFREE_RING as u64);
    for n in first..total {
        let s = &ASFREE_SLOTS[(n as usize) % ASFREE_RING];
        let a = s.seq.load(Ordering::Acquire);
        let (t, p, q) = (s.tgid.load(Ordering::Relaxed), s.pages.load(Ordering::Relaxed), s.pt_pages.load(Ordering::Relaxed));
        if a == n + 1 && s.seq.load(Ordering::Acquire) == a { out.push((n, t, p, q)); }
    }
    total
}

/// Called every [`TEARDOWN_BATCH`] pages of an address-space teardown:
/// `sched` registers `cond_resched` here (open an IRQ window, give up the CPU
/// if the tick asked for it; a no-op wherever sleeping is not allowed).
static TEARDOWN_RESCHED: core::sync::atomic::AtomicPtr<()> =
    core::sync::atomic::AtomicPtr::new(core::ptr::null_mut());

/// Pages freed between two reschedule points of a teardown. Freeing one page
/// costs well under a microsecond, so a batch is a few hundred microseconds
/// at most, while a multi-GiB address space is hundreds of thousands of
/// pages: seconds with IRQs masked when done in one go (`[WDOG]` on the PC).
pub const TEARDOWN_BATCH: usize = 512;

/// Install the teardown reschedule hook (see [`TEARDOWN_RESCHED`]).
pub fn register_teardown_resched(f: fn()) {
    TEARDOWN_RESCHED.store(f as *mut (), core::sync::atomic::Ordering::Release);
}

#[inline]
fn teardown_resched() {
    let p = TEARDOWN_RESCHED.load(core::sync::atomic::Ordering::Acquire);
    if !p.is_null() {
        let f: fn() = unsafe { core::mem::transmute(p) };
        f();
    }
}

/// Number of reschedule points the last teardowns passed (diagnostics: a
/// big teardown must show them, see memtest `teardown_irq_latency`).
pub static TEARDOWN_RESCHED_POINTS: core::sync::atomic::AtomicU64 =
    core::sync::atomic::AtomicU64::new(0);

impl Drop for AddressSpace {
    /// Unmap and free all VMAs, then release the page-table root page.
    ///
    /// Runs where the last reference goes: normally the exiting thread itself
    /// (`sched::release_exiting_address_space`), execve's displaced image, or
    /// the reaper. This is the authoritative cleanup path for per-process
    /// physical memory.
    ///
    /// Nobody else can reach this address space any more (no reference, not
    /// loaded on any CPU), so the work may be interrupted and preempted at
    /// any page boundary: every [`TEARDOWN_BATCH`] pages it passes a
    /// reschedule point. A multi-GiB teardown used to run start to end with
    /// IRQs masked.
    fn drop(&mut self) {
        let mut batch = 0usize;
        let mut freed = 0usize;
        let mut pt_freed = 0usize;
        // Free all VMA backing pages.
        for slot in self.regions.iter_mut() {
            if let Some(region) = slot.take() {
                if region.lazy {
                    for (_, phys) in region.lazy_pages.present() {
                        if crate::pageref::unref_or_free(phys, 0) { freed += 1; }
                        batch += 1;
                        if batch >= TEARDOWN_BATCH {
                            batch = 0;
                            TEARDOWN_RESCHED_POINTS.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                            teardown_resched();
                        }
                    }
                } else if region.file_cap == usize::MAX {
                    device_vma_release(region.phys, region.end - region.start);
                } else if region.phys != 0 {
                    let pages = (region.end - region.start) / PAGE_SIZE;
                    buddy_free(region.phys, pages_to_order(pages));
                    freed += pages;
                }
                if is_file_backed(region.file_cap) {
                    file_release(region.file_cap);
                }
                vmo_release(region.vmo);
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
            // Flush stale TLB entries on any CPU that may still cache this
            // root before its table pages are returned (normally none: a
            // dying address space is loaded nowhere).
            tlb_flush_as(self.page_table_root);
            pt_freed = unsafe { crate::paging::free_user_page_tables(self.page_table_root) };
            buddy_free(self.page_table_root, 0);
        }
        asfree_record(self.owner_tgid.load(Ordering::Relaxed), freed, pt_freed);
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
            lock_epoch: core::sync::atomic::AtomicU64::new(0),
            stack_top: 0,
            hiwater_rss: 0,
            hiwater_vm: 0,
            min_flt: core::sync::atomic::AtomicU64::new(0),
            maj_flt: core::sync::atomic::AtomicU64::new(0),
            find_hint: AtomicUsize::new(0),
            owner_tgid: AtomicUsize::new(0),
        }
    }

    /// Map `size` bytes (rounded up to pages) at virtual address `virt`,
    /// backed by freshly allocated physical pages.
    ///
    /// Returns `true` on success, `false` if OOM or the VMA table is full.
    pub fn map(&mut self, virt: usize, size: usize, flags: PageFlags) -> bool {
        if size == 0 { return note_fail(FAIL_ZERO_SIZE); }
        let pages = (size + PAGE_SIZE - 1) / PAGE_SIZE;
        let phys = match buddy_alloc(pages_to_order(pages)) {
            Some(p) => p,
            None    => return note_fail(FAIL_BUDDY),
        };
        // Zero the backing memory via HHDM virtual address.
        unsafe { (crate::phys_to_virt(phys) as *mut u8).write_bytes(0, pages * PAGE_SIZE); }
        if self.map_prefilled(virt, size, flags, phys) { return true; }
        buddy_free(phys, pages_to_order(pages));
        false
    }

    /// Physically contiguous block for an eager mapping of `size` bytes,
    /// to fill *before* it is published with [`map_prefilled`]. Contents are
    /// not zeroed. Free it with [`free_prefill`] if it never gets mapped.
    pub fn alloc_prefill(size: usize) -> Option<usize> {
        if size == 0 { return None; }
        buddy_alloc(pages_to_order((size + PAGE_SIZE - 1) / PAGE_SIZE))
    }

    /// Return a block from [`alloc_prefill`] that was not mapped.
    pub fn free_prefill(phys: usize, size: usize) {
        buddy_free(phys, pages_to_order((size + PAGE_SIZE - 1) / PAGE_SIZE));
    }

    /// Install an eager anonymous-private VMA over `phys` (a block from
    /// [`alloc_prefill`] for the same `size`, already filled by the caller).
    /// The block's ownership passes to the VMA on success; on failure it
    /// stays with the caller. Filling before publishing matters: once the
    /// VMA is in `regions`, a sibling thread's munmap may free the block,
    /// so it must never be written through the direct map afterwards
    /// without the address-space lock.
    pub fn map_prefilled(&mut self, virt: usize, size: usize, flags: PageFlags, phys: usize) -> bool {
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
        // Map each page.  If any individual mapping fails (OOM in page-table
        // node allocation), unmap the pages already installed and report
        // failure; the caller still owns (and frees) the block.
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
                return note_fail(FAIL_PTE_INSTALL);
            }
        }

        self.regions[slot] = Some(VmaRegion {
            start: virt,
            end:   virt + pages * PAGE_SIZE,
            phys,
            flags,
            lazy: false,
            lazy_pages: crate::pagevec::PageVec::new(),
            lazy_count: 0,
            prot:      PROT_READ | PROT_WRITE,
            map_flags: MAP_ANONYMOUS | MAP_PRIVATE,
            file_cap:  0,
            file_off:  0,
            file_len:  0,
            cow:       false,
            written:   Vec::new(),
            shm:       None,
            shm_pgoff: 0,
            vmo: 0,
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
            lazy_pages: crate::pagevec::PageVec::new(),
            lazy_count: 0,
            prot:      PROT_READ | PROT_WRITE,
            map_flags: MAP_SHARED, // Devices are shared
            file_cap:  usize::MAX, // Special marker for device mappings (do not free)
            file_off:  0,
            file_len:  0,
            cow:       false,
            written:   Vec::new(),
            shm:       None,
            shm_pgoff: 0,
            vmo: 0,
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
            lazy_pages: crate::pagevec::PageVec::new(),
            lazy_count: 0,
            prot:      PROT_READ | PROT_WRITE,
            map_flags: MAP_ANONYMOUS | if is_shared { MAP_SHARED } else { MAP_PRIVATE },
            file_cap:  0,
            file_off:  0,
            file_len:  0,
            cow:       false,
            written:   Vec::new(),
            // Shared: one object for every mapping this VMA will ever be
            // forked or split into (see `crate::shmem`).
            shm:       if is_shared { Some(crate::shmem::ShmObj::new()) } else { None },
            shm_pgoff: 0,
            vmo: 0,
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
        self.map_vmo_frames(virt, frames, flags, false)
    }

    /// `MAP_PRIVATE` twin of [`map_shared_frames`](Self::map_shared_frames): the
    /// VMA aliases the object's frames (each pinned +1 by the caller) but is a
    /// copy-on-write region. PTEs are installed read-only whatever `flags` say;
    /// the first store to a page finds it with another owner (the object) and
    /// takes the ordinary CoW promotion, copying it privately, so the file
    /// never sees the write. `mprotect(PROT_WRITE)` and fork follow the same
    /// rule as for any private region.
    pub fn map_private_vmo_frames(&mut self, virt: usize, frames: &[usize], flags: PageFlags) -> bool {
        self.map_vmo_frames(virt, frames, flags, true)
    }

    fn map_vmo_frames(&mut self, virt: usize, frames: &[usize], flags: PageFlags, private: bool) -> bool {
        let pte_flags = if private { flags & !PageFlags::WRITABLE } else { flags };
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
                map_page(self.page_table_root, virt + i * PAGE_SIZE, frames[i], pte_flags)
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
            lazy_pages: crate::pagevec::PageVec::from(frames.to_vec()),
            lazy_count: pages,
            prot:      {
                let mut p = PROT_READ;
                if flags.contains(PageFlags::WRITABLE) { p |= PROT_WRITE; }
                if flags.contains(PageFlags::EXECUTE)  { p |= PROT_EXEC; }
                p
            },
            map_flags: if private { MAP_PRIVATE } else { MAP_SHARED },
            file_cap:  0,
            file_off:  0,
            file_len:  0,
            cow:       private,
            written:   Vec::new(),
            shm:       None,
            shm_pgoff: 0,
            vmo: 0,
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

    /// `mmap(2)` of a regular file, `MAP_SHARED`, from a descriptor open for
    /// writing: every page is the file's page-cache frame itself
    /// ([`crate::pagecache`]), so stores are visible to read(2) and to every
    /// other mapping at once, and reach the file through write-back. A page
    /// is mapped read-only until its first write, which marks it dirty in
    /// the cache. Same end-of-file rules as [`map_private_file`]; fork shares
    /// the frames (no copy-on-write).
    ///
    /// [`map_private_file`]: Self::map_private_file
    pub fn map_shared_file(
        &mut self,
        virt: usize,
        size: usize,
        flags: PageFlags,
        file_cap: usize,
        file_off: u64,
    ) -> bool {
        let span = ((size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)) as u64;
        self.map_lazy_file_with(virt, size, flags, file_cap, file_off, span,
                                MAP_SHARED | MAP_EOF_SIGBUS)
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
            lazy_pages: crate::pagevec::PageVec::new(),
            lazy_count: 0,
            prot,
            map_flags,
            file_cap,
            file_off,
            file_len,
            cow:       false,
            written:   Vec::new(),
            shm:       None,
            shm_pgoff: 0,
            vmo: 0,
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
            // The tmpfs lookup must not run under `busy`; kernel accesses
            // reach these pages through the unlocked path (prefault_range
            // stops at them), so only a stray locked caller lands here.
            FaultPlan::Vmo(_) => Fault::Bus,
        }
    }

    /// Second half of a tmpfs/memfd fault: map `frames` (one reference each,
    /// from [`vmo_pages`]; the pages of the fault-around window starting at
    /// `p.win_va`, possibly fewer than `p.win_n`) for `p`. Re-validated as
    /// [`install_file_fault`](Self::install_file_fault) is; on any mismatch
    /// the references are dropped and the access retries. Pages already
    /// mapped are left alone; the faulting page missing is SIGBUS.
    pub fn install_vmo_fault(&mut self, p: &VmoFault, frames: &[usize]) -> Fault {
        let release_all = |from: usize| {
            for &f in &frames[from.min(frames.len())..] { crate::pageref::unref_or_free(f, 0); }
        };
        if backing_seq() != p.seq { release_all(0); return Fault::Handled; }
        let page_table_root = self.page_table_root;
        let region = match self.regions.iter_mut().filter_map(|r| r.as_mut()).find(
            |r| p.page_va >= r.start && p.page_va < r.end
        ) {
            Some(r) => r,
            None => { release_all(0); return Fault::Handled; }
        };
        let page_idx = (p.page_va - region.start) / PAGE_SIZE;
        if !region.lazy || region.vmo != p.vmo
            || region.file_off + (page_idx * PAGE_SIZE) as u64 != p.off
            || region.lazy_pages.get(page_idx).copied().unwrap_or(0) != 0
            || p.win_va < region.start
            || p.win_va + p.win_n * PAGE_SIZE > region.end
        {
            release_all(0);
            return Fault::Handled;
        }
        let first = (p.page_va - p.win_va) / PAGE_SIZE;
        if frames.len() <= first { release_all(0); return Fault::Bus; }
        // A private (CoW) mapping shares the object's frame: read-only, the
        // first store copies it.
        let install = if region.cow { region.flags & !PageFlags::WRITABLE } else { region.flags };
        let base_idx = (p.win_va - region.start) / PAGE_SIZE;
        if region.lazy_pages.len() < base_idx + frames.len() {
            region.lazy_pages.resize(base_idx + frames.len(), 0);
        }
        for (i, &phys) in frames.iter().enumerate() {
            let idx = base_idx + i;
            if region.lazy_pages[idx] != 0 { crate::pageref::unref_or_free(phys, 0); continue; }
            let va = p.win_va + i * PAGE_SIZE;
            if !unsafe { map_page(page_table_root, va, phys, install) } {
                crate::pageref::unref_or_free(phys, 0);
                if i == first { release_all(i + 1); return Fault::Segv; }
                release_all(i + 1);
                break;
            }
            region.lazy_pages[idx] = phys;
            region.lazy_count += 1;
        }
        Fault::Handled
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
        // PROT_NONE: no access at all — pthread stack guards (musl maps the
        // whole thread stack PROT_NONE and mprotects all but the guard), and
        // reservations. Such a VMA never has PTEs installed (see `mprotect`,
        // `cow::clone_as`), so every touch lands here. Until 2026-09-26 the
        // fault path ignored `prot`: a read of a guard page got a fresh zero
        // page and a write livelocked on the read-only PTE it installed.
        if region.prot & PROT_ACCESS == 0 {
            return FaultPlan::Done(Fault::Segv);
        }

        // Compute the page index within this VMA.
        let page_idx = (page_va - region.start) / PAGE_SIZE;

        let lazy_phys = region.lazy_pages.get(page_idx).copied().unwrap_or(0);
        if lazy_phys != 0 {
            // First write to a page of a shared file mapping: the frame is
            // the file's cached page, so the write goes to it in place — mark
            // it dirty for write-back and make this PTE writable. A frame the
            // cache no longer holds (truncated away) is detached: still this
            // mapping's to write, it just no longer reaches the file.
            if is_write && region.map_flags & MAP_SHARED != 0 && is_file_backed(region.file_cap)
                && region.flags.contains(PageFlags::WRITABLE) && region.clean_file_page(page_idx)
            {
                if let Some(pg) = region.cache_pgoff(page_idx) {
                    crate::pagecache::mark_dirty(file_key(region.file_cap), pg, lazy_phys);
                }
                if !unsafe { map_page(page_table_root, page_va, lazy_phys, region.flags) } {
                    return FaultPlan::Done(Fault::Segv);
                }
                tlb_flush_local_page(page_va);
                region.set_written(page_idx, true);
                return FaultPlan::Done(Fault::Handled);
            }
            // A first write to a clean private file page in a writable VMA:
            // from now on it is this process's own (anonymous) copy. The
            // frame may be shared with the page cache (and through it with
            // every other process mapping that file page), so it takes the
            // copy-on-write promotion below: copied while it has another
            // owner, upgraded in place once it is this mapping's alone.
            let clean_write = is_write && region.flags.contains(PageFlags::WRITABLE)
                && region.clean_file_page(page_idx);
            // Page already present. A write to a CoW-shared page needs a
            // promotion (below). Any other fault on a present page is most
            // likely a *concurrent* fault: a sibling thread touched the same
            // fresh page, lost the race on the per-AS fault lock, and by the
            // time it got here the winner had already mapped the page. If
            // the region's protections allow the access, resume — the retry
            // will succeed. Only an access the region forbids is a real
            // protection violation.
            if !(is_write && (region.cow || clean_write)) {
                return FaultPlan::Done(if !is_write || (region.prot & PROT_WRITE) != 0 { Fault::Handled } else { Fault::Segv });
            }

            // Serialize the get→copy→dec promotion against clone_as and
            // against promotions in the sibling address space — see
            // cow::COW_LOCK's doc comment.
            let t_promo = crate::paging::tlbstat::now_ns();
            let _cow_guard = crate::cow::COW_LOCK.lock();
            let refcount = crate::pageref::get(lazy_phys);
            if refcount <= 1 {
                // Sole remaining owner: same frame, read-only → writable. A
                // permission upgrade needs no break-before-make and no remote
                // flush (a stale read-only entry elsewhere only re-faults and
                // lands here); drop this CPU's copy of the entry that faulted.
                if !unsafe { map_page(page_table_root, page_va, lazy_phys, region.flags) } {
                    return FaultPlan::Done(Fault::Segv);
                }
                tlb_flush_local_page(page_va);
                region.set_written(page_idx, true);
                crate::paging::tlbstat::COW_REUSE.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                return FaultPlan::Done(Fault::Handled);
            }
            let new_phys = match buddy_alloc(0) {
                Some(p) => p,
                None    => return FaultPlan::Done(Fault::Segv), // OOM
            };
            unsafe {
                core::ptr::copy_nonoverlapping(
                    crate::phys_to_virt(lazy_phys) as *const u8,
                    crate::phys_to_virt(new_phys)  as *mut u8,
                    PAGE_SIZE,
                );
            }
            // Break-before-make with ONE flush: the output address changes,
            // which AArch64 only permits through an invalid entry plus TLB
            // invalidation, and which on x86 must not let one thread's CPU
            // keep reading the old frame while another already writes the new
            // one. Clear, flush this page on every CPU that may cache this
            // address space (x86: only CPUs with this root loaded — none but
            // this one for a single-threaded process; AArch64: one broadcast
            // `tlbi vaae1is`), then install. invalid → valid needs no second
            // flush on either architecture (neither caches a not-present or
            // translation-faulting entry). A sibling thread touching the page
            // meanwhile faults and waits for this address space's `busy`.
            // Until 2026-09-25 this did an all-CPU shootdown both before and
            // after the install.
            unsafe { unmap_page(page_table_root, page_va); }
            tlb_flush_range(page_table_root, page_va, 1);
            if !unsafe { map_page(page_table_root, page_va, new_phys, region.flags) } {
                buddy_free(new_phys, 0);
                return FaultPlan::Done(Fault::Segv);
            }
            region.lazy_pages[page_idx] = new_phys;
            region.set_written(page_idx, true);
            // Drop our reference to the shared frame only now that no CPU can
            // still reach it through this address space: the other owner may
            // reuse it in place (and write it) as soon as it is sole owner.
            crate::pageref::dec(lazy_phys);
            {
                use crate::paging::tlbstat as ts;
                ts::COW_COPY.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                ts::add(&ts::COW_COPY_NS, &ts::COW_COPY_MAX_NS, ts::now_ns().saturating_sub(t_promo));
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
            let key = if region.cache_pgoff(page_idx).is_some() { file_key(region.file_cap) } else { 0 };
            // Page cache hit: map the shared frame (and the cached pages
            // that follow it in the fault-around window) without any read.
            if key != 0 {
                let exec = region.flags.contains(PageFlags::EXECUTE);
                let pg0 = region.cache_pgoff(page_idx).unwrap();
                if let Some(phys) = crate::pagecache::lookup(key, pg0, exec) {
                    // A shared mapping's write fault installs the page
                    // writable at once (and dirty); everything else starts
                    // read-only (clean).
                    let shared_w = is_write && region.map_flags & MAP_SHARED != 0
                        && region.flags.contains(PageFlags::WRITABLE);
                    if shared_w { crate::pagecache::mark_dirty(key, pg0, phys); }
                    if !region.install_cached(page_table_root, page_idx, phys, shared_w) {
                        crate::pageref::unref_or_free(phys, 0);
                        return FaultPlan::Done(Fault::Segv);
                    }
                    let mut i = 1usize;
                    while i < FAULT_AROUND_PAGES && page_idx + i < region_pages
                        && region.lazy_pages.get(page_idx + i).copied().unwrap_or(0) == 0
                    {
                        let pg = match region.cache_pgoff(page_idx + i) { Some(g) => g, None => break };
                        let phys = match crate::pagecache::lookup(key, pg, exec) { Some(p) => p, None => break };
                        if !region.install_cached(page_table_root, page_idx + i, phys, false) {
                            crate::pageref::unref_or_free(phys, 0);
                            break;
                        }
                        i += 1;
                    }
                    #[cfg(target_arch = "aarch64")]
                    if region.flags.contains(PageFlags::EXECUTE) {
                        unsafe { core::arch::asm!("ic ialluis", "dsb ish", "isb"); }
                    }
                    return FaultPlan::Done(Fault::Handled);
                }
            }
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
                key,
                write: is_write,
                seq: backing_seq(),
            });
        }

        // A tmpfs/memfd page that is absent was taken away by a truncate or
        // a hole punch of the object (its frames are installed at map time).
        // Map the object's page again if it has one now (the file grew back,
        // or the hole is read as zeros, as on Linux); past its end it is
        // SIGBUS. Zero-filling privately would detach this mapping from the
        // object without a word.
        if region.vmo != 0 {
            let win = VMO_FAULT_AROUND * PAGE_SIZE;
            let win_va = (page_va & !(win - 1)).max(region.start);
            let win_end = (win_va - (win_va & (win - 1)) + win).min(region.end);
            return FaultPlan::Vmo(VmoFault {
                vmo: region.vmo,
                page_va,
                off: region.file_off + (page_idx * PAGE_SIZE) as u64,
                win_va,
                win_n: (win_end - win_va) / PAGE_SIZE,
                seq: backing_seq(),
            });
        }

        // Anonymous: one zeroed page. A write to a region userspace may not
        // write is refused up front rather than after installing a page.
        if is_write && region.prot & PROT_WRITE == 0 && !region.flags.contains(PageFlags::WRITABLE) {
            return FaultPlan::Done(Fault::Segv);
        }
        // Shared anonymous memory: the page is the object's, whichever
        // mapping (in whichever process) touched it first.
        if let Some(obj) = region.shm.as_ref() {
            let phys = match obj.get_or_alloc(region.shm_pgoff + page_idx) {
                Some(p) => p,
                None    => return FaultPlan::Done(Fault::Segv), // OOM
            };
            if !unsafe { map_page(page_table_root, page_va, phys, region.flags) } {
                crate::pageref::unref_or_free(phys, 0);
                return FaultPlan::Done(Fault::Segv);
            }
            if region.lazy_pages.len() <= page_idx {
                region.lazy_pages.resize(page_idx + 1, 0);
            }
            region.lazy_pages[page_idx] = phys;
            region.lazy_count += 1;
            return FaultPlan::Done(Fault::Handled);
        }
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
        // A truncate started after the read was planned: the bytes may be
        // from before it, and its zap may have passed us already. Retry.
        if backing_seq() != p.seq { return Fault::Handled; }

        // How many bytes are valid, and how many pages to populate.
        let (valid, pages) = if p.eof_sigbus {
            if got < 0 { return Fault::Bus; } // I/O error: SIGBUS, as on Linux
            let valid = (got as usize).min(p.len).min(data.len());
            if valid == 0 { return Fault::Bus; } // page wholly past EOF
            // Pages wholly past the end stay absent, so touching them later
            // raises SIGBUS (or reads data the file has grown into).
            (valid, ((valid + PAGE_SIZE - 1) / PAGE_SIZE).min(p.window))
        } else {
            // ELF segment: the file extent is exact, so a read error or a
            // short read (file shrunk under the mapping) is SIGBUS
            // (BUS_ADRERR), as on Linux. Until 2026-10-25 it was SIGSEGV.
            if p.len != 0 && got != p.len as isize { return Fault::Bus; }
            (p.len.min(data.len()), p.window)
        };

        let region_pages = (region.end - region.start) / PAGE_SIZE;
        if region.lazy_pages.len() < (page_idx + pages).min(region_pages) {
            region.lazy_pages.resize((page_idx + pages).min(region_pages), 0);
        }

        let shared = region.map_flags & MAP_SHARED != 0;
        let exec = region.flags.contains(PageFlags::EXECUTE);
        for i in 0..pages {
            let idx = page_idx + i;
            if idx >= region_pages { break; }
            if region.lazy_pages[idx] != 0 { continue; }
            // A page that is file data from end to end is the cached frame.
            // So is an mmap(2) mapping's last page cut short by EOF: the
            // cache keeps the bytes past EOF zero. The tail of an ELF
            // segment's last page is BSS in this mapping (and the next
            // section's bytes in another), so that page stays private.
            let copy_start = i * PAGE_SIZE;
            let cacheable = copy_start + PAGE_SIZE <= valid || (p.eof_sigbus && copy_start < valid);
            let pgoff = if p.key != 0 && cacheable { region.cache_pgoff(idx) } else { None };
            if let Some(pg) = pgoff {
                if let Some(phys) = crate::pagecache::lookup(p.key, pg, exec) {
                    let shared_w = shared && i == 0 && p.write && region.flags.contains(PageFlags::WRITABLE);
                    if shared_w { crate::pagecache::mark_dirty(p.key, pg, phys); }
                    if !region.install_cached(page_table_root, idx, phys, shared_w) {
                        crate::pageref::unref_or_free(phys, 0);
                        return if i > 0 { Fault::Handled } else { Fault::Segv };
                    }
                    continue;
                }
            }
            // A shared mapping maps nothing but the cached frame: a page the
            // read did not leave in the cache (evicted meanwhile, or changed
            // under it) is read again by the retried access.
            if shared && p.key != 0 { if i == 0 { return Fault::Handled; } continue; }
            let phys = match buddy_alloc(0) {
                Some(p) => p,
                // OOM on a fault-around page is not a failure as long as the
                // faulting page itself (i == 0) was populated.
                None => return if i > 0 { Fault::Handled } else { Fault::Segv },
            };
            let dst = crate::phys_to_virt(phys) as *mut u8;
            unsafe { dst.write_bytes(0, PAGE_SIZE); }
            if copy_start < valid {
                let n = (valid - copy_start).min(PAGE_SIZE);
                unsafe {
                    core::ptr::copy_nonoverlapping(data.as_ptr().add(copy_start), dst, n);
                }
            }
            // AArch64: clean the D-cache for executable pages so the I-cache
            // invalidate below refetches the freshly written bytes.
            // A shared frame may later be mapped executable by another
            // process, so every page entering the cache is cleaned too.
            #[cfg(target_arch = "aarch64")]
            if region.flags.contains(PageFlags::EXECUTE) || pgoff.is_some() {
                unsafe {
                    let mut line = dst as usize & !63;
                    let end_a = dst as usize + PAGE_SIZE;
                    while line < end_a {
                        core::arch::asm!("dc cvac, {}", in(reg) line);
                        line += 64;
                    }
                    core::arch::asm!("dsb ish");
                }
            }
            region.set_written(idx, false);
            let install = region.install_flags(idx, region.flags);
            let mapped = unsafe {
                map_page(page_table_root, region.start + idx * PAGE_SIZE, phys, install)
            };
            if !mapped {
                crate::pageref::unref_or_free(phys, 0);
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

    /// Is `va` a resident, still-unwritten page of a writable private file
    /// VMA (mapped read-only until its first write)?
    fn is_clean_writable_file_page(&self, va: usize) -> bool {
        self.regions.iter().filter_map(|r| r.as_ref())
            .find(|r| va >= r.start && va < r.end)
            .map_or(false, |r| {
                let idx = (va - r.start) / PAGE_SIZE;
                r.flags.contains(PageFlags::WRITABLE)
                    && r.lazy_pages.get(idx).copied().unwrap_or(0) != 0
                    && r.clean_file_page(idx)
            })
    }

    /// Is `va` a resident page of a file VMA userspace may not write, whose
    /// frame has another owner (the page cache, or a fork sibling)?
    fn is_shared_readonly_file_page(&self, va: usize) -> bool {
        self.regions.iter().filter_map(|r| r.as_ref())
            .find(|r| va >= r.start && va < r.end)
            .map_or(false, |r| {
                let phys = r.lazy_pages.get((va - r.start) / PAGE_SIZE).copied().unwrap_or(0);
                r.lazy && is_file_backed(r.file_cap) && !r.flags.contains(PageFlags::WRITABLE)
                    && phys != 0 && crate::pageref::get(phys) > 1
            })
    }

    /// Does userspace have write access to all of `[addr, addr+len)`? The
    /// kernel checks this before it stores into a user buffer through a
    /// plain pointer (read(2) into the buffer): such a store into a
    /// read-only page takes a kernel-mode fault under filesystem locks, which
    /// cannot be answered with EFAULT there.
    pub fn range_writable(&self, addr: usize, len: usize) -> bool {
        if len == 0 { return true; }
        let end = match addr.checked_add(len) { Some(e) => e, None => return false };
        let mut va = addr;
        while va < end {
            let r = match self.regions.iter().filter_map(|r| r.as_ref())
                .find(|r| va >= r.start && va < r.end)
            {
                Some(r) => r,
                None => return false,
            };
            if r.prot & PROT_WRITE == 0 && !r.flags.contains(PageFlags::WRITABLE) { return false; }
            va = r.end;
        }
        true
    }

    /// Is `va` an absent page of a file-backed VMA — one whose fault must read
    /// the file, which is only done with the address space unlocked?
    fn is_absent_file_page(&self, va: usize) -> bool {
        self.regions.iter().filter_map(|r| r.as_ref())
            .find(|r| va >= r.start && va < r.end)
            .map_or(false, |r| {
                r.lazy && (is_file_backed(r.file_cap) || r.vmo != 0)
                    && r.lazy_pages.get((va - r.start) / PAGE_SIZE).copied().unwrap_or(0) == 0
            })
    }

    /// Demand-page all unmapped pages in `[addr, addr+len)` so the kernel can
    /// safely write to user buffers without taking a kernel-mode page fault.
    ///
    /// Never reads a file: it stops at the first absent page of a file-backed
    /// VMA and returns its address. The caller (it holds this address space)
    /// must drop the lock, fault that page in through the unlocked fault path
    /// (`sched::prefault_user_page`) and call again from the next page —
    /// `sched::prefault_current_range` is that loop. `None`: the whole range
    /// was handled.
    #[must_use]
    pub fn prefault_range(&mut self, addr: usize, len: usize) -> Option<usize> {
        if len == 0 { return None; }
        let page_start = addr & !(PAGE_SIZE - 1);
        let page_end   = (addr + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let mut va = page_start;
        while va < page_end {
            if self.virt_to_phys(va).is_none() {
                if self.is_absent_file_page(va) { return Some(va); }
                self.handle_user_page_fault(va, false);
            } else {
                // A kernel store into a writable page still shared
                // copy-on-write would fault (the PTE is read-only for the
                // kernel too) — possibly under a lock the fault path cannot
                // take. Prefaulting promises no fault afterwards, so give
                // this side its private copy now.
                let _ = self.unshare_cow_page(va, true);
                // Same promise for a clean private file page, whose PTE is
                // read-only until its first write (see `VmaRegion::written`).
                if self.is_clean_writable_file_page(va) {
                    self.handle_user_page_fault(va, true);
                }
            }
            va += PAGE_SIZE;
        }
        None
    }

    /// [`prefault_range`](Self::prefault_range) for a buffer the kernel will
    /// only READ: absent pages are faulted in, but a present page still shared
    /// copy-on-write is left shared — a read through the read-only PTE (or
    /// the HHDM) never faults. Copying it would be pure waste: execve reads
    /// argv/envp from a freshly forked child's CoW stack just before throwing
    /// the whole address space away.
    ///
    /// Stops at an absent file-backed page like `prefault_range`.
    #[must_use]
    pub fn prefault_range_ro(&mut self, addr: usize, len: usize) -> Option<usize> {
        if len == 0 { return None; }
        let page_start = addr & !(PAGE_SIZE - 1);
        let page_end   = (addr + len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let mut va = page_start;
        while va < page_end {
            if self.virt_to_phys(va).is_none() {
                if self.is_absent_file_page(va) { return Some(va); }
                self.handle_user_page_fault(va, false);
            }
            va += PAGE_SIZE;
        }
        None
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
             file_cap, right_off, right_len, cow, tail_written,
             tail_shm, tail_shm_pgoff, tail_vmo) = {
            let r = self.regions[idx].as_mut().unwrap();
            r.make_lazy();

            let split_idx   = (boundary - r.start) / PAGE_SIZE;
            let split_bytes = (split_idx * PAGE_SIZE) as u64;
            let orig_len    = r.file_len;

            let tail = if split_idx < r.lazy_pages.len() {
                r.lazy_pages.split_off(split_idx)
            } else {
                crate::pagevec::PageVec::new()
            };
            // Same cut through the written bitmap: bit `split_idx + k` of
            // the left half becomes bit `k` of the right one.
            let mut tail_w: Vec<u64> = Vec::new();
            for i in split_idx..r.written.len() * 64 {
                if r.is_written(i) {
                    let k = i - split_idx;
                    if tail_w.len() <= k / 64 { tail_w.resize(k / 64 + 1, 0); }
                    tail_w[k / 64] |= 1u64 << (k % 64);
                }
            }
            if split_idx / 64 < r.written.len() {
                r.written.truncate(split_idx / 64 + 1);
                if split_idx % 64 == 0 { r.written.truncate(split_idx / 64); }
                else { r.written[split_idx / 64] &= (1u64 << (split_idx % 64)) - 1; }
            }
            let r_end = r.end;

            // Left half: ends at boundary, keeps its file offset but its file
            // extent is clipped to what precedes the boundary.
            r.end = boundary;
            r.lazy_count = r.lazy_pages.count_present();
            let right_off = r.file_off + split_bytes;
            let right_len = orig_len.saturating_sub(split_bytes);
            if is_file_backed(r.file_cap) && r.file_len > split_bytes {
                r.file_len = split_bytes;
            }

            (r_end, tail, r.flags, r.prot, r.map_flags,
             r.file_cap, right_off, right_len, r.cow, tail_w,
             r.shm.clone(), r.shm_pgoff + split_idx, r.vmo)
        };

        // Each surviving VMA holds its own reference on the backing file.
        if is_file_backed(file_cap) { file_retain(file_cap); }
        vmo_retain(tail_vmo);

        let right_count = tail_pages.count_present();
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
            written: tail_written,
            shm: tail_shm,
            shm_pgoff: tail_shm_pgoff,
            vmo: tail_vmo,
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

        // Resident and mapped totals are about to drop: record the peaks.
        self.update_hiwater();

        // After splitting at both boundaries, any VMA overlapping [virt,end) is
        // fully contained within it.
        self.split_at(virt);
        self.split_at(end);

        let pt = self.page_table_root;
        let mut did_unmap = false;
        // Frames are released only after the TLB flush below: until every
        // CPU has dropped its translation, a sibling thread can still write
        // through it, and a frame already handed to another process would
        // take that write.
        let mut released: Vec<usize> = Vec::new();
        let mut released_blocks: Vec<(usize, usize)> = Vec::new();
        let mut released_dev: Vec<(usize, usize)> = Vec::new();

        for slot in self.regions.iter_mut() {
            let region = match slot {
                Some(r) if r.start >= virt && r.end <= end && r.start < r.end => r,
                _ => continue,
            };

            let r_start = region.start;
            let r_end   = region.end;
            let n_pages = (r_end - r_start) / PAGE_SIZE;

            if region.lazy {
                for (i, phys) in region.lazy_pages.present() {
                    {
                        unsafe { unmap_page(pt, r_start + i * PAGE_SIZE); }
                        released.push(phys);
                        did_unmap = true;
                    }
                }
            } else if region.file_cap == usize::MAX {
                // Device mapping: drop the PTEs but never free the phys range.
                // Its reference on the range goes after the TLB flush below.
                for i in 0..n_pages { unsafe { unmap_page(pt, r_start + i * PAGE_SIZE); } }
                released_dev.push((region.phys, r_end - r_start));
                did_unmap = true;
            } else {
                // Eager, contiguous buddy-backed block: unmap and free whole.
                for i in 0..n_pages { unsafe { unmap_page(pt, r_start + i * PAGE_SIZE); } }
                if region.phys != 0 {
                    released_blocks.push((region.phys, pages_to_order(n_pages)));
                }
                did_unmap = true;
            }

            if is_file_backed(region.file_cap) {
                file_release(region.file_cap);
            }
            vmo_release(region.vmo);
            *slot = None;
        }

        // Only a PTE that was present can be cached in a TLB (an absent
        // lazy page never had one; every path that clears a lazy page's PTE
        // shoots down itself). So a VMA that was never touched — ld.so's
        // whole-library reservation it then MAP_FIXED-overlays with the
        // segments, now that file mappings are demand-paged — goes without
        // the all-CPU shootdown, which cost 0.5-1.2 s per overlay on
        // x86_64/TCG.
        //
        // Page-table nodes the range leaves empty go too. Without this a
        // process that maps and unmaps in a moving window (a video decoder's
        // frame buffers) kept every PT/PD it ever touched until exit. They
        // are unlinked now and freed only after the flush below, which covers
        // every VA they translated within the range: until then a walk cache
        // may still point into them (x86 `invlpg`/CR3 reload and AArch64
        // `tlbi vaae1is`/`vmalle1is` drop the non-leaf entries too).
        let mut tables: Vec<usize> = Vec::new();
        unsafe { crate::paging::prune_user_page_tables(pt, virt, end, &mut tables); }
        let mut tickets = Vec::new();
        if did_unmap || !tables.is_empty() { tickets.push(tlb_flush_range_start(pt, virt, len / PAGE_SIZE)); }
        free_after_flush(Deferred { tickets, frames: released, blocks: released_blocks, dev: released_dev, tables });
    }

    /// True if `[start, start+len)` overlaps no VMA and no arch-reserved range.
    pub fn range_is_free(&self, start: usize, len: usize) -> bool {
        let end = match start.checked_add(len) { Some(e) => e, None => return false };
        if self.regions.iter().filter_map(|r| r.as_ref()).any(|r| start < r.end && end > r.start) {
            return false;
        }
        let mut i = 0;
        while let Some((s, e)) = crate::paging::user_reserved_range(i) {
            if start < e && end > s { return false; }
            i += 1;
        }
        true
    }

    /// Lowest address in `[MMAP_BASE, MMAP_TOP)` where `len` bytes (rounded
    /// to pages) fit without touching a VMA, an arch-reserved range or the
    /// brk heap's growth room: the placement for every mmap that has neither
    /// MAP_FIXED nor a usable hint.
    ///
    /// First fit over this address space's own VMAs, so a freed range is
    /// reused. Until 2026-10-14 placement was a system-wide bump cursor that
    /// never went back: a long-lived process that maps and unmaps (Firefox
    /// decoding video) walked an ever-moving window up its address space,
    /// paying fresh page tables for every step (~10 KiB of MemFree per
    /// map/unmap cycle, returned only at exit) on the way to exhausting it.
    pub fn find_free_range(&self, len: usize) -> Option<usize> {
        let len = len.checked_add(PAGE_SIZE - 1)? & !(PAGE_SIZE - 1);
        if len == 0 || len > MMAP_TOP - MMAP_BASE { return None; }
        let mut spans: Vec<(usize, usize)> = Vec::with_capacity(self.regions.len() + 4);
        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if r.end > MMAP_BASE && r.start < MMAP_TOP { spans.push((r.start, r.end)); }
        }
        let mut i = 0;
        while let Some(span) = crate::paging::user_reserved_range(i) {
            spans.push(span);
            i += 1;
        }
        // brk grows up from heap_start; keep room above the break so a heap
        // that happens to sit inside the window is not walled in by mmap.
        if self.heap_start != 0 {
            let brk = self.heap_end.max(self.heap_start);
            spans.push((self.heap_start, brk.saturating_add(BRK_ROOM)));
        }
        spans.sort_unstable_by_key(|&(s, _)| s);
        let mut cur = MMAP_BASE;
        for (s, e) in spans {
            if s >= cur && s - cur >= len { return Some(cur); }
            if e > cur { cur = e; }
            if cur >= MMAP_TOP { return None; }
        }
        if MMAP_TOP - cur >= len { Some(cur) } else { None }
    }

    /// `madvise(MADV_DONTNEED)`: drop the resident pages of `[virt, virt+len)`
    /// so the next touch faults afresh. For a private mapping that is the
    /// whole point of the call: anonymous pages read back as zeros and private
    /// file pages as the file's bytes again, discarding private writes. Shared
    /// mappings keep their data in the shared object: shared anonymous memory
    /// is zapped (PTEs and this mapping's references dropped) and refaults
    /// the same frames from its [`crate::shmem`] object; tmpfs/memfd and
    /// page-cache mappings, which cannot refault, and device mappings are
    /// left alone. Returns false if part of the range is not mapped (ENOMEM).
    ///
    /// Allocators depend on the zeros. Scudo's secondary cache (Firefox's
    /// allocator on Alpine) releases idle blocks with MADV_DONTNEED, marks
    /// them `Time = 0`, and later hands one out for calloc() *without* a
    /// memset because "released means zeroed". This used to be a no-op, so
    /// calloc returned stale data: hash tables with live-looking slots holding
    /// a NULL key, double frees ("Scudo ERROR: invalid chunk state").
    ///
    /// Every PTE is cleared and the range flushed on every CPU before any
    /// frame is released, so no CPU can reach a frame after it is reused.
    pub fn discard_range(&mut self, virt: usize, len: usize) -> bool {
        let virt = virt & !(PAGE_SIZE - 1);
        let len  = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let end  = match virt.checked_add(len) { Some(e) => e, None => return false };
        if len == 0 { return true; }
        self.update_hiwater();

        let pt = self.page_table_root;
        let mut covered = 0usize;
        let mut released: Vec<usize> = Vec::new();
        for region in self.regions.iter_mut().filter_map(|r| r.as_mut()) {
            if region.end <= virt || region.start >= end { continue; }
            let lo = region.start.max(virt);
            let hi = region.end.min(end);
            covered += hi - lo;
            let shared = region.map_flags & MAP_SHARED != 0;
            // Shared anonymous memory is zapped like Linux does it: the PTEs
            // and this mapping's references go, the data stays in the object
            // and the next touch maps the very same frame again. Other shared
            // mappings (tmpfs/memfd frames, page-cache frames) have no way to
            // refault the frame they map, so they keep it.
            if (shared && region.shm.is_none()) || region.file_cap == usize::MAX { continue; }
            if region.lazy {
                let first = (lo - region.start) / PAGE_SIZE;
                let last  = ((hi - region.start) / PAGE_SIZE).min(region.lazy_pages.len());
                for i in first..last {
                    let phys = region.lazy_pages[i];
                    if phys == 0 { continue; }
                    unsafe { unmap_page(pt, region.start + i * PAGE_SIZE); }
                    region.lazy_pages[i] = 0;
                    region.set_written(i, false);
                    region.lazy_count = region.lazy_count.saturating_sub(1);
                    released.push(phys);
                }
            } else if region.phys != 0 && !is_file_backed(region.file_cap) {
                // An eager anonymous block is never shared (fork converts it
                // to a lazy CoW VMA first): zero it in place.
                let off = lo - region.start;
                unsafe {
                    core::ptr::write_bytes(crate::phys_to_virt(region.phys + off) as *mut u8, 0, hi - lo);
                }
            }
        }
        if !released.is_empty() {
            let t = tlb_flush_range_start(pt, virt, len / PAGE_SIZE);
            free_after_flush(Deferred { tickets: alloc::vec![t], frames: released, blocks: Vec::new(), dev: Vec::new(), tables: Vec::new() });
        }
        covered == len
    }

    /// Drop the resident pages of the *private lazy* VMAs overlapping
    /// `[virt, virt+len)` (PTEs cleared, one TLB flush, then frames
    /// released), leaving every VMA in place. The first half of a munmap
    /// split into bounded holds; a later `unmap_range` removes the VMAs.
    /// Shared, device and eager mappings are untouched (unmap_range frees
    /// them whole). Returns the number of pages released.
    pub fn strip_range(&mut self, virt: usize, len: usize) -> usize {
        let virt = virt & !(PAGE_SIZE - 1);
        let len  = (len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1);
        let end  = match virt.checked_add(len) { Some(e) => e, None => return 0 };
        if len == 0 { return 0; }
        self.update_hiwater();
        let pt = self.page_table_root;
        let mut released: Vec<usize> = Vec::new();
        for region in self.regions.iter_mut().filter_map(|r| r.as_mut()) {
            if region.end <= virt || region.start >= end { continue; }
            if !region.lazy || region.map_flags & MAP_SHARED != 0 || region.file_cap == usize::MAX { continue; }
            if region.lazy_count == 0 { continue; }
            let lo = region.start.max(virt);
            let hi = region.end.min(end);
            let first = (lo - region.start) / PAGE_SIZE;
            let last  = ((hi - region.start) / PAGE_SIZE).min(region.lazy_pages.len());
            for i in first..last {
                let phys = region.lazy_pages[i];
                if phys == 0 { continue; }
                unsafe { unmap_page(pt, region.start + i * PAGE_SIZE); }
                region.lazy_pages[i] = 0;
                region.set_written(i, false);
                region.lazy_count = region.lazy_count.saturating_sub(1);
                released.push(phys);
            }
        }
        let n = released.len();
        if n != 0 {
            let t = tlb_flush_range_start(pt, virt, len / PAGE_SIZE);
            free_after_flush(Deferred { tickets: alloc::vec![t], frames: released, blocks: Vec::new(), dev: Vec::new(), tables: Vec::new() });
        }
        n
    }

    /// Unmap `size` bytes starting at `virt` and free the backing pages.
    ///
    /// Delegates to [`unmap_range`]; kept for compatibility with existing call sites.
    pub fn unmap(&mut self, virt: usize, size: usize) {
        self.unmap_range(virt, size);
    }

    /// Page-cache keys of the shared file mappings overlapping
    /// `[addr, addr+len)` (msync). None if part of the range is unmapped.
    pub fn shared_file_keys(&self, addr: usize, len: usize) -> Option<Vec<u64>> {
        let end = addr.checked_add((len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1))?;
        let mut keys: Vec<u64> = Vec::new();
        let mut covered = 0usize;
        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if r.end <= addr || r.start >= end { continue; }
            covered += r.end.min(end) - r.start.max(addr);
            if r.lazy && r.map_flags & MAP_SHARED != 0 && is_file_backed(r.file_cap) {
                let k = file_key(r.file_cap);
                if k != 0 && !keys.contains(&k) { keys.push(k); }
            }
        }
        if covered < end - addr { return None; }
        Some(keys)
    }

    /// Lowest page in `[from, end)` that belongs to a file-backed VMA and is
    /// not populated yet — what a kernel prefault must read in (unlocked,
    /// through the real fault path) before its locked `prefault_range` walk.
    pub fn first_absent_file_page(&self, from: usize, end: usize) -> Option<usize> {
        let from = from & !(PAGE_SIZE - 1);
        let mut best: Option<usize> = None;
        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            if !r.lazy || !(is_file_backed(r.file_cap) || r.vmo != 0) || r.end <= from || r.start >= end {
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

    /// Tag the tmpfs/memfd VMA starting at `virt` (just made by
    /// [`map_shared_frames`](Self::map_shared_frames)) with the VFS identity
    /// of its VMO and the byte offset of its first page in it.
    pub fn set_vmo(&mut self, virt: usize, vmo: u64, off: u64) {
        let virt = virt & !(PAGE_SIZE - 1);
        if let Some(r) = self.regions.iter_mut().filter_map(|r| r.as_mut())
            .find(|r| r.start == virt)
        {
            let old = r.vmo;
            r.vmo = vmo;
            r.file_off = off;
            vmo_retain(vmo);
            vmo_release(old);
        }
    }

    /// Lazy `MAP_SHARED` mapping of a tmpfs/memfd object: the VMA only names
    /// the object (`vmo`, byte offset `off` of its first page); every page
    /// is found by the fault path (`FaultPlan::Vmo`), so nothing is
    /// allocated, zeroed or pinned at mmap time.
    pub fn map_shared_vmo_lazy(&mut self, virt: usize, size: usize, flags: PageFlags, vmo: u64, off: u64) -> bool {
        if vmo == 0 || !self.map_lazy(virt, size, flags, false) { return false; }
        let virt = virt & !(PAGE_SIZE - 1);
        let r = match self.regions.iter_mut().filter_map(|r| r.as_mut()).find(|r| r.start == virt) {
            Some(r) => r,
            None => return false,
        };
        r.map_flags = MAP_SHARED;
        r.prot = {
            let mut p = PROT_READ;
            if flags.contains(PageFlags::WRITABLE) { p |= PROT_WRITE; }
            if flags.contains(PageFlags::EXECUTE)  { p |= PROT_EXEC; }
            p
        };
        r.vmo = vmo;
        r.file_off = off;
        vmo_retain(vmo);
        true
    }

    /// What `mremap` must know about `[addr, addr+len)` before it decides
    /// anything: the range must lie inside one VMA (else EFAULT).
    pub fn mremap_source(&self, addr: usize, len: usize) -> Result<MremapSource, isize> {
        let end = addr.checked_add(len).ok_or(-14isize)?;
        let r = self.find(addr).ok_or(-14isize)?;
        if end > r.end { return Err(-14); }
        Ok(MremapSource {
            vmo: r.vmo,
            vmo_off: r.file_off + (addr - r.start) as u64,
            device: r.file_cap == usize::MAX,
            private_anon: r.file_cap == 0 && r.vmo == 0 && r.shm.is_none()
                && r.map_flags & (MAP_SHARED | MAP_FILE_COPY) == 0,
            ends_vma: end == r.end,
        })
    }

    /// `mremap` growing `[addr, addr+old_len)` to `new_len` bytes where it
    /// is (Linux's `vma_expandable`): the range must end its VMA and the room
    /// above must be free. `tail` holds the frames of the new pages of a
    /// tmpfs/memfd VMA (see [`MremapSource::vmo`]), empty for every other
    /// kind, whose new pages fault in as usual; they are taken over only
    /// when this returns `addr`. Errors: EFAULT (-14) if the old range is
    /// not inside one VMA, ENOMEM (-12) if it cannot grow here.
    pub fn mremap_grow(&mut self, addr: usize, old_len: usize, new_len: usize, tail: &[usize]) -> isize {
        if addr & (PAGE_SIZE - 1) != 0 { return -22; }
        let old_end = match addr.checked_add(old_len) { Some(e) => e, None => return -14 };
        let new_end = match addr.checked_add(new_len) { Some(e) => e, None => return -12 };
        let idx = match self.regions.iter().position(|r| matches!(r,
            Some(r) if addr >= r.start && addr < r.end))
        {
            Some(i) => i,
            None => return -14,
        };
        {
            let r = self.regions[idx].as_ref().unwrap();
            if old_end > r.end { return -14; }
            if old_end != r.end || r.file_cap == usize::MAX || new_end > USER_VA_END
                || new_end <= old_end
                || (r.vmo != 0 && tail.len() != (new_end - old_end) / PAGE_SIZE)
            {
                return -12;
            }
        }
        if !self.range_is_free(old_end, new_end - old_end) { return -12; }
        let pt = self.page_table_root;
        let r = self.regions[idx].as_mut().unwrap();
        r.make_lazy();
        let old_pages = r.pages();
        r.end = new_end;
        r.extend_tail(pt, old_pages, tail);
        addr as isize
    }

    /// `mremap(MREMAP_MAYMOVE)`: move `[addr, addr+old_len)`, which lies in
    /// one VMA, to `[dest, dest+new_len)` (`new_len >= old_len`), which the
    /// caller has made free. The page-table entries move; the frames and the
    /// backing do not, so a moved shared mapping (shared anonymous memory, a
    /// shared file, a tmpfs/memfd object) keeps sharing with every other
    /// mapper, and a private one keeps its copy-on-write state. A grown tail
    /// faults in through the VMA's object or file as usual; a tmpfs/memfd
    /// VMA's tail is `tail` (taken over only on success). With `dontunmap`
    /// (MREMAP_DONTUNMAP, private anonymous only) the old range stays mapped,
    /// empty. Returns `dest` or a negative errno.
    pub fn mremap_move(&mut self, addr: usize, old_len: usize, dest: usize, new_len: usize,
                       tail: &[usize], dontunmap: bool) -> isize {
        if addr & (PAGE_SIZE - 1) != 0 || dest & (PAGE_SIZE - 1) != 0 || new_len < old_len
            || old_len == 0
        {
            return -22;
        }
        let old_end = match addr.checked_add(old_len) { Some(e) => e, None => return -14 };
        let new_end = match dest.checked_add(new_len) { Some(e) => e, None => return -22 };
        if new_end > USER_VA_END { return -22; }
        if dest < old_end && new_end > addr { return -22; }
        if self.overlap_end(dest, new_len).is_some() { return -12; }
        let src = match self.mremap_source(addr, old_len) { Ok(s) => s, Err(e) => return e };
        let grow_pages = (new_len - old_len) / PAGE_SIZE;
        if src.vmo != 0 && tail.len() != grow_pages { return -12; }
        if dontunmap && !src.private_anon { return -22; }
        if src.device {
            // A device aperture is never split or grown (Linux: EFAULT for a
            // VM_PFNMAP grow); a whole one moves.
            let whole = self.find(addr).map_or(false, |r| r.start == addr && r.end == old_end);
            if grow_pages != 0 || !whole { return -14; }
        } else {
            self.split_at(addr);
            self.split_at(old_end);
        }
        let idx = match self.regions.iter().position(
            |r| matches!(r, Some(r) if r.start == addr && r.end == old_end))
        {
            Some(i) => i,
            None => return -14,
        };
        let pt = self.page_table_root;
        let mut r = self.regions[idx].take().unwrap();
        let old_pages = r.pages();

        if r.file_cap == usize::MAX {
            for i in 0..old_pages { unsafe { unmap_page(pt, addr + i * PAGE_SIZE); } }
            tlb_flush_range(pt, addr, old_pages);
            for i in 0..old_pages {
                unsafe { map_page(pt, dest + i * PAGE_SIZE, r.phys + i * PAGE_SIZE, r.flags); }
            }
            r.start = dest;
            r.end = new_end;
            self.regions[idx] = Some(r);
            return dest as isize;
        }

        r.make_lazy();
        let access = r.prot & PROT_ACCESS != 0;
        let mut had_pte = false;
        if access {
            for (i, _) in r.lazy_pages.present() {
                unsafe { unmap_page(pt, addr + i * PAGE_SIZE); }
                had_pte = true;
            }
        }
        let mut tables: Vec<usize> = Vec::new();
        if !dontunmap {
            unsafe { crate::paging::prune_user_page_tables(pt, addr, old_end, &mut tables); }
        }
        if had_pte || !tables.is_empty() { tlb_flush_range(pt, addr, old_pages); }
        if !tables.is_empty() {
            PT_PAGES_PRUNED.fetch_add(tables.len(), Ordering::Relaxed);
            for phys in tables { buddy_free(phys, 0); }
        }

        r.start = dest;
        r.end = dest + old_len;
        if access {
            let mut lost: Vec<usize> = Vec::new();
            for (i, phys) in r.lazy_pages.present() {
                // A frame still shared copy-on-write stays read-only; a
                // clean file page too (`install_flags`).
                let mut f = r.install_flags(i, r.flags);
                if r.cow && crate::pageref::get(phys) > 1 { f = f & !PageFlags::WRITABLE; }
                if !unsafe { map_page(pt, dest + i * PAGE_SIZE, phys, f) } { lost.push(i); }
            }
            // No page table for the destination (out of memory): such a
            // page is dropped rather than left present without a PTE.
            for i in lost {
                let phys = core::mem::replace(&mut r.lazy_pages[i], 0);
                r.set_written(i, false);
                r.lazy_count -= 1;
                crate::pageref::unref_or_free(phys, 0);
            }
        }
        r.end = new_end;
        r.extend_tail(pt, old_pages, tail);
        #[cfg(target_arch = "aarch64")]
        if r.flags.contains(PageFlags::EXECUTE) {
            unsafe { core::arch::asm!("ic ialluis", "dsb ish", "isb"); }
        }
        let (flags, prot) = (r.flags, r.prot);
        self.regions[idx] = Some(r);
        if dontunmap && self.map_lazy(addr, old_len, flags, false) {
            self.set_prot(addr, prot);
        }
        dest as isize
    }

    /// Drop the resident pages of every VMA of this address space backed by
    /// file `file_cap` or tmpfs object `vmo` (whichever is non-zero) whose
    /// backing bytes start in `[from, to)`: the object lost them (truncate,
    /// hole punch). The next touch of such a page faults again, and past end
    /// of file that is SIGBUS, as on Linux (whose truncate unmaps private
    /// copy-on-write copies too). Returns the number of pages dropped.
    pub fn zap_backing(&mut self, file_cap: usize, vmo: u64, from: u64, to: u64) -> usize {
        if (file_cap == 0 && vmo == 0) || from >= to { return 0; }
        let pt = self.page_table_root;
        let mut released: Vec<usize> = Vec::new();
        let mut flush: Vec<(usize, usize)> = Vec::new();
        let mut peak_noted = false;
        for i in 0..self.regions.len() {
            let hit = match self.regions[i].as_ref() {
                Some(r) => r.lazy && ((file_cap != 0 && r.file_cap == file_cap)
                    || (vmo != 0 && r.vmo == vmo)),
                None => false,
            };
            if !hit { continue; }
            if !peak_noted { self.update_hiwater(); peak_noted = true; }
            let r = self.regions[i].as_mut().unwrap();
            let base = r.file_off;
            // An ELF segment's pages past its file extent are BSS, not file.
            let extent = if r.vmo != 0 { u64::MAX } else { r.file_len };
            let mut dropped = false;
            let present: Vec<usize> = r.lazy_pages.present().map(|(i, _)| i).collect();
            for idx in present {
                let off = base + (idx * PAGE_SIZE) as u64;
                if off < from || off >= to || (idx * PAGE_SIZE) as u64 >= extent { continue; }
                // Linux keeps a private page the mapper already CoW'd.
                if r.tracks_cow_vmo() && r.is_written(idx) { continue; }
                let phys = core::mem::replace(&mut r.lazy_pages[idx], 0);
                unsafe { unmap_page(pt, r.start + idx * PAGE_SIZE); }
                r.set_written(idx, false);
                r.lazy_count = r.lazy_count.saturating_sub(1);
                released.push(phys);
                dropped = true;
            }
            if dropped { flush.push((r.start, r.pages())); }
        }
        let n = released.len();
        let tickets: Vec<_> = flush.into_iter().map(|(va, k)| tlb_flush_range_start(pt, va, k)).collect();
        free_after_flush(Deferred { tickets, frames: released, blocks: Vec::new(), dev: Vec::new(), tables: Vec::new() });
        n
    }

    /// Write-back of the page-cache pages `pgoffs` (sorted) of file `key` is
    /// about to copy them: make every PTE of this address space that maps
    /// one of them writable read-only again and forget that it was written,
    /// so the next store takes the first-write fault, which marks the page
    /// dirty again (`pagecache::mark_dirty`). Linux's `page_mkclean`.
    /// Returns the number of PTEs write-protected.
    pub fn wrprotect_cached(&mut self, key: u64, pgoffs: &[u64]) -> usize {
        if key == 0 || pgoffs.is_empty() { return 0; }
        let (lo, hi) = (pgoffs[0], pgoffs[pgoffs.len() - 1]);
        let pt = self.page_table_root;
        let mut flush: Vec<(usize, usize)> = Vec::new();
        let mut n = 0usize;
        for i in 0..self.regions.len() {
            let r = match self.regions[i].as_mut() { Some(r) => r, None => continue };
            if !r.lazy || r.map_flags & MAP_SHARED == 0 || !is_file_backed(r.file_cap)
                || !r.flags.contains(PageFlags::WRITABLE) || r.written.iter().all(|&w| w == 0)
            { continue; }
            if file_key(r.file_cap) != key { continue; }
            let (mut first, mut last) = (usize::MAX, 0usize);
            for idx in 0..r.file_pages().min(r.lazy_pages.len()) {
                if !r.is_written(idx) { continue; }
                let phys = r.lazy_pages[idx];
                if phys == 0 { continue; }
                let pg = match r.cache_pgoff(idx) { Some(pg) => pg, None => continue };
                if pg < lo || pg > hi || pgoffs.binary_search(&pg).is_err() { continue; }
                let va = r.start + idx * PAGE_SIZE;
                r.set_written(idx, false);
                let fl = r.install_flags(idx, r.flags) & !PageFlags::WRITABLE;
                unsafe { map_page(pt, va, phys, fl); }
                first = first.min(va);
                last = last.max(va);
                n += 1;
            }
            if first != usize::MAX { flush.push((first, (last - first) / PAGE_SIZE + 1)); }
        }
        for (va, k) in flush { tlb_flush_range(pt, va, k); }
        n
    }

    /// The highest end of the regions that overlap `[virt, virt + len)`, or
    /// None when the range is free (as far as VMAs go: what every `map*`
    /// rejects with `vma-overlap`). mmap's placement uses it to step past
    /// whatever already occupies a candidate range.
    pub fn overlap_end(&self, virt: usize, len: usize) -> Option<usize> {
        let end = virt.saturating_add(len);
        self.regions.iter()
            .filter_map(|r| r.as_ref())
            .filter(|r| virt < r.end && end > r.start)
            .map(|r| r.end)
            .max()
    }

    /// Look up the VmaRegion that contains `virt`, if any.
    ///
    /// The slot last found is tried first (`find_hint`). Futex keying
    /// (`futex_backing`), page faults and `virt_to_phys` hit the same VMA
    /// over and over, and the full scan walks every slot of `regions`
    /// (L10.11). The hint is validated against the live slot, so it cannot
    /// return a region that does not contain `virt`.
    pub fn find(&self, virt: usize) -> Option<&VmaRegion> {
        let h = self.find_hint.load(Ordering::Relaxed);
        if let Some(Some(r)) = self.regions.get(h) {
            if virt >= r.start && virt < r.end { return Some(r); }
        }
        let (i, r) = self.regions.iter().enumerate()
            .find_map(|(i, r)| r.as_ref().filter(|r| virt >= r.start && virt < r.end).map(|r| (i, r)))?;
        self.find_hint.store(i, Ordering::Relaxed);
        Some(r)
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

    /// The identity of the memory a shared (non-`FUTEX_PRIVATE`) futex at
    /// `virt` names, as Linux's `get_futex_key` computes it:
    ///
    /// * not `MAP_SHARED`: [`FutexBacking::Private`] — the futex belongs to
    ///   this address space (the caller keys it by (mm, address));
    /// * shared anonymous memory: (object, byte offset in the object), the
    ///   same through every mapping of it, across fork and VMA splits;
    /// * a shared file mapping through the page cache: (file key, byte
    ///   offset in the file), the same in every process that maps the file,
    ///   at whatever address;
    /// * anything else shared (tmpfs/memfd frames aliased by
    ///   `map_shared_frames`, a file whose pages are not cached): the
    ///   physical address of the byte, which those mappings share by
    ///   construction and pin while mapped.
    ///
    /// `NotPresent`: the page of a physical-keyed mapping is not faulted in;
    /// the caller touches it (no lock held) and asks again.
    pub fn futex_backing(&self, virt: usize) -> FutexBacking {
        let Some(vma) = self.find(virt) else { return FutexBacking::Unmapped };
        if vma.map_flags & MAP_SHARED == 0 { return FutexBacking::Private; }
        let off = virt - vma.start;
        if let Some(obj) = vma.shm.as_ref() {
            let pos = vma.shm_pgoff * PAGE_SIZE + off;
            return FutexBacking::Shared(1, alloc::sync::Arc::as_ptr(obj) as u64, pos as u64);
        }
        if is_file_backed(vma.file_cap) {
            let key = file_key(vma.file_cap);
            if key != 0 { return FutexBacking::Shared(2, key, vma.file_off + off as u64); }
        }
        match self.virt_to_phys(virt) {
            Some(p) => FutexBacking::Shared(3, p as u64, 0),
            None    => FutexBacking::NotPresent,
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
            // A never-touched anonymous page (demand-zero .bss, heap, stack)
            // is a valid destination: populate it as the user's own store
            // would. Safe under this lock (`prefault_range` does the same).
            // An absent file-backed page needs file I/O with the address
            // space unlocked and is left to the caller's prefault — or to a
            // fault-tolerant copy (`uaccess::copy_to_user`) instead of this.
            if self.virt_to_phys(va).is_none() && !self.is_absent_file_page(va) {
                self.handle_user_page_fault(va, true);
            }
            if !self.unshare_cow_page(va, false) { return false; }
            // Never store into a page-cache frame through a mapping that may
            // not write it: the frame is every other mapper's file data too.
            if self.is_shared_readonly_file_page(va) { return false; }
            // A store through the HHDM bypasses the read-only PTE of a clean
            // file page: account it as written, as the user's store would.
            if self.is_clean_writable_file_page(va) {
                self.handle_user_page_fault(va, true);
            }
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

    /// Tag the VMAs overlapping `[addr, addr+len)` as mapping a file from a
    /// noexec mount ([`MAP_NOEXEC_MOUNT`]); mprotect then refuses PROT_EXEC.
    pub fn mark_noexec_mount(&mut self, addr: usize, len: usize) {
        let addr = addr & !(PAGE_SIZE - 1);
        let end = addr.saturating_add((len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1));
        self.split_at(addr);
        self.split_at(end);
        for r in self.regions.iter_mut().flatten() {
            if r.start >= addr && r.end <= end && r.start < r.end { r.map_flags |= MAP_NOEXEC_MOUNT; }
        }
    }

    /// Does any VMA overlapping `[addr, addr+len)` carry [`MAP_NOEXEC_MOUNT`]?
    pub fn range_noexec_mount(&self, addr: usize, len: usize) -> bool {
        let addr = addr & !(PAGE_SIZE - 1);
        let end = addr.saturating_add((len + PAGE_SIZE - 1) & !(PAGE_SIZE - 1));
        self.regions.iter().flatten().any(|r|
            r.start < end && r.end > addr && r.map_flags & MAP_NOEXEC_MOUNT != 0)
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
            let no_access = prot & PROT_ACCESS == 0;

            // Remap every already-backed page of this now wholly-contained VMA.
            // PROT_NONE drops the PTEs instead (the frames stay owned by the
            // VMA, and a later mprotect reinstalls them): PageFlags have no
            // "present but inaccessible" encoding.
            if no_access {
                if region.lazy {
                    // Only faulted pages have PTEs; a large reservation that
                    // is mostly untouched costs nothing here.
                    for (i, _) in region.lazy_pages.present() {
                        unsafe { unmap_page(self.page_table_root, region.start + i * PAGE_SIZE); }
                    }
                } else if region.phys != 0 {
                    let n_pages = (region.end - region.start) / PAGE_SIZE;
                    for i in 0..n_pages {
                        unsafe { unmap_page(self.page_table_root, region.start + i * PAGE_SIZE); }
                    }
                }
            } else if region.lazy {
                let is_cow = region.cow;
                for (i, phys) in region.lazy_pages.present() {
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
                            region.install_flags(i, new_flags)
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

        if changed { tlb_flush_range(self.page_table_root, addr, (end - addr) / PAGE_SIZE); }
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
            self.update_hiwater();
            let region = self.regions[idx].as_mut().unwrap();
            let heap_start = region.start; // = self.heap_start
            let old_end    = region.end;
            region.end     = new_end;

            // Page indices are relative to the VMA start (heap_start).
            let first_idx = (new_end - heap_start) / PAGE_SIZE;
            let last_idx  = (old_end  - heap_start + PAGE_SIZE - 1) / PAGE_SIZE;
            // Released after the flush, as in `unmap_range`.
            let mut released: Vec<usize> = Vec::new();
            for i in first_idx..last_idx.min(region.lazy_pages.len()) {
                if region.lazy_pages[i] != 0 {
                    let page_va = heap_start + i * PAGE_SIZE;
                    unsafe { unmap_page(self.page_table_root, page_va); }
                    released.push(region.lazy_pages[i]);
                    region.lazy_pages[i] = 0;
                    region.set_written(i, false);
                    region.lazy_count = region.lazy_count.saturating_sub(1);
                }
            }
            tlb_flush_range(self.page_table_root, heap_start + first_idx * PAGE_SIZE,
                            last_idx.saturating_sub(first_idx));
            for phys in released { crate::pageref::unref_or_free(phys, 0); }
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

// ── Per-process memory accounting (/proc/<pid>/status, statm, smaps) ─────────
//
// Nothing here walks a page table. Every VMA already keeps an exact count of
// the frames it maps: an eager VMA maps all of its pages, a per-page tracked
// one keeps `lazy_count` in step with `lazy_pages` on every fault, fork,
// split, brk shrink and unmap (the only places a frame enters or leaves a
// VMA). A CoW promotion swaps one frame for another and leaves the count
// alone. So the process totals are a sum over its VMAs — O(VMAs), taken
// under the address space's `busy` flag — and a frame still shared
// copy-on-write after fork counts once in each address space that maps it,
// which is what Linux's RSS does too.

/// Which RSS bucket a VMA's resident pages fall in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RssKind {
    /// Device/MMIO aliasing (`map_device`): not RAM this process owns, and
    /// not in RSS on Linux either (VM_PFNMAP).
    None,
    Anon,
    File,
    Shmem,
}

/// Memory totals of one address space, in 4 KiB pages.
#[derive(Clone, Copy, Default, Debug)]
pub struct MemCounters {
    /// Mapped span of every VMA (`VmSize`).
    pub size: usize,
    pub rss_anon: usize,
    pub rss_file: usize,
    pub rss_shmem: usize,
    /// Private writable span outside the stack (`VmData`).
    pub data: usize,
    /// Main-thread stack span (`VmStk`).
    pub stk: usize,
    /// Executable span (`VmExe`; libraries included, there is no exe/lib
    /// split).
    pub exe: usize,
    /// Peak of `rss()` (`VmHWM`).
    pub hwm: usize,
    /// Peak of `size` (`VmPeak`).
    pub peak: usize,
    /// Page faults: minor (no I/O) and major (file read).
    pub minflt: u64,
    pub majflt: u64,
}

impl MemCounters {
    pub fn rss(&self) -> usize { self.rss_anon + self.rss_file + self.rss_shmem }
}

/// One VMA's line in `/proc/<pid>/smaps`.
#[derive(Clone, Copy, Default, Debug)]
pub struct VmaStat {
    pub start: usize,
    pub end: usize,
    pub prot: u32,
    pub shared: bool,
    pub file_backed: bool,
    pub file_off: u64,
    pub is_stack: bool,
    pub is_heap: bool,
    /// Resident pages.
    pub rss: usize,
    /// Resident pages counted in the anonymous bucket.
    pub anon: usize,
    /// Resident pages some other owner also holds (fork CoW, shared memory).
    pub shared_pages: usize,
    /// Proportional set size in bytes: each frame's 4 KiB divided by the
    /// number of its owners.
    pub pss_bytes: usize,
    /// Backing file capability (0: none) and tmpfs/memfd object (0: none),
    /// so the reader can name the mapping (`/proc/<pid>/maps`).
    pub file_cap: usize,
    pub vmo: u64,
    /// A device aperture.
    pub device: bool,
}

/// `map_flags` bit (kernel-private, like [`MAP_EOF_SIGBUS`]): an eager VMA
/// holding a *copy* of file data (mmap's non-f2fs / MAP_SHARED f2fs path).
/// Only accounting reads it: such pages are `RssFile`, not `RssAnon`.
pub const MAP_FILE_COPY: u32 = 1 << 29;

impl VmaRegion {
    fn pages(&self) -> usize { (self.end - self.start) / PAGE_SIZE }

    /// After `end` moved up from `old_pages` pages (mremap grow): a mmap(2)
    /// file mapping's span is its extent (EOF is checked at fault time), and
    /// a tmpfs/memfd VMA takes `tail`, the object's frames for the new pages
    /// (one reference each, the caller's), installed at once like
    /// `map_shared_frames` does. Every other kind's new pages fault in.
    fn extend_tail(&mut self, root: usize, old_pages: usize, tail: &[usize]) {
        if is_file_backed(self.file_cap) && self.map_flags & MAP_EOF_SIGBUS != 0 {
            self.file_len = (self.end - self.start) as u64;
        }
        if self.vmo == 0 || tail.is_empty() { return; }
        let need = old_pages + tail.len();
        if self.lazy_pages.len() < need { self.lazy_pages.resize(need, 0); }
        let access = self.prot & PROT_ACCESS != 0;
        for (k, &phys) in tail.iter().enumerate() {
            let idx = old_pages + k;
            if access && !unsafe { map_page(root, self.start + idx * PAGE_SIZE, phys, self.flags) } {
                // No page table: leave the page absent (SIGBUS on touch).
                crate::pageref::unref_or_free(phys, 0);
                continue;
            }
            self.lazy_pages[idx] = phys;
            self.lazy_count += 1;
        }
    }

    /// Convert an eager (one contiguous buddy block) VMA to the per-page
    /// lazy form in place, every page present. Not for device mappings.
    fn make_lazy(&mut self) {
        if self.lazy || self.file_cap == usize::MAX { return; }
        let n = self.pages();
        let mut v = Vec::with_capacity(n);
        if self.phys != 0 {
            for i in 0..n { v.push(self.phys + i * PAGE_SIZE); }
            free_eager_tail(self.phys, n);
        }
        self.lazy = true;
        self.phys = 0;
        self.lazy_count = v.len();
        self.lazy_pages = crate::pagevec::PageVec::from(v);
    }

    /// Pages holding file data (the rest of a file VMA is BSS).
    fn file_pages(&self) -> usize {
        ((self.file_len as usize) + PAGE_SIZE - 1) / PAGE_SIZE
    }

    /// Private file-backed per-page VMA: its pages start as clean copies of
    /// the file and are tracked in `written` once they diverge.
    fn tracks_written(&self) -> bool {
        self.lazy && is_file_backed(self.file_cap)
    }

    /// Private copy-on-write alias of a tmpfs object's frames
    /// (`map_private_vmo_frames`). Not `tracks_written`: its pages start as the
    /// object's own frames, and the write-fault logic must not change.
    fn tracks_cow_vmo(&self) -> bool {
        self.lazy && self.cow && self.vmo != 0 && self.file_cap == 0
    }

    pub fn is_written(&self, idx: usize) -> bool {
        self.written.get(idx / 64).map_or(false, |w| (w >> (idx % 64)) & 1 != 0)
    }

    /// Record that page `idx` was (`on`) or was not written. Only pages of
    /// the file extent of a tracking VMA are ever set, and only while
    /// present, so a popcount of `written` is the written-page total.
    fn set_written(&mut self, idx: usize, on: bool) {
        if on {
            if self.tracks_cow_vmo() {
                // A private alias of a tmpfs object: `written` marks the
                // pages the mapper CoW'd, which a truncate must leave alone.
                if idx >= self.pages() { return; }
            } else if !self.tracks_written() || idx >= self.file_pages() { return; }
            if self.written.len() <= idx / 64 { self.written.resize(idx / 64 + 1, 0); }
            self.written[idx / 64] |= 1u64 << (idx % 64);
        } else if let Some(w) = self.written.get_mut(idx / 64) {
            *w &= !(1u64 << (idx % 64));
        }
    }

    /// Resident page `idx` is still an unwritten copy of file data, so its
    /// PTE stays read-only whatever the VMA's protection: the first write
    /// must fault to be seen.
    fn clean_file_page(&self, idx: usize) -> bool {
        self.tracks_written() && idx < self.file_pages() && !self.is_written(idx)
    }

    /// Page offset in the file of page `idx`, if that page may be shared
    /// through the page cache: a private file mapping at a page-aligned
    /// offset, and a page that lies wholly inside the mapping's file extent
    /// (a segment's partial last page is BSS past `file_len`).
    fn cache_pgoff(&self, idx: usize) -> Option<u64> {
        if !self.lazy || !is_file_backed(self.file_cap) { return None; }
        // An mmap(2) mapping's `file_len` is its span (EOF is checked at fault
        // time); an ELF segment's is exact, and its partial last page is BSS.
        if self.map_flags & MAP_EOF_SIGBUS != 0 {
            if (idx * PAGE_SIZE) as u64 >= self.file_len { return None; }
        } else if ((idx + 1) * PAGE_SIZE) as u64 > self.file_len {
            return None;
        }
        crate::pagecache::pgoff_of(self.file_off + (idx * PAGE_SIZE) as u64)
    }

    /// Map the page-cache frame `phys` (a reference already taken for this
    /// mapping) at page `idx`, which must be absent. Clean, so read-only —
    /// unless `written`: a shared mapping's write fault, installed writable
    /// and recorded as written.
    fn install_cached(&mut self, root: usize, idx: usize, phys: usize, written: bool) -> bool {
        let region_pages = self.pages();
        if self.lazy_pages.len() < (idx + 1).min(region_pages) {
            self.lazy_pages.resize((idx + 1).min(region_pages), 0);
        }
        self.set_written(idx, written);
        let install = if written { self.flags } else {
            self.install_flags(idx, self.flags) & !PageFlags::WRITABLE
        };
        if !unsafe { map_page(root, self.start + idx * PAGE_SIZE, phys, install) } {
            return false;
        }
        self.lazy_pages[idx] = phys;
        self.lazy_count += 1;
        true
    }

    /// PTE flags for resident page `idx`: `flags` without WRITABLE while it
    /// is a clean file page.
    pub(crate) fn install_flags(&self, idx: usize, flags: PageFlags) -> PageFlags {
        if flags.contains(PageFlags::WRITABLE) && self.clean_file_page(idx) {
            flags & !PageFlags::WRITABLE
        } else { flags }
    }

    /// Frames this VMA maps.
    pub fn resident_pages(&self) -> usize {
        if self.file_cap == usize::MAX { 0 }
        else if self.lazy { self.lazy_count }
        else if self.phys != 0 { self.pages() }
        else { 0 }
    }

    pub fn rss_kind(&self) -> RssKind {
        if self.file_cap == usize::MAX { RssKind::None }
        else if self.map_flags & MAP_SHARED != 0 { RssKind::Shmem }
        else if is_file_backed(self.file_cap) || self.map_flags & MAP_FILE_COPY != 0 { RssKind::File }
        else { RssKind::Anon }
    }

    /// Split of `resident_pages()` into (file, anon). A file-backed VMA's
    /// pages past its file extent are the ELF segment's BSS — anonymous
    /// memory on Linux too — so only that tail is walked, never the file
    /// part. A written page of the file part is anonymous as well (Linux's
    /// CoW'd private file page): `written` has one bit per such page.
    fn file_anon_split(&self) -> (usize, usize) {
        let n = self.resident_pages();
        if !is_file_backed(self.file_cap) || !self.lazy { return (n, 0); }
        let file_pages = self.file_pages();
        let bss = if file_pages >= self.pages() { 0 } else {
            self.lazy_pages.present().filter(|&(i, _)| i >= file_pages).count()
        };
        let dirty: usize = self.written.iter().map(|w| w.count_ones() as usize).sum();
        let anon = (bss + dirty).min(n);
        (n - anon, anon)
    }
}

impl AddressSpace {
    fn is_stack_vma(&self, r: &VmaRegion) -> bool {
        self.stack_top != 0 && r.start < self.stack_top && self.stack_top <= r.end
    }

    /// (resident, mapped) page totals — the cheap part of `mem_counters`.
    fn totals(&self) -> (usize, usize) {
        let mut rss = 0; let mut size = 0;
        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            size += r.pages();
            rss += r.resident_pages();
        }
        (rss, size)
    }

    /// Record `VmHWM`/`VmPeak` before something lowers them.
    pub fn update_hiwater(&mut self) {
        let (rss, size) = self.totals();
        if rss > self.hiwater_rss { self.hiwater_rss = rss; }
        if size > self.hiwater_vm { self.hiwater_vm = size; }
    }

    /// Mark the eager VMA starting at `virt` as a copy of file data (see
    /// [`MAP_FILE_COPY`]).
    pub fn mark_file_copy(&mut self, virt: usize) {
        let virt = virt & !(PAGE_SIZE - 1);
        if let Some(r) = self.regions.iter_mut().filter_map(|r| r.as_mut())
            .find(|r| r.start == virt)
        {
            r.map_flags = (r.map_flags & !MAP_ANONYMOUS) | MAP_FILE_COPY;
        }
    }

    /// Process memory totals for `/proc/<pid>/status` and `statm`.
    pub fn mem_counters(&self) -> MemCounters {
        let mut c = MemCounters::default();
        for r in self.regions.iter().filter_map(|r| r.as_ref()) {
            let span = r.pages();
            c.size += span;
            let stack = self.is_stack_vma(r);
            if stack { c.stk += span; }
            else if r.map_flags & MAP_SHARED == 0 && r.prot & PROT_WRITE != 0
                && r.file_cap != usize::MAX
            {
                c.data += span;
            }
            if r.prot & PROT_EXEC != 0 && r.prot & PROT_WRITE == 0 { c.exe += span; }
            match r.rss_kind() {
                RssKind::None  => {}
                RssKind::Anon  => c.rss_anon += r.resident_pages(),
                RssKind::Shmem => c.rss_shmem += r.resident_pages(),
                RssKind::File  => {
                    let (f, a) = r.file_anon_split();
                    c.rss_file += f;
                    c.rss_anon += a;
                }
            }
        }
        c.hwm = self.hiwater_rss.max(c.rss());
        c.peak = self.hiwater_vm.max(c.size);
        c.minflt = self.min_flt.load(core::sync::atomic::Ordering::Relaxed);
        c.majflt = self.maj_flt.load(core::sync::atomic::Ordering::Relaxed);
        c
    }

    /// Per-VMA figures for `/proc/<pid>/smaps`, in address order, in one go.
    pub fn vma_census(&self, emit: &mut dyn FnMut(&VmaStat)) {
        let mut cur = CensusCursor::default();
        while !self.vma_census_from(&mut cur, usize::MAX, true, emit) {}
    }

    /// Resumable census. Visits at most about `budget_pages` resident frames
    /// (the owner count is what `Pss` and `Shared_*` are made of), then
    /// returns `false` with `cur` saying where to continue; `true` = done.
    /// `pages == false` (`/proc/<pid>/maps`) skips all per-page counting. The
    /// snapshot is not atomic across calls: if the VMA at the cursor changed
    /// between calls it is skipped.
    pub fn vma_census_from(&self, cur: &mut CensusCursor, budget_pages: usize, pages: bool,
                           emit: &mut dyn FnMut(&VmaStat)) -> bool {
        let mut order: Vec<(usize, usize)> = self.regions.iter().enumerate()
            .filter_map(|(i, r)| r.as_ref().map(|r| (r.start, i))).collect();
        order.sort_unstable();
        let mut spent = 0usize;
        for (start, i) in order {
            if start < cur.next_start { continue; }
            let r = match self.regions[i].as_ref() { Some(r) => r, None => continue };
            let mut resumed = None;
            if cur.page > 0 {
                match cur.partial.take() {
                    Some(p) if p.start == r.start && p.end == r.end => resumed = Some(p),
                    _ => { cur.page = 0; cur.next_start = start + 1; continue; }
                }
            }
            let kind = r.rss_kind();
            let (mut st, first) = match resumed {
                Some(p) => (p, false),
                None => (VmaStat {
                    start: r.start, end: r.end, prot: r.prot,
                    shared: r.map_flags & MAP_SHARED != 0,
                    file_backed: is_file_backed(r.file_cap) || r.map_flags & MAP_FILE_COPY != 0,
                    file_off: r.file_off,
                    is_stack: self.is_stack_vma(r),
                    is_heap: self.heap_start != 0 && r.start == self.heap_start,
                    file_cap: if is_file_backed(r.file_cap) { r.file_cap } else { 0 },
                    vmo: r.vmo,
                    device: r.file_cap == usize::MAX,
                    ..VmaStat::default()
                }, true),
            };
            if pages && kind != RssKind::None {
                if first {
                    st.rss = r.resident_pages();
                    st.anon = match kind {
                        RssKind::Anon => st.rss,
                        RssKind::File => r.file_anon_split().1,
                        _ => 0,
                    };
                    if !r.lazy {
                        st.pss_bytes = st.rss * PAGE_SIZE; // eager: never shared
                    }
                }
                if r.lazy {
                    // A shared mapping's frames carry one more reference
                    // than they have mappings: the backing object's own (the
                    // shmem object, the tmpfs/memfd VMO, the page cache).
                    // Linux counts mappings (mapcount), so a shared page only
                    // one process maps is Private_* there, not Shared_*.
                    let backing = if st.shared { 1 } else { 0 };
                    for (pi, phys) in r.lazy_pages.present_from(cur.page) {
                        if spent >= budget_pages {
                            cur.partial = Some(st);
                            cur.page = pi.max(1);
                            cur.next_start = r.start;
                            return false;
                        }
                        let owners = (crate::pageref::get(phys) as usize)
                            .saturating_sub(backing).max(1);
                        if owners > 1 { st.shared_pages += 1; }
                        st.pss_bytes += PAGE_SIZE / owners;
                        spent += 1;
                    }
                }
            }
            emit(&st);
            cur.next_start = r.end;
            cur.page = 0;
        }
        true
    }
}

/// Where a resumable [`AddressSpace::vma_census_from`] stands.
#[derive(Default)]
pub struct CensusCursor {
    pub next_start: usize,
    pub page: usize,
    pub partial: Option<VmaStat>,
}
