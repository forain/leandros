# Graphics

LeandrOS drives the display through a Linux-compatible DRM/KMS interface implemented in the kernel's driver layer and a small DRM server, on top of a virtio-gpu driver that speaks the full 2D, 3D, blob and Venus protocol. Unmodified Mesa, smithay and COSMIC run on it, and the desktop renders on the **host GPU** — through Zink over Vulkan/Venus, or through virgl — never in software unless a developer explicitly asks for it.

## The stack at a glance

```
 cosmic-comp / kmscube / DOOM / vkwl                    (unmodified userspace)
        │  GLES / EGL / GBM            │ Vulkan
        ▼                              ▼
 Mesa megadriver: zink · virgl · softpipe (· v3d)    Venus ICD (libvulkan_virtio.so)
        │  libdrm ioctls on /dev/dri/card0 and /dev/dri/renderD128
        ▼
 DRM server (servers/drm) ──► DRM/KMS core (drivers/src/drm_device_interface.rs, drivers/src/drm/)
        │  atomic KMS, planes, PRIME, fences, syncobj, per-open state, master per VT
        ▼
 virtio-gpu driver (drivers/src/virtio_gpu.rs)
        │  control queue (async, MSI-X / INTx completion), cursor queue, blob window
        ▼
 QEMU virtio-gpu / virtio-vga-gl ──► virglrenderer (virgl or vkr/Venus) ──► host GPU driver
```

The framebuffer console ([console.md](console.md)) shares the same scanout surface and yields it to whichever DRM client is presenting.

## GPU-only rendering policy

The desktop renders on a GPU, and a software-rendered COSMIC is never a silent fallback. This is a deliberate rule, adopted on 2026-09-25 after a fix that made the greeter's blur cheaper *under softpipe* (by patching cosmic-greeter) was reverted: the right answer to a slow software path is a working GPU path, not a userland patch or a renderer tweak. COSMIC performance is measured on the GPU path; softpipe numbers are informational only. See [design principles](../overview/design-principles.md).

The policy is enforced at both ends:

- **Host side** — [`scripts/run-qemu.sh`](../../scripts/run-qemu.sh) picks a GPU device by default (`--gpu auto`): Venus where the host QEMU exposes a `venus=` property and a render node exists, virgl where only GL passthrough exists, and none otherwise. Every check is a capability probe of the QEMU binary about to run, never an OS-name guess. `--venus`, `--virgl` and `--no-gpu` force a choice.
- **Guest side** — `/bin/gpu-env` ([`ports/greetd/data/gpu-env`](../../ports/greetd/data/gpu-env)) is sourced by every launcher of a compositor. It verifies each candidate renderer by having `/bin/gpuprobe` ([`ports/mesa/gpuprobe.c`](../../ports/mesa/gpuprobe.c)) create a real GBM + EGL context the way smithay does and read `GL_RENDERER` back. Zink is tried first, then virgl. If neither yields a hardware renderer the result is `refused`: the greeter exits 78, init prints a `NO GPU RENDERER` banner and stops respawning, and the serial login is unaffected. Softpipe is reachable only through `LEANDROS_RENDERER=software`, `/etc/leandros/renderer` or `/etc/leandros/allow-software-render`, and even then a banner says so.

COSMIC's own clients (greeter, settings, terminal and other apps) are built with libcosmic's `wgpu` feature ([`ports/cosmic-wgpu`](../../ports/cosmic-wgpu)); `gpu-env` sets `ICED_BACKEND=wgpu` and pins `WGPU_BACKEND` to the verified path (Vulkan on Venus, GLES on virgl), so a client that cannot get the GPU fails loudly instead of rasterising on the CPU. Two exceptions remain: the **panel applets** stay on iced's tiny-skia renderer, because they are clients of cosmic-panel's embedded Wayland server, which offers dmabuf only if its GLES renderer already exists when the display is bound (see [Session](../desktop/session.md#the-panel-and-its-applets)); and on a virgl host whose GL is too old for wgpu (GLES < 3.1 or no SSBOs) clients keep iced's tiny-skia fallback behind a banner. Compositing itself is always on the GPU.

## Render paths by host

| Host | Accelerator | Path chosen by default | Notes |
|---|---|---|---|
| Linux desktop, AMD iGPU (RADV/radeonsi) | x86_64 KVM | Venus → Zink | ~20–21 fps full desktop; greeter keystroke p50 50 ms |
| Same | aarch64 TCG | Venus → Zink | works; greeter at 1280×800, key p50 ~79 ms |
| Linux laptop, Intel UHD 620 | x86_64 KVM | Venus → Zink (ANV) or virgl (iris) | Zink 23.4 fps; Venus needs `vulkan-intel` installed |
| Mac, from-source GPU QEMU (`scripts/mac-qemu-gpu/build.sh --angle-vulkan`) | aarch64 HVF | virgl over ANGLE → Vulkan → MoltenVK | guest GLES 3.1 with SSBOs, so wgpu works |
| Mac, Homebrew QEMU | aarch64 HVF | none | no virglrenderer; COSMIC refused, serial login only |

Venus is not available on macOS with any QEMU today: upstream virglrenderer's Venus renderer is Linux-only, and UTM's fork fails host `vkCreateInstance`. See [zink-and-virgl.md](zink-and-virgl.md#the-mac-path).

## Pages in this section

- [drm-kms.md](drm-kms.md) — the DRM/KMS interface: EDID, modes, atomic commits, planes, damage, PRIME, buffer-object lifetime, fences, syncobj, per-open state, DRM master per VT.
- [virtio-gpu.md](virtio-gpu.md) — the virtio-gpu driver: asynchronous control queue, MSI-X/INTx completion, cursor queue, blob resources and the host-visible window, `virtio-vga-gl`.
- [console.md](console.md) — the framebuffer console: bitmap font, VT emulator, atomic writes, write-back shadow, write-combining, and how it arbitrates the scanout with DRM clients.
- [vulkan-venus.md](vulkan-venus.md) — Vulkan through Venus: capset, render node, the kernel pieces Mesa needed, and how presenting was proven.
- [zink-and-virgl.md](zink-and-virgl.md) — COSMIC on Zink and on virgl, the Mac GPU path, host-side workarounds, and measured performance.
- [mesa.md](mesa.md) — the Mesa ports: how they are built, which drivers ship, patches, and the v3d driver for the Raspberry Pi 5.

## See also

- [Audio](../audio.md) and [Input](../input.md) — the other two device stacks the desktop depends on.
- [TTY and VT](../servers/tty-and-vt.md) — virtual terminals and VT switching, which decide who may present.
- [Desktop](../desktop/README.md) — the COSMIC session that sits on top of all this.
- [Running](../getting-started/running.md) — `--venus`, `--virgl`, `--no-gpu` and display options.
- [K4 DRM design record](../design/k4-drm-design.md) — the original DRM/sysfs/evdev implementation design.
