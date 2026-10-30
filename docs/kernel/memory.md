# Memory management

The `mm` crate owns physical memory (a buddy allocator with an O(1)-coalesce bitmap and release-time sanity checks, a slab allocator on top of it, and an IRQ-safe pool), per-process virtual memory (an `AddressSpace` holding a list of VMAs, demand paging, lazy copy-on-write fork, a shared page cache for file pages), and the kernel's own device mappings. This page also covers how the kernel touches user memory, W^X, and the `ENOMEM` sites that say which limit they hit.

## Overview

| Layer | Code | One-line summary |
|---|---|---|
| Physical pages | [mm/src/buddy.rs](../../mm/src/buddy.rs) | Power-of-two blocks, order 0–18 (4 KiB – 1 GiB), doubly-linked free lists, a free-head bitmap for O(1) coalescing, every `free()` checked |
| Kernel heap | [mm/src/slab.rs](../../mm/src/slab.rs) | Size classes 16 B – 4 KiB; larger requests go to the buddy allocator; empty pages returned beyond a 4-page reserve per class |
| IRQ-context heap | [mm/src/atomic_pool.rs](../../mm/src/atomic_pool.rs) | 256 KiB carved at boot; never blocks; used automatically by the global allocator in hard-IRQ context |
| Address spaces | [mm/src/vmm.rs](../../mm/src/vmm.rs) | `AddressSpace` = page-table root + `Vec<Option<VmaRegion>>` + heap break + a `busy` flag |
| Fork | [mm/src/cow.rs](../../mm/src/cow.rs) | `clone_as`: share every private page copy-on-write, copy nothing |
| Sharing bookkeeping | [mm/src/pageref.rs](../../mm/src/pageref.rs), [mm/src/pagecache.rs](../../mm/src/pagecache.rs) | Refcounts for shared frames; one cached frame per (file, page) for private file mappings |
| Frame tables | [mm/src/pagevec.rs](../../mm/src/pagevec.rs) | Chunked per-VMA table of faulted frames, so a sparse 2 GiB reservation pays for what it touches |
| Kernel device mappings | [mm/src/kva.rs](../../mm/src/kva.rs), `map_kernel_device` in [mm/src/paging.rs](../../mm/src/paging.rs) | `ioremap`/`vmap` in a 256 GiB window with per-mapping cache type |
| Arch page tables | [arch/x86_64/src/paging.rs](../../arch/x86_64/src/paging.rs), [arch/aarch64/src/paging.rs](../../arch/aarch64/src/paging.rs) | 4-level, 4 KiB leaves, TLB shootdown |

