# lane/aslock 2026-10-23 -- long IRQs-off AS holds (L1.13 remainder)

Status: FIXED for fork, smaps/maps and MAP_FIXED/MREMAP_FIXED destinations. Not done: step 5
(`strip_range` for eager VMAs; optional) and the mremap move itself (one hold, by design).

## Changes
- `clone_as` is resumable (`clone_as_begin/step/finish`, `CloneCursor`): 2048 pages per hold, COW_LOCK per
  step, dst consistent after every step. `sched/src/clone.rs` drops the AS lock + `irq_window()` between
  steps and restarts if `AddressSpace::lock_epoch` moved (bumped on every `busy` release); the third
  attempt is a single hold. `[FORK]` line gains `holds=`.
- `vma_census_from` + `CensusCursor`: smaps chunked at 4096 pages with an address cursor; `maps` no
  longer counts pages at all. `PageVec::present_from` skips whole chunks.
- MAP_FIXED (5 sites) and MREMAP_FIXED destinations are emptied by `unmap_chunked` before the hold.
  `[MREMAP] pages= hold_us=` logged when the move hold exceeds 5 ms.

## Evidence (aarch64, HVF, host load avg ~40 so absolute numbers are noisy)
memtest mmlock `fork_smaps_irq_latency` (1 GiB resident, 8 spinners, min late_tick over 3 rounds, bar 50 ms):
- unfixed kernel: late_tick_max_ms=60 and 60 (two runs) -> FAIL (discriminates)
- fixed: late_tick_max_ms=0 (3 runs), PASS. `[FORK] shared_pages=255676 ... holds=125`.
racetest `munmap_during_eager_read` (32 MiB eager, 200 iterations): bad=0, lost_kib<=72 both kernels;
as_op_max_ms 32 unfixed vs 1 fixed. It guards correctness; it passed on the unfixed kernel for the
latency bar (as_op_max 32 ms < 250).
Gates: memtest (full), mmlock, forktest x4, racetest x4, shmtest, exectest, regress suite green
(pollnarrowtest red once under load, green on rerun).

## Open
- Real-laptop confirmation: a `[FORK]` line with holds>1 and no late ticks.
- Step 5 (eager VMAs in `strip_range`) not done.
