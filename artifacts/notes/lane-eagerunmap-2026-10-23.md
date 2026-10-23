# lane/eagerunmap 2026-10-23 -- eager-VMA munmap hold (L1.13 remainder)

Verdict: REFUTED as a kernel problem on aarch64/HVF; no chunking added.

The aslock design estimated a 256 MiB eager VMA munmap at ~13 ms in one hold (65k PTEs). Measured with a new
munmap hold counter (`sched::note_as_hold_ns`, `/proc/sysstat` `as_hold_max_us`, read-and-reset):
- unfixed kernel, racetest `munmap_eager_256m` (3x 256 MiB unaligned-offset file mmap, i.e. the eager path,
  then munmap, with a toucher, an smaps reader and an AS-op prober running): `munmap_hold_max_us=187`.
- Tried design step 5 (`strip_range` calls `make_lazy` on eager VMAs, then chunks): `munmap_hold_max_us=768`,
  WORSE, because `make_lazy` builds a 65k-entry per-page vector in one hold and each page is then freed
  individually. Reverted.
Reason: an eager VMA is one buddy block; unmap clears the PTEs and frees the block once. There is no per-page
frame free, so the hold is ~0.2 ms. The 13 ms figure assumed per-page work and was never measured.

Delivered: the racetest case (guard, bound 5 ms) and the hold counter, so a laptop run can confirm with real
numbers (`cat /proc/sysstat | grep as_hold_max_us` after a big munmap).
Open: laptop confirmation (expected the same order of magnitude).
