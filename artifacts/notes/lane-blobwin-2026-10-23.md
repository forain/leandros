# lane/blobwin 2026-10-23 — host-visible blob window span outlives its mappings (L1.14 remainder)

FIXED (by construction + host unit tests; in-guest guard written, skips on the Mac).

## Bug
A HOST3D blob's window offset (virtio-gpu shared-memory BAR window, `hostvis_alloc`) was freed and the
host resource UNMAP_BLOB/UNREF'd on the blob's last unref, while a user `map_device` VMA could still map
it. First-fit then placed the next blob at the same token: the stale mapping read/wrote the new blob.

## Fix
- `drivers/src/hostvis_core.rs` (new, pure): span table; a span is held by its blob and by `maps`
  (live user VMAs). Owner gone while mapped -> zombie (keeps offset, refuses new mmaps, host resource
  kept); last unmap -> ready; `hostvis_reap` does UNMAP_BLOB+UNREF with no lock held, then frees.
- mm: `set_device_vma_hooks(retain, release)`; release on munmap of a device VMA (after the TLB flush)
  and on address-space teardown; retain on fork aliasing (cow.rs).
- `sys_mmap`: `mmap_pin_window(token, len)` takes the VMA's reference (EINVAL for a zombie/unknown
  span or a length past the span end); released if the mapping isn't installed.
- `blob_unref` defers host unmap/unref for a still-mapped span; reaping runs at blob_unref, before each
  window allocation, and in GETPARAM HOSTVIS_SPANS.

## Evidence (aarch64, HVF)
- Host unit tests drivers/uapi-tests: 3 new hostvis tests pass (23/23).
- venustest phase 4c2 (BLOBWIN_*): no host-visible window on the Mac (Venus unsupported) -> prints SKIP.
  Red/green NOT observed in-guest. Run `/bin/venustest` under `--venus` on the x86_64 box to see it.
- `--virgl --suite drm` + `drmsmoke --maplife`: all PASS, 0 [PAN]. regress: PASS, 0 [PAN].

## Open
- In-guest red/green needs a Venus host (x86_64/KVM box). x86_64 not run (per-merge-batch rule).
- DRM mmap token validation is still global, not per-open (pre-existing).
