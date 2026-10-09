# lane/nvkms — 2026-10-08 — M8: card0 scans out on the RTX 3070 (copy-engine blits into the GOP surface)

Branch `lane/nvkms`, based on main `4de1e22` (nvuapi + nvwd merged). Not
pushed. Goal: the KMS cosmic-comp's smithay DRM backend needs on the real PC,
with no modeset. QEMU-validated for "nothing changed when no NVIDIA backend is
registered"; everything past registration is **only testable on the PC** —
the stick's autotest now runs `/bin/nvkmstest` for that (section 6).

## 1. Design

```
 cosmic-comp / smithay (atomic) / nvkmstest
   ioctls on /dev/dri/card0 (DRM core, nr < 0x40)
 ───────────────────────────────────────────────────────────────────────
 drivers/src/drm_device_interface.rs   (UNCHANGED object model)
   one connector / CRTC / primary plane, atomic properties, MODE_ID blobs,
   flip events (queued; the 100 Hz tick delivers = synthetic vblank),
   DRM master per VT, console hand-off (SCANOUT_WRITES -> drm_scanout_claim)
   + "Backend framebuffers": ADDFB/ADDFB2 of a backend GEM handle -> fb with
     ext_fb token; SETCRTC / PAGE_FLIP / ATOMIC / DIRTYFB of it -> backend
     present with NO DRM lock; RMFB / creator close -> backend destroy
 drivers/src/gpu_kms.rs   (NEW seam, separate from gpu_uapi on purpose)
   KmsBackend { mode, modifiers, fb_create, fb_destroy, present }
 drivers/nvidia/src/uapi_kms.rs   (NEW)
   own RM client: device, subdevice, VA space, TSG + channel on the first
   async CE (class c7b5), own doorbell; GOP surface = RM console memory
   (NV01_MEMORY_FRAMEBUFFER_CONSOLE, VRAM offset 0) mapped into the VA space
   fb_create: dup the GEM's RM memory into this client, map once
   present: implicit-sync wait (fences of every open holding the bo), one 2D
            CE copy per damage rect (pitch-linear, or block-linear de-tiled
            by the CE), semaphore release, CPU wait (synchronous)
```

Decisions and why:

- **No modeset, one mode.** The connector lists exactly the GOP mode while the
  seam is registered; SETCRTC and atomic MODE_ID (TEST_ONLY too, which is how
  smithay learns) with any other size answer EINVAL.
- **Reuse, don't fork, the KMS model.** smithay's atomic path already runs on
  it in QEMU (cosmic-comp). The seam only replaces "how pixels get to the
  surface" for buffers the backend owns. Dumb buffers keep the existing CPU
  copy into the GOP surface (it already worked on the PC; the console's own
  BAR1 mapping).
- **IN_FORMATS = {XR24, AR24} x {LINEAR}**, ADDFB2_MODIFIERS cap = 1, only
  while registered. smithay then allocates through
  gbm_bo_create_with_modifiers2([LINEAR]); Zink asks NVK for a LINEAR
  DRM-modifier image (NVK renders those via its linear_tiled_shadow).
  ADDFB2 additionally accepts NVIDIA block-linear modifiers (uncompressed) and
  implicit-layout buffers (GEM tile_flags kind != 0 -> block height from
  tile_mode): the CE de-tiles them. Compressed modifiers are refused.
- **No cursor plane** while registered (the existing cursor plane drives the
  virtio cursor queue; nothing would show it): GETPLANERESOURCES lists only
  the primary, commits naming plane 31 are EINVAL. smithay composites the
  cursor into the primary.
- **Synchronous present, unfenced event.** The copy is ~0.3 ms at 1024x768
  and ~0.7 ms at 1080p (CE measured 12.9 GB/s on this GPU), so the ioctl
  waits for the semaphore (300 us spin, then yield, 500 ms bound -> the
  channel is declared dead, on-screen ERROR line, presents answer EIO). The
  flip event is queued with fence 0; `drm_tick` delivers at most one per
  10 ms tick = up to ~100 Hz synthetic vblank. No 60 Hz pacing, no tearing
  protection (the display engine scans the surface while the CE writes it).
