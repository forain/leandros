# lane/bar1ref 2026-10-26 — NVIDIA BAR1 / vidmem mmap references (openbugs #96)

PREMISE REFUTED (no BAR1/vidmem user mmap token exists); one adjacent real bug FIXED.

## Premise check: "BAR1/vidmem tokens are mapped via map_device with no reference"
Every way an NVIDIA buffer reaches `sys_mmap` was traced; none names BAR1 or VRAM:
- `uapi::gem_new`: every CPU-mappable request (MAPPABLE, or GART without VRAM) is placed in
  **sysmem** (contiguous RAM). Only unmappable VRAM requests get vidmem, with `phys = 0` and
  **no** `MMAP_TOKENS` entry.
- `uapi::mmap` (the backend's 0x1007 hook) authorizes only a token inside a live sysmem
  buffer the open holds; `mmap_pin` then pins its pages (pageref) and refuses a freed one
  (nvtoken 6fe3d297). Any unpinned RAM token is refused by device_mmap_policy (mmapaudit).
- `Client::bar1_map` has no callers; `gpu_uapi::set_wc_range` (the "aperture" hook) is never
  called; PRIME exports of VRAM are token-only (`prime_export`: pages 0, not mmap-able).
- uapi_kms keeps its framebuffers GPU-side (CE copies); no user token.
So there is no BAR1 span or vidmem allocation behind any user mapping to hold. The
mmapaudit table row "NVIDIA BAR1/vidmem -> map_device" described no live path.
Consequently the requested tests (map -> free -> span not reusable; unmap / exit release;
stale token refused) have nothing to exercise; the stale-token case is already covered by
nvtoken's `authorized_then_freed_*` guards.

## Real bug found next to it (commit e48cc0ba)
A vidmem BO's `map_handle` was `offset_for(0, ..).unwrap_or(0)` = **0**, and so was a sysmem
BO's when the fake-offset window could not mint one. Offset 0 is the DRM **firmware-framebuffer
token**: a card0 master that mmapped such a handle got the live scanout (the GOP surface behind
BAR1, unreferenced) instead of an error, and wrote over the console/scanout.
Fix: `mmap_tokens::map_handle(fake)` returns the fake offset, else `UNMAPPABLE`
(0xFFFF_F000_0000_0000: page aligned, outside both drm_offset fake windows), which
`handle_ioctl_mmap` refuses (EINVAL). GEM_NEW and GEM_INFO use it. Render node behaviour is
unchanged (token 0 was already refused there). Non-NVIDIA paths untouched.

## Guards / evidence
- drivers/uapi-tests `mmap_tokens_tests`: +3 tests. With the old rule (`fake.unwrap_or(0)`)
  `unmappable_buffer_handle_is_not_the_framebuffer_token` FAILS (44/45); fixed 45/45
  (`cargo test --release --target aarch64-apple-darwin`).
- Builds and regress: see the bottom of this note.

## 3070 step to validate (PC, `--nvidia` x86_64 kernel)
As the first card0 opener (DRM master, before the greeter/compositor starts, e.g. from the
serial/SSH shell with greetd stopped): open /dev/dri/card0, DRM_IOCTL_NOUVEAU_GEM_NEW
(`DRM_IOWR(0x40 + 0x40, struct drm_nouveau_gem_new)`) with `info.domain = NOUVEAU_GEM_DOMAIN_VRAM`
(2), size 64 KiB; expect `info.map_handle == 0xfffff00000000000` and
`mmap(NULL, 65536, PROT_READ|PROT_WRITE, MAP_SHARED, fd, map_handle)` == MAP_FAILED (EINVAL),
the GOP console unchanged. Then the same with `domain = GART|MAPPABLE`: map_handle in
[1<<44, 1<<45), mmap succeeds. Old kernel: the VRAM case returned map_handle 0 and mapped the
scanout. Also run nvktest + a COSMIC session: no `[nvuapi]` errors, no new mmap refusals.

## Still open
- If BAR1 user mappings are ever added (nvtoken open item "VRAM dma-buf mmap": RM
  NV01_MEMORY_LOCAL_USER + MapMemory to BAR1), they must not use plain `map_device`: hold the
  RM BAR1 mapping and the vidmem allocation by a map count released on last munmap / exit,
  the virtio-gpu `mmap_pin_window` pattern, and keep the token in `mmap_tokens` so a freed
  buffer's token is refused.
- Token 0 on the PC is the GOP surface behind BAR1, mapped unreferenced for the DRM master.
  It is RM-reserved console memory (preserved only when the GOP FB is at BAR1 base,
  device.rs "NOT at BAR1 base" warning); same exposure as the kernel console, not changed here.

## Builds / regress
- x86_64 release `--nvidia`: clean; x86_64 release: clean; aarch64 release: clean.
- aarch64 regress (runtests bar1ref): boots failed 0/1, overall PASS.
