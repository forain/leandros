# Vulkan via Venus

Guest Vulkan reaches a real host GPU through Mesa's Venus ICD, which serialises Vulkan calls into virtio-gpu command streams that the host's virglrenderer (`vkr`) decodes and replays on the host driver. It runs on a stock Linux host with no custom virglrenderer, and it is also the foundation of the default desktop path, Zink ([zink-and-virgl.md](zink-and-virgl.md)).

## Overview

`vktest` running inside LeandrOS enumerates and opens the host GPU:

```
vkEnumeratePhysicalDevices count=1
  "Virtio-GPU Venus (AMD Ryzen 9 7950X (RADV RAPHAEL_MENDOCINO))"
  INTEGRATED_GPU vendor 0x1002 device 0x164e api 1.4.307
vkCreateDevice VK_SUCCESS
```

The path is: guest Vulkan client → Mesa Venus ICD (`libvulkan_virtio.so`) → `DRM_IOCTL_VIRTGPU_EXECBUFFER` on the render node → virtio-gpu control queue → host virglrenderer's `vkr` decoder → host GPU driver. It was first proven against an unmodified Arch host (virglrenderer 1.3.0, package files verified unaltered, the system `virgl_render_server`, no debug variables) on 2026-08-04.

## How it works

### The kernel pieces Venus needed

Each of these was found by running real Mesa against a real host, not by reading the protocol:

- **A host-populated Venus capset** from `VIRTGPU_GET_CAPS`: capset 4, 160 bytes, with the host advertising capset ids `0x16` (VIRGL, VIRGL2, VENUS). On a host without Venus the ioctl fails with `EINVAL` rather than returning a buffer of zeros (`04bde83`).
- **A discoverable render node** (`d5410ee`). Venus never touches card0: it requires `DRM_NODE_RENDER` in libdrm's available nodes. `opendir("/dev/dri")` also had to work — its entries were synthesised in `getdents64` but the directory itself could not be opened — and `drmGetVersion` must report `virtio_gpu` at major 0.
- **3D contexts per open file**, not per process (`997a45e`) — the original blocker: one global context id meant `CONTEXT_INIT` tore down any other client's context. See [drm-kms.md](drm-kms.md#per-open-state).
- **`GETPARAM` writes through the user pointer** (`bdab516`). Upstream does `copy_to_user(u64_to_user_ptr(param->value), ...)`: the `value` field is a pointer, not an out-field. The kernel wrote into the struct, clobbering Mesa's pointer, so Mesa read 0 for every parameter. The transport test had missed it because it was written against the same wrong ABI on both sides — a test that shares the implementation's assumption cannot catch it.
- **HOST3D blob mapping** through the host-visible window (`0dfc362`). Mesa's command ring is a `HOST3D` blob shared with the host; this cannot be avoided through feature parameters (with `HOST_VISIBLE` cleared Mesa refuses outright). See [virtio-gpu.md](virtio-gpu.md#blob-resources-and-the-host-visible-window).
- **Blob cacheability as the host requests it** (`18a7a9f`). Mesa's fence-feedback slot is host-coherent but not host-cached memory; mapping it write-back left `vn_GetFenceStatus` polling a stale cache line forever on KVM, while TCG (no guest cache model) passed.
- **Correct 3D wire protocol** (`3598e80`): `CTX_ATTACH_RESOURCE` sent, `ring_idx` honoured and bounds-checked against the context's ring count, and hard failures instead of silent ones. (`CTX_ATTACH_RESOURCE` turned out not to be load-bearing for Venus — virglrenderer registers a blob at `RESOURCE_CREATE_BLOB` — and is sent for conformance.)
- **Device VMAs aliased across `fork`** rather than copied (`ce383ed`), which had panicked the kernel.
- **`nanosleep` rounds up** and `clock_nanosleep` honours `TIMER_ABSTIME` (`fb398c7`). The sleep truncated to the 100 Hz tick, so every sub-10 ms sleep returned immediately; Mesa's ring watchdog fired 200× early and `abort()`ed. That was the bug that cost the most time, and it passed every correctness test in the tree. See [Time](../kernel/time.md).

Per-BO fences, handle-scoped `VIRTGPU_WAIT`, `bo_handles` resolution and per-open GEM ownership landed together, because `VIRTGPU_WAIT` already took a BO handle and had been throwing it away.

### Host requirements

The QEMU device line (the runner selects it automatically where the host can do it):

```
-device virtio-gpu-gl-pci,venus=on,blob=on,hostmem=4G -display egl-headless
```

