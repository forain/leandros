# lane/lazymmap: demand-paged MAP_PRIVATE file mmaps (2026-09-25)

Branch `lane/lazymmap` from `origin/integ-wave-0924` (b1ddeaa). Commits:
`3c73435` adds the `[MMAP-BIG]` instrumentation, `83b584c` is the feature, `d0584ed` is the memtest bar.
Machine: Mac only. aarch64 ran on **HVF**; x86_64 ran on **TCG**. Nothing was measured on KVM.

## What changed
- `sys_mmap`: a MAP_PRIVATE mapping of an f2fs file (`VnodeKind::MountedFile`, page-aligned
  offset) becomes a file-backed lazy VMA (`AddressSpace::map_private_file`). No read happens in
  the syscall, and the fd position is not touched.
- New registry `MMAP_FILES` in `kernel/src/syscall.rs`. Caps start at `0x1_0000`. It is keyed by
  (mount port, inode), deduplicated, and refcounted per VMA. The inode is pinned with the new
  `f2fs::pin_inode` / `unpin_inode` (checked by `ino_is_open`), so unlink or rename-over cannot
  reclaim its blocks. It uses no open-file slot. Reads go through `f2fs::pread_ino_by_port`.
- EOF semantics come from `MAP_EOF_SIGBUS` (VMA flag bit 30). A page wholly past the current EOF
  gives SIGBUS/BUS_ADRERR. Delivery goes through `sched::take_fault_sigbus()` into the x86 #PF and
  aarch64 abort handlers. The rest of the last page is zero-filled.
- The fault is split in two: `plan_user_page_fault` and `install_file_fault`.
  `sched::handle_page_fault` pins the file cap, **drops the address-space lock for the file read**,
  then relocks and re-validates (same cap and file offset, page still absent). The kernel
  `prefault_user` uses the same unlocked path. Only the ELF loader and the prefault_range fallback
  still read under the lock.
- The eager path is kept for MAP_SHARED f2fs, tmpfs and non-f2fs files. It now maps the final
  protection directly and no longer does the mprotect fixup pass (on TCG that pass cost 0.5–1.7 s
  per map). It also cleans the aarch64 I-cache for exec mappings.
- `unmap_range` skips `tlb_shootdown_all` when no PTE was present (ld.so's untouched
  whole-library reservation).
- `mremap` grow now prefaults the old range.
- Fork, split and mprotect reuse the existing lazy/CoW machinery. The child retains the file cap.

## Numbers
Per mmap, `[MMAP-BIG]` µs:

| mapping | aarch64 HVF before → after | x86_64 TCG before → after |
|---|---|---|
| comp 16.6/17 MiB | 12,000–17,500 → 0–1 | 75,000–120,000 → 12–26 |
| comp exe 33 MiB | 23,800–28,300 → 3–4 | 137,000–161,000 → 27–30 |
| greeter-login 37 MiB | 41,000 → 14 | 354,000–754,000 → 25 |

A memtest mmap of a 33 MiB file takes 0.14–0.38 ms on aarch64 and 0.1–0.9 ms on x86 TCG.

- `[MMAP-SLOW]` per x86 TCG session: base 115, 105, 113 and 105 → lazy 14, 22, 16 and 13. What is
  left is all `map_ms` of MAP_FIXED or anon maps (lock wait or shootdown). No file copies remain.
  aarch64 HVF shows 0 in both builds.
- Desktop visible after login, x86 TCG, screendump poll: base 258, 228 and 238 s, and one run did
  not come up within 330 s. Lazy: 219, 215, 259, 189 and 246 s. That is roughly 5–10 % and within
  noise. On HVF the eager copies cost only about 50–90 ms per process, so nothing is measurable
  there.
- The KVM premise ("1–1.5 s per process") could not be checked here: HVF copies at about 1 GB/s.

## Tests
- aarch64 HVF: memtest 14/14, forktest (11 PASS), exectest and vfstest all rc=0. The desktop
  (panel, dock, wallpaper) came up 5 times.
- x86_64 TCG: memtest 14/14, forktest, exectest and vfstest rc=0. The desktop came up in 4 of 4
  lazy runs.
- New memtest cases: lazy_content (content, zeroed tail, fd position, private writes, fork, split),
  sigbus_past_eof (child dies with signal 7), no_leak (0 pages lost after the worker exits,
  1496-page file × 32), survives_unlink (block reuse), and map_cost.
- Run memtest with the greeter killed. A greeter that is starting drains free RAM and fails the
  leak bars on TCG.

## Open, not this lane
x86 TCG shows `[WDOG] … cosmic-panel last syscall execve holds ADDRSPACE_BUSY` in **base as well**:
a base+diagnostics run saw execve pre-args take up to 8.8 s and 182 faults of 300 ms or more.
The cause is prefault's CoW unshare, where every copy promotion does 2 all-CPU shootdowns
(forkcow's break-before-make). The shootdown cost is the next target.

Shared files touched: mm/src/vmm.rs, sched/src/lib.rs (fault entry), kernel/src/syscall.rs, servers/f2fs, arch fault handlers.
