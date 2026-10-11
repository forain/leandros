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
| Private file (`map_private_file`, demand-paged exec images) | true | registry token | Faults read the file through a kernel hook, 16 pages of fault-around; clean pages may come from the page cache. `MAP_EOF_SIGBUS` (kernel bit 30) makes a page wholly past EOF raise `SIGBUS`. |
| Shared tmpfs/memfd (`map_shared_frames`) | true | 0 | The file's own frames, pre-filled and pinned; see [VMOs](#shared-tmpfs-and-memfd-mappings-vmos). |
| Device (`map_device`) | false | `usize::MAX` | An MMIO or host-visible range, aliased (never copied, never freed) across fork. |

`AddressSpace::drop` frees every frame (`pageref::unref_or_free` for lazy pages), releases file references, then frees **the whole page-table tree** and the root. Until 2026-09-18 only the root was returned and every intermediate table leaked — about 50 pages per `brush -c true`, about 1000 per greeter chain.

### User address-space layout

| Region | Address | Source |
|---|---|---|
| ET_EXEC image / ET_DYN main executable + `brk` heap | from `0x20_0000` (2 MiB), heap grows up | `MAIN_DYN_BASE`, ELF loader |
| Dynamic interpreter (`ld-musl`) | `0x3000_0000` (768 MiB) | `INTERP_BASE` |
| `mmap` without a usable hint | bump cursor from `0x4000_0000` (1 GiB) upward | `MMAP_BUMP` |
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
- **`MAP_SHARED`** pages keep full permissions on both sides, refcounted so teardown frees each frame once.
- **Device mappings** are aliased.
- **Never-faulted pages** stay absent on both sides.

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

### Kernel mappings, device memory and the KVA window

- **HHDM** — all RAM, mapped by the loader (1 GiB/2 MiB blocks). The kernel never splits those huge entries (`ensure_table` refuses), so the HHDM cannot be used to give a range a different cache type.
- **`map_kernel_device`** — the early device mappings (LAPIC, UART, GIC, ECAM, framebuffer) at `phys + HHDM offset`, with device attributes; it also exposes the root for DRM's device mmaps.
- **KVA window** ([mm/src/kva.rs](../../mm/src/kva.rs), 2026-10-07) — `0xffff_f000_0000_0000`, 256 GiB, top-level slot 480 on both architectures, mapped with 4 KiB PTEs. `ioremap(phys, len, Uncached|WriteCombining|WriteBack)`, `vmap(pages, mode)` for scattered pages, next-fit with a one-page guard after each mapping, and freed ranges reused only after the cursor wraps, so a CPU that missed a bounded x86 shootdown acknowledgement can never see a recycled range. On x86-64 every user PML4 *copies* the kernel half when created, so `kfacil_init` creates the window's top-level entry before the first user process.
- **Cache types.** `PageFlags::NOCACHE`/`MMIO` give strongly-ordered device memory (Device-nGnRnE / UC-); `WRITECOMBINE` gives non-cached *Normal* memory (MAIR index 2, Normal-NC, on AArch64; PAT PA5 = WC on x86-64). They are deliberately not merged: Device memory faults on the first unaligned access, so a GPU blob userspace `memcpy`s through must be Normal-NC, while a sound card's registers must never become speculatively readable Normal memory. See [arch-aarch64.md](arch-aarch64.md#memory-attributes-mair) and [arch-x86_64.md](arch-x86_64.md#pat-identical-on-every-cpu).

### TLB shootdown

Every unmap, protection downgrade and CoW promotion flushes **only its range** of **only the affected address space**:

- **x86-64:** `LOADED_ROOT[cpu]` is published at every CR3 write; the shootdown IPI (vector 0xFD) goes only to CPUs that have the flushed root loaded — none for a single-threaded process, which gets a local `invlpg`. Acknowledgements are per-CPU flush generations (no global TLB lock), and every IRQ-masked spinner (on `busy`, on a tracked lock, or waiting for an ack) services pending flushes itself.
- **AArch64:** broadcast `tlbi vaae1is` + `dsb ish` per page, falling back to `vmalle1is` above 16 pages; no IPI.

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

### "SMAP" is a discipline here, not a hardware feature

The README and the design-principles table describe the kernel as using the architecture's supervisor-mode access prevention. **The code does not enable it:** `CR4.SMAP`/`CR4.SMEP` are never set on x86-64, and `PSTATE.PAN` is never set on AArch64 (verified 2026-10-08 by grepping the arch crates). The kernel and the in-kernel servers dereference validated user pointers directly by design, and some servers read path strings by plain dereference of a kernel or user pointer. What *is* enforced is the discipline: every cross-boundary copy is bounds-checked against `USER_SPACE_END` with checked arithmetic, copies under a lock use the HHDM accessors or the fault-tolerant `uaccess` copy, and `CR0.WP` is set on every CPU so supervisor stores honour read-only user PTEs (APs ran with `WP=0` until 2026-09-27, so kernel stores into CoW and `PROT_READ` pages silently succeeded on x86-64 APs). Enabling SMAP/PAN for real would require wrapping every direct user access in `stac`/`clac` (or `PSTATE.PAN` toggles) — a large audit that is open work.

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

