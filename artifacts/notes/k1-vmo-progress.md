# K1-B implementation progress (drop-proof log)

Task: genuinely shared file-backed mmap for tmpfs/memfd + real memfd seals.
Acceptance: scmtest 4/4 both arches + new tests + baselines green. Brief binding:
/Users/forain/.claude-forain/jobs/afde2e74/tmp/k1-vmo-brief.md

## STATUS: discovery complete, starting edits

## Key decisions / mechanism
- VMO store in vfs: `TmpVmo { pages: Vec<usize>, len, seals: u32, is_memfd }` in
  `TMP_VMOS: Mutex<[Option<TmpVmo>; MAX_TMP_FILES]>` keyed by owner slot.
- Refcount: alloc leaves frame UNTRACKED (=1, VMO owns). map-acquire inc. VMA
  release (munmap/exit/fork already handle). VMO teardown unref_or_free.
- Mirror `vmo.len` into `entry.len` on write/ftruncate so fstat/lseek/poll
  (which read entry.len) stay correct WITHOUT gating. Only gate read/write DATA
  access + ftruncate frame-resize/seals + fcntl seals.
- fork/munmap/exit/mprotect: NO CHANGES (existing lazy MAP_SHARED paths handle).

## Located symbols (post-de461b9)
### servers/vfs/src/lib.rs
- MAX_TMP_FILES=128 (256), MAX_TMP_SIZE=32768 (257). TmpFileEntry struct @260,
  empty() @300, static TMP_FILES @311.
- tmp_owner @1392. tmp_alias_count @1398. tmp_nlink @1409.
- tmp_drop_name @1431: 3 empty() sites: @1437 (alias free, NO vmo), @1439 (owner
  free after last alias, VMO release), @1446 (owner==idx free, VMO release).
- tmp_release_ephemeral @1642: empty() @1656 — CLOSE path reclaim of ephemeral
  owner; a memfd unlinked-while-open becomes ephemeral → MUST release VMO here.
- Other empty() sites are INIT of a fresh free slot (position !in_use): @1752,
  @1782, @1883, @2108, @4121, @4174, @4214, @4316, @4360. Not reclaim. Defensive:
  clear TMP_VMOS[idx] when grabbing a fresh slot is optional insurance.
- IMPORTANT: open resolves fd→OWNER idx (@2075 `tmp_owner`), so every fd's
  VnodeKind::TmpFile{idx} already carries the owner. => read/write/ftruncate/
  fcntl/fstat can key TMP_VMOS[idx] directly.
- handle_read TmpFile arm @2349 (reads entry.data, bounds entry.len, 4K cap).
- handle_write TmpFile arm @2435 (writes entry.data, cap MAX_TMP_SIZE @2449,
  grows entry.len).
- handle_close @2550 → TmpFile arm @2599 calls tmp_release_ephemeral.
- handle_fcntl @3167, catch-all `_ => ok_reply()` @3230. Add F_ADD_SEALS(1033)/
  F_GET_SEALS(1034) arms.
- handle_ftruncate @3903 TmpFile arm @3908 (cap MAX_TMP_SIZE @3912, zero-fill
  entry.data, set entry.len).
- handle_fstat @5156 TmpFile arm @5206/5211 uses e.len (mirrored → OK unchanged).
- reclaim funnels: unlink→tmp_drop_name; rename clobber→tmp_drop_name @4038;
  close→tmp_release_ephemeral; rmdir @4360 (dirs only, never VMO).

### mm/src/vmm.rs
- MAP_SHARED=1<<0 @26. VmaRegion struct @33 (lazy_pages @45, lazy_count @47,
  map_flags @53, file_cap @58, cow @66).
- AddressSpace::new @175. map() eager @190 (shows map_page rollback pattern).
- map_lazy @340 (VMA shape to mirror). map_lazy_file @392.
- AddressSpace::drop @148: lazy → unref_or_free each lazy_pages frame; file_cap
  MAX skip. Handles my VMA (file_cap=0, lazy).
