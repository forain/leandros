# Lane gpudefault — 2026-09-25

Branch `lane/gpudefault` (from origin/integ-wave-0924 `d16c563`), head `a03f1c1`, pushed.
Worktrees: Mac `~/code/leandros-gpudefault`, desktop `/run/media/forain/samsung970pro512/leandros-siblings/leandros-gpudefault`.
Scratch on desktop: `/run/media/forain/samsung970pro512/gpudefault-tmp/` (build.sh, gd-vnclag.py latency probe, zb/ zinkbench logs).

## Phase 1: which GPU path works, per machine and arch
| host | arch/accel | path | status |
|---|---|---|---|
| desktop (RADV) | x86_64 KVM | Venus→zink | works: greeter, full session (uid 1000 through greetd), 20.6 fps |
| desktop | aarch64 TCG | Venus→zink | works: gpuprobe reports zink/RADV, greeter renders at 1280x800, key p50 79 ms |
| desktop | both | virgl | the GL context works (`virgl (… radeonsi …)`). It is now the fallback when zink fails |
| Mac, Homebrew QEMU 11.1.1 | aarch64 HVF | none | no virglrenderer and no `*-gl` devices. run-qemu prints a warning and the guest refuses COSMIC |
| Mac, UTM 4.7.5 (QEMU 10.0.2) | aarch64 HVF | virgl over ANGLE→Metal | GL works: `virgl (ANGLE (Apple, Apple M4 Max, OpenGL 4.1 Metal))`. **Venus is impossible**: UTM's virglrenderer has no vkr/venus and there is no MoltenVK. Scanout was noise, because the kernel 2D-transferred stale guest pages over the host-rendered virgl texture. That is fixed in `d44b1e0`, but the fix is **not pixel-verified** (`screencapture` stopped working mid-lane) |
| laptop (UHD 620) | x86_64 KVM | virgl (iris) | expected to work. **Venus is blocked**: `vulkan-intel` (ANV) is not installed, only `radeon_icd.json` is present. Fix: `sudo pacman -S vulkan-intel` (needs the user). Not run, because the laptop was owned by sessmisc |

## Phase 2 (what changed)
- `ports/mesa/build-gpu-stack{,-alpine}.sh`: a single megadriver `zink,virgl,softpipe` (+`v3d` on aarch64), the Venus ICD, the Vulkan loader, libzstd and `gpuprobe`. It is built in an Alpine container of the target arch: podman on the desktop takes about 3 min, Docker Desktop on the Mac takes about 4 min. The output goes to `leandros-artifacts/m3-gl-stack/gpu-stage-<arch>`, and both stages are synced to the Mac and the desktop. **Other machines need them copied**. mkfs prints a warning when they are missing.
- `ports/mesa/gpuprobe.c`: `caps` (capsets over GETPARAM) and `gl` (GBM+EGL the way smithay does it, which yields GL_RENDERER; rc=2 when the renderer is software).
- `ports/greetd/data/gpu-env`: tries zink first, then virgl. Each is verified, and if neither works the result is `refused`. It is sourced by `/etc/profile`, `greeter-env`/`greeter-real` and `start-cosmic-leandros`. `GBM_ALWAYS_SOFTWARE=1` is removed everywhere. softpipe is only possible as an explicit opt-in: `/etc/leandros/allow-software-render` or `LEANDROS_RENDERER=software`.
- init: when greeter-real exits with 78, it prints a `NO GPU RENDERER` banner and stops respawning. The serial login is unaffected.
- run-qemu.sh: `--gpu auto` is now the default. It picks venus when QEMU has a `venus=` property and a render node exists, virgl when only GL is available, and none otherwise. Explicit choices are `--no-gpu`/`--venus`/`--virgl`. driver.py is unchanged and stays the headless path.
- **Landmine fixed**: mkfs used to prefer `~/code/leandros-artifacts/m6-session-data/start-cosmic-leandros` over the repo copy. That copy is months old and has `GBM_ALWAYS_SOFTWARE=1`, which means earlier "LEANDROS_ZINK=1" serial runs may have been running a stale launcher. The repo copy now wins.

## Evidence
- Greeter keystroke latency, ORIGINAL cosmic-greeter, desktop x86_64/KVM zink: **p50 0.050 s** (0.049–0.061, 12/12). The latency is measured over a persistent VNC connection, and it includes the 30 ms key hold. Softpipe baseline: 35.5 s.
- zinkbench, default (no `--zink`): **20.59 fps**, 0/41 zero-flip samples, 0 timeouts (with DRM_STATS=true as a local, uncommitted change).
- drmsmoke failed=0 on x86_64/KVM venus and on aarch64/HVF plain.
- aarch64/HVF with no GPU: the refusal banner appears and serial tests are unaffected.

## Open
- Pixel-verify the virgl scanout fix on UTM, and verify virgl/iris on the laptop.
- Clients (greeter, panel, applets) still draw their widgets with iced **tiny-skia** (CPU), because the binaries were built without wgpu. The compositing and blur now run on the GPU. Moving the clients to the GPU needs rebuilding the clients with iced's wgpu backend (that is a build flag, not a source patch).
- Every harness that needs a greeter under driver.py (greeterleak and others) must now pass `--venus`, or run on a GPU host.
- UTM contains two test VMs, "LeandrOS gpudefault" and "gpudefault2" (the second is in /tmp/gd-utm). Deleting them is left to the user. Removing the first VM's images hit a macOS container-permission hang.