- **Implicit sync as a safety net.** smithay without IN_FENCE_FD waits for its
  render on the CPU before committing; the backend also waits for the newest
  fence of every channel of every open that holds the buffer (1 s bound).
- **Console.** Unchanged mechanism: a present claims the scanout
  (`drm_scanout_claim` before the copy, so no console line lands mid-copy);
  the last card0 close of the master open gives it back (VT repaint).
  DROP_MASTER alone does not (Linux: fbcon restores on lastclose). New: a VT
  switch that lands while a copy runs is detected on the way out
  (`!master_ok && scanout unowned`) and the text VT is repainted
  (`vt_console_reclaim`), so a stale frame cannot stay over the console.
- **Verified before registering, without touching the screen.** At boot the
  CE reads the first 8 rows of the GOP surface; they must equal the CPU's
  uncached BAR1 read. Mismatch -> not registered (card0 keeps CPU-copy KMS).
- **Merge hygiene vs lane/nvzink.** New files `gpu_kms.rs` and `uapi_kms.rs`;
  `uapi.rs` gets one init line, one summary line and a self-contained block
  at the end (`kms_is_handle`, `kms_bo`, `kms_bo_fences`), plus the side-item
  diagnostics in `channel_alloc`. `UapiBackend` is untouched. No C glue
  change (the CE methods are encoded in Rust; the shared libnvrm.a is not
  rebuilt).

## 2. Files

| file | change |
|---|---|
| `drivers/src/gpu_kms.rs` | new seam |
| `drivers/src/drm/framebuffer.rs` | `ext_fb` token field |
| `drivers/src/drm_device_interface.rs` | backend framebuffers, IN_FORMATS, mode refusal, cursor plane hidden, ADDFB2_MODIFIERS cap, ext SETCRTC/PAGE_FLIP/ATOMIC/DIRTYFB/RMFB/close |
| `drivers/nvidia/src/uapi_kms.rs` | the NVIDIA KMS half |
| `drivers/nvidia/src/uapi.rs` | init/summary hooks, `kms_*` lookups, notifier-slot diagnostics |
| `ports/nvk-tests/nvkmstest.c` | new KMS test (raw ioctls, optional NVK) |
| `ports/nvk-tests/{build.sh,nvk-autotest.sh}` | build + run it (last); `NVK_TESTS_OUT` |
| `scripts/{mkfs-f2fs-populated.py,nvidia-test-stick.sh}` | stage `/bin/nvkmstest`; rebuild trigger; hints |

Options: `nvrm.kms=0` (do not register), `nvrm.kms_log=0|1|2`.
`NVK_AUTOTEST_KMS=0`, `NVK_AUTOTEST_KMS_ARGS="--frames=300"`.

## 3. QEMU evidence (Mac: aarch64 HVF, x86_64 TCG)

| check | result |
|---|---|
| `./scripts/build-all.sh` (both arches, release, default) | ok |
| `cargo check --features nvidia` (x86_64) | clean, no new warnings |
| `runtests --suite regress` aarch64 / x86_64 | **15/15** / **15/15** |
| COSMIC desktop, aarch64 virgl (`ffsession --nofirefox`) | greeter 0.6 s, panel, cosmic-term up; screenshot: wallpaper + panel + terminal |
| `nvkmstest` on aarch64 virtio (`leandros-drm`) | 4 PASS 2 SKIP: dumb 60 flips 720/s scanout 64/64, atomic modeset + 30 commits, console back 64 -> 0 |
| x86_64 nvidia stick image, `--usb-stick --no-gpu` (GOP only, no GPU) | `no NVIDIA display controller present`, nvktest 0 devices exit 3, nvkmstest 4 PASS 2 SKIP on the plain GOP KMS at 1920x1080 (10 flips/s: TCG CPU drawing), console restored, `nvk-autotest: done` |

