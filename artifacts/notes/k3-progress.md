# K3 Dynamic Linking — Progress

Started 2026-07-21. Owner: deep-reasoner (Opus).

## Plan (3 commits)
1. vmm split_at + unmap_range middle-split + mprotect sub-range split
2. elf loader bias + PT_INTERP extraction + syscall execve interp load + auxv expansion
3. userland/scripts test packing (ld-musl, libc.so, test binaries into f2fs image)

## Status
- [ ] Read design + current code (IN PROGRESS)
- [ ] vmm changes
- [ ] elf/syscall changes
- [ ] test packing
- [ ] test ladder both arches
- [ ] regression both arches

## Decisions / deviations
- VMM: unify unmap_range + mprotect via "split_at(virt); split_at(end); then operate on
  wholly-contained VMAs". Eliminates front/back/middle special cases (fixes the eager
  middle-punch buddy-order corruption that region.end=clip_s left behind).
- split_at converts an EAGER VMA to per-page lazy tracking in place before splitting
  (identical to the transform clone_as already does + trusts). Avoids splitting a single
  buddy block; each half frees its pages order-0 and the block reconstructs by coalescing.
  Device maps (file_cap==usize::MAX) refuse to split.
- Loader: caller passes `bias`; peeks e_type at buf[16..18]. AT_ENTRY always = biased entry.
  entry jumped-to = interp_entry if PT_INTERP else main entry. heap_start restored to MAIN's
  after loading interp (interp load() would otherwise clobber it to after INTERP_BASE).
- auxv: append AT_ENTRY/AT_BASE/AT_SECURE=0/AT_HWCAP=0/AT_EXECFN before AT_NULL → 18 pairs.
  Emit uniformly for ET_EXEC too (standard Linux tags, musl/relibc ignore extras).
- Packing: place ld-musl-<arch>.so.1 in /lib and libc.so in /usr/lib as the SAME host path
  (hardlink dedup) = real file, no symlink resolution needed. Corpus libc.so is ~4.3-4.8MB.
- Milestone 1 (separate static-PIE): using hello-dyn as first bring-up (design calls it the
  "cleanest first bring-up target"); hello-dyn exercises MAIN_DYN_BASE bias anyway. Will build
  a static-PIE only if hello-dyn fails and I need to isolate bias-vs-interp.

## Current build/test state
- All code written (vmm split_at/unmap/mprotect; elf loader bias+interp; syscall execve
  interp-load + auxv; init.rs bias=0; open_exec_header interp-string; packing).
- Both arches BUILD clean (exit 0). Images packed with ld-musl + libc.so + hello-dyn +
  hello-dyn-rs + dlopen-host + plugin.so, /usr/lib added.
- NOT committed yet (commit after tests pass both arches).
- AARCH64 ALL GREEN (2026-07-22):
  - Ladder: hello-dyn OK, hello-dyn-rs OK, dlopen-host OK (dlopen/dlsym/call result=0x4d41474b).
  - Regressions: scmtest 19/19, epolltest 8/8, forktest, memtest, sigtest, polltest,
    idletest IDLE_CPU_US=0, vfstest, waittest (wait_on_process_group PASS 1st try), boot-login.
- X86_64 ALL GREEN (2026-07-22): ladder hello-dyn/hello-dyn-rs/dlopen-host all OK;
  scmtest 19/19, epolltest 8/8, forktest, memtest (buddy_survives_churn + map_shared_fork
  OK), sigtest 6/6, polltest, idletest IDLE_CPU_US=0, waittest (wait_on_process_group PASS).
  vfstest 34/34 0-FAIL on a FRESH regenerated image (the one xattr_list_f2fs FAIL seen
  earlier was the documented dirty-image residue from a 2nd back-to-back vfstest run).

## DONE — committed to main
- 522d891 mm: split VMAs at range boundaries for unmap and mprotect
- fbac196 elf, exec: load ET_DYN at a bias and support PT_INTERP
- 894b72f mkfs: pack the musl dynamic linker and K3 test ladder
All clean (author Leandro Forain, no AI/co-author mentions). Both arches fully green.

## Follow-up gaps (documented, not blockers)
- Interpreter (ld-musl, ~4.8MB) is loaded EAGER (read_file_from_vfs + elf::load) on every
  dynamic exec. Fine for tests; could be demand-paged (load_lazy) later if dynamic exec
  frequency matters -- needs a file_cap for the interp.
- Eager file-backed mmap slack: converting an eager VMA to per-page lazy in split_at frees
  only the mapped pages order-0; buddy slack (2^order - pages) of a non-pow2 eager block is
  leaked. Pre-existing (clone_as already does this); tiny; only on split of a non-pow2 eager
  VMA. Not introduced by K3.
- No new ld.so syscall gaps surfaced: openat/pread64/mmap(MAP_FIXED)/mprotect/brk all
  sufficient for the corpus (BIND_NOW, so no lazy-PLT needed).
