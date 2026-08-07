# M8 cursor lane — implementation checkpoint

Baseline: main @ 93e1a7a, clean tree.
Spec: /Users/forain/code/leandros-artifacts/notes/m8-research/cursor-plane-findings.md

## Status
- [~] Stage 0: virtio-gpu cursor queue (GATE) — code written, aarch64 builds clean,
      awaiting boot gate result
- [ ] Stage 1: atomic KMS
- [ ] Stage 2: cursor plane -> cursorq
- [ ] Stage 3: FB_DAMAGE_CLIPS
- [ ] Stage 4: drm_tick <1, evdev monotonic_us

## Stage 0 — what was written (uncommitted)
drivers/src/virtio_gpu.rs:
- VirtioGpuCmd += UpdateCursor 0x0300 / MoveCursor 0x0301
- structs VirtioGpuCursorPos (16B), VirtioGpuUpdateCursor (56B)
- consts CURSOR_W/H = 64, CURSOR_RESOURCE_ID = 2, CURSOR_DEBUG flag
- VirtioGpuDevice += cursor_phys/cursor_virt/cursor_ready/cursor_pos/cursor_visible
- init_device(): queues[1] = setup_queue(1)
- cursor_reap()  — lazy used-ring reclaim (never free in submit path)
- send_cursor_command() — ONE read-only descriptor, no response desc, notify via
  queue 1's own notify_off
- cursor_init() — order-2 buddy alloc (16 KiB contiguous) + create_resource_2d(2,64,64)
  + attach_backing
- cursor_update(pixels,...) / cursor_present(...) / cursor_move(x,y) / cursor_hide()
- module fns cursor_update/cursor_move/cursor_hide/cursor_selftest
drivers/src/kms.rs: detect_and_configure() calls cursor_selftest() under CURSOR_DEBUG

CURSOR_DEBUG currently TRUE for the gate run — MUST be flipped back to false before
the final commit.

## Log
- 2026-07-30: wave started, read spec.
- 2026-08-02: Stage 0 code written; aarch64 build exit 0 (only pre-existing warnings).

## 2026-08-02 — STAGE 0 GATE: **PASSED**
Serial (aarch64, driver.py start):
  [GPU] cursor queue + resource ready
  [GPU] cursor selftest update=ok move=ok drained=ok
=> queue 1 sets up, resource 2 created+backed, host consumed BOTH
   UPDATE_CURSOR and MOVE_CURSOR (used ring fully drained).
Honest scope: this proves queue mechanics + host acceptance. It does NOT
prove the cocoa CALayer overlay renders (run was -display none).
Committed: 6edc295 "drivers/virtio-gpu: bring up the cursor queue"
CURSOR_DEBUG reset to false in that commit.
NOTE: rdebug() is compiled out (RENDER_DEBUG=false) — cursor logging uses a
local cdebug() -> pci::serial_debug so it is actually visible.

## Stage 1 in progress
drm_device_interface.rs: constants + PROPS table added (uncommitted).

## 2026-08-02 progress
- Stage 1 COMMITTED: dc3419b "drm: implement atomic KMS"
  (accept CLIENT_CAP_ATOMIC, CURSOR_WIDTH/HEIGHT=64, 2 planes 30+31,
   13-prop table, blob store, MODE_ATOMIC w/ TEST_ONLY + ALLOW_MODESET).
  Builds clean BOTH arches.
- Stage 2 in progress (uncommitted): commit_cursor_plane() routes cursor
  plane to virtio_gpu cursor_update / cursor_move / cursor_hide.
  Re-upload only when FB_ID differs from LAST_CURSOR_FB => reposition is
  zero pixel traffic. phys==0 guard (DRIimage path) warns instead of
  uploading garbage.
  DRM_STATS now also prints curs_up= curs_mv= atomic=.
- LAUNCHER changed (artifacts, not repo):
  /Users/forain/code/leandros-artifacts/m6-session-data/start-cosmic-leandros
  SMITHAY_USE_LEGACY no longer defaults to 1; export it to fall back.

