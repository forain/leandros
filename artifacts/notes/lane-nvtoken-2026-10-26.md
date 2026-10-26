# lane/nvtoken 2026-10-26 — NVIDIA mmap range checks (L11.9, software part)

PARTIAL (software parts FIXED; HW parts need the RTX 3070 PC).

## Bug (mmap tokens checked by base only)
- ioctl `uapi::mmap` checked only `MMAP_TOKENS.contains_key(token)` + ownership; never saw the length.
- `mmap_pin` (sys_mmap's pin hook) checked the length but not ownership, with exact-base lookup.
- A GEM_CLOSE on another thread between the two steps made the pin return None ("not ours") ->
  sys_mmap fell back to `map_device(phys, len)`: an UNPINNED mapping of freed RAM, any length.
  If the pages were reused by another client's buffer in between, the pin mapped that buffer.

## Fix (commit 6fe3d297)
- `drivers/nvidia/src/mmap_tokens.rs` (pure, host-tested): live buffers carry a generation; the ioctl
  (under STATE -> MMAP_TOKENS, same order as gem_close) authorizes an address inside an owned buffer;
  the pin claims the authorization and requires `[addr, addr+len)` page-aligned, non-empty, no overflow,
  inside the SAME live buffer. Authorized-but-freed or -reused => EINVAL, never the device fallback.
  Mid-buffer offsets (fake offset + delta, as `drm_offset::resolve` already produces) are now accepted.

## Guards (drivers/uapi-tests, `cargo test --target <host>`)
- 9 mmap_tokens tests; on the OLD semantics 7/9 FAIL (incl. authorized_then_freed_is_refused_not_fallthrough
  and authorized_then_freed_and_reused_is_refused); new: 9/9.
- 5 pageref tests for bomapref 0b341556 (mm/src/pageref.rs verbatim, mock buddy): pinned pages survive
  free_block_pinned, freed once by the last unmap, partial munmap, unmap-before-close, double pin. 37/37 total.

## Evidence
- aarch64 release build + regress: overall PASS, 0 boots failed. x86_64 release and x86_64 `--nvidia` build clean.
- Not run on the 3070 (PC unavailable).

## Still open — needs the real 3070
- mmap HW check: boot `--nvidia` on the PC, run nvktest/vkcube + drmsmoke-style map/GEM_CLOSE/unmap on a
  sysmem BO; expect no `[nvuapi]` errors and EINVAL for an overlong mmap.
- vblank sync / 100 Hz pacing (uapi_kms.rs synthetic tick): needs the head's real vblank interrupt from
  RM/nvkms (NV0073 / disp channel vblank callback) wired to the flip-event queue; verify on the PC with the
  `[nvkms] frames` stats line (vblank events = refresh rate, queue->deliver avg < 1 ms) and a tearing test
  (moving vertical bar, camera or capture card).
- contiguous host-visible alloc (uapi.rs GEM_NEW `contiguous: true`, 865/880): large sysmem BOs fail when
  memory is fragmented; needs scatter support (non-contiguous RM pte_array + per-page mmap pin) proven on HW
  by a long desktop session allocating >= 64 MiB host-visible BOs.
- VRAM dma-buf mmap: VRAM exports are token-only (no BAR1 user mappings); needs a BAR1 mapping via RM
  (NV01_MEMORY_LOCAL_USER + MapMemory to BAR1) and a mmap of a VRAM PRIME fd checked on the PC.
