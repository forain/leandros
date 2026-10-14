# lane/vmreuse2 — 2026-10-19

Bug list items L1.1 and L1.2 (artifacts/notes/openbugs-2026-10-19.md).

## L1.1 — anon mmap VA never reused, munmap never freed page tables

Cherry-picked lane/vmreuse e97bd8aa onto main and reconciled it with the
placement work that landed after it (fccbcbd8 free-range placement, 92986f2d,
f36d4fa5 MAP_FIXED_NOREPLACE, the /dev/zero `mmap_anon` split, shmfork's
shared-anonymous `mremap` move):

- Main's structure is kept: every non-fixed path calls `mmap_place` inside the
  address-space hold that installs the mapping. Its body is now the first-fit
  search from e97bd8aa: a hint is used when `AddressSpace::range_is_free`,
  otherwise (and with no hint) `AddressSpace::find_free_range` over the
  process's own VMAs in `[MMAP_BASE, MMAP_TOP)`, skipping the AArch64
  UART/GIC identity pages and 256 MiB above the brk break.
- `MMAP_BUMP` (the system-wide counter that only grew) is gone, including its
  last user in `sys_mremap`'s shared-move path. `MmapCand::Bump` became `Any`.
- `unmap_range` unlinks page-table nodes the range leaves empty (both arches,
  user half only) and frees them after the TLB flush covering their VAs.
- Futex: a word in a private mapping is keyed per process without
  FUTEX_PRIVATE_FLAG (needed now that identical layouts recur across processes).
- memtest: `mmap_reuses_freed_va`, `munmap_frees_page_tables`.

## L1.2 — `with_task_address_space` used a root with no reference held

`sched::with_task_address_space` (evdev and DRM user copies) read the page-table
root's physical address under RUN_QUEUE, dropped the lock, then loaded that
root into CR3/TTBR0. A group exiting and being reaped on another CPU in that
window freed the tables under it. It now clones the `Arc<AddressSpace>` under
RUN_QUEUE and drops it only after the previous root is restored. The busy flag
is deliberately not taken: the closure is a raw user copy that may fault, and
the fault path takes busy. Concurrent munmap/prune from a sibling thread is
covered by the TLB shootdown (x86 publishes the root for this CPU before the
CR3 write; AArch64 TLBI is broadcast).

## Evidence

aarch64 (HVF) regress, run in parallel with other lanes' QEMUs: everything
green except three host wake-count tests (evherdtest, listentagtest,
nettag2test: 1-2 stray wakes in ~100k round trips). Re-run alone under the
qemu lock with no other QEMU on the host: all three PASS, so the failures came
from host load.

memtest:
- `mmap_reuses_freed_va`: 5000 cycles, span 17 MiB, hiwater growth 0,
  MemFree drift 0 KiB, 4999/5000 same-address reuse, same_again=1: PASS
- `munmap_frees_page_tables`: 40 rounds x 64 regions 1 GiB apart, lost 0 KiB: PASS

## Open

- `with_task_address_space` does not prefault the target range: a
  not-yet-present lazy page faults with IRQs masked under a foreign root
  (pre-existing; how that fault resolves is unverified).
- `docs/kernel/memory.md` updated for the new placement.
- x86_64 not run in the lane (per protocol); the x86 prune path
  (`arch_prune_user_tables`) needs the both-arch batch run on main.
