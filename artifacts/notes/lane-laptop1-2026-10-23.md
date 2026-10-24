# lane/laptop1 (2026-10-23): trial of main cd0c41f3 on the real laptop - NOT PROMOTED

Kernel: main cd0c41f3 release x86_64, trial id e5e9336fddab, cmdline + `pan=audit` (first two boots), then plain.
Good kernel kept: 202b6ea6e9b7 (deployspeed-legant), still default.

- L7.4: `[CPU] CR4 SMEP=1 SMAP=1`; `[SMAP] audit:` lines = 0 (no raw user accesses seen during boot, ls -R /usr, memtest).
- L1.13: /proc/sysstat after activity: as_hold_max_us 3817, late_tick_max_ms 3640 (teardown_points 288, observer_gaveup 0). [FORK] lines holds=1 (one holds=3, clone_us 741).
- L8.10: `[CPUS] xAPIC mode (x2APIC available, not needed)` - x2APIC not enabled.
- L8.11: 6 samples 2 min apart over ~11 min: realtime deltas 120/121/120/120/120 s vs uptime 120.2 s; no backwards jump.
- Health: wifi associated + DHCP (wifi0), NVMe 1 controller (read-only default), i915 G3b exec ready, no panic/[WDOG]/[BUDDY].
- FAIL: graphical login NOT started. greetd.log: `gpu-env: iris probe failed (rc=139 renderer='')` (SIGSEGV in the iris probe),
  then "NO GPU RENDERER - refusing to start COSMIC". Reproduced on 3 trial boots (with pan=audit, f2fs.rf=0, and with the good
  entry's plain cmdline), and absent on the good kernel (greetd started). So it is a regression between deployspeed-legant and
  cd0c41f3, not caused by pan=audit. No fault/SMAP line is logged for the segfault.
- Not promoted; trial discarded; laptop back on good 202b6ea6e9b7.
- Note: deploy-kernel added f2fs.rf=0 on the first deploy (it judged the good kernel pre-f2fsync).

Next: bisect the iris probe SIGSEGV (run the gpu-env probe command by hand on the trial: espctl arm, ssh, run it) between deployspeed-legant and main.
