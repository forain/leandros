# Lane macgpu — 2026-09-25

Branch `lane/macgpu` (from `lane/gpudefault` a03f1c1), head `ee7e01c`, pushed.
Worktrees: Mac `~/code/leandros-macgpu`, laptop `~/Projects/leandros-macgpu`.

## Scope changes during the lane
- Goal 1 (a GPU QEMU for the Mac) was DROPPED on the user's instruction. The new lane `macqemu` owns it, and its binary is `~/.local/qemu-gpu/bin/qemu-system-aarch64`.
  - Before the drop, a ~30-line launcher that dlopen()s UTM's `qemu-aarch64-softmmu.framework` did run (`-version`, egl-headless, virtio-gpu-gl-pci). Auto-mode denied it as "Security Weaken" (UTM's sandboxed QEMU run outside its sandbox), so it was removed and not committed.
  - Building UTM's own dependency chain needs full Xcode, because ANGLE comes from WebKit's xcodeproj via xcodebuild. Only CommandLineTools is installed.
- UTM test VM "LeandrOS macgpu" was created and then deleted with utmctl. The user's "LeandrOS aarch64" VM was not touched.
- **Privacy incident**: one full-screen `screencapture` accidentally grabbed the user's desktop (a chat app). It was deleted immediately and never reused.

## FIXED: d44b1e0 alone did nothing, and the virgl path presented garbage
1. `VIRTGPU_RESOURCE_CREATE` never sent CTX_ATTACH_RESOURCE, so the host reported `Illegal resource 16` and put the context into the error state.
2. Page flips of a virgl 3D fb went through the CPU-copy present into console res 1, which put stale staging memory on screen as noise. `present_blob_fb` now does SET_SCANOUT(3D res) + RESOURCE_FLUSH. A trace confirmed that SET_SCANOUT had never targeted a 3D resource.
3. `VIRTGPU_RESOURCE_INFO` refused virgl BOs, so the Mesa dmabuf import failed and cosmic-comp logged "create_immed … invalid wl_buffer". That killed the panel.

## Evidence
| path | greeter | key p50 | session | drmsmoke |
|---|---|---|---|---|
| laptop x86_64/KVM Venus→zink (ANV) | ok | 0.085 s (11/12) | full (panel+dock) · zinkbench 24.0 fps before / 22.3 fps after the fix, 0 zero-flip samples | 0 fail |
| laptop x86_64/KVM virgl (iris) | noise/black → **ok** | 0.061 s | wallpaper, **no panel/dock** | 0 fail |
| Mac aarch64/HVF virgl/ANGLE-Metal (macqemu QEMU) | **ok** (pixel-verified) | 0.036 s | wallpaper + launcher overlay (Super), **no panel/dock** | 1 fail: FLIP_EVENT_DELIVERED_ON_FENCE 17/32. It is a 2D dumb-fb path my change does not touch; flips run at ~15 ms, host-paced |
| Mac aarch64/HVF plain | – | – | – | 0 fail |

## Open
- Under virgl the panel/dock still do not appear on either host. After fix 3 there are no protocol errors, and the panel log stops at "Requires relayout: resizing list" + backtrace. Next step: run with the panel's `RUST_LOG` and check PRIME imports of handles that `RESOURCE_INFO` still reports unknown (0x7A/0x7C/0x8B/0x90 on aarch64).
- FLIP_EVENT_DELIVERED_ON_FENCE on the Mac GPU QEMU (see the table).
- Venus on the Mac: not feasible with any QEMU today. UTM's virglrenderer has no vkr, and upstream vkr is Linux-only (memfd/eventfd/render-server). The only macOS Venus stack is libkrun/krunkit (patched virglrenderer + MoltenVK), which is compute-oriented and has no scanout. KosmicKrisp would be a possible host ICD later. A scanout would still need a GL/ANGLE import of Venus blobs.

## Re-test on origin/integ-wave-0924 @ ee7c029 (macgpu + virglpanel), laptop x86_64/KVM
- virgl (iris): greeter key p50 **0.054 s** (11/12). **Full COSMIC session with panel and dock**, pixel-verified, and QEMU stderr clean.
- Venus→zink (ANV): greeter p50 **0.084 s**, full session with panel and dock. zinkbench **23.4 fps**, 0/33 zero-flip samples.
- drmsmoke: the first scripted run on each path reported failed=1 (the case was not captured). The next 4 runs (virgl ×2, venus ×2) were all failed=0, so this is intermittent and the case is still unidentified.
