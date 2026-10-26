# lane/laptopsync6 (2026-10-26): laptop good = main eb32409f -- DELIVERED

- Build: INTEL=1 x86_64 release from main eb32409f, uname -v `#1 eb32409fae-20261026T1028Z`.
- Trial 681be444580e deployed, booted once, then promoted via remote.py; reboot confirmed good 681be444580e boots the same build.
  Previous good: 936fa75df020 (main 86a2bd4c).
- Checks on the trial: klog `[F2FS] Linux node format (lxnode)` x2 (/ and /home); `stat -c %d / /home /tmp` = 2050 2052 10 (distinct);
  wifi0 bound with default route, ssh ok; scripts/desktop-login.sh rc 0 (session up ~6 s, cosmic-comp + cosmic-panel uid 1000);
  `gpuprobe gl`: Mesa Intel(R) UHD Graphics 620 (KBL GT2), OpenGL ES 3.2 Mesa 25.3.6.
- Contains per-mount st_dev (lane/stdev) and the ddkill init fix.
