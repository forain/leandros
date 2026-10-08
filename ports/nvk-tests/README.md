# nvk-tests — NVK bring-up test for real NVIDIA hardware

`nvktest` drives Mesa's NVK (`/usr/lib/libvulkan_nouveau.so`) directly through
`vk_icdGetInstanceProcAddr` (no Khronos loader), the same design as the Venus
lane's `vkrender`. `/bin/nvk-autotest` runs it unattended at boot and prints
everything to the console, so one boot of the real PC produces a complete log.

## Subtests

| subtest | proves |
|---|---|
| `info` | the ICD loads, an instance exists, and NVK accepts the DRM device. Prints name, IDs, API/driver version, driver properties, PCI address, heaps, memory types, queue families, key limits, and whether timeline semaphores and sync2 are listed. Always runs. 0 devices -> `nvktest info: 0 physical devices (no nouveau DRM device)`, exit 3 |
| `device` | `vkCreateDevice` (timeline semaphores enabled), the queue, command pool and fence. This is a step, not a selectable subtest |
| `fill` | the shaderless path: (A) `vkCmdFillBuffer` into VRAM then `vkCmdCopyBuffer` to sysmem, (B) fill sysmem directly, (C) fill a CPU-mapped VRAM (BAR) buffer when a `DEVICE_LOCAL\|HOST_VISIBLE` type exists. Every word is checked against CPU poison |
| `timeline` | timeline semaphores: one submit signals value 1 (`vkWaitSemaphores`), the host signals 2, and waiting on 3 must return `VK_TIMEOUT` within 50 ms. These are DRM timeline syncobjs on the kernel side |
| `compute` | an NAK-compiled shader `C[i] = A[i] + B[i]` over 65536 uints in host memory, then again with A/B/C in VRAM, staged by copies |
| `triangle` | clear + one red triangle into a 256x256 `R8G8B8A8_UNORM` OPTIMAL image: 13 named pixels, red area 18432 +/- 600, no other colours, FNV-1a. Writes `/root/nvktest-triangle.ppm` and prints an ASCII view plus a 32x32 PPM in base64 |
| `triangle_depth` | the same triangle with a depth attachment (D32_SFLOAT, otherwise D24S8/D32S8) at z=0.5 against a 1.0 clear, test LESS (depth PTE kinds). Also reads back the depth image: 0.5 under every red pixel, 1.0 everywhere else |
| `timestamps` | two timestamps around a 16 MiB fill, with the delta in ns (SKIP if `timestampValidBits` is 0) |

Each subtest ends with exactly one `nvktest <name>: PASS|FAIL|TIMEOUT|SKIP (<why>)`
line. Every failing call is logged as `[ERR ]` with its VkResult. The last line
is `==== nvktest done: P passed, F failed, S skipped, T timeout ====`. Exit code:
the number of failures (at most 120), 126 after a timeout, 3 for zero devices.

Nothing hangs:
- Fence and semaphore waits use `--timeout-ms` (default 20000).
- A SIGALRM watchdog (default 3*timeout+30 s per subtest) covers driver calls
  that have no timeout of their own.
- Crashes are caught and still produce a subtest line and the summary.
- After a timeout, nothing is torn down.

The preflight lists `/dev/dri`. For each render node it also prints the DRM
driver name (NVK only accepts `nouveau`) and the SYNCOBJ/SYNCOBJ_TIMELINE caps.

Usage: `nvktest [--icd PATH] [--timeout-ms=N] [--watchdog-s=N] [--out-dir=DIR] [--only=a,b | a b ...]`

## Build

```sh
ports/nvk-tests/build.sh all   # or x86_64 / aarch64
```

The build runs in Alpine 3.21 containers of each arch (x86_64 runs emulated on
an arm64 Mac). It uses Vulkan headers from `$MESA_SRC/include` and patches the
soname to `libc.so`. The output goes to
`${LEANDROS_ARTIFACTS:-~/code/leandros-artifacts}/nvk-tests/stage-<arch>/usr/bin/nvktest`.
`scripts/mkfs-f2fs-populated.py` stages that file as `/bin/nvktest`, and this
directory's `nvk-autotest.sh` as `/bin/nvk-autotest`.

`nvktest_spv.h` is generated and committed. After editing `shaders/`, run
`ports/nvk-tests/gen-spv.sh` (glslc in a container).

## Reconstructing the 32x32 image from a log

```sh
grep '^nvktest ppm32| ' boot.log | sed 's/^nvktest ppm32| //' | tr -d '\r' | base64 -d > tri32.ppm
```

When `triangle_depth` renders a different image, it prints a second one with the
`ppm32d` prefix. The image is point-sampled every 8 px. `tr -d '\r'` strips the CRs a
serial console adds.
