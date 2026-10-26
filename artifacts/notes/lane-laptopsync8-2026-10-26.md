# lane/laptopsync8 (2026-10-26): laptop good = main 2ce3e6bc (drmrace) -- DELIVERED

- Build: INTEL=1 x86_64 release from main 2ce3e6bc, uname -v `#1 2ce3e6bc7a-20261026T1259Z`.
- Trial 96f56c95ade5 deployed + booted; verified, promoted via remote.py, rebooted: good 96f56c95ade5 boots the same build.
  Previous good: 7b4f8f7a2d15 (main ec6e0164). Whole sequence in ONE laptop lock hold (scratchpad laptopsync8/run.sh).
- Root: `[BLK] /dev/root -> sda2`, `warning: no root=` count 0; klog lxnode lines 10; `stat -c %d /home /tmp` = 2052 10.
- Network: ping 1.1.1.1 rc 0 over Wi-Fi, ssh ok; scripts/desktop-login.sh rc 0 (cosmic-comp + panel as uid 1000).
- GPU: gpuprobe gl = Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL ES 3.2 Mesa 25.3.6.
  Iris GL load: 1144 gpuprobe gl runs in 30 s as leandro on the live session, 0 failures.
  DOOM in the COSMIC window 20 s: ~34 fps steady, busy=0.
- klog (all boots) after the load: 'freed before pin' 0, i915 EINVAL/refusal lines 0, oops/panic/BUG 0.
- Gaps: guest has no grep, so /proc/i915 counters and the wifi0 inet line were not read (ping rc 0 stands in);
  no scripted MAME session exists for the laptop. nvtoken/pimem built into x86 without issue.
