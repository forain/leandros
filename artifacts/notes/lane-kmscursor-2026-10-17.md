# lane/kmscursor (2026-10-17, from local main a3f226cc) — i915 hardware cursor + real-vblank async flips

UNTESTED ON HARDWARE. QEMU: regress both arches + the fake-device path
(`i915.fake=2 i915.kms=1`, i915test) — see "Tests" below.

## Problem

COSMIC on the laptop's UHD 620 was sluggish even moving the pointer:

- no cursor plane was exposed while a `gpu_kms` backend was registered, so
  every pointer move was a full compose + flip of the primary;
- `kms::present` ran synchronously inside the atomic-commit ioctl (render
  fence wait ≤ 1 s, clflush, PLANE_SURF, then msleep(1)-polling
  PLANE_SURFLIVE until the latch, ≤ a frame); the flip event came from the
  DRM layer's 60 Hz hrtimer (`queue_ext_flip_event`), not the panel's
  vblank; display interrupts were fully masked. The compositor thread sat in
  the ioctl and fps quantised to 30/20/15.

## What changed

`drivers/src/gpu_kms.rs` — two optional halves registered separately
(`register_flip`, `register_cursor`), so NVIDIA (`drivers/nvidia`) is
untouched:
- `KmsFlip { queue, event, busy, wait_idle }`: real-vblank flips; the
  backend completes the commit's event itself.
- `KmsCursor { size, set, move_to, hide }`: a hardware cursor plane.
- `cursor_plane_exposed()`: always without a backend (virtio host cursor),
  with a backend only when it has a cursor half.

`drivers/src/drm_device_interface.rs`:
- cursor plane (id 31, AR24, CURSOR type) exposed in GETPLANERESOURCES /
  GETPLANE / object props under a backend with a cursor; DRM_CAP_CURSOR_*
  from the backend (64).
- atomic commit with a flip half: EBUSY for a NONBLOCK commit while one is
  in flight, a blocking one waits (≤ 250 ms) — Linux's semantics; checked
  before any state is applied. The primary goes to `ext_present_async`
  (queue only); the event cookie travels with it. A commit that queued no
  flip (cursor only / damage-skip) completes its event at the next vblank
  (`KmsFlip::event`). Legacy PAGE_FLIP of a backend fb: EBUSY while pending.
  SETCRTC waits for the in-flight flip, then the synchronous `present`.
- backend-completed events: `ext_event_new` / `ext_event_complete` (any
  context, IRQ-safe slot table + try-locked READY_EVENTS; leftovers drained
  by the ext timer and the tick); a closing open's slots are freed.
- `commit_cursor_plane_ext`: copy only when a different fb is named, else a
  position write; coordinates keep their sign.
- Without a flip half (virtio-gpu, NVIDIA, `i915.vblank=0`) every path is
  the old one.

`drivers/src/i915/vblank.rs` (new): MSI + GEN8_MASTER_IRQ, DE pipe A
VBLANK + PLANE1_FLIP_DONE (IER set once, IMR unmasked only while something
is pending); `queue` arms in the ioctl when the fence has signalled and no
clflush is needed, else the `i915-flip` kthread waits the fence, clflushes
and arms; completion when PIPE_FRMCOUNT moved past the arm frame and
PLANE_SURFLIVE shows the surface (or a console restore intervened, or 3
frames passed: counted as "unlatched"); event = hardware frame counter +
vblank-start time (IRQ time minus PIPEDSL lines since vblank start). A
backstop hrtimer (frame + 3 ms after the arm, then every 2 ms) polls the
same check, so a silent interrupt degrades to polling, never a stall. On
the fake device it is a 60 Hz software frame counter. GT/PCU/other DE banks
are only acknowledged in `isr` (sources stay masked; exec still polls) —
**lane/gtfreq: if it needs GT PM interrupts, dispatch from `vblank::isr`'s
GT-bank loop, do not request the MSI again.**

