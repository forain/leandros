# Lane gpufencefb — 2026-10-01

Branch `lane/gpufencefb`, base `origin/main` aff49e1, head **b1d9975** (pushed, one commit). Worktrees removed (Mac `~/code/leandros-gpufencefb`, desktop `leandros-siblings/leandros-gpufencefb`).
Harness (session scratchpad `gf/`, copies on desktop `leandros-siblings/gfsess.py`, `gfsmoke.py`, outputs `leandros-siblings/gf-out/`):
- `gfsmoke.py <arch> <gpu> <tag>`: boot, stop the greeter, run drmsmoke and `drmsmoke --fence-loss-demo`, diff QEMU stderr.
- `gfsess.py <arch> <tag> <gpu>`: greeter → session; cosmic-settings ×10 started from serial and closed with Super+Q; 8× Super+T, then 8× Super+Q; VT switch (VT=1); `drmsmoke --fb-stats` at each phase; serial and host error counts.

## 1. Out-fence EXECBUFFER: mechanism CONFIRMED; behaviour was already Linux-atomic

`sys_ioctl` already reserves the out-fence eventfd **before** it submits. On EMFILE/ENFILE it returns with nothing submitted and `fence_fd = -1`, which is upstream `virtio_gpu_execbuffer_ioctl` semantics, so no ordering change was needed. The commands are lost anyway, because Mesa's virgl winsys (`virgl_drm_winsys_submit_cmd`) logs "expect bad rendering" and discards the command buffer on **any** EXECBUFFER error. Real Linux behaves the same way.

**Forced and verified end to end** (`drmsmoke --fence-loss-demo`):
- Setup: fill the process fd table, then submit CREATE_OBJECT(DSA, handle 4660); then free one fd and submit BIND(4660).
- Result: the first call returned EMFILE, `fence_fd = -1` and LAST_FENCE was unchanged. The host then logged exactly one `vrend_object_bind_dsa_to_sub_context: context error ... Illegal handle 4660`.
- The control open ran the same stream pair with free fds and produced 0 host errors.
- Seen on both the Mac aarch64/HVF gles31 virgl host and the desktop x86_64/KVM virgl host.

This is the "Illegal handle" class that macmagenta inferred. On a fd-table ceiling, the client's context dies under Linux semantics too; the real defence is fd headroom (already raised to 512).

Change: one serial line on that path, rate-limited (first 8, then every power of two):
`[DRM] EXECBUFFER refused: out-fence fd unavailable, errno=0x18 pid=… (nothing submitted; total=…)`

## 2. Framebuffer lifetime: FIXED
- `DrmFramebuffer` gains `bo_obj` and `owner`. ADDFB and ADDFB2 (blob and dumb/virgl-3D paths) take one BO reference (`fb_bo_ref`) before any host resource is created.
- RMFB:
  - is refused for another open's framebuffer (ENOENT-class, as upstream), and an unknown id is now an error;
  - unlinks the framebuffer from the planes, LAST_PRIMARY_FB and LAST_CURSOR_FB;
  - if the host scanout is on the framebuffer's resource, sends `SET_SCANOUT res 0` (new `VirtioGpuDevice::disable_scanout`, which also resets `current_resource_id` so the next present re-points);
  - logs `[DRM] fb N removed while scanned out`;
  - drops the reference after the device lock is released.
- `drm_release_open` calls `fb_release_open` **first**, before the handle sweeps. This mirrors `drm_fb_release`.
- New private GETPARAMs: `0x1000_0006` DUMB_OBJS and `0x1000_0007` FB_STATS (swept<<16 | scanout-disables).

## Evidence
drmsmoke (new cases: FB_HOLDS_BO_REF, FB_RMFB_DROPS_BO_REF, FB_RMFB_TWICE_FAILS, FB_RMFB_OTHER_OPEN_REFUSED, FB_SWEPT_ON_CLOSE, FB_RMFB_SCANOUT_DISABLES_PLANE, EXECBUF_OUT_FENCE_SIGNALS / _EMFILE_ATOMIC / _AFTER_FREE):

| run | result |
|---|---|
| aarch64 HVF virgl, greeter stopped | failed=0, skipped=0 |
| x86_64 KVM virgl | failed=0, skipped=0 |
| x86_64 KVM Venus | failed=0, skipped=0 |
| x86_64 KVM no-gpu | failed=0, skipped=3 (the EXECBUF cases need 3D) |

polltest, all PASS:

| arch | poll overshoot |
|---|---|
| aarch64 HVF | p50 421 µs |
| x86_64 KVM | p50 49 µs, max 109 µs |

COSMIC sessions (gfsess):

| run | settings opened | cosmic-term open / after close | dumb or blob objs: terms open → closed | greeter exit | host vrend / 0x12xx errors | refused / unknown_bo / underflow / panic | greetd errors | VT2 → VT1 |
|---|---|---|---|---|---|---|---|---|
| aarch64 HVF virgl (a64-s2) | 10/10 | 8 / 0 | 585 → 80 | 3 fbs swept, 1 scanout disable, session displayed | 0 / 0 | 0 / 0 / 0 / 0 | 0 | session back, new terminal drawn |
| x86_64 KVM virgl | 10/10 | 8 / 0 | 598 → 101 | 2 swept, 1 disable | 0 / 0 | 0 / 0 / 0 / 0 | 0 | ok |
| x86_64 KVM Venus | 10/10 | 8 / 0 | blob 204 → 100 | 3 swept, 1 disable | 0 / 0 | 0 / 0 / 0 / 0 | 0 | ok |

Earlier aarch64 run (a64-s1, the launcher-typed settings did not open): same 0 errors; objects returned to baseline 608 → 101.

Notes:
- After the greeter was killed (scanout disabled by the sweep), the text console was reclaimed and showed correctly (screenshot).
- VT2 screenshots are uniformly black. VT2 has no getty; this is not from this change (FB_STATS showed no disable during the switch).

## Shared files touched
- `drivers/src/drm_device_interface.rs`, `drivers/src/drm/framebuffer.rs`, `drivers/src/virtio_gpu.rs` (DRM owner lanes)
- `kernel/src/syscall.rs` (EXECBUFFER block only)
- `userland/drmsmoke/src/main.rs`

## Open
- The fd-limit failure is still fatal to a virgl client's context, as on Linux. If it should be survivable, the option is "submit, then fail the fence" (non-Linux): Mesa would get no fence on a flush, which needs a review of Mesa's fence-NULL paths. Not done.
- RMFB of an unknown fb id now returns an error (Linux ENOENT; our server maps it to -1/EPERM). No client was seen relying on the old Ok.
