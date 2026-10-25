# Lane gamewin (openbugs L12 #55) 2026-10-25: DOOM/MAME slow in a COSMIC window

Status: PARTIAL. DOOM FIXED (now holds 35 fps = its tic rate on both machines);
MAME REFUTED as a window problem: it has no Wayland path at all (see below), open.

## Files touched (rendering/window path only)
- scripts/vendor/doomgeneric/leandros_wayland.h (viewporter + fps stats)
- .claude/skills/run-leandros/doomsession.py (fps_check guard, one call after doom.log is saved; exit code)
No change to leandros_sdl.c (audio, lane doomsnd) or the VT phase (lane gamevt).

## Root cause (DOOM)
The window is 960x600; the client upscaled 320x200 x3 on the CPU into a 2.3 MiB
wl_shm buffer every frame (2.4-2.8 ms per frame on the laptop) and cosmic-comp then
uploaded all 2.3 MiB to the GPU. Commit -> frame-callback latency is ~27-28 ms on
both machines, so the extra per-frame cost pushed presents below the 35 Hz tic rate
(32-33 fps on the laptop). Frame pacing was already right (frame callback + 2 buffers,
drops instead of queueing); game logic was never slow (tics=35.0). Not kernel/futex.

## Fix (commit 9f3fdbb6)
wp_viewporter: buffer = 320x200 frame (256 KiB), set_destination 960x600, the
compositor scales on the GPU. Fallback to the CPU upscale when the global is missing;
`LEANDROS_DOOM_WL_SCALE=3` forces it for A/B. DOOM logs every 5 s:
`[doom] fps= calls= tics= cb= busy= cb_ms= copy_us=` (hand-rolled formatting: the
x86_64 static libc misreads stack varargs, my first snprintf version printed garbage).

## Evidence (steady 5 s windows; first window = startup)
| machine | mode | fps | tics | copy_us | cb_ms |
|---|---|---|---|---|---|
| laptop i915 (rekeychk-ea22c4fc kernel), x86_64 | CPU x3 (before) | 31.6-33.4 | 35.0 | 1900-2800 | 27-29 |
| laptop | viewporter (after) | 34.6-34.9 | 35.0 | ~430 | 28 |
| QEMU aarch64 HVF virgl, doomsession | CPU x3 | 34.8-35.2 | 35.0 | ~400 | 28 |
| QEMU aarch64 | viewporter | 35.0-35.2 | 35.0 | ~62 | 28 |
Windows with tics~28 contain the menu/new-game level load (both modes).

## Guard
doomsession.py `fps_check`: fails the run (exit != 0, `desktop-doom-fps` in
analysis.json) when a steady window is below DOOM_MIN_FPS (default 30). Passes on
aarch64 (35.0). It is a regression guard; QEMU aarch64 did not reproduce the laptop's
deficit, so it was not shown red on the unfixed code there.

## MAME (open)
src/osd/leandros (../mame, 6bb7839812a) never talks Wayland: window_init opens
/dev/dri/card0, software-rasterizes (rendersw) and presents with the legacy LeandrOS
ioctls CREATE_FB/FLIP_PAGE (0x1002/0x1004; the kernel software-scales to the mode).
Inside COSMIC it therefore competes with cosmic-comp for scanout instead of being a
window. Fix = a wl_shm/xdg_shell window in the OSD (reuse the leandros_wayland.h client,
render the MAME target at window size into the shm buffer, frame-callback paced,
-str N "Average speed" as the guard). Not done (budget). Only ROM on the laptop:
/captcomm.zip. Note ../mame has an uncommitted leandros_input.cpp edit from another lane.

## Also open
- copy_us ~430 for a 256 KiB memcpy into the shm mapping on the laptop vs ~60 on HVF:
  the memfd mapping may be uncached/WC or fault per page there.
- cosmic-comp frame callbacks arrive ~28 ms after commit (> one 60 Hz vblank).
