# lane/shmfork — shared anonymous memory across fork (2026-10-18)

Branch `lane/shmfork`, cut from main `0a63f5cb`. It is not merged and not pushed.

## Bug
lane/percpu's stopcont test found that `mmap(MAP_SHARED|MAP_ANONYMOUS)` stops being shared after `fork()`. On Linux the mapping is backed by a shmem object and stays shared. Here it was a per-VMA lazy region: `clone_as` shared only the frames that were already present. A page first touched after the fork was demand-paged separately in each process, so the child's writes never reached the parent. This went unnoticed because every existing test (memtest, forktest) touched its shared page before forking. `MAP_SHARED` of `/dev/zero` took the eager file-copy path, so it was a private, eagerly allocated copy. `mremap(MAYMOVE)` of any shared mapping copied it into a new private anonymous region.

## Fix
- **`mm/src/shmem.rs`** (new) adds `ShmObj`, a sparse `BTreeMap` from page offset to frame under a spin mutex, with live counters for objects and pages. `get_or_alloc(pgoff)` allocates and zeroes a page once and takes a `pageref` for the caller's mapping. The object holds one ref per frame; dropping it unrefs them all.
- **`VmaRegion`** gains `shm: Option<Arc<ShmObj>>` and `shm_pgoff`. `map_lazy(.., is_shared=true)` creates the object. `clone_as` clones the Arc. `split_at` gives the tail half `shm_pgoff + split_idx`. Teardown needed no change, because the Arc goes with the VMA after its pages were unref'd.
- **Fault path:** an absent page in a VMA with an object is resolved through the object. This is one branch before the private zero-fill.
- **`discard_range` (MADV_DONTNEED):** a range with an object is zapped (PTEs plus this mapping's refs) and refaults the same frame. memfd, page-cache and device mappings are still skipped, because they cannot refault.
- **mremap:** new `can_move_shared`/`move_shared`. A shared mapping (anon with an object, shared file with EOF-SIGBUS, or memfd that is not growing) moves its frames. The old PTEs are cleared and flushed, and the same frames are installed at the new range. `grow_in_place` also accepts shared anon. Private mappings keep the old copy path.
- **`/dev/zero`:** `sys_mmap_inner` routes `VnodeKind::DevZero` to the new `mmap_anon` helper, which is the old anonymous branch factored out.
- **Accounting:** in `vma_census` (smaps), shared VMAs subtract the backing reference before counting owners, so a page mapped by a single process is Private_* and Pss divides by the number of mappers, as with Linux mapcount. `/proc/meminfo` now has `Shmem:`, the number of object pages. RssShmem was already right (`rss_kind` returns Shmem for MAP_SHARED).
- The `vmm.rs` changes are local: new fields, one fault branch, one discard condition, the grow condition, the census owners, and two new methods. The fault-accounting code that procfix touches was left alone.

## Tests
`userland/shmtest` (relibc-linked, in `regress` after forktest) has 12 cases. Each one checks pages that were left untouched until after the fork:
- `live_both_ways`
- `multi_fork` (6 children plus a grandchild)
- `munmap` (child punches a hole, child unmaps everything, parent unmaps while the child uses the region)
- `madvise` (shared keeps its data, private still zeroes)
- `mprotect` (3-way split, then a PROT_NONE round trip)
- `threads_fork` (a thread forks while the main thread forks)
- `devzero` (shared vs private)
- `memfd` (mapping plus pread)
- `private_cow`
- `mremap` (move plus grow, child still coherent)
- `release` (Shmem stays up while a child maps the region after the parent's munmap, an execve child drops only its own ref, then everything returns to base)
- `rss_shmem`

aarch64/HVF quick run: shmtest 12/12; forktest, memtest and scmtest are green.

## Results (kernel `3eeb42f0`, tests `70773c6c`)
- **regress** passed on both architectures: aarch64/HVF and x86_64/TCG each ran 28/28 with RC=0, including shmtest 12/12, memtest, forktest, scmtest and pcachetest.
- **net-e2e**, run on its own, passed on both architectures: netctltest, nmtest full and `--ro`, nmrealtest and netroamtest.
- Do not pass `--suite regress --suite net-e2e` in a single invocation. wifisimtest, the last entry in regress, stops the VM, so net-e2e's in-guest commands then fail with "cannot connect to serial socket". This is a harness ordering issue, not a test failure.
- I did not check that shmtest fails on the old kernel; that would have taken a second build. By construction, `live_both_ways`, `multi_fork`, `devzero` and `mremap` depend on pages first touched after the fork, which is exactly what the old kernel got wrong.

## Open
- An object never shrinks while it is mapped. This matches Linux. `MADV_REMOVE` is not implemented, because without a reverse map other mappers' PTEs cannot be zapped.
- mremap that grows a memfd mapping still copies to private memory. The VMA does not record its VMO, so the tail has no frames.
- Unverified: whether any compositor or client grows a memfd mapping with mremap. If one does, the item above is a live bug for it.
