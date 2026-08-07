# K1-B implementation brief — shared file-backed mmap (tmpfs/memfd)

Synthesis of two independent designs (full docs alongside this file:
`k1-vmo-design-minimal.md` — ADOPTED base; `k1-vmo-design-vmo.md` — mined for
extras). Read the minimal design in full before starting; this brief lists the
decision, deltas, and non-negotiables.

## Decision
Implement the **minimal design**: frame store lives in vfs as
`TmpVmo { pages: Vec<usize>, len, seals: u32, is_memfd }` in
`TMP_VMOS: Mutex<[Option<TmpVmo>; MAX_TMP_FILES]>` keyed by `tmp_owner(idx)`
(survives fd-passing and hard links). Rationale: identical kernel-side surface
to the VMO-registry variant, reversible later by moving storage into `mm`;
K1 needs wl_shm, not the M7 substrate. Keep the TmpVmo manipulation behind a
small set of functions (acquire/resize/read/write/release) so a future
migration into `mm` is mechanical.

## Core mechanism (both designs converged on this)
- First MAP_SHARED on a tmpfs/memfd file **promotes** the owner inode: copy
  inline `TmpFileEntry.data` bytes into freshly allocated page frames
  (pageref-managed), record in TMP_VMOS. Inline path stays byte-identical for
  never-promoted files (vfstest must not notice).
- `sys_mmap` MAP_SHARED + TmpFile branch: `vfs::vmo_acquire_frames` (allocates/
  promotes, `pageref::inc` per frame — pin BEFORE publish), then new
  `AddressSpace::map_shared_frames(virt, frames, flags)`: a VMA shaped exactly
  like an already-faulted anonymous MAP_SHARED region — `lazy=true` with
  `lazy_pages` pre-filled, `map_flags` MAP_SHARED, `file_cap=0`, `cow=false`,
  PTEs installed eagerly, writable per prot.
- **fork/munmap/partial-munmap/exit/mprotect: NO CHANGES.** The existing
  `clone_as` MAP_SHARED branch, `unmap_range`, and `AddressSpace::drop` handle
  the frames via `mm::pageref`. If you find yourself editing those paths, stop
  and re-read the designs.
- read/write/ftruncate/fstat gain a VMO-gated branch (only when
  `TMP_VMOS[owner].is_some()`): operate on frames via HHDM. This is what makes
  read()↔mmap coherence free — the pages ARE the file.
- ftruncate: resize via helper; shrink uses `unref_or_free`, never raw
  `buddy::free`. Keep `pages.len()` (capacity in frames) decoupled from
  `vmo.len` (EOF).
- fcntl: replace the `_ => ok_reply()` catch-all fallout at
  `servers/vfs/src/lib.rs:3161` for F_ADD_SEALS/F_GET_SEALS with real arms:
  seals stored in TmpVmo (promote-on-seal is fine), F_SEAL_SHRINK makes
  shrinking ftruncate fail EPERM; F_GET_SEALS returns the set. Non-memfd:
  EINVAL for F_ADD_SEALS. Other seal bits may be accepted+stored but only
  SHRINK must be enforced.
- `sys_memfd_create` (~kernel/src/syscall.rs:6033): mark_memfd + fix the
  **O_WRONLY → O_RDWR** bug (both designs found it independently).

## Non-negotiables (from the VMO design's audits)
1. **Teardown audit:** ALL inode-free sites in `tmp_drop_name` (~vfs :1431 — the
   VMO design counted 3) must release the TmpVmo (unref each frame). Miss one =
   frame leak; double = buddy corruption.
2. **Stale-data audit:** grep every use of `entry.data` / `entry.len` in vfs and
   gate on promotion — a stale inline read after promotion is silent corruption.
3. **Refcount rule:** one rule only — acquire incs, VMA release decs, VMO slot
   holds its own ref until teardown. Every frame: 1 (VMO) + 1 per mapping PTE.
4. **Lock order:** pin frames under the VMO/TMP locks FIRST, then take AS
   `busy` to map. Never nest TMP_FILES inside AS-busy. Never touch RUN_QUEUE.
   **Never dereference user memory under any spinlock** (deadlock class fixed
   in 82d0cc3) — user buffers only via the established prefault/HHDM paths.

## Acceptance (both arches, release builds)
- scmtest subtest 4 (seals) PASS. Subtest 3 (shared_memfd_pixels) PASS if
  SCM_RIGHTS (K1-A) has landed; if not yet merged, prove the VMO half with the
  single-process test below.
- New tests (add to scmtest or a small new crate, follow its conventions):
  (a) single-process double-mmap alias check (two MAP_SHARED mappings of one
  memfd see each other's writes) — isolates VMO from SCM_RIGHTS;
  (b) read()↔mmap coherence both directions (wl_shm requirement, NOT covered
  by scmtest);
  (c) >32768-byte memfd write+mmap (proves the inline 32K cap is lifted for
  promoted files);
  (d) fork + write-visibility both directions; partial munmap; close-while-
  mapped then continue writing; ftruncate grow/shrink under live mapping;
  (e) teardown-order loop (map/unmap/exit permutations, ~100 iterations) —
  a leak or double-free surfaces as buddy exhaustion/corruption.
- Baselines green: vfstest (inline tmpfs path byte-identical), polltest,
  forktest, memtest, sigtest, waittest (flake-retry allowed), boot-to-login.

## Out of scope (do not build)
SCM_RIGHTS itself (K1-A), f2fs MAP_SHARED (stays degraded), /dev/shm +
/run/user mounts (K1-C), SIGBUS-on-hole, shrink-unmaps-live-PTEs,
F_SEAL_WRITE/GROW enforcement, writeback, lazy fault-time population.