- unmap_range @661: unref_or_free each lazy_pages frame in range (@688), reshapes.
- NEW METHOD to add: map_shared_frames(virt, frames:&[usize], flags) — lazy VMA,
  map_flags=MAP_SHARED, file_cap=0, cow=false, lazy_pages=frames.to_vec(),
  lazy_count=len, map_page each. On failure: unmap installed + unref_or_free ALL
  frames, return false (map_shared_frames OWNS rollback of frames).
- map_page signature: unsafe map_page(root, virt, phys, flags)->bool (via
  self.page_table_root). unmap_page(root, virt).

### mm/src/cow.rs
- clone_as lazy MAP_SHARED branch @184-201: pageref::inc each frame, map into
  child with region.flags, file_cap=0 → no file_retain. Handles my VMA. NO CHANGE.

### mm/src/pageref.rs
- get/inc/dec/unref_or_free(phys, order). All my frames order 0.

### mm/src/buddy.rs
- PAGE_SIZE=4096 @8. alloc(order)->Option<usize> @153. free(addr,order) @199.
- free_pages() @19, total_pages() @17.
- vfs already uses mm::buddy, mm::phys_to_virt, mm::pageref accessible.

### kernel/src/syscall.rs
- prot_to_page_flags @1351. sys_mmap @1371. MAP_SHARED=0x01, MAP_FIXED=0x10,
  MAP_ANONYMOUS=0x20 (local consts).