## Commits so far
- 6edc295 Stage 0 virtio-gpu cursor queue
- dc3419b Stage 1 atomic KMS
- d15a657 Stage 2 cursor plane -> cursor queue
Stage 4 edited (uncommitted): drm_tick throttle 2 -> 1 tick;
evdev push_event timestamps from an inlined monotonic_us() (cannot import
drivers::snd — drivers already depends on evdev-server, would be a cycle).
DRM_STATS temporarily TRUE for measurement — MUST go back to false.
Harness: /Users/forain/code/leandros-artifacts/m8_cursor.py

## 2026-08-02 RUN 1 (tag s1) — atomic path CONFIRMED LIVE, cursor plane NOT used
DRMSTAT showed atomic=0x23 (35) == flips_sub, i.e. cosmic-comp DID take
CLIENT_CAP_ATOMIC and every frame is a MODE_ATOMIC commit. Desktop composited.
BUT curs_up=0 curs_mv=0 throughout => plane 31 never mentioned.
Confounds in run 1:
 - my QMP input-send-event sent BOTH axes in one event (m7z3 known-good form
   uses one event per axis) -> pointer motion may never have reached the guest;
   flips froze at 35 during the "burst", vs baseline 0.9 flips/s under motion.
 - cosmic-panel log spam flooded the serial and DRMSTAT stopped at t=112s.
Fixes: harness now sends one event per axis and checks the QMP reply; added
kernel counters atest= (TEST_ONLY commits) and cplane= (requests naming plane
31) to tell "never attempted" from "attempted and rejected".

## RUN 2 (tag s2) — pointer input delivered but compositor does NOT react
- QMP per-axis input-send-event ACCEPTED: 1012 moves in 25.0s (40.4/s).
  virtio-tablet-pci IS attached (driver.py:150), so abs events have a target.
- Compositor response: ZERO. flips_sub frozen at 10 across the whole burst,
  curs_up=0 curs_mv=0.
- NEW counters: atest=5 (TEST_ONLY commits), cplane=70 (property writes naming
  plane 31). So the compositor DOES know about and write to the cursor plane —
  70 property writes, ~6 commits worth. It is NOT "never attempted".
- Interpretation blocked: cannot tell "atomic broke input" from "input delivery
  is broken in this build/harness independent of atomic", because the Stage 4
  evdev monotonic_us timestamp change is ALSO in this build.
=> Running LEGACY=1 control on the IDENTICAL build (harness now supports
   LEGACY=1 which prefixes SMITHAY_USE_LEGACY=1). Decisive either way.

## RUN 3 (tag ctl) — LEGACY CONTROL, same build: input ALSO dead
SMITHAY_USE_LEGACY=1, identical build. atomic=0 atest=0 cplane=0 (confirms the
control really was on the legacy path). flips_sub froze at 6 from t=10s.
1006 pointer moves delivered (40.2/s) -> ZERO compositor response, same as the
atomic run.
=> CONCLUSION: pointer input is broken in this build INDEPENDENT of atomic KMS.
   The atomic work did not cause it. Prime suspect = the uncommitted Stage 4c
   evdev monotonic_us timestamp change (present in both runs).
ACTION: reverted servers/evdev/src/lib.rs (Stage 4c backed out), rebuilding to
re-test. Stage 4a (drm_tick < 1) kept.

## ROOT CAUSE FOUND (2026-08-02): COSMIC_DISABLE_DIRECT_SCANOUT killed the cursor plane
smithay FrameFlags::ALLOW_SCANOUT is a UNION:
  ALLOW_PRIMARY_PLANE_SCANOUT | ALLOW_OVERLAY_PLANE_SCANOUT | ALLOW_CURSOR_PLANE_SCANOUT
  (smithay .../drm/compositor/mod.rs:1032-1040)
cosmic-comp backend/kms/surface/mod.rs:716-721 does
  if COSMIC_DISABLE_DIRECT_SCANOUT { frame_flags.remove(FrameFlags::ALLOW_SCANOUT) }
=> our long-standing COSMIC_DISABLE_DIRECT_SCANOUT=1 silently disabled the CURSOR
   plane too. try_assign_cursor_plane returns None on its first line (mod.rs:3038).
   No log line at any level. cosmic-comp has a SEPARATE
   COSMIC_DISABLE_OVERLAY_SCANOUT for the overlay-only case — that is what we want.
