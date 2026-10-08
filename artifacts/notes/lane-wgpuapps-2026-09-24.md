# lane/wgpuapps — COSMIC clients on iced's wgpu backend

Branch `lane/wgpuapps`, head `338a65c2` (pushed). Base `a5b5b62`.

## Commits
- `ae3ac7a9` build recipe `ports/cosmic-wgpu/build-wgpu-clients.sh` (feature flags only), gpu-env exports ICED_BACKEND/WGPU_BACKEND per renderer path, mkfs overlay dir (`LEANDROS_COSMIC_BINS_OVERLAY`).
- `47d15d89` panel-embedded applets stay on tiny-skia (cosmic-panel's embedded server has no linux-dmabuf global); `/etc/leandros/iced-backend` override for A/B.
- `82cf4187` Mesa patch: EGL robust-access-unsupported → EGL_BAD_ATTRIBUTE so wgpu-hal retries without robustness (virgl on ANGLE/Metal). `LEANDROS_GPU_STAGE` for mkfs.
- `338a65c2` virgl path keeps `ICED_BACKEND=wgpu,tiny-skia`.

## Built clients (wgpu)
cosmic-greeter, -launcher, -workspaces, -osd, -notifications, -files-applet (x86_64 + aarch64).
Staged into the shared `leandros-artifacts/m6-session-bins/out/` on Mac and desktop; the originals are kept as `*.orig-tinyskia`. To revert, move them back.

## Measurements
x86_64/KVM desktop, Venus/Zink, pcsample host-QEMU CPU:
- session idle: tiny-skia 5.2–7.6 %, wgpu 5.4 %
- workspaces overview open: tiny-skia 186–192 % (comp 80 %, workspaces 14 %), wgpu 180–200 % (comp 68–70 %, workspaces 12–14 %). Most of the cost is cosmic-comp's thumbnail capture, not client rasterisation. Esc does not close the overview in either build (pre-existing).
- app-library open/close loop: tiny-skia 37.6 %, virgl+wgpu 27.8 %
- 16 min soak: MemFree drift about 200 KiB, QEMU RSS flat
- x86_64 virgl with patched Mesa: every client reports `backend: Gl`, and nothing failed.

aarch64/HVF Mac, virgl on ANGLE/Metal:
- Shared (unpatched) stage: wgpu cannot get a context, so the `wgpu,tiny-skia` fallback renders and the session works.
- Patched stage (scratch build only): wgpu selects `virgl (ANGLE …)`. Across 2 boots, iced clients died with `Io error: Bad file descriptor` (EBADF): the greeter exits and restarts, the launcher daemon exits at start, and cosmic-workspaces exits on toggle. On the same image with tiny-skia forced, 2 boots gave 0 EBADF and 0 exits. **Do not ship the patched aarch64 stage yet.**

## Open
- Root-cause the EBADF: wgpu GLES on aarch64 virgl, possibly a kernel fd bug. vfsmisc has a nearby EBADF item.
- `/usr/lib/libvulkan.so.1` (aarch64 stage) fails to relocate `__stack_chk_guard`. This predates the lane; the shared stage has the identical file.
- The launcher latency probe (openlag) did not detect the launcher popup. There are no launcher latency numbers.
- cosmic-settings and app-library were not rebuilt.