On x86_64 the runner uses `virtio-vga-gl` with the same properties so firmware gets a GOP on the same device ([virtio-gpu.md](virtio-gpu.md#one-device-one-console-virtio-vga-gl)). `venus=on` fails twice before it works, each time named by QEMU: it needs an OpenGL-capable display backend, and it needs `blob` and `hostmem`. **`-nographic` silently overrides `-display`** and reproduces the first error even when `-display egl-headless` was given, so the runner refuses the combination. `--venus` opens a `gtk,gl=on` window when the host has a display server and `egl-headless` otherwise; `--venus-headless` forces offscreen and `LEANDROS_VENUS_DISPLAY` overrides outright. Do not measure guest performance through `gtk,gl=on`: it spams host `eglMakeCurrent failed` and stalls.

Venus requires a **Linux host**. Upstream virglrenderer's Venus renderer depends on Linux-only facilities (memfd, eventfd, udmabuf, the render server), and QEMU's rutabaga device never sets the Venus capset bit in any version, so it is no substitute. On macOS the GPU path is virgl over ANGLE ([zink-and-virgl.md](zink-and-virgl.md#the-mac-path)).

### The guest Vulkan stack

Mesa's Venus ICD is built in the same tree as the Zink megadriver, so the two are ABI-consistent ([mesa.md](mesa.md)). The original `vktest` deliberately had no Khronos loader — it `dlopen`ed the ICD and called `vk_icdGetInstanceProcAddr` directly, which removed a whole port from the path. Zink, however, `dlopen`s `libvulkan.so.1` and has no other way to find an ICD, so the Khronos loader now ships, with one ICD manifest per image (`virtio_icd.<arch>.json`).

### Presenting

Enumerating a device is not putting a pixel on a screen, so both ends of the path were checked separately, and by photograph rather than by return code:

- **`vkrender --present`** draws with Vulkan straight to the scanout with no compositor in the way.
- **`vkwl`** is a Vulkan Wayland client that reaches `vkQueuePresentKHR → VK_SUCCESS` against cosmic-comp inside the guest (300/300 presents sustained on x86_64; 28/28 on aarch64).

Both were photographed on **both** architectures against pass criteria committed *before* the first capture — a criterion written after seeing the image is a description, not a test. The aarch64 vkwl census matched x86_64's to the pixel (151,868 px of the predicted colour, a 478×318 bounding box at 99.91% fill), with a control capture containing none of the predicted colours and the panel clock proving the captures were distinct live frames. The same run refuted the recorded belief that COSMIC on aarch64 under TCG was impractically slow: the session bound its Wayland socket 2 s after launch.

Two traps from this work:

- **Check which surface extension a test binary uses.** `vktest`, `vkrender` and the first swapchain test all used `VK_EXT_headless_surface`; none touched Wayland, so the planned vehicle for the Wayland milestone could never have carried it. `vkwl` was written for it: it bound 54 globals, with `wl_shm` present and `zwp_linux_dmabuf_v1` absent.
- **A host control is a client sanity check, never a guest prediction.** `MESA_VK_WSI_DEBUG=sw,noshm` was measured correctly on the RADV host and written down as though it held generally. Inside the guest, Venus reports `VK_EXT_external_memory_host = no`, so Mesa already selects the memcpy path and plain `sw` is right.

A further trap affects every pixel check of a GL scanout: once GL scanout engages, QEMU's console holds a texture rather than a display surface and QMP `screendump` answers "no surface". Captures go over VNC (`-vnc …,display=<gpu id>`) with a client actually connected.

### Test binaries

`vktest`, `vkrender` and `vkwl` are C probes built against the Mesa port and staged into images; they are not version-controlled, because the repository is Rust-only. They delivered their findings; capabilities worth guarding permanently are rewritten as Rust tests under `userland/`. The in-tree guard is `venustest`.

## Design decisions

- **Venus rather than gfxstream or a custom protocol** — it is the only real Vulkan passthrough QEMU offers with stock virglrenderer, and it needs no host patches.
- **Keep the render node's identity upstream-exact** (`virtio_gpu`, major 0) — Mesa hard-`strcmp`s it.
- **Prove each layer at the destination.** The first host round trip deliberately sent a non-Venus payload and was judged a pass by the host's own `vkr: vkCreateInstance resulted in CS error` line: it proved the bytes arrived and were decoded. A negative control on plain `virtio-gpu-pci` showed every layer refusing explicitly while the boot still reached login.

## Limitations and open work

- No Venus on macOS hosts.
- A malformed `RING_IDX` stream that the host rejects is never completed (see [virtio-gpu.md](virtio-gpu.md#limitations-and-open-work)).

## Code and tests

- [`drivers/src/drm_device_interface.rs`](../../drivers/src/drm_device_interface.rs) (virtgpu ioctls, per-open contexts, blob window), [`drivers/src/virtio_gpu.rs`](../../drivers/src/virtio_gpu.rs), [`servers/drm/src/lib.rs`](../../servers/drm/src/lib.rs) (render node).
- [`userland/venustest`](../../userland/venustest) — transport conformance: a non-empty host-populated capset, per-open contexts and GEM ownership, `bo_handles`, dmabuf/blob lifetime across `close`, PRIME import of a blob into another open (phase 5b), and `SET_SCANOUT_BLOB` scanout of a host-memory buffer (phase 7). Its blob phase carries a gate asserting its own assertions ran, which distinguishes "passed" from "silently skipped". Needs a blob-capable device (a Venus host); most phases FAIL rather than SKIP elsewhere.

## See also

- [zink-and-virgl.md](zink-and-virgl.md) — COSMIC on Vulkan.
- [mesa.md](mesa.md) — how the ICD and loader are built.
- [virtio-gpu.md](virtio-gpu.md) — blobs, fences, the control queue.
- [Methodology](../development/methodology.md) — pass criteria before capture, controls, destination measurement.
