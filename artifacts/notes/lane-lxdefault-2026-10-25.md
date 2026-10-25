# lane/lxdefault (L6): lx is the only format for new f2fs volumes -- DELIVERED

- mkfs-f2fs-populated.py: LX default; LEANDROS_F2FS_LX opt-in removed. mkfs-f2fs-home.py dispatches every command to mkfs-f2fs-home-lx.py unless `--legacy` or LEANDROS_F2FS_LEGACY=1. build-all.sh, write-usb.sh, run-qemu.sh, make-utm-vm.sh and the test fixtures all go through these, so all produce lx. No in-guest mkfs writer exists.
- Legacy fixtures: F2FS_TEST_LEGACY=1 for f2fscptest/f2fscrashtest/f2fsfulltest (replaces F2FS_TEST_LX). Legacy mounts rw: cptest + crashtest PASS on both formats.
- data0 is rewritten on every build; run-qemu.sh stale-image check also watches f2fslx*.py / mkfs-f2fs-home-lx.py. An EXISTING f2fs-data1 (home, user data) is kept as is until --reset-home.
- Gate: aarch64 regress PASS, x86_64 regress PASS, net-e2e PASS, coldstart ok, f2fscrashtest lx and legacy PASS, box fsck.f2fs --dry-run rc=0 on fresh root and home.
- A/B (aarch64 HVF, 3 runs, median; lx vs legacy): boot-to-prompt 2.1 vs 2.3 s; libxul read 54.9 vs 60.7 ms; walk /usr 174 vs 218 ms; 4K write+fsync p50 278 vs 267 us (p99 748 vs 1090); fsyncdirty p50 185 vs 300 us; 300 creates 0.73 vs 0.81 s; coldstart term 1.08 vs 1.06 s, settings 1.07 vs 1.07, portal 1.15 vs 1.10 (+4%, inside 0.3 s sampling); image build 2.8 vs 5.9 s (different load); root 1846560 vs 1854128 KiB, home 32.8 vs 16.4 MiB allocated.