Not testable in QEMU: everything past `uapi_kms::init` (needs GSP-RM): the
CE channel, the console-memory mapping and read-back, ADDFB2 of GEM buffers,
the copy, IN_FORMATS / mode refusal / hidden cursor plane under a registered
backend.

## 4. On the PC

```sh
diskutil list external
./scripts/nvidia-test-stick.sh /dev/diskN        # full build (kernel + userland changed)
```
Boot it; leave the keyboard alone. After nvktest (~1-3 min) the KMS test
takes the screen four times for a few seconds each (dumb, gem, vk, atomic):
a band at the top that changes colour every frame and a yellow bar sliding
down over a dark background. Then the text console returns with the
`---- nvkmstest summary ----` block. `sync; poweroff`, then
`sudo ./scripts/read-usb-log.sh /dev/rdiskN > nvrm-boot5.log` and
`grep -a 'nvkms\|nvkmstest\|notifier before' nvrm-boot5.log`.

### Expected success lines
```
[nvkms] +...ms ==== NVIDIA KMS ready: card0 scans out 1024x768 (the GOP mode, no modeset) by copy-engine blits into the GOP surface; GOP surface at GPU VA 0x... (RM console memory, VRAM offset 0x0), pitch 4096, CE2 (engine 0xb) class 0xc7b5, read-back of 8 rows matches BAR1 (content verified; .. us); modifiers LINEAR; setup .. ms ====
nvkmstest probe: driver 'nouveau' 1.4.0 (...); SET_MASTER ok
nvkmstest probe: caps DUMB_BUFFER=1 ADDFB2_MODIFIERS=1
nvkmstest probe: crtc 1, connector 1 (...), 1 mode(s): 1024x768@60*
nvkmstest probe: planes 30(primary, 2 fmt); IN_FORMATS XR24/LINEAR AR24/LINEAR
nvkmstest dumb: PASS (120 flips with events, ~50-100 flips/s; scanout shows the last frame (64/64 samples))
nvkmstest gem: PASS (120 flips with events, ~60-100 flips/s; scanout shows the last frame (64/64 samples))
nvkmstest vk: PASS (120 flips with events, ...; scanout shows the last frame (64/64 samples))
nvkmstest atomic: PASS (GEM buffers, modeset + 60 flip commits with events, ...; a non-listed mode is refused)
nvkmstest console: PASS (DROP_MASTER ok; the last frame is gone from the scanout (64 -> 0 of 64 samples): console repainted)
==== nvkmstest done: 6 passed, 0 failed, 0 skipped ====
```
The flip rate is bounded by the event delivery (one per 10 ms tick), not
by the copy (`[nvkms] ... present fb N: 1 rect(s), window ..., NNN us
(first present)` gives the copy cost; ~250 us expected at 1024x768).

### Decision tree
1. `NVIDIA KMS NOT available: <why>` — the RM call that failed is named
   (CE channel, console memory, mapping). `the copy engine's view of the GOP
   surface differs from BAR1` = RM's console memory is not the scanned-out
   surface on this boot: report the numbers. Without registration the dumb
   and atomic subtests still run on the CPU path; gem/vk then FAIL at ADDFB2.
2. `gem: FAIL ... ADDFB2 GEM ...: Invalid argument` -> the `[nvkms]` line
   before it names the reason (format, modifier, pitch, size, duplicate,
   mapping).
3. `gem: FAIL ... scanout does not show the last frame` with presents logged
   -> the copy runs but lands elsewhere / the source VA is wrong: rerun with
   `--cmdline "nvrm.kms_log=2"`.
4. `ERROR copy engine did not finish ... within 500 ms` (on screen) -> the
   KMS channel hung; Xid lines say why. Presents answer EIO afterwards.
