# lane/mamewl (openbugs L12 #55) - 2026-10-25

Status: FIXED in QEMU (aarch64); laptop measurement DEFERRED.

## What changed
- ../mame cf644ba32b0 (local master, not pushed): Wayland window for the LeandrOS OSD
  (src/osd/leandros/leandros_wayland.{cpp,h}; m_wayland branch in leandros_window.{cpp,h}
  window_init/get_size/update/complete_destroy; lua file list). The KMS path is untouched.
  MAME inherits the session's WAYLAND_DISPLAY and scales the 384x224 buffer to 896x672 via wp_viewporter.
- lane/mamewl: .claude/skills/run-leandros/mamesession.py, a guard that logs in to COSMIC, runs
  `mame captcomm -rompath / -str 30` in cosmic-term and checks (1) the log says Wayland, not KMS,
  (2) "Average speed" >= MAME_MIN_SPEED (default 90). `--check <log>` analyses a saved log offline.
  The guard passes `-keyboardprovider none` (see bug below). It no longer pins LEANDROS_VNC_PORT,
  so the driver auto-picks a free port.
- No build/install change: Makefile.leandros is regenerated from leandros.lua.

## QEMU numbers (aarch64, HVF, virgl, inside COSMIC)
Final run after merging main (run-final3): Average speed 99.99 %, offered 59.6/s, presented 55.7-58.4 fps,
draw_us 166-184, cb_ms 16-17, MAME exit after 32.9 s. Earlier run: 100.00 % (29 s).

## Laptop
Deferred to the next full laptop sync (the 390 MB binary is not re-pushed now). The earlier laptop2.sh run
produced no data: its kill-loop `>` overwrote the log. Re-run with `>>`, -keyboardprovider none, no 200 s cut.

## Open bug (openbugs #65, handled by another lane)
With the evdev keyboard module on, MAME hard-resets the machine every ~10 emulated s inside COSMIC
(14 "Average speed (10 seconds)" lines in 150 s, never exits). With `-keyboardprovider none` it runs
once for 30 s. Suspect leandros_input.cpp (event0 global read).
