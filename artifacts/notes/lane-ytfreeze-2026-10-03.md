# Lane ytfreeze (2026-10-03): "Firefox froze while playing a YouTube video"

Branch `lane/ytfreeze`, base main `0a52161`. aarch64 on the Mac (HVF), GPU QEMU
`~/.local/qemu-gpu-gles31` (virglrenderer 1.3.0 on ANGLE -> Vulkan -> MoltenVK
1.4.2 -> Metal, Apple M4 Max), `-display egl-headless` + VNC.

## What happened (host log of the user's session, QEMU pid 45816)

The freeze was the HOST losing its GPU, not the guest. macOS's unified log
for the user's QEMU (started 02:42:20):

    02:57:28.9  WindowServer  Display:Power Did Sleep   (displaysleep = 2 min, on battery)
    02:58:58.4  kernel  (IOGPUFamily) Cmd queue <private> sleep port_name 25711 value: 00000002 timed out!
    02:58:58.4  qemu    (Metal) ... Caused GPU Timeout Error (00000002:kIOGPUCommandBufferCallbackErrorTimeout)
    02:59:03.4  kernel  (IOGPUFamily) Cmd queue <private> sleep port_name 31439 value: 00000001 timed out!
    02:59:03.4  kernel  IOGPUCommandQueue::retireCommandBuffer: Deny submissions/ignore app[qemu-system-aarc]
                        with 2 GPURestarts in 775 submissions.
    02:59:03.4  qemu    4 x GPU Timeout Error

- A Metal command queue was **sleeping on an event** (a GPU-side wait for a
  MTLSharedEvent to reach value 2) that never got there; the Metal watchdog
  killed it after ~5 s. A second queue, waiting on another event for value 1,
  died the same way 5 s later.
- After two GPU restarts IOGPU **refuses every further submission from the
  process**. ANGLE's context is lost for good: that is QEMU's
  `eglMakeCurrent failed: EGL_CONTEXT_LOST` loop, then every virgl context
  failing (`Unknown 1286` = GL_INVALID_FRAMEBUFFER_OPERATION on the first
  TRANSFER3D, `Illegal command buffer 852011`, `1287` OOM on a copy) and
  `Failed to create fence sync object`. QEMU's own readback of the scanout
  (egl-headless -> VNC) is GL too, so the screen stops updating: "frozen".
  Nothing in that process can recover; only restarting QEMU does.
- Nothing points at memory: no OOM/page-fault error codes, and the guest-side
  numbers below are flat.

## Reproduced the failure signature on demand

`lane-ytfreeze-files/hang.html` (WebGL2 draw with a fragment loop far longer
than the watchdog) on stock host + guest: Metal `GPU Hang Error` then
`InnocentVictim`s, and QEMU's stderr shows exactly the user's lines,
including the same `Illegal command buffer 852011`; 7617 EGL_CONTEXT_LOST
lines in 2 minutes. The guest itself stayed alive (page JS ran to the end of
the 150 s wait, serial shell answered), the screen was frozen from the hang
on. So "frozen" = host GPU gone; any GPU timeout reproduces it.

## Which GPU waits exist, and the fix (host side, launcher env)

ANGLE enables `useVkEventForImageBarrier` / `useVkEventForBufferBarrier` on
tile-based GPUs, Apple included (`vk_renderer.cpp`: `isTileBasedRenderer ||
isSoftwareRenderer`). It then replaces pipeline barriers with
vkCmdSetEvent/vkCmdWaitEvents, and MoltenVK turns every vkCmdWaitEvents into a
GPU-side `encodeWaitForEvent` on a MTLSharedEvent (`MVKEventNative`). Those are
the only GPU-side waits in this stack (no swapchain, no semaphores).

MoltenVK call trace (`MVK_CONFIG_TRACE_VULKAN_CALLS=1`), 60 s of the 1080p
video after desktop boot:

| | vkCreateEvent | vkCmdSetEvent | vkCmdWaitEvents | vkCmdPipelineBarrier | vkCreateSemaphore |
|---|---|---|---|---|---|
| ANGLE default | 5384 | 5384 | 5821 | 48676 | 0 |
| `ANGLE_FEATURE_OVERRIDES_DISABLED=useVkEventForImageBarrier:useVkEventForBufferBarrier` | 0 | 0 | 0 | 56332 | 0 |

With the override there is no GPU-side event wait left that the watchdog
could fire on. `scripts/run-qemu.sh` (`setup_moltenvk_env`) and `driver.py`
(`_qemu_env`) now export it for every ANGLE-on-Vulkan QEMU (a caller's own
value, even empty, wins). No rebuild of the host prefix needed.

Why a wait went unsatisfied is not proven. `MVKEventNative::encodeWait`
(MoltenVK 1.4.2 `MVKSync.mm`) decides at *encode time* on the CPU: `if
(!isSet()) encodeWaitForEvent(value: signaledValue + 1)`, reading
`signaledValue` twice while the GPU may be signalling it. A fresh event read
as 0 then 1 encodes a wait for **2**, which nothing ever signals: exactly the
"value: 00000002" in the user's log (a fresh VkEvent is only ever set to 1).
A native stress test (`lane-ytfreeze-files/evrace.m`: set on one submit, wait
on the next, 0-400 us apart, a stuck wait released from the host through
VK_EXT_metal_objects before the watchdog) did not hit it in 550 000
iterations at full wake (two early "stuck" hits happened while the Mac was in
dark wake and are not conclusive: the GPU may simply have been suspended), so
the window is narrow. The user's hit came 90 s after the display went to
sleep, 16 minutes into playback.

## Soaks (aarch64/HVF, Firefox, 1080p30 VP9+Opus served from the Mac)