5. `vk: FAIL vkAllocateMemory (VRAM, export dma-buf)` / `PRIME_FD_TO_HANDLE`
   -> the NVK export/import path (shared with lane/nvzink's work); `gem`
   passing isolates it from the scanout itself.
6. `console: FAIL` -> the VT repaint did not happen after close; check the
   screen (it may simply be the read-back).

## 5. Side item: the work-submit token in error slot 0

RM 570.86.16 writes the token notification only from
`kchannelNotifyWorkSubmitToken` (CPU-RM, at GET_WORK_SUBMIT_TOKEN) into
`notifyIndex[WORK_SUBMIT_TOKEN]` (3 after our SET_..._NOTIF_INDEX), with
status 0xffff, through `kchannelUpdateNotifierMem`, whose offset handling and
our `nv_alloc_kernel_mapping` page offset both look correct. But boot4's
slot-0 entry has **status 0**, which that path never writes, and
`check_token_notification` reports the FIRST slot holding the token, so a
correct slot-3 write would have been hidden behind it. Most likely
explanation: slot 0 is written by GSP-RM (its own channel bookkeeping through
`errorNotifierMem`, which SET_..._NOTIF_INDEX — CPU-RM only, never RPC'd —
does not move), at channel allocation or scheduling. Not resolved here
(cheap only): CHANNEL_ALLOC now logs all four slots **before**
SET_..._NOTIF_INDEX/GET_TOKEN and after (`notifier before ...: [0] info32 ..
status ..; after: ...`). Next boot: token already in slot 0 "before" ->
written at alloc/bind/schedule (GSP); slot 3 holding it with status 0xffff
"after" -> the index works and slot 0 is a second, GSP-side copy. Harmless
either way (status-only watchdog + the token guard).

## 6. Risks

- **Tearing**: no vblank sync; the copy races the scanout (visible as a tear
  line during motion, not corruption).
- **Event pacing** is the 100 Hz tick, not the panel's 60 Hz: compositors may
  render up to ~100 fps; presentation timestamps are "copy done".
- **Zink/NVK LINEAR**: smithay will intersect IN_FORMATS (LINEAR) with what
  EGL/Zink can render to. If NVK/Zink does not offer LINEAR as renderable for
  XRGB8888 dma-bufs, cosmic-comp finds no format. Fallback ready in the
  kernel: ADDFB2 accepts uncompressed NVIDIA block-linear modifiers and
  implicit tiled buffers (CE de-tiling) — advertising them in IN_FORMATS is a
  one-line change in `KMS_MODIFIERS` (untested on hardware).
- VRAM exports are token-only (lane nvuapi): fine for scanout (the kernel
  dups the RM memory), but no CPU mmap of such dma-bufs.
- The copy holds the ioctl ~0.3-1 ms per frame; the producer wait can hold it
  up to 1 s if the GPU is busy.
- The GOP surface is whatever RM's console memory is; a monitor whose GOP
  mode changes at runtime is not handled (no hotplug).

## 7. Between this and cosmic-comp on the PC

1. **GL on NVK (lane nvzink)**: Mesa's loader must pick Zink for the
   `nouveau` card0 (GBM + EGL), Zink's dma-buf export/import through our
   PRIME, sync-file / syncobj fd export (ENOSYS today) for explicit sync.
2. **Session start**: `ports/greetd/data/gpu-env` + `gpuprobe` are
   virtio-centric (the greeter shows NO GPU RENDERER on the PC); they must
   accept `nouveau` and set the Zink env.
3. **Input**: usbhid keyboard works; the mouse on the PC is untested under
   cosmic-comp (evdev).
4. cosmic-comp/smithay paths not exercised by nvkmstest: GBM
   `create_with_modifiers2`, the IN_FENCE_FD-less CPU wait, damage clips on
   real frames (supported), VT switch with libseat.