- Anonymous branch @1408. File-backed device check @1442-1487. INSERT shared
  tmpfs/memfd branch after device check (~@1488, before "Normal file-backed
  mmap follows"). Existing eager-copy file path @1489+ unchanged (f2fs etc).
- vfs::vfs_get_node_kind(pid,fd)->Option<VnodeKind> already used @1442.
- with_current_address_space_mut(|as_| ...) -> Option<T>.
- sys_memfd_create @6015: FIX O_WRONLY(0x041)→O_RDWR(0x042) @6033; call
  vfs::mark_memfd(pid, fd) after open.

## New vfs public API to add (called from kernel)
- pub fn vmo_acquire_frames(pid, fd, off, len) -> Option<Vec<usize>> (pins).
- pub fn vmo_release_frames(frames: &[usize]) (unref each; for kernel None-AS case).
- pub fn mark_memfd(pid, fd) (create empty is_memfd VMO on owner slot).

## Lock order (NON-NEGOTIABLE)
Pin frames under TMP_VMOS+TMP_FILES FIRST (inc inside lock = pin-before-publish),
THEN with_current_address_space_mut (AS busy) to map. Never nest TMP_FILES inside
AS-busy. Never touch RUN_QUEUE. No user-mem deref under any spinlock.

## Stale-data audit RESULT
Only TmpFileEntry.data regular-file readers: read@2358, write@2452, ftruncate@2913
(orig lines) — all gated. Others are RAMFS static / symlink body / gen_proc init /
readlink — none VMO-eligible. entry.len mirrored → fstat/lseek/poll unchanged. DONE.

## EDITS APPLIED (all done)
- [x] mm/vmm.rs: map_shared_frames added after map_lazy (owns frame rollback on fail).
- [x] vfs: TmpVmo struct + TMP_VMOS + consts (F_ADD_SEALS/F_GET_SEALS/F_SEAL_SHRINK)
      + helpers: vmo_alloc_zeroed_frame, vmo_copy_out/in, vmo_zero_range,
      vmo_free_slot, tmpfile_owner_of, mark_memfd(pub), vmo_acquire_frames(pub),
      vmo_release_frames(pub). Inserted after static TMP_FILES.
- [x] vfs tmp_drop_name: vmo_free_slot(owner) at both owner-free sites (alias site
      skipped — alias never owns VMO).
- [x] vfs tmp_release_ephemeral: vmo_free_slot(idx) before empty().
- [x] vfs handle_read TmpFile arm: VMO-gated copy via vmo_copy_out; else inline.
- [x] vfs handle_write TmpFile arm: VMO branch (grow frames, vmo_copy_in, no 32K
      cap, mirror entry.len); else inline.
- [x] vfs handle_ftruncate TmpFile arm: VMO branch (F_SEAL_SHRINK EPERM, grow/
      shrink frames via unref_or_free, mirror entry.len); else inline.
- [x] vfs handle_fcntl: F_ADD_SEALS/F_GET_SEALS arms before catch-all.
- [x] kernel sys_memfd_create: 0x041→0x042 (O_RDWR) + vfs::mark_memfd(pid,fd).
- [x] kernel sys_mmap: MAP_SHARED+TmpFile branch after device check, before eager
      file path. Some(true)→virt; Some(false)→-12 (frames freed by map_shared_frames);
      None→vmo_release_frames + -12.

## TODO
- [ ] BUILD release both arches (background)
- [ ] new tests (extend scmtest): double-mmap alias, read<->mmap coherence both
      dirs, >32768 memfd, fork+visibility, partial munmap, close-while-mapped,
      ftruncate grow/shrink live, teardown-loop ~100 iters
- [ ] scmtest 4/4 both arches, baselines (vfstest/polltest/forktest/memtest/
      sigtest/waittest/boot-to-login)
- [ ] commit (mm,vfs core; separate commit for tests OK)

## BUILD 1 + aarch64 scmtest RESULT (core validated)
- build-all.sh EXIT 0, both arches, only pre-existing warnings. /tmp/k1_build1.log
- HARNESS GOTCHA: driver.py cmd breaks early on "> " — scmtest "-> " diagnostics
  trip it AND socket disconnect drops later output. USE persistent reader:
  /Users/forain/.claude-forain/jobs/afde2e74/tmp/scmrun.py "<cmd>" <dur>
- aarch64 scmtest: fd_pass PASS, cmsg_flags PASS, shared_memfd_pixels PASS,
  seals PASS = 4/4. Child sees pattern A, parent sees child pattern B (true
  cross-proc VMO alias). Seals: ADD=0, GET=0x2, shrink -1/EPERM.
- x86_64 scmtest: NOT YET RUN.
- NEXT: add new tests to scmtest, rebuild, run full suite both arches.

## BUILD 2 + aarch64 FULL RESULT
- build-all.sh EXIT 0 (new scmtest compiled).
- aarch64 scmtest: 12/12 PASS (4 orig + 8 new). teardown_loop completed 150 iters.
- aarch64 vfstest: 34 PASS, 0 FAIL, "vfstest done" (tmpfs inline path unaffected).
- REMAINING aarch64 baselines: polltest, forktest, memtest, sigtest, waittest.
- x86_64: not yet run.

## aarch64 BASELINES: ALL GREEN
- polltest done (all PASS), forktest done (all PASS), sigtest done (all PASS),
  waittest PASS (wait_on_process_group first try), memtest done (all PASS incl.
  map_shared_fork_visibility + buddy_survives_churn), boot-to-login OK.
- aarch64 COMPLETE. Now x86_64.

## x86_64: ALL GREEN — BOTH ARCHES COMPLETE
- x86_64 scmtest 12/12 (teardown 150 iters), vfstest 34 PASS, polltest/forktest/
  sigtest/waittest/memtest all done+PASS, boot-to-login OK.
- ACCEPTANCE MET on both arches. Ready to commit.

## COMMITTED — DONE
- 9da6aa4 mm, vfs: share tmpfs/memfd pages across processes via frame-backed promotion
- 337cf92 userland: extend scmtest with shared-VMO mmap and seal-lifecycle tests
- Tree clean except pre-existing untracked wayland_cosmic_plan.md. ACCEPTANCE MET.
