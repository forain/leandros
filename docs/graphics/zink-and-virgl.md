# COSMIC on the GPU: Zink and virgl

The COSMIC compositor renders with GLES, and LeandrOS gives that GLES a host GPU in one of two ways: **Zink** (OpenGL on Vulkan) over Venus, the default and fastest path, or **virgl** (OpenGL command-stream passthrough), the fallback on hosts without Venus and the only GPU path on macOS. Neither needs a line of COSMIC or smithay source changed.

## Overview

| | Zink over Venus | virgl |
|---|---|---|
| Guest driver | Mesa `zink` gallium driver + Venus ICD | Mesa `virgl` gallium driver |
| Host | virglrenderer `vkr` → host Vulkan (RADV, ANV) | virglrenderer `vrend` → host GL (radeonsi, iris, ANGLE) |
| QEMU device | `virtio-vga-gl`/`virtio-gpu-gl-pci`, `venus=on,blob=on,hostmem=4G` | `virtio-vga-gl`/`virtio-gpu-gl-pci` |
| Runner | `--venus` (default where possible) | `--virgl` (default where only GL exists) |
| Guest GL | GLES 3.2 | GLES 3.0–3.2, host-dependent (3.1 on the Mac's ANGLE-Vulkan build) |
| Measured desktop | ~20–21 fps (AMD iGPU), 23.4 fps (Intel UHD 620) | panel/dock/session verified; not benchmarked against Zink |

The guest picks between them at session start: `/bin/gpu-env` tries Zink when the device offers Venus, then virgl, each verified by `gpuprobe` creating a GBM + EGL context and reading back a hardware `GL_RENDERER`. If neither comes up, COSMIC is not started at all (the greeter exits 78 and init stops respawning it); softpipe requires an explicit opt-in. See [Graphics](README.md#gpu-only-rendering-policy).

## Zink: why and how

**smithay has no Vulkan renderer.** Across the vendored revisions the complete set of `impl Renderer` is `GlesRenderer`, `GlowRenderer`, `PixmanRenderer` and `DummyRenderer`. cosmic-comp does enable smithay's `backend_vulkan`, but that is an *allocator* (`VK_EXT_image_drm_format_modifier`) used only by the nested-X11 backend; the KMS backend LeandrOS runs mentions Vulkan once, in a TODO comment. "Enable Vulkan rendering" was therefore never a switch: the choices were to write a Vulkan renderer (a large smithay fork) or to run the existing GLES renderer through Zink, which turns every draw into Venus work on the host GPU.

Proven first with `kmscube`:

```
renderer: "zink Vulkan 1.4(Virtio-GPU Venus (AMD Ryzen 9 7950X 16-Core Processor (RADV RAPHAEL_MENDOCINO)) (MESA_RADV))"
version:  "OpenGL ES 3.2 Mesa 25.3.6"
Rendered 2 frames in 0.020008 sec (99.961490 fps)
```

The full desktop composites at **~20 fps** against ~2.3–2.45 fps on softpipe with the same kernel, and greeter keystroke latency fell from a 35.5 s median on softpipe to **50 ms** (p50, measured over a persistent VNC connection including a 30 ms key hold).

### What it took

Getting real frames out of Zink took kernel fixes, none of them in the GPU driver proper:

1. **One resource-id space** (`a32f53d`). Framebuffer paths derived host resources as `handle + 10`, overlapping the 3D allocator that starts at 16. Zink is the first workload to use dumb buffers and 3D resources at once, so it was the first to collide. See [virtio-gpu.md](virtio-gpu.md#resource-ids).
2. **`CLONE_PARENT_SETTID`** (`8897e02`). It was ignored, so musl's thread ids were 0 and the first `fork()` from a compositor thread deadlocked on musl's thread-list lock — the same root cause behind COSMIC system keybindings never firing, since a bound action's `fork()` is exactly that path. See [Threads and futex](../kernel/threads-and-futex.md).
3. **`PRIME_FD_TO_HANDLE` mints the importer its own handle** (`51d9248`) — blob handles are per open, so echoing the exporter's handle made the importer's `RESOURCE_INFO` fail.
4. **A scanout path for host-memory buffers** (`5c3c552`): `ADDFB2` accepts blob handles and presents with `SET_SCANOUT_BLOB` + `RESOURCE_FLUSH`, zero-copy, on a strict `ADDFB` path. Without it the compositor flipped at ~20 fps into a black screen.

### The refuted "0.65 fps"

An earlier measurement put the Zink desktop at 0.65 fps — slower than softpipe — with a ~2 s input freeze and a catch-up burst after every keystroke. Both readings were wrong:

- The 0.65 fps was real but answered the wrong question: it counted 52 page flips over 80 s while the desktop was compositing **zero** frames end to end. The flips were geometry probes, not presents. Once the scanout path existed, the same desktop ran at 20 fps.
- The input stall was chased first as a poll-wake thundering herd. Coalescing wakes worked (~8.75× fewer) and the desktop was no faster. The control-queue busy-spin was the next suspect; it measured 0.4% of wall time — real and later fixed on its own merit, but incapable of a 2 s freeze. Neither the herd nor the spin was the cause; the absence of real frames was.

### Turning it on

Zink was opt-in (`LEANDROS_ZINK=1`) until 2026-09-25, when GPU rendering became the default and `gpu-env` started choosing it automatically. That change also uncovered a stale-launcher landmine: the image packer used to prefer an old out-of-tree copy of the session launcher that still exported `GBM_ALWAYS_SOFTWARE=1`, so some earlier "Zink" runs had been running softpipe. The repository copy now wins, and `gpu-env` unsets `GBM_ALWAYS_SOFTWARE`, `MESA_LOADER_DRIVER_OVERRIDE`, `GALLIUM_DRIVER` and `VK_ICD_FILENAMES` before deciding. `MESA_LOADER_DRIVER_OVERRIDE=zink` with no reachable ICD does not fall back to softpipe — it fails to create a screen — which is why the check requires both the ICD manifest and `libvulkan.so.1`.

## virgl

`run-qemu.sh --virgl` (or the automatic choice on a host with GL but no Venus) selects `virtio-vga-gl` with Mesa's `virgl` gallium driver: guest OpenGL reaches the host GPU through virglrenderer, and `kmscube` reports a real renderer string such as `virgl (AMD Ryzen 9 7950X … (radeonsi, raphael …))`.

Four independent blockers stood in the way:

1. **The device.** OVMF needs VGA registers, so `virtio-gpu-gl-pci` could not be used on x86_64, and the runner's GL branch was dead code because `grep virtio-vga` also matches `virtio-vga-gl`. `virtio-vga-gl` satisfies both (`84f1d56`).
2. **The guest driver.** Every Mesa in the tree was softpipe-only; it was rebuilt as `virgl,softpipe` — no LLVM needed.
3. **Kernel gates.** The 3D gates demanded `VIRGL && RESOURCE_BLOB && CONTEXT_INIT`, and `virtio-vga-gl` without Venus offers no blob support, so every context was refused. The gate had to be fixed in both `ctx_create` and `submit_3d` (fixing only the first produced a context refused at its first draw). Upstream Linux creates a default capset-0 context lazily on the first `EXECBUFFER` and Mesa's virgl relies on it; `ctx_ensure()` does the same.
4. **The DRM driver name.** card0 reported `leandros-drm` precisely so Mesa's loader would fall through to software; with virgl in the megadriver everything worked and Mesa still said `renderer: "softpipe"`. card0 now reports `virtio_gpu` ([drm-kms.md](drm-kms.md#driver-identity)).

Then COSMIC itself. Its "SIGSEGV" under virgl was a misread: run outside the session's launcher, cosmic-comp exited with `Backend initialized without output` because `VIRTGPU_RESOURCE_CREATE` was a stub with no backing store, so every dmabuf export returned an invalid fd and no CRTC could be configured; the signal 11 was the launcher restarting it into a crash storm. After that came noise on screen (virgl presents went through a 2D transfer of stale guest pages), a missing panel and dock (the PRIME import-handle bug, "Protocol error 7"), and magenta or frozen windows on the Mac (below). All are fixed; a 30-session soak on a Linux host (terminal, app library, workspaces overview and settings each session) and further sessions on the Mac showed no host context errors.

### Acceleration is not automatically a speedup

The first COSMIC-on-virgl measurement (2026-08-10) showed userspace CPU falling from 38.5% to 3.5% — the compositor stopped rasterising — but **total busy time went up**: the kernel now dominated, with `tgid_of` at 21.7%, `memcpy` 19.6% and `virtio_blk::do_io` 15.8%. Acceleration had moved the bottleneck, not removed it. The same investigation found the softpipe desktop's own sluggishness (0.20 fps under pointer motion) was the kernel, not softpipe: an uncached LAPIC read on every `current_pid()`, epoll walking all 512 interest slots per wait, and `tgid_of` taking the run-queue lock to scan 256 slots. Fixing those (`f7c4577`, `eb9422e`) took the softpipe desktop to its 1.00 fps content ceiling. See [Scheduler](../kernel/scheduler.md) and [Event loop](../kernel/event-loop.md).

### Host-side workaround: radeonsi's threaded context

On an AMD Raphael iGPU host (Mesa 26.1.3 radeonsi, virglrenderer 1.3.0), a virgl COSMIC session that opened a terminal faulted the **host** GPU in most boots: `[gfxhub] page fault … SQC (data)` at garbage addresses (0x0, `0x3f800000` = the float 1.0), a ring timeout, and QEMU exiting with "context is lost". The guest cannot be the cause — without blob resources virgl never lets the host GPU see guest pages, and GPU addresses come from the host driver — and the same signature is reported from bare-metal Chromium on that iGPU. With `GALLIUM_THREAD=0` it went from 2/2 faulting boots to 0 of 22; Venus (RADV) never faulted. The runner exports `GALLIUM_THREAD=0` to QEMU on Linux hosts whose render node is driven by `amdgpu`, unless the caller set it (`7960651`).

## The Mac path

Homebrew's QEMU has no virglrenderer and no `*-gl` devices, so an Apple Silicon Mac gets no GPU path from it. [`scripts/mac-qemu-gpu/build.sh`](../../scripts/mac-qemu-gpu/build.sh) builds a standalone QEMU (installed under its own prefix, nothing touching Homebrew) with virglrenderer on **ANGLE**, which supplies the EGL/GLES that macOS lacks:

```
guest Mesa virgl → virtio-gpu-gl → virglrenderer 1.3.0 → libepoxy (EGL on macOS) → ANGLE → Metal  (default build: GLES 3.0)
                                                                                     → Vulkan → MoltenVK → Metal  (--angle-vulkan: GLES 3.1)
```

The `--angle-vulkan` variant is the one the runner prefers (`~/.local/qemu-gpu-gles31` before `~/.local/qemu-gpu`), because wgpu — and so every libcosmic client — needs GLES 3.1 with storage buffers. ANGLE's Metal backend stops at ES 3.0 upstream; ANGLE on MoltenVK was capped at ES 2.0 by a single missing extension, `VK_EXT_provoking_vertex` (Metal has no last-vertex convention), and lifting that cap on Apple gives ES 3.1 with 19 vertex and 23 fragment SSBOs on an M4 Max, at the cost of flat varyings taking the first vertex. Apple's desktop GL 4.1 was a dead end (no SSBOs, no compute). The display is `egl-headless` with the pixels over VNC; the runner opens a Screen Sharing viewer.

Patches carried for this host (in [`scripts/mac-qemu-gpu/patches`](../../scripts/mac-qemu-gpu/patches) and [`ports/mesa/patches`](../../ports/mesa/patches)):

| Patch | Problem it fixes |
|---|---|
| virglrenderer integer vertex attributes on GLES hosts | virglrenderer declares every vertex input `vec4` unless the host is "ARM"; integer attributes then feed float inputs, undefined in ES. On ANGLE/Vulkan an instanced integer attribute returned instance 0's value for every instance, so wgpu text (cryoglyph feeds glyphs as `u32` instances) drew only the first glyph of each string; on ANGLE/Metal nothing drew |
| no triangle fans on GLES hosts (Mesa `0002` and virglrenderer) | MoltenVK draws only the first triangle of a fan; Mesa's scissored clears are 4-vertex fans, so half of each clear was lost |
| EGL robust-access error code (Mesa `0001`) | wgpu asks for a robust context and retries without only on `EGL_BAD_ATTRIBUTE`; Mesa's spec-correct `EGL_BAD_MATCH` left wgpu with no context on a host without robust buffer access |
| retire fences on context loss | a lost host context otherwise leaves every guest fence pending forever |
| macOS eventfd stand-in, libepoxy EGL on macOS, QEMU ANGLE EGL display + BGRA readback | basic bring-up; without the BGRA readback every `egl-headless` frame was black |

Host behaviours that looked like guest bugs:

- **Magenta windows.** On this stack a texture that was never written reads back as exactly `#FF00FF` (black or garbage on Linux hosts, which is why it looked Mac-only). Magenta means "a buffer nobody rendered into", here because a guest virgl context had died after an `EXECBUFFER` was refused for lack of a free fd — the process fd and global eventfd pools were too small for a busy compositor (raised 128→256 per process, then to 512, and eventfd 256→1024). See [drm-kms.md](drm-kms.md#fences).
- **A frozen display after minutes of video.** The Metal watchdog killed a command buffer that was waiting on a GPU-side event, and after two GPU restarts IOGPU denies every further submission from the process: `EGL_CONTEXT_LOST` forever. ANGLE turns pipeline barriers into `VkEvent` waits on tile-based GPUs, and MoltenVK turns each wait into a GPU-side `MTLSharedEvent` wait; a race in MoltenVK's encode-time check can wait for a value nothing will ever signal. The runner exports `ANGLE_FEATURE_OVERRIDES_DISABLED=useVkEventForImageBarrier:useVkEventForBufferBarrier`, which leaves no GPU-side event wait for the watchdog to fire on and costs no frames. The guest's fence watchdog keeps the shell alive if it happens anyway ([virtio-gpu.md](virtio-gpu.md#fences-and-the-host-hang-watchdog)).
- **6–8 minute stalls.** Host idle sleep on battery. The runner holds `caffeinate -i -w <qemu pid>` for QEMU's lifetime (`9fc4442`; `LEANDROS_ALLOW_HOST_SLEEP=1` opts out).
- **12 s per greeter keystroke.** A stale disk image from before GPU-by-default, still exporting `GBM_ALWAYS_SOFTWARE=1`. The runner now prints a `STALE IMAGE` warning when the image predates the last commit that changes its contents.

Venus is not feasible on the Mac with any QEMU today: UTM's virglrenderer fork fails host `vkCreateInstance`, and the only macOS Venus stack (libkrun/krunkit with MoltenVK) is compute-oriented with no scanout.

## Measured results

| Configuration | Result |
|---|---|
| Zink, x86_64/KVM, AMD iGPU, 1920×1080 | 20.3–21.4 fps over repeated runs; 0 zero-flip samples; ~16k MSI-X interrupts per run, 1768 of 1771 flips delivered on the fence |
| Zink, x86_64/KVM, Intel UHD 620 (ANV) | 23.4 fps; greeter key p50 84 ms |
| virgl, x86_64/KVM, Intel UHD 620 (iris) | greeter key p50 54 ms; full session with panel and dock |
| virgl, aarch64/HVF, Mac (ANGLE) | greeter key p50 36 ms; host CPU ~4% idle |
| softpipe (opt-in only), same AMD host | ~2.0–2.45 fps; greeter key p50 35.5 s |

Frame rates come from [`scripts/zinkbench.py`](../../scripts/zinkbench.py) (80 s window after a 40 s settle, `DRM_STATS = true`).

## Limitations and open work

- Some upstream COSMIC defects remain visible on the GPU path (a cosmic-term exit use-after-free in iced's SCTK thread; cosmic-workspaces screencopy crashes under overview stress); policy is not to patch COSMIC except for approved correctness fixes such as the `weak_window` closed-window texture leak fix.
- On the Mac, a Firefox content-area paint freeze appears in some long video runs on aarch64 (decode continues, no GPU errors); not root-caused.
- virgl contexts are created with the debug name `leandros-venus` regardless of capset.

## Code and tests

- [`ports/greetd/data/gpu-env`](../../ports/greetd/data/gpu-env), [`ports/mesa/gpuprobe.c`](../../ports/mesa/gpuprobe.c) — renderer selection and verification.
- [`scripts/run-qemu.sh`](../../scripts/run-qemu.sh) — device selection, host workarounds; [`scripts/mac-qemu-gpu/`](../../scripts/mac-qemu-gpu) — the Mac GPU QEMU.
- [`scripts/zinkbench.py`](../../scripts/zinkbench.py), [`scripts/gpuirqbench.py`](../../scripts/gpuirqbench.py) — benchmarks.
- [`userland/drmsmoke`](../../userland/drmsmoke/src/main.rs), [`userland/venustest`](../../userland/venustest).

## See also

- [vulkan-venus.md](vulkan-venus.md), [mesa.md](mesa.md), [drm-kms.md](drm-kms.md).
- [Desktop](../desktop/README.md) and [desktop limitations](../desktop/limitations.md).
- [Running](../getting-started/running.md) — `--venus`, `--virgl`, Mac prerequisites.
- [Methodology](../development/methodology.md) — the "0.65 fps" and other refuted premises.
