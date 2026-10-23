# lane/mmrest 2026-10-20

## L1.13 eager fill-before-publish, chunked munmap/mincore
- sys_mmap eager file path: alloc_prefill -> unlocked VFS read -> one locked publish (placement + map_prefilled).
  A sibling munmap during the read can no longer free frames under the read.
- munmap goes through `unmap_chunked` (8 MiB per AS hold, `strip_range` in mm/src/vmm.rs); mremap shrink tail too.
- mincore: 256 pages per hold, ENOMEM at the first hole; bytes before the hole (including the partial chunk)
  are written, as on Linux.
- Test: memtest `mincore_hole_huge_munmap` (also in `memtest mmlock`). Two test-side fixes found while
  running it: the 20 MiB cut overlapped the sentinel pages (moved after the page-3000 hole), and the
  vec must be touched first (the kernel copier does not fault lazy user pages in; same as before).
- Result: aarch64 regress PASS, `memtest mmlock` PASS (lost_kib=0 bad=0).
- Open: fork clone_as snapshot, smaps count, eager-VMA unmap, sleepable AS lock.

## L1.10 virgl scatter backing
- RESOURCE_CREATE tries one contiguous block first; on failure it allocates 2 MiB chunks plus single pages
  (max 4096 entries) and ATTACH_BACKINGs them with `attach_backing_sg` (no fixed entry limit: the command
  head is copied into a buddy block sized to it).
- `DumbBuf.sg` + `DUMB_SG` (keyed by obj, keeps DumbBuf Copy) hold the frame list; `dumb_free_backing` frees
  either shape. A scattered buffer has phys = 0, so VIRTGPU_MAP, MAP_DUMB, ADDFB/ADDFB2 backing,
  PRIME export and `dumb_buffer_phys_order` refuse it instead of using a bogus contiguous range.
- Not covered: CPU mmap of a scattered resource (would need map_shared_frames over the frame list).
