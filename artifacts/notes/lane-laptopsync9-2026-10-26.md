# lane/laptopsync9 (2026-10-26): laptop good = main b01ea05b (legacyrm2) -- DELIVERED

- Build: INTEL=1 x86_64 release from main b01ea05b, uname -v `#1 b01ea05b3e-20261026T2004Z`.
- Trial 7a0d1c759331 deployed + booted; verified, promoted via remote.py, rebooted: good 7a0d1c759331 boots the same build.
  Previous good: 96f56c95ade5 (main 2ce3e6bc).
- Root: `[BLK] /dev/root -> sda2`, `warning: no root=` 0; lxnode lines 10; "unsupported legacy" 0 (legacy f2fs removal OK on lx / and /home).
- rw write+sync+readback OK on / and /home; `stat -c %d / /home /tmp` = 2050 2052 10 (distinct).
  (first script run showed FAIL stat only because rd's tail -n 2 cut the first line; re-checked, promoted in a second lock hold with a uname guard.)
- Wi-Fi ping rc 0, ssh ok; desktop-login.sh rc 0. gpuprobe = Mesa Intel UHD 620 (KBL GT2), GLES 3.2 Mesa 25.3.6;
  1172 gpuprobe gl runs in 30 s, 0 failures; DOOM in window 20 s ~32-35 fps, busy=0.
- klog: oops/panic 0, 'freed before pin' 0, i915 EINVAL 0, 'killing stray' 0, sshd stayed up.
- Gaps: guest has no grep (i915 counters unread).
