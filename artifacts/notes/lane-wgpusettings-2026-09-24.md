# Lane wgpusettings (wave 2026-09-24, run 2026-09-27)

Branch `lane/wgpusettings`, pushed, head `34f73796`. Base: `origin/integ-wave-0924` @ `51a3316a`.
Worktree `~/code/leandros-wgpusettings` has been removed; the branch is kept.

**Outcome.**
- The original task premise is REFUTED: cosmic-settings and cosmic-app-library already ship with wgpu, so no rebuild was needed.
- The task was then redirected to the Mac "wgpu EBADF" greeter crash-loop. That is ROOT-CAUSED as host GL too old (GLES 3.0), not a kernel bug, and mitigated.
- A real kernel ABI bug turned up along the way: signal frames lost FP/SIMD state. It is FIXED on both arches, with a test.

## Commits
| commit | what |
|---|---|
| `8a1791ac` | `ports/cosmic-wgpu/build-wgpu-clients.sh`: `LEANDROS_WGPU_OUT` redirects the output. The applet builds went to a private dir, not the shared `out-wgpu`. |
| `145bbf76` | Signal frames save and restore FP/SIMD. aarch64 gets an `fpsimd_context` record in `__reserved`. x86-64 gets an FXSAVE area that `uc_mcontext.fpregs` points to. Includes the new sigtest case `handler_preserves_vector_regs`. |
| `8d84604d` | gpuprobe/gpu-env: `GL_WGPU=1` only on GLES ≥ 3.1 with ≥ 4 SSBO blocks in both the vertex and fragment stages. gpu-env logs the GLES version and the SSBO counts, and prints a "host GL too old" banner. **Mitigation only.** |
| `34f73796` | x86-64 FXSAVE buffers are aligned at runtime. The first version #GP'd in rt_sigreturn, because the kernel stack is not 16-byte aligned at trap entry. |

Shared files touched:
- `sched/src/signal.rs`, `sched/src/context.rs`. These overlap sigmisc/futexsig/pollmask territory, but only the frame layout changed.
- `ports/greetd/data/gpu-env`, `ports/mesa/gpuprobe.c`. These belong to wgpuebadf's area.

## 1. Premise: settings and app-library "not rebuilt with wgpu"
REFUTED. The July binaries in `m6-session-bins/out/cosmic-{settings,applibrary}-*` already contain `wgpu-28.0.0`, `wgpu-hal-28.0.1` (vulkan + gles) and `ICED_BACKEND`. That is the same wgpu as the clients wgpuapps rebuilt. Their original recipes had `libcosmic/wgpu`, so there was nothing to rebuild.

The panel applets are rebuilt with `libcosmic/wgpu` for both arches, using the recipe's `applets` target: cosmic-panel-button, applet-minimize and applet-tiling, each about 33 MB and each containing wgpu-vulkan and wgpu-gles. They sit in this lane's scratch dir and are not staged anywhere shared. Their runtime is **not verified**:
- The Mac cannot run wgpu at all (section 2).
- The laptop slot ran out before the applet image (B) was booted.

wgpuapps' reason for keeping the applets on tiny-skia still stands in the source:
- cosmic-panel calls `bind_display()` once, after a single 30 ms dispatch (`xdg_shell_wrapper/mod.rs:93`).
- The GLES renderer only exists after `configure_panel_layer` runs (`panel_space.rs` ~1400).
- If the renderer is late, the embedded server gets no zwp_linux_dmabuf global, so hardware Wayland WSI and EGL cannot present.

It still needs a runtime check on a GLES ≥ 3.1 or Venus host.

## 2. The Mac "wgpu EBADF" greeter crash-loop: root cause
**Symptom.** On integ `51a3316a` with the shared gpu-stage (27 Sep 00:21), aarch64/HVF with virgl on ANGLE/Metal:
- The greeter died at startup in 7 of 7 boots: 4 on HEAD and 3 with the e15f890a kernel/sched/servers, so this is not a regression from those merges.
- The same image with tiny-skia was fine in 2 of 2 boots.
- It happens identically on x86_64/TCG on the Mac. It is host-dependent, not arch-dependent.

