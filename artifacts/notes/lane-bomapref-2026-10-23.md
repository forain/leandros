# lane/bomapref 2026-10-23 — DRM mmap holds the BO's pages (L1.14; L11.9 by reasoning)

FIXED.

## Bug
A device mmap of a guest-RAM BO was a `map_device` VMA that owned nothing. GEM_CLOSE (or the
open's handle sweep) freed the frames while still mapped; the next allocation got them.

## Guard (drmsmoke `--maplife`, also in the main suite)
Map BO, write pattern, GEM_CLOSE, create 6 same-size BOs and scribble 0xEE over them, check:
no new BO got the old token, old mapping still reads the pattern, partial munmap keeps the rest.
- main 3927d2b9 (default GPU): `token reused by new BOs=1`, `mismatched bytes after close=262144`,
  MAPLIFE_SURVIVES_GEM_CLOSE FAIL, MAPLIFE_PARTIAL_MUNMAP FAIL (scatter case skipped: no virgl).
- fixed, `--virgl`: dumb + scattered cases all PASS. Scatter red not run separately (same map_device
  path as the dumb case on main).

## Fix
- `sys_mmap` (kernel/src/syscall.rs): DynamicDevice tokens ask `drm_device_interface::mmap_pin_frames`,
  which pins (+1 pageref) every mapped 4 KiB frame under the BO map's lock (DUMB_SG, DUMB_BUFFERS,
  BLOB_OBJS — covers dumb, scattered virgl, guest blobs, v3d); installed with `map_shared_frames`, whose
  VMA owns the pins (munmap incl. partial, fork, exit already release page by page). Range past the
  BO's end → EINVAL. Unknown tokens (fw fb, host-visible window, apertures) keep `map_device`.
- BO teardown frees through `mm::pageref::free_block_pinned`: whole block if no page is held,
  else drop the owner ref per page; the last holder frees.
- NVIDIA (L11.9 part "mapping can outlive buffer"): same mechanism via `BACKEND_MMAP_PIN` hook under
  MMAP_TOKENS (gem_close removes the token before RM frees); mem.rs `free_run` uses
  `free_block_pinned`. Type-checked for x86_64 only, no hardware test.

## Evidence (aarch64)
- `--virgl`: drmsmoke --maplife / drmsmoke / --force-scatter / --scatter / gpuprobe gl: all RC=0, 0 [PAN].
- regress: 40/42; netctltest wired_admin_up_rebinds + sshsmoke "Host is down" (DHCP lease lost),
  red again alone; a concurrent lane (lkreg) saw netctltest time out at the same time. Not DRM-related
  (the pin path returns None for every non-BO token). 0 [PAN].

## Open
- Host-visible blob window offsets (not RAM) can still be reused by a new blob while mapped.
- x86_64 not run in this lane (per-merge-batch rule).