`mm::init_with_map` ([mm/src/lib.rs](../../mm/src/lib.rs)) runs once from `kernel_main`, before arch init ([boot.md](boot.md#kernel_main-in-order)): HHDM offset, buddy from the boot memory map, per-call-site page tags, slab, atomic pool. All physical memory is reached through the higher-half direct map (`mm::phys_to_virt`).

## How it works

### Buddy allocator

- **Orders 0..`MAX_ORDER`=19**, i.e. blocks of 4 KiB up to 1 GiB, naturally aligned. (Older documentation said "up to 4 MiB, order 0–10"; the limit was raised.)
- **Initialised from the boot map** minus everything below 1 MiB (page 0 is the free lists' null sentinel; the first megabyte holds the AP trampoline and firmware structures) and minus up to 8 explicitly reserved ranges (`reserve_range`, used by the direct-boot paths for the kernel image, initrd and the Pi framebuffer). `PHYS_END` records the top of managed RAM.
- **Intrusive doubly-linked free lists.** A free block stores `next`, `prev` and its order in its own first words, through the HHDM, so unlinking an arbitrary block is O(1).
- **O(1) coalescing.** `FREE_HEAD` is one bit per physical page — set while that page heads a free block. `free()` asks "is my buddy a free head, and at my order?" with one bit test and one read of the buddy's stored order. The bitmap is sized from the real RAM: `init_from_map` carves it (1 bit per page) from the first usable region that fits and excludes it from the pool. It used to be a static 16 GiB array, and above that `free()` fell back to a list walk per coalesce step under `FREE_LISTS` with interrupts off, so tearing down a big address space on a machine with more than 16 GiB (the 32 GiB PC) wedged a CPU for tens of seconds (lane buddyhigh). The list-walk fallback remains, with a loud warning, only if no region fits the bitmap.
- **Release-time sanity checks, on in release builds.** Every `free(addr, order)` checks bounds, alignment, reserved ranges and double frees (the head bit), and every link the allocator follows is validated against `[0, PHYS_END)`. A refused free is **leaked and reported** (`[BUDDY] BAD FREE refused: … from file:line`), never applied; a corrupt link panics at the node that is corrupt (`[BUDDY] CORRUPT LINK`). The Ctrl-T census prints the count of refused frees.
- **Per-call-site accounting.** `alloc` is `#[track_caller]`; a per-page tag array charges live and peak pages to the allocating source line (`site_census`), which is how leaks are attributed.
- **`alloc_below(order, limit)`** walks the free lists for a block under a physical limit, for DMA engines that address only part of RAM (an xHCI controller without 64-bit addressing).

### Slab and kernel heap

The global allocator is `mm::slab::SlabAllocator`. Nine power-of-two classes, 16 B to 4 KiB (8-byte requests use the 16-byte class: each free slot holds both a `next` and a `prev` link); anything larger is a direct buddy allocation. Each class keeps a doubly-linked free list and a per-page live-object count (a `u16` per physical page). When a page's last object is freed and the class already holds `EMPTY_RESERVE`=4 empty pages, the page goes back to the buddy allocator — until 2026-09-26 no slab page was ever returned, so every burst of kernel allocations stayed charged to the slab for the rest of the boot.

In hard-IRQ context the global allocator tries the [atomic pool](../../mm/src/atomic_pool.rs) first: one 256 KiB block carved into per-class free lists (32 B … 4 KiB, ~216 KiB usable), whose lock is only taken with IRQs masked, so an interrupt can never spin on an allocator lock its own CPU holds. It never grows; an exhausted class logs once.

### Address spaces and VMAs

An `AddressSpace` ([mm/src/vmm.rs](../../mm/src/vmm.rs)) is a page-table root, a `Vec<Option<VmaRegion>>` (128 slots to start, grows), `heap_start`/`heap_end`, `stack_top`, RSS high-water marks, and `busy` — the per-address-space exclusive-access flag held around every mutation (fault service, `mmap`/`munmap`/`mprotect`/`brk`, fork's clone) **instead of** the scheduler lock. Threads of a process share one `Arc<AddressSpace>`; the running task's own pointer is published per CPU (`CURRENT_AS`) so its own faults and mm syscalls take only `busy`, not `RUN_QUEUE` ([scheduler.md](scheduler.md#lock-free-per-cpu-identity)).

A `VmaRegion` records `start..end`, arch-neutral `PageFlags`, POSIX `prot` and `map_flags`, and its backing:

| Kind | `lazy` | `file_cap` | Backing |
|---|---|---|---|
| Eager anonymous (`map`) | false | 0 | One contiguous buddy block, zeroed. Used by the ELF loader's eager path and a few kernel callers. |
| Demand-paged anonymous (`map_lazy`) | true | 0 | Frames appear in `lazy_pages` (a [`PageVec`](../../mm/src/pagevec.rs)) one 4 KiB page per fault, zero-filled. Heap, stacks, anonymous `mmap`. |
| Shared anonymous (`map_lazy`, shared) | true | 0 | Like the above, but every absent page is resolved through the VMA's `shm` object, so all mappings of it, in every process, get the same frame. `MAP_SHARED\|MAP_ANONYMOUS` and `MAP_SHARED` of `/dev/zero`; see [shared anonymous memory](#shared-anonymous-memory). |
| Private file (`map_private_file`, demand-paged exec images) | true | registry token | Faults read the file through a kernel hook, 16 pages of fault-around; clean pages may come from the page cache. `MAP_EOF_SIGBUS` (kernel bit 30) makes a page wholly past EOF raise `SIGBUS`. |
| Shared tmpfs/memfd (`map_shared_frames`) | true | 0 | The file's own frames, pre-filled and pinned; see [VMOs](#shared-tmpfs-and-memfd-mappings-vmos). |
| Device (`map_device`) | false | `usize::MAX` | An MMIO or host-visible range, aliased (never copied, never freed) across fork. |

`AddressSpace::drop` frees every frame (`pageref::unref_or_free` for lazy pages), releases file references, then frees **the whole page-table tree** and the root. Until 2026-09-18 only the root was returned and every intermediate table leaked — about 50 pages per `brush -c true`, about 1000 per greeter chain.

### User address-space layout

| Region | Address | Source |
|---|---|---|
| ET_EXEC image / ET_DYN main executable + `brk` heap | from `0x20_0000` (2 MiB), heap grows up | `MAIN_DYN_BASE`, ELF loader |
| Dynamic interpreter (`ld-musl`) | `0x3000_0000` (768 MiB) | `INTERP_BASE` |
| `mmap` without a usable (free) hint | lowest free range of the process in `0x4000_0000..0x7f00_0000_0000`, first fit, freed ranges reused | `AddressSpace::find_free_range` (`MMAP_BASE`/`MMAP_TOP`) |
| AArch64 sigreturn trampoline | `0x7fff_ff00_0000` | [sched/src/signal.rs](../../sched/src/signal.rs) |
| Main-thread stack | 8 MiB below `0x7fff_ffff_f000`, demand-paged | `USER_STACK_SIZE` |
| End of user space | `0x0000_8000_0000_0000` | `USER_SPACE_END` |

The exec'd stack VMA is fixed at 8 MiB (the Linux `ulimit -s` default) and **demand-paged**: only the pages holding argv/envp/auxv plus a 4-page prefault below the initial SP are populated at exec, and nothing is mapped below the VMA, so running off its bottom faults outside any VMA and raises `SIGSEGV`. Until 2026-09-26 all 8 MiB were allocated eagerly at every exec — about 8 MiB of untouched RSS per process, ~250 MiB across a COSMIC session. Anonymous and private-file mappings can be up to 64 GiB each (SpiderMonkey reserves ~2 GiB of JIT space and up to ~8 GiB per wasm memory with `MAP_NORESERVE`); paths that populate at map time (device apertures, shared VMO frames, the eager private-copy fallback) stay limited to 256 MiB. The ELF and exec side of this is in [processes.md](processes.md#the-elf-loader).

### Demand paging and the fault path

Both architectures' fault handlers call `sched::handle_page_fault(addr, is_write)` ([sched/src/lib.rs](../../sched/src/lib.rs)), for user faults **and** for kernel-mode faults on user addresses (a syscall touching a lazy page):

1. Take the faulting process's address space with `lock_leader_address_space` (per-CPU fast path for the running task).
2. `AddressSpace::plan_user_page_fault` decides: zero-fill, copy-on-write promotion, a page-cache hit, segfault, or **a file read to do**.
3. A file read is done with the address space **unlocked**: the file cap is pinned with an extra reference, `busy` is released, the window is read into a bounce buffer, then the lock is retaken and `install_file_fault` re-validates (same cap, same offset, page still absent) before installing. A concurrent `munmap`/`MAP_FIXED`/`mprotect` costs at most a retried access.
4. The outcome is `Handled`, `Segv` (with a one-line `report_segv` naming the pid, address, PC and the neighbouring VMAs) or `Bus` (a per-CPU flag the arch handler turns into `SIGBUS`/`BUS_ADRERR`).

A task already marked Zombie by `exit_group` yields instead of being killed a second time; that double entry into the group kill used to wedge two CPUs against each other (`[PF] no address space for faulting task` + `[WDOG]`).

### Copy-on-write fork

`cow::clone_as` builds the child's address space:

- **Private pages** (writable or not) are mapped read-only in **both** parent and child and their refcount is raised in `pageref`; a write fault later promotes just that page. Nothing is copied at fork.
- **`MAP_SHARED`** pages keep full permissions on both sides, refcounted so teardown frees each frame once. A shared anonymous VMA also hands its `shm` object to the child, so a page neither side had touched before the fork is still one page afterwards.
- **Device mappings** are aliased.
- **Never-faulted pages** stay absent on both sides. For a private mapping either side faults its own zero page; for shared anonymous memory the first to fault allocates the object's frame and the other maps that same frame.

A promotion is **break-before-make with exactly one flush**: unmap, shoot down, map the new frame, and only then drop the shared frame's refcount. `COW_LOCK` serialises every compound `pageref` transaction (fork's inc-and-downgrade sweep, a promotion's get-copy-dec), because parent and child hold *different* `busy` flags and could otherwise both decide "sole owner, reuse in place". A fork from a multithreaded process first **quiesces** its sibling threads (dispatch suspended, mid-flight siblings IPI'd off their CPUs), so a sibling with a stale writable TLB entry cannot write into a frame the child now shares ([processes.md](processes.md#fork-and-clone)).

Kernel stores into user memory go through the HHDM and so bypass a read-only PTE; `write_user_buf` therefore **unshares a CoW page before every store**, and `prefault_range` unshares writable CoW pages so "prefaulted ⇒ no later fault" still holds.

### The unified page cache

[mm/src/pagecache.rs](../../mm/src/pagecache.rs) (lane pagecache, 2026-10-11; before it, from 2026-10-02, the cache held only the pages of files that were mapped and dropped them with the last mapping, and `read(2)` had no cache at all, so every `exec` re-read its binary and libraries from the stick). It is now a cache of regular-file data keyed by **(file key, page index)**, where the filesystem chooses the key: (mount, inode) for f2fs (`f2fs_server::page_cache_key`), never 0. Rename does not change it, hard links share it, and the filesystem drops it before an inode number can be reused. One frame serves `read(2)`/`pread`/`readv` and every mapping fault, and it stays after close and after the last unmap; memory pressure, the cap or the file going away is what frees it.

- **Reads.** A hit copies out of pinned frames without taking the filesystem lock (the open file's offset lives in a per-slot sleeping lock for the same reason). A miss inserts `FILLING` placeholders for the pages it will read, reads the whole run into fresh contiguous frames with the lock dropped, and publishes them; concurrent misses on a page wait for that one fill. Sequential misses double a readahead window up to 2 MiB, so a cold read becomes large commands on the device. A mapping that faults on a cached page maps it, and the fault-around window maps the following cached pages without a read; private mappings get copy-on-write because the frame has more than one owner. Pages never shared that way: a segment's partial last page (its tail is BSS) and the partial page at EOF.
- **Coherency is write-through.** Every data change runs under the filesystem lock, and at the end of the request the filesystem applies it to the cached pages in place, so writes reach the cache in order and a `MAP_SHARED` mapping of the frame sees them. A write or truncate that meets a fill in flight marks it stale and the fill discards that page. A truncate frees the pages past EOF and zeroes the new tail; a write past EOF zeroes the gap in the old last page; unlink of an open file keeps its pages until the last close; the file size lives in the cache entry. *Invalidate-on-write was the alternative and is wrong here:* without a reverse map a mapped frame cannot be taken away from the processes that map it, so dropping it from the cache would split the file into the mapping's stale copy and the filesystem's new one.
- **Writable `MAP_SHARED`.** A shared mapping of a file opened `O_RDWR` maps the cached frames, read-only until the first store; the write fault marks the page dirty and makes it writable. Stores are visible to `read(2)` at once. Dirty pages are never evicted; they are written back by `msync` (now implemented), `fsync`/`sync`, shutdown, and the background task once nothing maps them. A writable `MAP_SHARED` of a descriptor **not** open `O_RDWR` is `EACCES`, as on Linux. Read-only shared mappings of f2fs files are demand-paged through the cache instead of copied whole per process (fontdb maps every font in every COSMIC process; the copy was the session-start I/O storm).
- **Eviction** is a clock over clean, unmapped, unpinned pages; a page referenced since the hand last passed gets a second chance. It runs before every fill (cap: half of RAM, or `pcache.max_mb=N`; plus a free-memory low watermark of RAM/8, giving back up to RAM/6), from a 100 ms background task, and from the buddy allocator's failure path (`emergency_reclaim`: try-locks only, no heap traffic). The background task works with interrupts masked and the emergency path masks them itself, so the cache's spin lock is never held across a context switch; the emergency path's first sweep skips pages referenced since the clock last passed (a fill just published for a fault about to map it). Lock order: `CACHE` -> `pageref` -> buddy/slab.
- **Observability.** `/proc/pcache` (`name value` lines: `enabled`, `cached_pages`, `cap_pages`, `dirty_pages`, `filling_pages`, `files`, and cumulative `hits`, `misses`, `readahead`, `fills`, …), `Cached`, `Dirty` and `MemAvailable` in `/proc/meminfo`, and a `[pcache]` line every 5 s while the cache has been used. `pcache=0` on the kernel command line restores uncached `read(2)` and the old mapping-lifetime caching, for A/B runs.

On AArch64, every page entering the cache is cleaned to the point of coherency, and a hit in an executable mapping does `ic ialluis`. Executable images are registered by inode, so one binary run by fifty processes has one set of text pages.

### Shared tmpfs and memfd mappings (VMOs)

`MAP_SHARED` of a tmpfs file or memfd is genuinely shared: the VFS keeps the file's bytes in a per-inode frame store (`TmpVmo`), and `sys_mmap` maps those very frames with `map_shared_frames` — a VMA shaped like an already-faulted anonymous shared region. `read()`/`write()` operate on the same frames through the HHDM, so read↔mmap coherence is free: the pages *are* the file. Fork, `munmap`, partial `munmap`, exit and `mprotect` needed no changes, because the existing `MAP_SHARED` paths already handle refcounted frames. One refcount rule: the VMO holds one reference, each mapping one more. Lock order: pin frames under the VFS locks first, then take `busy` to map; never nest the VFS lock inside `busy`. Seals (`F_ADD_SEALS`/`F_GET_SEALS`, with `F_SEAL_SHRINK` enforced) live in the VMO. This is what Wayland `wl_shm` pools need; the design record is in the K1 lane notes and the server side in [servers/vfs.md](../servers/vfs.md).

**Lazy mapping (lane mmapB, 2026-10-29).** `sys_mmap` of a tmpfs/memfd no longer walks the object. It calls `vfs::vmo_lazy_prepare` (promote the file to a VMO, assign it an id, touch no frame) and creates only the VMA (`AddressSpace::map_shared_vmo_lazy`). Pages arrive through the normal fault path as `FaultPlan::Vmo`, with the address space unlocked, so the `TMP_FILES`/`TMP_VMOS` locks are never held across a range and never nest under `busy`. Before this a 4 MiB memfd `mmap` cost 65 us and `munmap` 51 us because every page was acquired and mapped eagerly; now they cost 2.6 us and 2.2 us (aarch64/HVF), and the cost moved to first touch. Because the fd is usually closed right after `mmap`, the object could die with unfaulted pages; VMAs therefore count themselves on the object (`vmo_retain`/`vmo_release` on `set_vmo`, fork clone, split, unmap and address-space drop), and an object destroyed while mapped moves to `TMP_ZOMBIES` and is served from there until the last VMA goes. Dmabuf and borrowed objects and ranges past EOF keep the eager path.

**Fault-around (lane faultaround, 2026-10-29).** A single-page fault per VMO page made a touched 4 MiB memfd ten times slower than the eager map (1.17 ms against 127 us). `FaultPlan::Vmo` now carries an aligned window of `VMO_FAULT_AROUND` = 32 pages, clamped to the VMA, and `vfs::vmo_fault_frames` acquires the whole window in one `TMP_VMOS` round trip (existing pages referenced, holes allocated zeroed, stop at EOF or OOM; only a failure of the first page is `SIGBUS`). `install_vmo_fault` revalidates sequence, VMA, VMO and offset with the address space locked and drops the reference of any page already mapped. The same bench now reads 161 us touched, with untouched `mmap` at 3.1 us. `shmtest memfd_lazy` and `shmtest bench` cover it.

### Shared anonymous memory

`mmap(MAP_SHARED | MAP_ANONYMOUS)` and `MAP_SHARED` of `/dev/zero` give a VMA that names an [`mm::shmem::ShmObj`](../../mm/src/shmem.rs), the kernel's version of the Linux shmem file behind such a mapping. The object is a sparse map from page offset to frame. The VMA holds an `Arc` to it and records `shm_pgoff`, the object page of its first page. Fork and `split_at` (partial `munmap`, `mprotect` of a sub-range, `MAP_FIXED` overlays) clone the `Arc`, and the tail VMA's `shm_pgoff` is moved by the split. An absent page faults through `ShmObj::get_or_alloc`, which allocates and zeroes the frame the first time and takes a `pageref` for the mapping every time, so all mappings of the page share one frame. Refcounting works as for VMOs: the object holds one reference per frame and each mapping holds one more. The object frees its frames when the last VMA naming it goes, whether by `munmap`, exit, or `execve` of the last process that maps it. Until then it keeps every page it ever allocated, as Linux does. Lock order is `busy`, then the object lock, then `pageref`.

- **`MADV_DONTNEED`** zaps a shared anonymous range: the PTEs and this mapping's references go, and the next touch maps the same frame again, so the data survives as it does on Linux. Private anonymous memory still reads back as zeros. tmpfs/memfd and page-cache mappings are left alone, because they cannot refault.
- **`mremap`** never copies (since 2026-10-19; shared mappings since lane shmfork): growing in place extends the VMA (`mremap_grow`), and a move carries the VMA's page-table entries to the new range (`mremap_move`), for every kind of mapping, so the frames and backing stay: shared anonymous memory, shared files and tmpfs/memfd objects keep sharing, private memory keeps its pages and copy-on-write state. A grown tail is demand-paged from the object or the file, except a tmpfs/memfd VMA's, whose new pages are the VMO's frames acquired by identity (`vmo_acquire_frames_by_id`; the VMA records the VMO it aliases in `VmaRegion::vmo`) before the address space is taken. `MREMAP_FIXED` moves to exactly `new_addr`, unmapping what was there (EINVAL without `MAYMOVE` or for overlapping ranges); `MREMAP_DONTUNMAP` (private anonymous, equal sizes) leaves the old range mapped and empty; the old range must lie in one VMA (EFAULT). `old_size == 0` (duplicating a shared mapping) is EINVAL.
- **Accounting.** The pages are `RssShmem`. In `smaps`, a frame's backing reference (from the object, the VMO or the page cache) is not counted as a mapper, so a shared page that only one process maps is `Private_*` and `Pss` divides by the number of mappers, as Linux's mapcount does. `/proc/meminfo` `Shmem:` is the number of frames held by live objects.

Before lane shmfork (2026-10-18), shared anonymous memory had no object. It was a per-VMA lazy region, so a page first touched after `fork()` was faulted separately in each process and writes stopped crossing the fork; only pages touched before the fork were shared. `MAP_SHARED` of `/dev/zero` was an eager private copy of the device's zeros, and `mremap` of any shared mapping copied it into private memory. `shmtest` covers all of these cases.

### Kernel mappings, device memory and the KVA window

- **HHDM** — all RAM, mapped by the loader (1 GiB/2 MiB blocks). The kernel never splits those huge entries (`ensure_table` refuses), so the HHDM cannot be used to give a range a different cache type.
- **`map_kernel_device`** — the early device mappings (LAPIC, UART, GIC, ECAM, framebuffer) at `phys + HHDM offset`, with device attributes; it also exposes the root for DRM's device mmaps.
- **KVA window** ([mm/src/kva.rs](../../mm/src/kva.rs), 2026-10-07) — `0xffff_f000_0000_0000`, 256 GiB, top-level slot 480 on both architectures, mapped with 4 KiB PTEs. `ioremap(phys, len, Uncached|WriteCombining|WriteBack)`, `vmap(pages, mode)` for scattered pages, next-fit with a one-page guard after each mapping, and freed ranges reused only after the cursor wraps, so a CPU that missed a bounded x86 shootdown acknowledgement can never see a recycled range. On x86-64 every user PML4 *copies* the kernel half when created, so `kfacil_init` creates the window's top-level entry before the first user process.
- **Cache types.** `PageFlags::NOCACHE`/`MMIO` give strongly-ordered device memory (Device-nGnRnE / UC-); `WRITECOMBINE` gives non-cached *Normal* memory (MAIR index 2, Normal-NC, on AArch64; PAT PA5 = WC on x86-64). They are deliberately not merged: Device memory faults on the first unaligned access, so a GPU blob userspace `memcpy`s through must be Normal-NC, while a sound card's registers must never become speculatively readable Normal memory. See [arch-aarch64.md](arch-aarch64.md#memory-attributes-mair) and [arch-x86_64.md](arch-x86_64.md#pat-identical-on-every-cpu).

### TLB shootdown

Every unmap, protection downgrade and CoW promotion flushes **only its range** of **only the affected address space**:

- **x86-64:** `LOADED_ROOT[cpu]` is published at every CR3 write; the shootdown IPI (vector 0xFD) goes only to CPUs that have the flushed root loaded — none for a single-threaded process, which gets a local `invlpg`. Acknowledgements are per-CPU flush generations (no global TLB lock), and every IRQ-masked spinner (on `busy`, on a tracked lock, or waiting for an ack) services pending flushes itself.
- **AArch64:** broadcast `tlbi vaae1is` + `dsb ish` per page, falling back to `vmalle1is` above 16 pages; no IPI.

**The wait happens outside the address-space lock (x86-64, lane mmapA, 2026-10-29).** `flush_remote` is split into `flush_remote_start` (send the IPIs, no wait), `flush_remote_poll` and `flush_remote_wait` (bounded; returns false on timeout), and `mm::paging` wraps them in a `FlushTicket`. `unmap_range`, `strip_range` and the zero-range and VMO-truncate paths start the flush and hand the frames, buddy blocks, device ranges and pruned page-table pages to `free_after_flush`: freed at once if every acknowledgement is already in, otherwise queued on a global deferred list that `drain_deferred(wait)` empties from `unmap_chunked` after the address-space lock is dropped. **A timed-out wait never frees:** the entry stays queued (`DEFERRED_FREES`, `DEFERRED_TIMEOUTS`), which also closes a latent use-after-free where an `ACK_SPINS` timeout freed frames a remote CPU could still reach through a stale TLB entry. `syscall_dispatch`, `copy_from_user`, `copy_to_user` and `uaccess::prefault` service pending flushes so a waiting initiator is not waiting on a CPU that is itself spinning. The motivation was Firefox on the laptop, whose siblings sit in `recvmsg`/`ioctl` and answer a shootdown late; the synthetic `mmapbench` on the KVM box (short-syscall siblings) does not reproduce that and showed before roughly equal to after, so the proof is the laptop run. Not converted yet, still waiting synchronously under the lock: `mprotect`/`madvise` and the CoW sites, `mremap` moves, the module window and the KVA window. Deferred entries are drained only by `munmap`. AArch64 is unaffected (the broadcast `tlbi` is complete on return; its stubs return an already-done ticket).

Measured on x86-64/KVM during a COSMIC session start (lane cowtlb, 2026-09-25): IPIs ~24 k → 1.9–2.0 k, summed initiator ack wait 9–11 s → 32–66 ms, average CoW copy 3.5–4 ms → 9–18 µs, worst exec argument prefault 65–152 ms → 51–96 µs.

### How the kernel touches user memory

There are three mechanisms, and which one applies depends on whether a lock is held:

1. **Direct dereference after validation.** Syscall code checks `[ptr, ptr+len)` lies below `USER_SPACE_END` (`validate_user_buf`, with `checked_add`) and dereferences the user pointer. A fault on a valid lazy, CoW or file page is serviced exactly like a user fault. This is the common case, and it is only legal **with no spinlock held**.
2. **`AddressSpace::read_user_buf` / `write_user_buf`** — walk the page tables and copy through the HHDM; they never fault (an absent page returns `false`, so callers prefault first) and `write_user_buf` unshares CoW pages. Used where the target is another task, or where a lock is unavoidable.
3. **`sched::uaccess`** ([sched/src/uaccess.rs](../../sched/src/uaccess.rs), 2026-09-27) — `copy_from_user`/`copy_to_user`/`read_user`/`write_user` over one asm copy loop (`rep movsb` on x86-64, a word/byte loop on AArch64) whose faults are *expected*: an unresolvable fault inside it lands on a fixup address and the copy returns the bytes not copied, which the caller turns into `EFAULT` (Linux's `copy_{from,to}_user` contract). `prefault()` makes a buffer resident before a filesystem lock is taken.

Before (3), an unresolvable kernel fault on a user address killed the task with its lock still held (AArch64) or halted the CPU (x86-64), wedging every later user of that lock — the unix-socket table after a `recv()` into a `PROT_NONE` page, for instance. The hard rule behind all of this is in [invariants.md](invariants.md#never-touch-user-memory-under-an-irq-off-spinlock).

## Design decisions

### W^X is enforced, at both entry points

`sys_mmap` returns `EINVAL` for `PROT_WRITE|PROT_EXEC` ([kernel/src/syscall.rs](../../kernel/src/syscall.rs)), and `AddressSpace::mprotect` refuses the same combination. The RW → RX flip JITs use is supported: `mprotect` splits VMAs at the range boundaries and re-installs every already-faulted page with the new flags, then flushes. Firefox's JIT is configured W^X rather than the kernel relaxing the policy. *Cost:* a one-shot RWX `mmap` (an occasional LLVM RuntimeDyld pattern) gets `EINVAL`; nothing in the shipped software set uses it.

<a id="smap-is-a-discipline-here-not-a-hardware-feature"></a>
### SMEP/SMAP and PAN are on; `uaccess` is the only way in

Until 2026-10-18 the kernel did not enable supervisor-mode access prevention: it dereferenced validated user pointers directly and relied on bounds-checked copies as a discipline. That is no longer true on either architecture, and the discipline is now enforced by hardware.

- **AArch64** uses `PSTATE.PAN` (FEAT_PAN). `SCTLR_EL1.SPAN = 0`, so every exception entry sets PAN and the kernel cannot load from or store to a user page except inside a `sched::uaccess` copy or an explicit `UserAccess` window; each AP sets its own PAN on entry. The boot log prints `[MMU] PAN on (strict)`.
- **x86-64** sets `CR4.SMEP` and `CR4.SMAP` on the BSP and every AP in `enable_sse`, each only when CPUID leaf 7 reports it (a BSP without SMAP drops the mode to off so no `stac`/`clac` runs, which would be `#UD`). `copy`/`cmpxchg32`/`load32` in `sched::uaccess` and the `UserAccess` windows `stac` before and `clac` after; `cpu_switch_to` saves RFLAGS and restores only `AC`, so a window survives blocking and preemption and never leaks into another task; `fault_common` clears `AC` on entry because interrupt gates do not.
- **`pan=strict|audit|off`** (default `strict`, read in `kernel/src/main.rs` before arch init) drives both. A raw kernel access to user memory outside `uaccess` is, in `strict`, a task kill on AArch64 and a panic on x86-64 (`SMAP violation at <rip> addr <cr2>`; an instruction fetch from a user page is `SMEP violation`). `audit` logs the first sightings (`[SMAP] audit:` on x86-64, 64 of them) and lets the access through, which is how stray raw accesses are found. `off` is for bisecting. `sys_secprobe` and `privtest` (`kernel_smap_traps_raw_access`, `kernel_uaccess_under_smap_ok`, `kernel_smep_refuses_user_code`) pin the behaviour on both architectures.
- **What it flushed out.** Every driver or server that dereferenced an ioctl argument directly broke under strict mode: the i915 uapi `read_user`/`write_user` helpers (49 uses), DRM and f2fs user copies, and the virgl `gpuprobe` SIGSEGV that looked like a Mesa bug (lanes uaccess2, smapx86, nvsmap). The fix is always the same: copy in with `sched::uaccess::copy_from_user`/`read_user`, work on a kernel buffer with no lock held, copy out with `copy_to_user`/`write_user` (the audit in [invariants.md](invariants.md#never-touch-user-memory-under-an-irq-off-spinlock)).

`CR0.WP` is also set on every CPU so supervisor stores honour read-only user PTEs (APs ran with `WP=0` until 2026-09-27). What PAN and SMAP do not cover is *execution* of user pages on AArch64: see the PXN limitation below.

### Lazy copy-on-write fork, not eager copies

*Choice:* share every private page at fork. *Alternative tried:* an eager copy of all writable private VMAs (`e9510cb`), adopted to work around lost writes in brush/tokio. *Why it was replaced:* the lost writes had two real causes — stale writable TLB entries in sibling threads (closed by the quiesce plus the final shootdown) and kernel stores through the HHDM bypassing the read-only PTE (closed by unsharing in `write_user_buf`) — so the eager copy was pure cost. Measured (aarch64/HVF, lane forkcow, 2026-09-25): a `cosmic-comp` fork went from 53,776 pages copied (210 MiB) in 90.3 ms to 0 copied, 71,392 shared, in 12.7 ms; a 128 MiB resident fork from 51 ms to 4.6–7.4 ms. The same lane found a long-latent AArch64 bug the lazy path exposed: a diagnostic stub used `PAR_EL1` as scratch for `x1`, which does not round-trip on real hardware or HVF ([arch-aarch64.md](arch-aarch64.md#exception-vectors-and-the-el1-fault-path)).

### Demand-paged private file mappings, read without the lock

*Choice:* a `MAP_PRIVATE` mapping of an f2fs file is a lazy file-backed VMA; no read happens in `mmap`, and the fault reads with the address space unlocked. *Why:* the eager copy cost 12–17 ms per 17 MiB mapping on HVF and 75–120 ms on x86-64 TCG; the lazy path costs 0–30 µs (lane lazymmap, 2026-09-25). Reading with the lock dropped matters because the address-space census found one `busy` hold of 1.5 s during session start — every sibling thread's faults stalled behind a filesystem read (lane runqlock). The file is pinned by inode (`f2fs::pin_inode`) so `unlink` or rename-over cannot reclaim its blocks, and it consumes no open-file slot.

### A shared page cache

Before 2026-10-02 every process touching a library page got its own frame: 471 MiB of duplicated library pages with only the COSMIC desktop up, 987 MiB once Firefox's seven processes mapped the 143 MB `libxul.so` — enough to push a 2 GiB guest under init's memory-pressure floor. With the cache, a second process mapping a touched 512-page file allocates one page, and Firefox runs in 2 GiB on both architectures (lane ffmem). The first version tied the cache's lifetime to its mappers so it could never cost more memory than the copies it replaced. That is gone: the unified cache outlives its mappers (so a second `exec`, a second `read` and a Firefox restart cost no device I/O on a slow stick) and is bounded instead by the cap and the watermarks above, with init's memory-pressure floor (RAM/10) sitting below them so the guard sees memory the cache could still give back.

### O(1) buddy coalescing and checked frees

The coalesce check used to walk the whole order-0 list per freed page. Fork's eager copies fragmented RAM fast (2,285 order-0 blocks after 40 fork+exec cycles) and exec-time teardown of a forked child went from 50 ms to seconds, growing with uptime. The bitmap makes it one bit test. The release-time checks exist because a real double free — a fork rollback path freeing the child's page-table root twice — corrupted the intrusive lists and faulted much later in an unrelated `free()`; now such a free is refused at the call that made it.

### Exit releases the address space before the zombie is visible

`exit()` drops the last thread's address space (detach CR3/TTBR0 under `RUN_QUEUE`, then drop once `busy` clears) **before** the task is marked Zombie, as Linux's `exit_mm` runs before `exit_notify`. Before this, `wait4` could return while the reaping CPU still held the child's 8–15 MiB, so `sysinfo` read right after `wait4` "leaked" a child that was simply not yet freed. The two premised exec leaks were refuted the same way, by reading the buddy census before and after with the greeter killed; one real leak, ~56 B of `Box<AddressSpace>` per exec, was fixed (lane execleak, 2026-09-24).

### `ENOMEM` must name its cause

`ENOMEM` from this kernel used to mean about a dozen unrelated things, two of them about RAM. Two investigations started from errno 12 and hunted a leak while the guest reported more than a gigabyte free; the real cause was a 64-entry IPC port table ([ipc.md](ipc.md#live_buckets-the-ceiling-that-looked-like-ram-history-the-table-is-a-slab-since-2026-10-19)). Now every `ENOMEM` in the memory, address-space and exec paths returns through `enomem_site("…")` or `enomem_map_site("…")` (21 sites), which print the site — and, for mapping failures, `mm::vmm::last_map_fail()`: `vma-overlap`, `buddy-order-unavailable`, `va-overflow`, `pte-install`, `no-heap`, `bad-filecap` — when `DBG_ENOMEM` is on (committed `false`, with a 64-line budget, because the console costs ~0.19 s per line). `sys_brk` is instrumented even though `brk(2)` has no errno: a heap that cannot grow answers with the old break, musl turns that into a `NULL` from `malloc`, and it was otherwise invisible. `sys_mincore` is deliberately not instrumented: its `ENOMEM` means "not mapped" and is a normal answer to a pointer probe. Fixed tables elsewhere say when they are full in one ungated line (`[IPC] port table FULL`, `[SCHED] task table FULL`).

## Limitations and open work

- **HHDM accessors are not PAN-checked, by design:** a copy to *another* task's memory goes through that task's page tables via the HHDM, which is a kernel mapping and never PAN-checked. Only the current task's own user addresses are policed.
- **No ASLR** (KASLR is off too): mmap placement is first fit per address space, so two processes running the same code get the same layout. `munmap` frees the page-table nodes a range leaves empty (since 2026-10-19).
- **AArch64 PXN on executable user pages:** `translate_flags` clears both UXN and PXN when a user page is executable, so the kernel could execute user code. PXN should stay set on EL0 pages.
- **No huge pages** for user mappings or the KVA window (a 256 MiB BAR costs 512 KiB of page tables).
- **A private mapping of a non-f2fs file** (tmpfs `MAP_PRIVATE`, the eager copy) counts every page as `RssFile`, written or not; demand-paged f2fs mappings track written pages and count them `RssAnon` (543d02f0).
- **Copies to a named task** (evdev, DRM: `sched::copy_to_task` / `copy_from_task` / `zero_task`, 2026-10-19). The pid is the caller's own process in practice, and then the copy is the fault-tolerant `uaccess` copy: not-present pages fault in as usual and a bad pointer answers `EFAULT` (the old raw copy under `with_task_address_space` killed the caller with SIGSEGV instead). Another process's memory goes through its page tables via the HHDM under its `busy` flag, present pages only, VMA protection honoured; no root switch.
- **Observers of another process** (`/proc/<pid>/smaps`, `status`, `cmdline`: `with_process_as`) spin on its `busy` flag for at most 50 µs, then park 1 ms at a time, and give up after 2 s or on SIGKILL (`[AS] observer gave up ...`). A wedged holder can no longer take the reader's CPU with it.
- **Teardown** (`Drop for AddressSpace`) passes a `cond_resched` point every 512 freed pages, so a multi-GiB exit no longer holds its CPU with IRQs masked. Still one hold: `munmap`/`mremap`/`mincore` of a huge range under `busy`.
- **Write-back of a mapped page visits every process** (since 2026-10-23, lane dirtywp). Before copying a dirty page a mapping holds, write-back write-protects it in every address space (`sched::wrprotect_cache_all`, Linux's `page_mkclean`), and cleans it only if no store faulted since (`seq` per cached page); a later store re-marks it dirty. There is no reverse map, so the walk is per file per write-back batch, skipped when no mapping holds the pages. `/proc/pcache` `mkclean_ptes` counts the PTEs write-protected.
- **Truncate under a live mapping** (since 2026-10-19): `ftruncate`/`truncate` shrinking an f2fs file or a tmpfs/memfd object, and a tmpfs hole punch, drop the pages past the new end from every address space that maps them (`AddressSpace::zap_backing`, through `sched::zap_backing_all`), private copy-on-write copies included as on Linux, so the next touch is `SIGBUS`. There is no reverse map: every process is visited, and only when the object is known to be mapped (a registered cap, a VMO with an identity). Not covered: `open(O_TRUNC)`, the eager-copy mappings (tmpfs `MAP_PRIVATE`, which are private snapshots anyway), and a fault whose unlocked file read raced the truncate (it may still install one stale page past the end). A tmpfs/memfd page dropped this way stays `SIGBUS` even if the file grows again (its VMA cannot refault: the frames are installed at map time).
- The cache is one global lock (`CACHE`); it is held briefly and never across I/O, but its contention under many CPUs has not been measured.
- **A shared anonymous object never shrinks while it is mapped.** A page stays in the object, even after every process has unmapped that range, until the last VMA naming the object goes (as on Linux). `MADV_REMOVE` (punch a hole) is not implemented, because without a reverse map the other mappers' PTEs cannot be zapped.
- `pageref` is a global `BTreeMap` under one lock; it has not shown up in profiles but is a serialisation point for CoW-heavy workloads.
- **Shootdown conversion is partial** (see above): only the unmap family frees after the flush; the other `tlb_flush_range` callers still wait under `busy`, and `munmap` of an anonymous 4 MiB range is dominated (about 300 us) by freeing 1024 pages one at a time. A batched free is the next item.

## Code and tests

| What | Where |
|---|---|
| Physical allocators | [mm/src/buddy.rs](../../mm/src/buddy.rs), [mm/src/slab.rs](../../mm/src/slab.rs), [mm/src/atomic_pool.rs](../../mm/src/atomic_pool.rs) |
| Address spaces, faults, accounting | [mm/src/vmm.rs](../../mm/src/vmm.rs), [mm/src/pagevec.rs](../../mm/src/pagevec.rs) |
| Fork and sharing | [mm/src/cow.rs](../../mm/src/cow.rs), [mm/src/pageref.rs](../../mm/src/pageref.rs), [mm/src/pagecache.rs](../../mm/src/pagecache.rs), [mm/src/shmem.rs](../../mm/src/shmem.rs) |
| Kernel VA | [mm/src/kva.rs](../../mm/src/kva.rs), [mm/src/paging.rs](../../mm/src/paging.rs) |
| Fault entry, address-space locking | `page_fault`, `lock_leader_address_space` in [sched/src/lib.rs](../../sched/src/lib.rs) |
| User copies | [sched/src/uaccess.rs](../../sched/src/uaccess.rs) |
| `mmap` family, `enomem_site` | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |

Regression tests ([userland/](../../userland)): **`memtest`** (CoW isolation, `mremap`, buddy churn, `MAP_SHARED`, `fill_ram_no_leak` at guest-RAM scale, lazy file mappings incl. `sigbus_past_eof`/`survives_unlink`/`no_leak`, the page-cache cases, per-process RSS), **`pcachetest`** (page-cache coherency; [testing.md](../development/testing.md)), **`forktest`** (`cow_isolation`, `cow_kernel_write`, `cow_threads_fork`, `cow_socket_fork`, `cow_fork_cost`), **`shmtest`** (shared anonymous memory across fork, with pages first touched after the fork: both directions while both processes run, multiple forks and a grandchild, `munmap` on either side, `MADV_DONTNEED`, `mprotect` splits, threads plus fork, `/dev/zero` shared and private, memfd, private CoW, `mremap`, `Shmem:` release on last reference and on `execve`, `RssShmem`), **`uptrtest`** (26 syscalls × {anon, file, cow, stack} plus `EFAULT` cases; 112/112 on both arches), **`scmtest`** (shared memfd pixels, seals, double-mmap alias, read↔mmap coherence, teardown loop), **`killmt`** (memory returned after multithreaded deaths), and `exectest`. Run `memtest` with the greeter killed: a starting greeter drains free RAM and fails the leak bars on TCG. See [development/testing.md](../development/testing.md).

## See also

- [invariants.md](invariants.md) — user memory under locks, checked arithmetic, stack policing
- [processes.md](processes.md) — fork, `execve`, and the ELF loader
- [scheduler.md](scheduler.md) — `busy` vs `RUN_QUEUE`, per-CPU current address space
- [servers/vfs.md](../servers/vfs.md) — tmpfs, memfd and the VMO store
- [arch-x86_64.md](arch-x86_64.md), [arch-aarch64.md](arch-aarch64.md) — page tables, cache types, shootdowns