`ENOMEM` from this kernel used to mean about a dozen unrelated things, two of them about RAM. Two investigations started from errno 12 and hunted a leak while the guest reported more than a gigabyte free; the real cause was a 64-entry IPC port table ([ipc.md](ipc.md#live_buckets-the-ceiling-that-looked-like-ram)). Now every `ENOMEM` in the memory, address-space and exec paths returns through `enomem_site("…")` or `enomem_map_site("…")` (21 sites), which print the site — and, for mapping failures, `mm::vmm::last_map_fail()`: `vma-overlap`, `buddy-order-unavailable`, `va-overflow`, `pte-install`, `no-heap`, `bad-filecap` — when `DBG_ENOMEM` is on (committed `false`, with a 64-line budget, because the console costs ~0.19 s per line). `sys_brk` is instrumented even though `brk(2)` has no errno: a heap that cannot grow answers with the old break, musl turns that into a `NULL` from `malloc`, and it was otherwise invisible. `sys_mincore` is deliberately not instrumented: its `ENOMEM` means "not mapped" and is a normal answer to a pointer probe. Fixed tables elsewhere say when they are full in one ungated line (`[IPC] port table FULL`, `[SCHED] task table FULL`).

## Limitations and open work

- **No SMAP/SMEP/PAN** — see the decision above.
- **`MMAP_BUMP` is one system-wide cursor** that never reclaims address space. It moves through a ~128 TiB hole, so exhaustion is theoretical, but layouts are not per-process and not randomised (no ASLR; KASLR is off too).
- **AArch64 PXN on executable user pages:** `translate_flags` clears both UXN and PXN when a user page is executable, so the kernel could execute user code. PXN should stay set on EL0 pages.
- **No huge pages** for user mappings or the KVA window (a 256 MiB BAR costs 512 KiB of page tables).
- **Written private file pages count as `RssFile`** until dirty tracking exists; `/proc/<pid>/status` and `smaps` are otherwise Linux-shaped (lane procrss).
- `with_task_address_space` (used by evdev/DRM) reads another task's root without taking a reference — a race against that address space being dropped.
- **Dirty tracking in the page cache is coarse.** Without a reverse map a writable PTE cannot be write-protected again, so a dirty shared page stays dirty (and is written back whole) for as long as it is mapped writable.
- **Truncate under a live mapping** frees the cache's pages but cannot take them from the mapper: the process keeps a detached frame instead of getting `SIGBUS` on the next access, unlike Linux.
- The cache is one global lock (`CACHE`); it is held briefly and never across I/O, but its contention under many CPUs has not been measured.
- `pageref` is a global `BTreeMap` under one lock; it has not shown up in profiles but is a serialisation point for CoW-heavy workloads.

## Code and tests

| What | Where |
|---|---|
| Physical allocators | [mm/src/buddy.rs](../../mm/src/buddy.rs), [mm/src/slab.rs](../../mm/src/slab.rs), [mm/src/atomic_pool.rs](../../mm/src/atomic_pool.rs) |
| Address spaces, faults, accounting | [mm/src/vmm.rs](../../mm/src/vmm.rs), [mm/src/pagevec.rs](../../mm/src/pagevec.rs) |
| Fork and sharing | [mm/src/cow.rs](../../mm/src/cow.rs), [mm/src/pageref.rs](../../mm/src/pageref.rs), [mm/src/pagecache.rs](../../mm/src/pagecache.rs) |
| Kernel VA | [mm/src/kva.rs](../../mm/src/kva.rs), [mm/src/paging.rs](../../mm/src/paging.rs) |
| Fault entry, address-space locking | `page_fault`, `lock_leader_address_space` in [sched/src/lib.rs](../../sched/src/lib.rs) |
| User copies | [sched/src/uaccess.rs](../../sched/src/uaccess.rs) |
| `mmap` family, `enomem_site` | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |

Regression tests ([userland/](../../userland)): **`memtest`** (CoW isolation, `mremap`, buddy churn, `MAP_SHARED`, `fill_ram_no_leak` at guest-RAM scale, lazy file mappings incl. `sigbus_past_eof`/`survives_unlink`/`no_leak`, the page-cache cases, per-process RSS), **`pcachetest`** (page-cache coherency; [testing.md](../development/testing.md)), **`forktest`** (`cow_isolation`, `cow_kernel_write`, `cow_threads_fork`, `cow_socket_fork`, `cow_fork_cost`), **`uptrtest`** (26 syscalls × {anon, file, cow, stack} plus `EFAULT` cases; 112/112 on both arches), **`scmtest`** (shared memfd pixels, seals, double-mmap alias, read↔mmap coherence, teardown loop), **`killmt`** (memory returned after multithreaded deaths), and `exectest`. Run `memtest` with the greeter killed: a starting greeter drains free RAM and fails the leak bars on TCG. See [development/testing.md](../development/testing.md).

## See also

- [invariants.md](invariants.md) — user memory under locks, checked arithmetic, stack policing
- [processes.md](processes.md) — fork, `execve`, and the ELF loader
- [scheduler.md](scheduler.md) — `busy` vs `RUN_QUEUE`, per-CPU current address space
- [servers/vfs.md](../servers/vfs.md) — tmpfs, memfd and the VMO store
- [arch-x86_64.md](arch-x86_64.md), [arch-aarch64.md](arch-aarch64.md) — page tables, cache types, shootdowns