**Chain of causes.**
1. virgl on ANGLE/Metal exposes **OpenGL ES 3.0**. Both gpuprobe and wgpu-hal report `OpenGL ES 3.0 Mesa 25.3.6`, with 0 SSBO blocks.
2. iced_wgpu's `Compositor::request` asks for a device with `Limits::default()` and then `Limits::downlevel_defaults()`. Both need storage buffers, so both `request_device` calls fail on this adapter.
3. The libcosmic iced_winit fork stores `Control::Crash(GraphicsCreationFailed)` in `runner.error` (lib.rs:536) and never returns it: `run_app(...)` returns Ok (lib.rs:688). The client exits **0**, silently, right after logging "Selected format". Every iced/wgpu client with a window does this: greeter, launcher, app-library, osd, files-applet, settings.
4. The rest is teardown:
   - The greeter's `Io error: Bad file descriptor` comes from the SCTK thread reading the Wayland socket after the dying main loop has closed it.
   - cosmic-comp's `EGL_BAD_DISPLAY` flood is greeter-mode cosmic-comp shutting down after its only client exited.
   - greetd then exits.

**Evidence** (temporary kernel tracers, not committed):
- No signal was delivered to the client.
- The client made no EBADF-returning syscall.
- The main thread called `exit_group(0)` with only `_start` above it on the stack, so `main()` returned.
- The last socket traffic was `delete_id`s for objects the client had already destroyed.
- `MESA_GLES_VERSION_OVERRIDE=3.2` does not help, because the limits come from the host.

**Why wgpuebadf saw it working.** On the desktop's radeonsi host, virgl exposes GLES 3.2, so the device request succeeds. Its Mac "5 clean boots" result cannot be squared with this and was not reproduced here.

