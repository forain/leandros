# Lane finalverify — 2026-09-27

Integration verifier for wave 2026-09-24. Branch `integ-wave-0924`: base 16782a75, head **2f4214bd** (pushed).
Harness scripts (all in the session scratchpad `fv/`): `suites.sh` (in-guest suites), `fvsess.py` (greeter login, panel/dock, Super+T and typing, Super+A, Super+W then Esc, cosmic-settings open/close ×N), `fvmag.py`, `fvidle.py`, `stallwatch.py`.

## Commits added on integ
- `bc52498a` **run-qemu, driver: default to the GLES 3.1 Mac QEMU; open a viewer on interactive GPU boots.**
  - On macOS the prefix order is `~/.local/qemu-gpu-gles31`, then `~/.local/qemu-gpu`, then `$PATH`. `LEANDROS_QEMU_PREFIX` still overrides.
  - The ANGLE-on-Vulkan prefix is detected from the libvulkan path baked into its libGLESv2. The launchers check that loader and Homebrew's MoltenVK ICD, and set `VK_DRIVER_FILES` unless the caller already chose an ICD.
  - If either is missing they print what to `brew install` and fall back. With an explicit prefix they exit instead.
  - **Viewer**: Screen Sharing refuses no-auth VNC. It prompts for a password that nothing accepts; I verified this with an RFB probe.
    - On an interactive run (stdout is a TTY and `LEANDROS_NO_VIEWER` is unset), the script gives QEMU a random one-run VNC password (`-object secret` plus `password-secret=`). It waits until *this* QEMU's port is listening, then runs `open vnc://:pw@host:port`, so there is no prompt. This was verified with the window captured showing the COSMIC desktop.
    - Non-interactive runs and driver.py keep the no-auth listener and never open a window. Verified: no viewer opened, security types [1].
    - The banner mentions `--no-gpu`.
  - SKILL.md and the `build.sh` header were updated.
- `2f4214bd` **build: on macOS pick an SDK the linker can read when SDKROOT is unset.**
  - New `scripts/darwin-sdk.sh`, sourced by build-all, build-userland, and the busd, greetd, mkfs-fat and mkfs-xfs ports. `input-stack` is `/bin/sh` and does no host link.
  - Reproduced the failure: the default MacOSX27.0.sdk gives a tapi `arm64e.x1-macos` error. The script picks MacOSX26.5.
  - Verified three ways: busd rebuilt from an empty host target, and a full `./scripts/build-all.sh` with SDKROOT unset gave BUILD_EXIT=0.
- **NOT DONE: merging `origin/lane/maclag` @ 3d7b410b.** The auto-mode classifier denied the merge ("Modify Shared Resources"), so it needs the user's permission. The stale-image warning check was therefore not run.

## Matrix (integ images built from 16782a75 + scripts; kernel/userland identical to head)
| item | result |
|---|---|
| (a) Mac aarch64/HVF `--no-gpu` | **PASS 14/14 RC=0**: sigtest 20/20 (incl. vector regs, futex restart ×5, stop remainder, ppoll/pselect sigmask), timertest (blocked_read_sa_restart PASS), pthreadtest, polltest, sigtest2, jobtest, smpwaketest, memtest (lost_kib_per_death=0), forktest, exectest, killmt 100/100, drmsmoke failed=0, uptrtest 112/0, vfstest |
| (c) desktop x86_64/KVM `--no-gpu` | **PASS 14/14 RC=0**, same numbers |
| (b) Mac GPU session, gles31 default | **PASS**. gpu-env: `ICED_BACKEND=wgpu WGPU_BACKEND=gl`, `wgpu=1`, renderer `virgl (ANGLE (Apple, Vulkan 1.1.357 (Apple M4 Max…MoltenVK)`; greeter adapter `backend: Gl`, `OpenGL ES 3.1 Mesa 25.3.6`. Greeter with text; login; panel + dock + applets; Super+T + typing; Super+A; Super+W/Esc; cosmic-settings 5/5 open, Super+Q closes; 0 kernel exceptions; idle 7.5 %. 300 s hands-off: 300 VNC updates, max gap 1.04 s, guest uptime = host time |
| (c) desktop Venus session | **PASS** ×2: session, terminal, Super+A, Esc closes overview, settings 5/5 (Vulkan), 0 panics |
| (c) desktop virgl session (GALLIUM_THREAD=0 auto) | **PASS ×1, FLAKE ×1** (see open) |
| (c) zinkbench `--zink` (DRM_STATS=true local build) | **20.04 / 20.37 fps**, 0/37 and 0/40 zero-flip samples, timeouts=0 (baseline 20.5–21) |
| (c) audio-glitch-test | **PASS**: no holes, max zero-run 3363 ms (the known in-game run) |
| (c) greeterstorm `--venus` | all respawns returned. 5 deaths: −135 pages/death; **15 deaths: −98 pages/death** (vmm.rs:780/860 user sites, +98/death). Greeterleak's run was −26; see open |
| (d) laptop virgl (iris) | **PASS**. **wgpu settings open/close ×10**: 10/10 opened (ES 3.2), 10/10 closed by Super+Q, serial alive, 0 kernel exceptions. **No wedge**, so the uptrfault fix holds. Esc closes the overview (diff 0.0) |
| (d) laptop Venus (ANV) | **PASS**. Settings ×10 all opened (Vulkan) and closed, 0 panics, 0 kernel exceptions, idle 5 % |
| run-qemu.sh smoke (Mac) | aarch64 (image rebuilt without SDKROOT) greeter renders. x86_64 (TCG) greeter renders. Both use the gles31 QEMU |

## Open / found
1. **Mac gles31: cosmic-settings sometimes shows a solid magenta window.** 1/5, 1/4 and 4/8 instances. It persists ≥20 s, and the log shows no error. Not seen on the laptop or desktop. The likely suspect is a host-side ANGLE-Vulkan/MoltenVK path. It is not proven to be guest-side: an A/B against main was impossible, because main plus the current stage has no GPU.
2. **Mac: two multi-minute stalls** (display frozen, serial unresponsive, then recovered on its own): 8 min in one run and 6 min in another. They did not reproduce in 8 settings cycles with a VNC main-loop watchdog running, or in 300 s idle. Suspect a QEMU main-loop block in virglrenderer/ANGLE. `fv/stallwatch.py` samples the stack on the next occurrence.
3. **Desktop virgl flake (1 of 2 runs):**
   - After Esc on the overview, the dock was gone and a pid re-registered evdev.
   - The next settings launches exited immediately.
   - The host logged `vrend_set_framebuffer_state … Illegal surface 205 / failed to dispatch DRAW_VBO` in ctx 2. This is the class hostgpufault also saw on base.
   - Serial input then lost about 100 bytes, which left the shell in continuation mode.
4. greeterstorm at −98 pages/death, against −26 from greeterleak (wave 0918). There is no valid main baseline, for the same stage reason as item 1.
5. The shared gpu-stage gpuprobe prints `GLES ? SSBO ?`. This is the wgpusettings open item. Actual ES 3.1 is confirmed from the wgpu adapter.
6. Laptop panel has no clock: `~/code/leandros-artifacts/m7w-applet` is missing on the laptop. This is a machine setup gap.
7. On the desktop, a main build plus greeterstorm job that I started (pid 1135309) is still running. It holds `~/.leandros-build.lock` until the build ends, then runs an invalid (no GPU) storm for up to 60 min. Killing it was denied by the classifier, so the user decides. The worktree is `leandros-siblings/leandros-fvmain`.
8. Incident: while probing Screen Sharing I raised a password modal on the user's screen for about 1 min, then quit it.
