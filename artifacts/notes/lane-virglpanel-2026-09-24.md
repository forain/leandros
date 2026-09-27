# Lane virglpanel, 2026-09-25

Branch `lane/virglpanel` (from origin/integ-wave-0924 6de1f1e, plus a merge of origin/lane/macgpu ee7e01c because the virgl scanout needs it). Pushed.
Worktrees: Mac `~/code/leandros-virglpanel`, desktop `/run/media/forain/samsung970pro512/leandros-siblings/leandros-virglpanel`. Desktop scratch: `/run/media/forain/samsung970pro512/virglpanel-tmp/` (build.sh, type.py, zb/).

## Bug 1: no panel or dock under virgl (FIXED, `4544e3d`)

**Root cause (in the kernel, not in COSMIC):** PRIME_FD_TO_HANDLE gave a dumb or virgl-3D BO's importer the exporter's own global handle number. When the importer later called GEM_CLOSE or DESTROY_DUMB, or its open was released, `free_dumb` retired the *exporter's* handle.

Under virgl this is the normal flow. Mesa's virgl winsys imports every client dmabuf with PRIME_FD_TO_HANDLE + RESOURCE_INFO, then closes the handle when the wl_buffer goes away. Every later import of that buffer then landed on a retired handle, which the serial log shows as `[DRM] RESOURCE_INFO: unknown bo_handle=0x81/0x83/0x91/0x96`.

What happened next depended on the build:
- With macgpu's tree, the panel hung silently. The log stopped about 1 s after the panel started.
- With macqemu's snapshot, cosmic-comp posted `zwp_linux_buffer_params_v1.invalid_wl_buffer`, which is **"Protocol error 7"**. cosmic-panel then spun forever on the dead connection and never exited: procs stayed at 30.

The Venus/zink path was unaffected because blob BOs already had per-open imports (`prime_import_blob`).

**Fix:** `prime_import_dumb` gives each importing open its own alias handle. The alias holds one reference on the primary record and is deduplicated per (object, open). An alias resolves to the primary's host resource, fence and export reference. `free_dumb` retires an alias without touching the exporter's handle.

**Also fixed:** dumb handles came from a non-atomic `static mut` counter starting at 1, which could collide with the blob range after 0x4000. They now share `NEXT_BLOB_HANDLE`.

**drmsmoke:** `PRIME_FD_TO_HANDLE_OTHER_OPEN_DUMB` asserted the old echo semantics. It now asserts four things: a distinct handle, dedup on re-import, the same pages, and that the exporter's handle survives the importer's DESTROY_DUMB (`cae04fb`).

## Bug 2: log spam exhausting the guest (FIXED, `d674baa` and `316a250`)

- `greetd.log` is on the root **f2fs** (`/var/log`), not tmpfs.
- f2fs is **not** the problem. Appending 200 MB added +80 KiB of used memory. Two runs of 30k line writes added +0 KiB (one +340 KiB warm-up).
- It is not a kernel leak either. Diffing kmemstat during the loop put the growth at `mm/src/vmm.rs:692`, the lazy user page fault: +28.5k pages/min, all user heap.
- **The actual cause:** cosmic-session's launch-pad forwards each child line through an **unbounded channel**, so its heap grows whenever its sink lags the producer. The outer (cosmic-session) timestamps lagged the inner (panel) ones by 36 s. That queue is inside COSMIC, so it cannot be fixed here.

**Fixes, all in init:**
1. The chain's stdout/stderr now go through a pipe to a logger child. It writes in 64 KiB chunks and rotates to `greetd.log.1` at 8 MiB, so the log uses at most 16 MiB of disk. The logger is exempt from `sweep_strays` and exits on EOF.
2. A memory-pressure guard, in the style of systemd-oomd. It runs every 2 s while the graphical login is up. If MemAvailable stays below max(96 MiB, RAM/10) for two checks, it SIGKILLs greetd. The existing exit path then sweeps orphans and respawns the chain under backoff. The guard prints a banner on the serial console and also appends a line to greetd.log. The supervisor loop is now `wait4(WNOHANG)` polling every 250 ms.

## Evidence
| check | result |
|---|---|
| Mac aarch64/HVF virgl (ANGLE/Metal) | panel and dock visible (`/tmp/vp-s2.png`, `/tmp/vp-stab-start.png`); 0 `RESOURCE_INFO unknown`; host CPU 1.6–6 % idle |
| stability | used memory flat at 716 MiB over 16 min, 705 MiB at 41 min; log flat at 1116–1120 lines. After about 15 min idle, cosmic-idle blanks the screen (`loginctl lock-session` / `systemctl suspend` not found). That is DPMS, not a crash |
| stability, run 2 (Shift pressed every 60 s so the screen does not blank) | used memory 708.2 → 708.7 MiB over 16 min; panel and dock pixel-verified at the 16 min mark (`/tmp/vp-s3-end.png`); host CPU 6–29 % sampled right after each key, 1.6–6 % idle |
| repro of the spin (build flag `LEANDROS_REPRO_NO_VIRGL_RESINFO`, uncommitted) | rotation keeps the log at 8 MiB + 8 MiB. Growth dropped from ~113 to ~20–25 MiB/min with the pipe sink. The guard fired at a MemAvailable of about 196 MiB: memory went 1767 → 212 MiB, the greeter came back (`/tmp/vp-afterguard.png`), the guest stayed up at uptime 4279 s, and there was no OOM |
| drmsmoke aarch64/HVF virgl | failed=0, flips on fence 32/32 |
| desktop x86_64/KVM Venus/zink | panel and dock visible; drmsmoke failed=0; zinkbench (DRM_STATS=true, local only) ~21 fps (42–43 flips/2 s), 0/40 zero-flip samples; gpudefault baseline was 20.59 |

## Open
- cosmic-session's unbounded launch-pad queue is still an upstream weakness. The guard bounds its effect, and the panel fix removes the trigger.
- Pixels were not captured on x86_64/TCG virgl on the Mac, and the laptop was not tested because sessmisc owns it.
- VmRSS in `/proc/<pid>/status` is a constant, so the guard kills the whole graphical login rather than choosing one victim.

- The macqemu-test scratch images (`~/code/leandros-macqemu-test`) were booted once, to reproduce the pre-fix loop. Their f2fs image was written to.
