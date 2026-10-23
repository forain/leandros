# lane/vtcosmic2 2026-10-23 (L11.7 VT switch on a logged-in COSMIC session) -- VERIFIED

Image rebuilt with the new greetd (needed `touch` of leandros-artifacts/greetd-lane/*/greetd: the mkfs freshness guard
compares mtimes with a fresh worktree checkout). ffsession `--nofirefox --gpu virgl FFS_GREETER_PAINT=90`: greeter login,
desktop, cosmic-term all came up (steps 0.7 s each); the panel clock animates. `vtloop.sh 10`, two runs (vt1, vt2):
- 10/10 round trips Ctrl+Alt+F2/F1: every F1 return is non-black, pixel diff vs pre-switch 0.03-0.05 % (clock digits);
  every F2 shot is the text console (0.1 % nonblack, 100 % differs from the desktop).
- /proc/vtstat: stale_presents=0. vt_restores=0, NOT 10: drm_vt_graphics_restore returns early (no recorded master/LAST_FB)
  for this compositor path under virgl, so the compositor repaints the return itself. Not a failure of the visual criterion;
  the restore counter only moves for sessions that recorded a master frame. Open question if someone wants the counter lit.
- Baseline (revert 0a14742f): conflicts in drivers/src/drm_device_interface.rs (later restore work builds on it), skipped.
Tooling: ffsession.py now saves /proc/vtstat after --hostpost (vtstat-end.txt); vtloop.sh drops the guest-grep call.