My cplane=70 measurement CONFIRMS the diagnosis: those are smithay's reset_plane
writes (CRTC_ID=0 FB_ID=0 SRC_* CRTC_* = 11 props x ~6 full commits), which proves
planes.cursor IS populated (so UNIVERSAL_PLANES + possible_crtcs are correct).
Also verified from the smithay source:
 - NO format check on the cursor path (IN_FORMATS absence is harmless)
 - zpos / SIZE_HINTS optional
 - DRM_CAP_CURSOR_WIDTH/HEIGHT: unwrap_or(64) only on Err — returning success+0
   would give cursor_size 0x0 and reject every cursor. We return 64. OK.
 - COSMIC_DISABLE_SYNCOBJ is irrelevant to planes.
LAUNCHER FIX applied: COSMIC_DISABLE_OVERLAY_SCANOUT=1 instead of
COSMIC_DISABLE_DIRECT_SCANOUT=1 (latter still honored if explicitly exported).
Useful RUST_LOG filters: smithay::backend::drm::compositor=debug
 ("failed to create cursor buffer", "failed to export framebuffer for cursor",
  INFO "failed to test cursor {:?} state").

## RUN 4 (tag s4) — CURSOR PLANE IS LIVE ✅
With COSMIC_DISABLE_OVERLAY_SCANOUT=1 (instead of DIRECT_SCANOUT) and the
evdev Stage 4c change REVERTED:
  curs_up=0x1  curs_mv=0x6  cplane=0x96(150)  atest=6  atomic=10
=> smithay uploaded the cursor image ONCE and is now issuing MOVE_CURSOR for
   repositioning. Exactly the intended design: one 16 KiB upload, then zero
   pixel traffic per pointer move.
Awaiting the 60-moves/s burst window for the flips/s headline number.

## RUN 4 (s4) FINAL NUMBERS — 60 moves/s burst, atomic path, cursor plane live
  page flips/s : 6.00   (BASELINE 0.9)  => 6.7x
  delivered/s  : 6.00
  cursor mv/s  : 6.00
  cursor up/s  : 0.00   <= image uploaded ONCE, zero pixel traffic per move
  atomic/s     : 6.00
  cplane       : 350 total, atest 6
CAVEAT (honest): flips/s == atomic/s == cursor mv/s, i.e. smithay still flips
the PRIMARY plane on every cursor frame. The "zero softpipe work per pointer
frame" end state (compositor/mod.rs:2318 "skipping primary plane, no damage")
was NOT reached. Remaining win is the primary-plane recomposite, not the cursor.

## Stage 4c (evdev monotonic_us) — REVERTED, IT BROKE INPUT
Runs s1/s2/ctl all had it and ALL showed zero compositor response to 1000+
pointer moves (including the LEGACY control). s4 with it reverted -> input
works. Do NOT reland naively: libinput evidently rejects the
cntvct-derived timestamps. Stage 4a (drm_tick 2->1 tick) kept, committed.

## Commits
- 6edc295 Stage 0 virtio-gpu cursor queue
- dc3419b Stage 1 atomic KMS
- d15a657 Stage 2 cursor plane -> cursor queue
- e69f71b Stage 4a drm_tick 1 tick + atest/cplane counters
DRM_STATS + CURSOR_DEBUG both back to false.
NOT DONE: Stage 3 (FB_DAMAGE_CLIPS), Stage 4c (reverted).

## 2026-08-02 final phase
DRM_STATS=false, CURSOR_DEBUG=false. Both arches build clean (build-all.sh exit 0).
Running m7v_regress.py aarch64 uefi m8reg, then x86_64. idletest exists at
userland/idletest and MUST be run (guards the Stage 4a drm_tick change).

## Regression gotchas hit (2026-08-02)
- Piping a long background run through `tail` buffers ALL output -> the host
  reaper sees no progress and kills it (exit 144). Run the harness with NO pipe.
- First aarch64 regression pass (fresh image): vfstest 36/36 ALL PASS.
  Second pass on the now-dirty image: xattr_list_f2fs FAIL. Classic dirty-image
  residue (CLAUDE.md warns about exactly this). Re-running on a freshly
  regenerated image as m8reg2.
- ALSO: m7v_regress.py must be run as `python3 -u` — without it Python buffers
  stdout, the background task shows zero output and exits 1 looking like a
  crash. With -u on a FRESH image: vfstest PASS=36 FAIL=0 (confirms the
  xattr_list_f2fs FAIL was dirty-image residue, not a regression).