Video: `ffmpeg testsrc2 1920x1080@30 + sine`, 12 min, libvpx-vp9 4 Mb/s
(`yt.html` / `yt2.html`: the latter restarts the video when it ends with the
known WebM end-of-file demuxer error, so playback continues past 12 min).
Host sampled by `hostmon.sh` (footprint) and `gfxmon.sh` (vmmap "owned
unmapped (graphics)" = Metal memory); guest by `[GPURES]`.

| run | ANGLE VkEvents | host display | played | result |
|---|---|---|---|---|
| a64-webm1 | on (old default) | on | 12 min (21600 frames, 1110 dropped) | no GPU error |
| a64-soak-off | on | **off** (as for the user) | 12 min (21600 frames) | no GPU error |
| a64-soak2-off | on | off | 25 min (2 restarts) | no GPU error |
| a64-soak3-on | **off** (fix) | off | 25 min (2 restarts, 23 dropped in the last 52 s) | no GPU error |
| x64-video (TCG) | off | off | 5 min (TCG drops most frames) | no GPU error |

- The freeze did not recur in 49 minutes of the old configuration either
  (the user hit it once in 16 min; another QEMU on this Mac logged one
  `GPU Timeout Error` at 11:36 today without the second, fatal one). It is a
  rare race, so these runs show the fix costs nothing, not that it is needed;
  the evidence for the cause is the host log plus the call trace above.
- No leak: host Metal memory 490-650 MB, flat over 18 min (vmmap once a
  minute); guest live host resources 200-256 (estimated 530-680 MiB), never a
  second high-water mark. Created/unreferenced churn ~25 resources/s, all
  released.
- The fix does not cost frames: 60 s of video dropped 74 frames with VkEvents,
  32 without (trace runs).

## Resilience, demonstrated with a host that stops answering fences

`fencedrop.sh` runs QEMU on a private virglrenderer build (DYLD_LIBRARY_PATH)
with a test hook: from 60 s after the first fence on, every glFenceSync is
treated as failed, exactly what a lost context does (`Failed to create fence
sync object`). Shared prefix untouched.

| host virglrenderer | guest | outcome |
|---|---|---|
| stock 1.3.0 behaviour (fences dropped) | fence watchdog | `[GPU] HOST GPU HUNG: no fence answered for 10006 ms; wrote off 17 pending fences`; serial shell answers; Firefox and cosmic-files-applet then die (NULL access in libxul: their GPU work fails once the ring is full of chains QEMU keeps) instead of blocking forever |
| patched (fences retire) | same | one `Failed to create fence sync object (host GL context lost?): retiring fences ...`; watchdog silent; Firefox played all 150 s (4440 frames), no crash |

Induced real loss (`hang.html`, stock host): guest alive, display frozen (QEMU
cannot read back the scanout any more) - the user's symptom exactly.


## Guest/host resilience (if the host GPU is lost anyway)

1. **virglrenderer** (`scripts/mac-qemu-gpu/patches/virglrenderer-1.3.0-retire-fences-on-context-loss.patch`,
   applied by `build.sh`): upstream frees a fence whose `glFenceSync` returned
   NULL (lost context) and never retires it, so QEMU keeps the guest's fenced
   command forever. Now such a fence retires as soon as the ones before it, and
   the first failure is logged once. Built and checked in a private prefix;
   **the shared `~/.local/qemu-gpu-gles31` was not changed** (install with
   `build.sh --angle-vulkan --force virgl` when no QEMU runs from it).
2. **Kernel fence watchdog** (`drivers/src/virtio_gpu.rs`, `fence_watchdog`
   from `ctrlq_tick`): with fenced work pending and no fence answered for
   10 s (Linux's DRM scheduler job timeout), the host is declared hung, every
   pending fence is written off (retired in the accounting, as
   `ctx_abandon_fences` does for a destroyed context; the chain stays the
   device's), out-fence fds, VIRTGPU_WAIT and flip events return, and
   submissions fail at once instead of spinning out the ring-room bound. While
   hung, newer fences are written off after 1 s; the next fence the host
   answers clears the state. Serial: `[GPU] HOST GPU HUNG ...` /
   `[GPU] host answers fences again ...`.
   This cannot bring the picture back (the host GPU is gone for that QEMU), but
   the guest keeps running: shells, audio, closing apps, saving work.

## Tests

`runtests --virgl --suite regress --suite drm` (13 suites + vfstest +
nettest + drmsmoke): aarch64/HVF all PASS; x86_64/TCG all PASS except one
`timertest` case (see Open), which passed 3/3 on rerun. memtest's new
`mempolicy_single_node` PASS on aarch64 and x86_64. No `HOST GPU HUNG` in any
normal run.

## Also in this lane

- `get_mempolicy` (aarch64 236 / x86_64 239), `set_mempolicy`, `mbind`: Linux
  on one NUMA node (MPOL_DEFAULT, node 0). The ENOSYS in the report came from
  libnuma's constructor (DT_NEEDED of libx265 <- libavcodec) when Firefox
  started a media process (pid 723): benign, but now answered like Linux.
- `[GPURES]` host-resource census in the guest kernel: live resources and an
  estimated size, logged on each new high-water mark of 256.
- `build-all.sh`: `f2fs-data1` is a clone of data0 (`cp -c` / reflink) instead
  of a second 3.4 GiB copy per arch; the Mac's disk was full (455 MiB free) and
  the x86_64 image write failed mid-lane.

## Open

- **Install the virglrenderer patch** into `~/.local/qemu-gpu-gles31`
  (`scripts/mac-qemu-gpu/build.sh --angle-vulkan --force virgl`, back up the
  prefix first; only `lib/libvirglrenderer*` and `bin/virgl_test_server`
  change). Not done here: other lanes' QEMUs run from that prefix.
- The race itself is MoltenVK's (or Metal's); not reported upstream. If ANGLE
  is rebuilt, the same two features could be turned off in
  `angle-vulkan-moltenvk.patch` instead of the environment.
- Nothing can bring the picture back once IOGPU has denied the process; the
  guest-side answer is "keep running", not "recover". A QEMU-side reset of
  the virgl renderer (new EGL display) would be the next step if it recurs.
- x86_64/KVM not tested: the linux desktop (172.16.158.150) timed out on ssh.
- x86_64/TCG `timertest` `timerfd_abstime_future` failed once in the full
  regress run and passed 3/3 on a rerun (TCG timing; no timer code touched).

