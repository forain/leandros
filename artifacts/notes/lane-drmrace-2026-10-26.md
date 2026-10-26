# lane/drmrace 2026-10-26 — DRM mmap never maps freed RAM (openbugs #94, P0)

FIXED (QEMU-verified paths; i915 part needs a laptop boot).

## Confirmed (code reading)
- kernel/src/syscall.rs sys_mmap: the DRM 0x1007 ioctl (`handle_ioctl_mmap`, ownership,
  no length) returns the token; later `mmap_pin_frames` (length, no owner) pins it. `None`
  from the pin -> `as_.map_device(phys, len)`, unreferenced. A GEM_CLOSE on another
  thread between the two (DUMB_BUFFERS / DUMB_SG / BLOB_OBJS) => mapping of freed RAM,
  any length. Same race as nvtoken found for NVIDIA. Host-visible blobs were already
  safe (`mmap_pin_window` returns Some(false) for a dead span).
- WORSE, found here: i915 objects are guest RAM with NO pin at all and were freed with
  `buddy::free`. Single thread, no race: mmap a BO, GEM_CLOSE, keep the mapping =>
  user mapping of freed frames. (Laptop iris path.)

## Fix (commit 40c6a5d2)
- `drivers/src/drm_mmap_policy.rs` (pure, host-tested): on a DRM node, a token that
  neither pinned nor is a window span is refused if it is RAM.
- `mm::buddy::is_ram`: table of the boot map's Available regions (coalesced). MMIO /
  BARs / firmware framebuffer are not in it, so they keep `map_device`.
- `mmap_pin_frames(token, len, open_id)`: re-checks the open owns the buffer (dumb under
  the pin's lock; SG/blob pinned first, verified, unpinned on refusal) — closes the
  freed-and-reused-by-another-client ABA. `BACKEND_MMAP_PIN` gets `open_id`.
- i915: `mmap_pin` registered (STATE: live, owned, in range) and `reap` frees with
  `free_block_pinned`.

## Guards / evidence
- drivers/uapi-tests `drm_mmap_policy_tests` (5): on the old rule
  `freed_between_ioctl_and_pin_is_refused` FAILS (4/5); fixed 42/42 total.
- aarch64 release: regress PASS (nettag2test icmp_unbound flake, re-run PASS);
  drm suite --virgl: drmsmoke + gpuprobe gl PASS; ffsession --nofirefox: greeter,
  desktop, cosmic-term up; zero "freed before pin" refusals in any log.
- x86_64 release and x86_64 --nvidia builds clean.

## Open
- i915 pin/pinned-free unproven on hardware: laptop boot + iris/COSMIC session.
- No in-guest race reproducer (window is microseconds); host guard covers the decision.
- Other device servers (audio etc.) still fall back to map_device; not audited here.