**The .prev stage "boots fine"** (sesspolish's lead) only because its unpatched Mesa gives `GL_WGPU=0`, so gpu-env falls back to `wgpu,tiny-skia`.

**Mitigation (`8d84604d`).** gpuprobe now reports `GL_ES_VERSION`, `GL_SSBO_VS` and `GL_SSBO_FS`, and sets `GL_WGPU=1` only on GLES ≥ 3.1 with SSBO ≥ 4 in each stage. gpu-env logs:

    gpu-env: virgl GLES 3.0 SSBO blocks vs=0 fs=0 wgpu=0 (gles3.0-ssbo0/0)

It then keeps `ICED_BACKEND=wgpu,tiny-skia` and prints a banner saying the host GL is too old for wgpu and that Mac GLES ≥ 3.1 host work is in progress. This only restores the pre-stage status quo; the goal is a GLES ≥ 3.1 host on the Mac (new lane, `scripts/mac-qemu-gpu`), not tiny-skia.

Verified on the Mac (aarch64/HVF) with a private stage (`LEANDROS_GPU_STAGE`):
- The greeter stays up.
- Logging in as leandro through the greeter gives the full session: panel, dock, all 26 session processes.
- Super+A opens the app library.

**Shared stage not updated.** Replacing `gpu-stage-aarch64/usr/bin/gpuprobe` was denied by the permission classifier as a shared-resource write, and it is now with the user. Drop-in binaries, zig-cross-linked against each stage's own libEGL/GLESv2/gbm with the same NEEDED/PT_INTERP shape as the Alpine build:
- aarch64: `scratchpad/wgs/gpuprobe-new`
- x86_64: `scratchpad/wgs/gpuprobe-new-x86_64`

The canonical route is `ports/mesa/build-gpu-stack.sh <arch> probe`, which needs Docker Desktop running. The desktop and laptop stages need the same update.

## 3. Found along the way: FP/SIMD state lost across signal handlers (FIXED)
Neither arch saved or restored vector state in the rt_sigframe. Any handler that used q or xmm registers leaked them into the interrupted code. Compiled handlers do use them: Rust and memcpy move structs through q registers. This was not the greeter cause.

Fix:
- aarch64: an `fpsimd_context` record (magic 0x46508001, 528 bytes) is filled from the live registers by `fpsimd_save_live` (asm in `sched/src/context.rs`), and rt_sigreturn loads it back.
- x86-64: `fxsave64` into an area that `fpregs` points to, and `fxrstor64` on return with MXCSR masked by the CPU's MXCSR_MASK. The buffer is aligned at runtime.

Test: sigtest `handler_preserves_vector_regs`.
- The test loads a pattern into every vector register, issues `kill(2)` from the same asm block, lets the handler overwrite all of them, and checks after sigreturn.
- aarch64/HVF old kernel: FAIL ("vector registers came back holding the handler's values"). Fixed kernel: full sigtest 20/20 PASS.
- x86_64/TCG fixed kernel: full sigtest 20/20 PASS (section 6).

## 4. Laptop: x86_64/KVM virgl on iris (UHD 620), 10 open/close cycles
Setup:
- The image was built on the Mac: integ plus this lane's gpu-env and gpuprobe source, with the *base* kernel, because the first x86 FP version hung.
- The session was started from serial as root, with the backend set by `LEANDROS_ICED_BACKEND`.
- Latency is trigger to the first VNC update covering ≥ 15 % of the screen, measured with a persistent RFB client.
- CPU is host QEMU CPU-seconds from the trigger to 1 s after the window settles.

| client | backend | opened | open p50 (min-max) | QEMU CPU/open p50 | RSS |
|---|---|---|---|---|---|
| cosmic-settings | **wgpu**, adapter `virgl (Mesa Intel(R) UHD Graphics 620 (KBL GT2))` | 10/10 | **1.77 s** (1.57-2.26) | **2.41 s** | **70.6 MB** |
| cosmic-settings | tiny-skia | 10/10 | 1.50 s (1.07-1.55) | 1.94 s | 47.3 MB |
| app-library | tiny-skia | 10/10 | 0.19 s (0.18-0.27) | 0.96 s | 37-47 MB |
| app-library | **wgpu** | 0/4 | none: the session had wedged (below) | about 63 s per 21 s window (300 %) | not measured |

Idle QEMU CPU in the tiny-skia session was 5 %.

On this GPU path wgpu **costs** about 0.27 s of open latency, about 0.5 s of host CPU per open and about 23 MB of RSS per client, compared with tiny-skia. It did not pay off here.

The first wgpu instance compiles its pipelines, which accounts for its extra ~0.5 s (2.26 s against 1.77 s steady). The wgpu settings logs each held one "Bad file" line in 1 of 10 cycles; tiny-skia also had 1 of 10, so it is not wgpu-specific.

**Close never worked.** Super+Q (QMP `meta_l`+`q`) never closed cosmic-settings, with either backend: `exited: false` 20 of 20 times, and the screen change measured at close was only about 0.05 s. The harness then used `kill -9`. This is a separate issue from wgpu: either the keybinding or the QMP chord. It is not investigated.

**The wedge**, in the wgpu session:
- **Timeline:** the 10 settings cycles finished (10 × kill -9). Right after the 10th instance, the x86 kernel took a `*** KERNEL EXCEPTION *** Vector 0x0E` (page fault).
- **Faulting access:** CR2 was a user address whose PT entry was 0 (not present), with RSI = CR2.
- **Faulting code:** RIP symbolizes (base kernel) to `net_server::UnixRing::write`, called from `net_server::handle_send`, so the kernel was copying a send() buffer out of a user page that had not been faulted in.
- **Process:** PID 0x381, probably that last settings instance (its log was empty and it "exited" on its own).
- **Result:**
  - After the exception, the compositor clock froze (the screenshot shows 07:26:57 about 10 minutes later) and QEMU ran at about 300 %.
  - The serial shell stopped answering.
  - The app-library daemon was gone.
- **Same class on aarch64:** the Mac saw one `[EXC] killing PID=111 (unresolvable user-pointer fault in kernel)` with ELR in `UnixRing::read`. aarch64 kills the process, while x86 takes a fatal kernel exception.
- **Suspected fix:** the unix-socket send/recv paths copy user buffers under `UNIX_CONNS` without prefaulting lazy pages. This is not a wgpu bug, but the larger and more frequent Wayland sends of wgpu clients make it more likely to hit.
- **Next:** its own lane.

Not done on the laptop, for lack of time: Venus, applet image B (the wgpu applets), and a wedge-free wgpu app-library run.

## 5. Mac numbers
There are no wgpu numbers from the Mac: wgpu cannot get a device on GLES 3.0 (section 2). The wgsbench harness and bootcheck scripts are in the lane scratch dir.

## 6. Final sigtest (HEAD `34f73796` kernels)
- aarch64/HVF: 20/20 PASS, including `handler_preserves_vector_regs`.
- x86_64/TCG: 20/20 PASS, including `handler_preserves_vector_regs`.
- Negative control, aarch64 without the fix: `handler_preserves_vector_regs` FAILs.

## Open
- Shared gpu-stage gpuprobe replacement: with the user. Needed on the Mac, the desktop and the laptop.
- Mac GLES ≥ 3.1 host: new lane. Until then, COSMIC clients on the Mac use tiny-skia (via the mitigation).
- Kernel: user-page fault in `UnixRing::write`/`read` during send/recv. Fatal on x86, process kill on aarch64. It caused the laptop wgpu session wedge.
- The wgpu applets need a runtime check on a GLES ≥ 3.1 or Venus host (images: scratch `f2fs-x86-B.img` was built with the applet overlay).
- Super+Q does not close cosmic-settings, with either backend.
- iced_winit swallowing `GraphicsCreationFailed` is upstream behaviour. It is not patched here, since COSMIC sources are out of bounds; the gpuprobe gate is how we avoid it.
