# lane faultaround (2026-10-29)

Status: FIXED. Regression from mmapB (lazy MAP_SHARED memfd/tmpfs, one fault + one TMP_VMOS lock per page).

## Change
- `FaultPlan::Vmo` now carries an aligned 32-page window (`VMO_FAULT_AROUND`, clamped to the VMA).
- Hook is `vfs::vmo_fault_frames(id, off, out)`: acquires up to the window in ONE TMP_VMOS lock round trip
  (existing pages referenced, holes allocated zeroed, stops at EOF / OOM; first page failing = SIGBUS).
- `install_vmo_fault` revalidates (seq, VMA, vmo, offset) with the AS locked, skips already-mapped pages (drops their ref).
- AS stays unlocked during the vfs call; vmo_retain/release, zombies, eager dmabuf/borrowed path unchanged.

## Numbers (aarch64 HVF, 4 MiB memfd, ns/iter)
| | mmap | touch | munmap | total |
|---|---|---|---|---|
| mmapB (1 page/fault) | 2760 | 1169932 | 43623 | 1.22 ms |
| 16-page window | 2829 | 152112 | 46771 | 202 us |
| 32-page window (kept) | 3059 | 111270 | 46501 | ~161 us (2nd run 172 us) |
| old eager | 65707 | 10353 | 51171 | 127 us |
Untouched: mmap 2.6 us, touch 0.9 us, munmap 2.3 us (unchanged).

## Gates
shmtest (memfd_lazy) + bench, aarch64 default regress green, nmsession aarch64 desktop non-black (99% nonblack px),
x86_64 shmtest + scmtest pass.
