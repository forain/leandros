# lane/laptopsync7 (2026-10-26): laptop good = main ec6e0164 -- DELIVERED

- Build: INTEL=1 x86_64 release from main ec6e0164 (lane/rootpick), uname -v `#1 ec6e016443-20261026T1158Z`.
- Trial 7b4f8f7a2d15 deployed, booted once, promoted via remote.py; reboot confirmed good 7b4f8f7a2d15 boots the same build.
  Previous good: 681be444580e (main eb32409f).
- Root resolution (rootpick touched blkdev.rs): cmdline root=PARTUUID=a20ccfac-02 still resolves on real hardware:
  klog `[BLK] /dev/root -> sda2`; `[BLK] warning: no root=` count 0.
- Other checks: klog lxnode lines 8 (all-boot log; / and /home included); `stat -c %d / /home /tmp` = 2050 2052 10;
  wifi0 bound with default route, ssh ok; scripts/desktop-login.sh rc 0; `gpuprobe gl`: Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL ES 3.2 Mesa 25.3.6.
- Deviation: deploy+verify ran in one lock hold; promote+reboot in a second hold right after, guarded by a uname -v check.
