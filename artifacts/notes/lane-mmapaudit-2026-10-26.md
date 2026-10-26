# lane/mmapaudit 2026-10-26 — device mmap audit (openbugs #95, P0 security)

FIXED (hardening). The premise "audio / framebuffer / shm device servers hand out
unpinned RAM tokens" is REFUTED for today's tree: the DRM server is the only
token producer, and drmrace (2ce3e6bc) already covers it. The DRM-only gate in
the policy was generalised so any device server that ever answers the mmap
ioctl with RAM gets the same refusal.

## Audit: every sys_mmap path that reaches a physical mapping
`map_device` has exactly one caller in the tree (kernel/src/syscall.rs,
sys_mmap_inner, DynamicDevice token branch). The token comes from the
VFS_IOCTL cmd 0x1007 sent to the fd's device server; a token is taken only on
reply tag 0 with a non-errno slot 0. Device registration is kernel-only
(`register_device*` takes `&'static str`, no syscall reaches it).

| fd / server | answer to 0x1007 | phys can be freed RAM? | when | status |
|---|---|---|---|---|
| /dev/dri/card0, renderD128 (servers/drm), dumb / guest blob / v3d / SG virgl | RAM token | yes | GEM_CLOSE, open close | pinned (map_shared_frames, L1.14); unpinned RAM refused (drmrace) |
| same, i915 / NVIDIA sysmem (backend `mmap_pin`) | RAM token | yes | GEM_CLOSE | pinned (BACKEND_MMAP_PIN) |
| same, virtio-gpu host-visible blob | BAR window | no (not RAM) | blob unmap | window span held by map count |
| same, token 0 firmware framebuffer, NVIDIA BAR1/vidmem, apertures | MMIO/BAR | no | - | map_device (kept) |
| /dev/pipewire (audio, HDA / virtio-snd behind it) | ENOTTY (-25) | n/a: PCM is write()/ioctl, no mmap token | - | falls to file path; no hazard |
| /dev/input/event*, hotplug (evdev) | ENOTTY | n/a | - | no hazard |
| netctl | tag != 0 (ENOTTY) | n/a | - | no hazard |
| /dev/fb0 | VFS RamFile, not a DynamicDevice | n/a (private copy) | - | no hazard |
| /dev/zero, tmpfs/memfd, files | not map_device (VMO frames pinned / private copy) | - | - | no hazard |

## Fix (commit 7ab8f6b7)
- `drivers/src/drm_mmap_policy.rs` -> `drivers/src/device_mmap_policy.rs`;
  `decide(pin, win, ram)` drops the `drm_node` gate: any device token that
  neither pinned nor is a held window span is refused if `mm::buddy::is_ram`.
  MMIO / BAR / firmware framebuffer keep `map_device` (not in the RAM table).
- sys_mmap computes `ram` for every device token; log line
  "[MMAP] device token names RAM but no live pinned buffer".

## Guard / evidence
- drivers/uapi-tests `device_mmap_policy_tests::non_drm_device_ram_token_is_refused`:
  FAILS on the old rule (4/5, `decide(false, NotGuestRam, NotWindow, true)` was
  MapDevice); fixed 42/42 total (`cargo test --release --target aarch64-apple-darwin`).
- aarch64 release: regress PASS (0 boot failures); drm suite --gpu virgl:
  drmsmoke + gpuprobe gl PASS; audio-session-test.py (PipeWire in COSMIC):
  tone1/tone2 analysed, underruns=0, pf/segv/panic/wdog 0. No new
  "device token names RAM" refusal in any log.
- x86_64 release builds clean.

## Open
- NVIDIA BAR1 / aperture tokens are mapped unreferenced (not RAM): a freed
  vidmem BO's BAR1 range reassigned to another client's BO would alias it under
  a live mapping (cross-client leak, not kernel RAM). Not addressed here.
- Guard is a host decision test; no in-guest reproducer (no non-DRM server
  produces tokens today).