`drivers/src/i915/cursor.rs` (new): CURCNTR/CURBASE/CURPOS on pipe A,
64x64 ARGB (MCURSOR_MODE_64_ARGB_AX, pipe gamma/CSC mirrored from plane
1A), two 16 KiB kernel image buffers in the GGTT at 0x6000_0000 (above the
scanout window), copied from the commit's fb (dumb or i915) + clflushed,
alternating so the scanned one is never overwritten. DDB: the GOP's
CUR_BUF_CFG if valid and free, else the highest free 32 blocks of the 892
(boot 6: plane 1A had 446 blocks, so expect 860..=891). Watermarks:
plane 1A's current per level capped at the cursor DDB (an upper bound of the
cursor's own need), TRANS off. Fully off-pipe = disabled. Hidden on console
restore (lock-free), shown again by the next move/image.

`drivers/src/i915/kms.rs`: `present` split into `prepare` / `arm` /
`after_arm` (logs outside locks); `fb_create` accepts fbs smaller than the
mode (flip checks coverage); `fb_destroy` drops/waits an in-flight flip of
the fb; `console_restore` bumps the vblank generation and hides the cursor;
`native_setup` also records VTOTAL and the line time.

`userland/i915test`: `kms_async` — cursor plane exposed + CAP 64, nonblock
flip returns at once, second one EBUSY, event arrives, cursor TEST_ONLY /
image / move commits get their events, blocking commit waits, event
sequence advances. Skipped when another open holds master.

## Laptop expectations (cmdline unchanged: `i915.uapi=1 i915.exec=1 i915.kms=1`)

Boot log (grep `kms:`):
- `kms: vblank: before: MASTER_IRQ=0x00000000 DE pipe A IMR=0xffffffff ...`
- `kms: vblank: flips queued from the ioctl, ... (MSI, DE pipe A VBLANK +
  PLANE1_FLIP_DONE (IER now 0x00000009, MASTER_IRQ 0x80000000)) ...`
- `kms: cursor: hardware cursor on pipe A: 64x64 ARGB8888, images at GGTT
  0x60000000/0x60004000, DDB blocks 860..=891 ...`
- `kms: flips on real vblank (...); hardware cursor on pipe A (...)`
- at the first pointer: `kms: cursor: enabled: CUR_BUF_CFG 0x037b035c
  CUR_WM [...] ... CURCNTR 0x...27 ...`
- every 60 s while drawing (10 s with `i915.kmsdebug=1`):
  `kms: vblank: N completions in window (T total: I on IRQ, P by polling);
  armed A in-ioctl / W after fence; event-only E; dropped D; unlatched U;
  queue->event avg ... max ..., arm->event avg ... max ...; IRQs ...;
  EBUSY ...; underruns ...` and `kms: cursor: N images, M moves`.

Healthy: almost all completions "on IRQ", arm->event avg ≤ 16.7 ms,
unlatched 0, EBUSY ~0 (smithay waits for its event), underruns 0 (with
kmsdebug). Pointer motion smooth at 60 Hz with no primary recomposite;
fps no longer stuck at 30/20/15.

Bad signs and knobs (one change per boot):
- `the display interrupt never arrived` / "by polling" ≈ total: MSI not
  delivered — still works (2 ms granularity); `i915.vblank_irq=0` makes it
  explicit.
- stutter/hangs in flips or anything odd: `i915.vblank=0` = the old
  synchronous path + 60 Hz synthetic events (cursor plane still on).
- cursor garbage / wrong colours / missing / flicker near it, underruns
  climbing with `i915.kmsdebug=1`: `i915.hwcursor=0` (composited pointer,
  as before). Report the `kms: cursor:` lines (DDB + WM values).
- `no 32 free DDB blocks (in use: ...)`: no hardware cursor (logged); the
  list says who owns the DDB.

## Not done / risks

- Cursor 64x64 only (128/256 not offered). WMs approximate Linux's
  skl_compute_wm (no pcode latencies) — upper-bounded by plane 1A's.
- FRMCOUNT assumed to increment at vblank start (Linux relies on it for
  g4x+); if not, completion slips to the backstop (+3 ms), not a frame.
- No async (tearing) flips, no DRM_IOCTL_WAIT_VBLANK on real vblank, no
  IN_FENCE_FD (smithay waits its render on the CPU before committing).
- A scanout object that is not PRIME-shared still needs the 8 MB clflush —
  now in the `i915-flip` worker, not the ioctl.
