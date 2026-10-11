# Mesa and the GL/Vulkan userland stack

LeandrOS ships an unmodified-in-behaviour Mesa 25.3.6 — one gallium megadriver with `zink`, `virgl` and `softpipe` (plus `v3d` on aarch64), the Venus and NVK Vulkan ICDs, and the Khronos Vulkan loader — linked against dynamic musl and built in an Alpine container of the target architecture. This page covers how the stack is built, why it is built that way, and what the Raspberry Pi 5's `v3d` driver requires of the kernel.

## Overview

| Component | Source | Notes |
|---|---|---|
| `libgallium-25.3.6.so` | Mesa | megadriver: `zink,virgl,softpipe` (x86_64), `zink,virgl,softpipe,v3d` (aarch64) |
| `libEGL.so.1`, `libGLESv2.so.2`, `libgbm.so.1`, `gbm/dri_gbm.so` | Mesa | no glvnd; `libgbm` `dlopen`s `dri_gbm.so` |
| `libvulkan_virtio.so` + `virtio_icd.<arch>.json` | Mesa | Venus ICD |
| `libvulkan_nouveau.so` | Mesa | NVK ICD (built with `NVK=1`, the default); used on the real RTX 3070 through the nouveau UAPI ([pc.md](../hardware/pc.md#stage-m5m6-the-nouveau-uapi-and-nvk)), with Zink on top for GL |
| `libvulkan.so.1` | Alpine `vulkan-loader` | needed by Zink, which has no other way to find an ICD |
| `libzstd.so.1`, `libleandros_ssp.so.1` | Alpine / local | shader cache; stack-protector guard |
| `gpuprobe` | [`ports/mesa/gpuprobe.c`](../../ports/mesa/gpuprobe.c) | `caps` (capsets via `GETPARAM`) and `gl` (GBM + EGL as smithay does it; exit 2 on a software renderer) |
| libdrm, libwayland, libffi, expat, zlib, libdisplay-info | upstream | the rest of the closure |

Every library `DT_NEEDED`s musl's `libc.so` and runs under `ld-musl` — not relibc. Programs that consume the stack (kmscube, cosmic-comp, smithay's anvil) are built the same way; see [libc](../userland/libc.md).

## How it is built

### The GPU stack: Alpine, natively

[`ports/mesa/build-gpu-stack.sh`](../../ports/mesa/build-gpu-stack.sh) `<arch>` runs [`build-gpu-stack-alpine.sh`](../../ports/mesa/build-gpu-stack-alpine.sh) inside an Alpine 3.21 container **of the target architecture** (podman or Docker; the build takes 2–5 minutes natively and hours under qemu-user), and writes the ship-set to a `gpu-stage-<arch>` directory that the image packer ([`scripts/mkfs-f2fs-populated.py`](../../scripts/mkfs-f2fs-populated.py)) prefers. Zink and the Venus ICD come out of one tree so they are ABI-consistent.

The link recipe exists because Alpine's toolchain and LeandrOS's libc differ:

- LeandrOS's musl `libc.so` lacks `__stack_chk_guard`, which Alpine's static libstdc++/libgcc reference, so a local guard ([`ssp_guard.c`](../../ports/mesa/ssp_guard.c), shipped as `libleandros_ssp.so.1`) supplies it and the C++ runtime is linked statically.
- Alpine binaries name `libc.musl-<arch>.so.1`; every shipped ELF has that `DT_NEEDED` rewritten to `libc.so` with `patchelf`. Shipping a second musl was rejected.
- `libdisplay-info` is pinned to Alpine edge's 0.3.0, because 3.21's 0.2.0 yields a `.so.2` dependency against a sysroot that ships `.so.3` — it builds and then fails to load.
- NVK needs Mesa's Rust shader compiler NAK; its crates are fetched once on the host, verified against each wrap's sha256, and the container builds offline from that cache.

### Patches

Mesa patches live in [`ports/mesa/patches`](../../ports/mesa/patches) and are applied to a private copy inside the container:

- `0001-egl-robust-access-unsupported-is-bad-attribute` — return `EGL_BAD_ATTRIBUTE` instead of `EGL_BAD_MATCH` when a robust context is requested on a driver without robust buffer access. wgpu retries without robustness only on `BAD_ATTRIBUTE`, so on virgl over a host GL without robust access (ANGLE on macOS) it never got a context and every libcosmic client fell back to the CPU.
- `0002-virgl-no-triangle-fans-on-gles-hosts` — on hosts flagged GLES, draw triangle fans as indexed lists. MoltenVK draws only the first triangle of a fan, and Mesa's scissored clears are 4-vertex fans, so half of every such clear was lost.

Both are scoped to the hosts that need them; the request still fails, or the fan is still drawn, everywhere else.

### Where it started: cross-compiling on a Mac

Before the Alpine recipe, Mesa was cross-built on macOS with `zig cc` as the musl cross-compiler (the scripts and meson cross files in [`ports/mesa`](../../ports/mesa) and [`ports/gl-stack`](../../ports/gl-stack) record it). That proved the port had no musl-isms at all — the whole tree compiled — and needed only host fixes: Python `mako`/`packaging`/`pyyaml` visible to ninja as well as meson, Bison ≥ 3 ahead of Apple's 2.3, a wrapper that merges `-Wl,--version-script <path>` into one argument (zig rejects the positional path) and rewrites argv without word-splitting, linking libffi's `.so` by hand (darwin libtool cannot emit a Linux ELF `.so`), and a native meson file for `wayland-scanner` (meson clears `PKG_CONFIG_PATH` for native dependencies in a cross build). Useful facts it established:

- Modern Mesa has no per-driver `dri/*_dri.so`: the drivers are the megadriver, linked as a hard `NEEDED`, and the only runtime `dlopen` is `libgbm` → `dri_gbm.so`.
- `EGL_WL_bind_wayland_display` is double-gated (a `legacy-wayland` meson option, then dmabuf import/export support at runtime), so it is never advertised on a software display; software Wayland clients use `wl_shm`.
- smithay and cosmic-comp link only `libgbm`, `libinput`, `libseat`, `libudev`, `libxkbcommon`, `libdisplay-info`, `libpixman` and `libc`; EGL, GLES and Vulkan are `dlopen`ed at runtime. A smithay build-script probe silently falls back to older GBM entry points when the cc wrapper passes a four-part target triple — non-fatal, but worth fixing in a production build.

The zig route was replaced because the GPU drivers needed a C++ runtime and (for NVK) LLVM and Rust, which Alpine supplies natively.

### Why there is no llvmpipe

llvmpipe was built (Alpine, LLVM 19) when software rendering was still the plan, and crashed at screen creation on LeandrOS while the identical binary ran on Linux. The root cause was the kernel, not LLVM: `sys_mmap` honoured a non-`MAP_FIXED` address hint verbatim and its bump allocator did not account for hinted mappings, so a later unhinted allocation collided with a JIT region and failed with `ENOMEM`. Linux treats the hint as advisory (now fixed: `cb5a466`). A separate "null `__DRI*` extension" crash was likewise the kernel: `mincore` was a stub returning success, so Mesa's `_eglPointerIsDereferenceable((void*)3)` said yes and misread a `wl_egl_window` version field as a pointer (`30a4cb9`). Both are lessons in reading a userland crash as a kernel conformance bug first.

With the [GPU-only policy](README.md#gpu-only-rendering-policy), a faster software renderer stopped being a goal; softpipe is kept only so a guest without host 3D can still run headless tests, and a COSMIC session on it requires an explicit opt-in.

### Packaging landmines

- The image packer guards every artifact with an existence check, so a missing tree is silently omitted and the image still builds. A correct x86_64 data image with the GPU stack is roughly 300 MB, not 80 MB; the packer now warns when a GPU stage is missing.
- The megadriver's new dependencies are not optional: without `libzstd.so.1` the loader fails with `MESA-LOADER: failed to open dri`, GBM creation fails, and cosmic-comp panics in `Failed to load LibEGL`. Move a megadriver and its runtime libraries together or not at all.
- `gpuprobe` and `libzstd` from a newer stage need `libleandros_ssp.so.1`; an image packer that does not ship it boots with `gpu-env: card0=none … NO GPU RENDERER`.
- macOS `tar` emits AppleDouble `._*` sidecars; meson globs `*.wrap`, matches `._foo.wrap`, and dies with a `UnicodeDecodeError` reported as a meson bug. The build script deletes them.

## v3d for the Raspberry Pi 5

The Pi 5's GPU (V3D 7.1) is a `v3d` DRM device. Before 2026-08-20 every checked-in Mesa script pinned `-Dgallium-drivers=softpipe`, so no image had v3d and no kernel V3D work could have mattered (`5a40318`). It now builds for aarch64:

- **No LLVM** — v3d compiles through NIR and Broadcom's own QPU backend, which sidesteps the blocker that killed llvmpipe.
- The megadriver grew by 6.2 MB and its `DT_NEEDED` graph is byte-identical, so the runtime closure is unchanged.

**Single-device design.** Upstream Raspberry Pi OS splits display (`vc4`) and render (`v3d`) into two DRM devices joined by a `renderonly` object. LeandrOS reports `v3d` on both card0 and the render node instead. `ro == NULL` is upstream's primary path, not an edge case: the plain pipe-loader entry `v3d_drm_screen_create()` passes NULL, and the renderonly variant is called from exactly one place. `PIPE_BIND_SCANOUT` forces linear allocation regardless of `ro`, which is what a firmware framebuffer needs, and a KMS handle is the v3d GEM handle directly — valid for `ADDFB2` on the same device.

**The kernel contract v3d imposes** (all satisfied by the stub backend described in [drm-kms.md](drm-kms.md#the-v3d-backend)):

1. The DRM driver name must be exactly `v3d` — the loader plain-`strcmp`s it; anything else silently falls back to software, the same failure the virgl path hit with `leandros-drm`.
2. `DRM_IOCTL_V3D_GET_PARAM` is probed first and hard-fails screen creation if `CORE0_IDENT0`/`IDENT1` error. The version decodes as `major*10 + minor`, so V3D 7.1 reports 7 and 1. `MAX_PERF_COUNTERS` must be **0**: a non-zero count makes Mesa issue `PERFMON_GET_COUNTER`, and a failing perf-counter init fails screen creation.
3. `CREATE_BO` must accept non-round sizes: resources are over-allocated by 64 bytes (`V3D_TFU_READAHEAD_SIZE`) or 4 bytes for buffers.

In-guest under QEMU, with the backend armed, Mesa walked the entire kernel contract in source order — `GET_PARAM` for device info and features, perf-counter init — and accepted the device as V3D 7.1; disarmed, `kmscube` ran at ~100 fps on the same libraries as a control. `kmscube` then faulted inside Mesa userspace with no further ioctl, past the whole kernel interface; that fault is not yet root-caused.

Two integration hazards the single-device choice creates:

- **Linear stride is tightly packed** (`width × cpp`, no alignment for 2D): 1920×1080 XRGB8888 has a stride of exactly 7680. The firmware framebuffer's pitch comes from the mailbox and may not match, in which case the scanout path must accept the tight pitch or blit.
- A caller passing an explicit modifier list for a scanout resource must include `DRM_FORMAT_MOD_LINEAR`, or allocation fails with "Unsupported modifier requested"; `IN_FORMATS` must therefore advertise linear.

The `drm_version` string-length convention elsewhere in the tree counts the trailing NUL and works by self-consistency; the three-character `v3d` exposed it, and the v3d arm reports the true length. See [Raspberry Pi 5](../hardware/raspberry-pi-5.md).

## Limitations and open work

- The ship-set is built outside the repository's build (`build-all.sh` consumes a staged directory); a new machine needs the stage built or copied.
- v3d has no real hardware backend yet; the post-contract `kmscube` fault is open.
- NVK is built but only meaningful with the NVIDIA kernel bring-up (see [PC hardware](../hardware/pc.md)).

## Code and tests

- [`ports/mesa/`](../../ports/mesa) — build scripts, patches, `gpuprobe.c`, `ssp_guard.c`, the historical zig recipe ([`NOTES.md`](../../ports/mesa/NOTES.md)).
- [`ports/gl-stack/`](../../ports/gl-stack) — kmscube and the dynamic-musl Rust recipe ([`NOTES.md`](../../ports/gl-stack/NOTES.md)).
- [`ports/greetd/data/gpu-env`](../../ports/greetd/data/gpu-env) — runtime driver selection.
- `drmsmoke --arm-v3d` exercises the v3d UAPI.

## See also

- [zink-and-virgl.md](zink-and-virgl.md), [vulkan-venus.md](vulkan-venus.md).
- [Ports](../userland/ports.md) — every third-party port.
- [Raspberry Pi 5](../hardware/raspberry-pi-5.md), [PC hardware](../hardware/pc.md).
