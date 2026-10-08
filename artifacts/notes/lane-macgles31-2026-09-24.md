# Lane macgles31, 2026-09-27

Branch `lane/macgles31`, cut from `origin/integ-wave-0924` at 768c169c. Head is `4ed9f5f8`, and the branch is pushed. The worktree has been removed.

The new host QEMU is installed at `~/.local/qemu-gpu-gles31`, with its work dir at `~/.cache/leandros-qemu-gpu-gles31`. The existing `~/.local/qemu-gpu` was not touched.

## Outcome: FIXED

On aarch64/HVF on the Mac, the guest now gets **OpenGL ES 3.1 with SSBOs** through the host GPU:

- `GL_RENDERER = virgl (ANGLE (Apple, Vulkan 1.1.357 (Apple M4 Max …), MoltenVK))`
- guest SSBO limits: vertex 10, fragment 12, compute 12, bindings 32, block size 128 MiB

What now works on wgpu:

- **The greeter** runs on wgpu. gpu-env reports `GL_WGPU=1`, so `ICED_BACKEND=wgpu`. The adapter is `backend: Gl`, with `driver_info "OpenGL ES 3.1 Mesa 25.3.6"`, and all text renders.
- **`ICED_BACKEND=wgpu cosmic-settings`** stays up:
  - more than 6 min on the greeter compositor;
  - more than 3 min inside a logged-in session, with 0 panics and 0 EBADF.
- **The full COSMIC session** comes up with panel, applets and dock.
- Host cost: QEMU RSS about 0.8–1.3 GB, host CPU about 4 % idle.

On x86_64/TCG with the same QEMU, the guest also gets ES 3.1 (10/12 SSBOs), and the greeter renders on wgpu with full text.

## Options

| option | result |
|---|---|
| (a) ANGLE Vulkan on MoltenVK (Homebrew molten-vk 1.4.2) | **ES 3.1**, used. Stock ANGLE capped it at **ES 2.0**. Instrumenting that cap showed only one blocker: `VK_EXT_provoking_vertex`, which MoltenVK does not offer because Metal has no last-vertex convention. Lifting it on Apple gives ES 3.1. The trade-off is that flat varyings take the first vertex. Host limits: 19 vertex SSBOs and 23 fragment SSBOs. ES 3.2 is blocked by the missing EXT_geometry_shader, EXT_tessellation_shader and EXT_texture_border_clamp. KosmicKrisp was not needed; it is also not packaged, and Mesa 25.3.6 predates it. |
| (b) newer ANGLE Metal | **No.** The upstream README table still leaves ES 3.1 and 3.2 blank for Metal. Our revision reports `OpenGL ES 3.0` and GL_INVALID_ENUM for every SSBO query. |
| (c) Apple desktop GL 4.1 | **Dead.** `4.1 Metal - 90.5` has 43 extensions, with no ARB_shader_storage_buffer_object and no compute. |
| (d) Venus | Not re-tried. (a) solved the problem, and the macqemu note's CS error stands. |

## Second bug found and fixed: wgpu text drew only the first glyph

This happened on **every** GLES host, and on the old ANGLE/Metal build it was even worse.

- **Cause.** virglrenderer declares every vertex input `in vec4` unless `use_integer` is set, which upstream enables only for vendor "ARM" or `VIRGL_USE_INTEGER`. Integer attributes then feed a float input, which is undefined in ES.
  - On ANGLE/Vulkan, an instanced integer attribute returned instance 0's value for every instance.
  - On ANGLE/Metal, nothing was drawn at all.
  - cryoglyph, the renderer behind wgpu text, feeds glyph instances through u32 attributes, so each string showed only its first glyph (`greeter-es31-before-intfix.png`).
- **Evidence.** A guest `insttest` isolated the fault: the host ANGLE-Vulkan run passed all modes; the guest's integer instanced modes failed on both backends, and so did Metal's.
- **Fix.** `patches/virglrenderer-1.3.0-gles-integer-attribs.patch` sets `use_integer` on every GLES host and computes the integer masks at vertex-element creation, so the ES 3.0 path is covered too. After the fix, all 6 modes pass on Vulkan and both modes pass on Metal.

## Commits

- `c3b620f1` adds `build.sh --angle-vulkan`:
  - builds ANGLE with its Vulkan backend (VMA via googlesource git by hash, and Vulkan-Utility-Libraries);
  - adds `patches/angle-vulkan-moltenvk.patch`, which lifts the provoking-vertex cap, uses the absolute Homebrew `libvulkan.1.dylib` path, and uses the system loader;
  - sets QEMU's ANGLE default to `vulkan`;
  - uses its own prefix and work dir.
  - `ANGLE_DEFAULT_PLATFORM=metal` still switches back at runtime.
- `4ed9f5f8` adds the virglrenderer integer-attribute patch. It applies to both build variants.

## Open

- **Promotion.** Point run-qemu.sh/driver.py auto-detect at `qemu-gpu-gles31`, or rebuild the default with `--angle-vulkan`. For now, use `LEANDROS_QEMU_PREFIX=~/.local/qemu-gpu-gles31`.
- **The default `~/.local/qemu-gpu` lacks the integer fix.** Run `build.sh --force virgl` to add it.
- **Runtime dependencies.** The new prefix needs `brew` molten-vk and vulkan-loader at runtime.
- **EBADF.** On the Metal A/B boot the greeter crash-looped with `Io error: Bad file descriptor`. That is the wgpuebadf symptom, and it was not seen on the Vulkan path.
- **x86 `insttest` result is a harness artifact.** It picked an uninitialised pbuffer config on GBM, so treat its x86 result as invalid.
- **Probe sources and screenshots** are in `lane-macgles31-files/`.
