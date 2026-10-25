# lane/laptopsync3 — 2026-10-25: laptop kernel sync + rootfs userland refresh

## Verdict: DELIVERED (one soft finding: background scans while associated cost ~2 s blips)

### Kernel
- Built x86_64 release at main ab24cdfc (tag `ab24cdfc69-20261025T0456Z`), trialed with the default `--netwatch` (180),
  promoted: good = kernel id **b840210fc8c6** (sha256 bbd9c263…), default=good, armed=0. `uname -v` checked before measuring and before promoting.
- Trial health: SSH up after 34 s on wifi0 (172.16.149.179, gw 172.16.0.2); 120 s heartbeat 120 beats, 0 gaps > 2 s (max 1.34 s), 0.0% ping loss;
  desktop login ~6 s; YouTube PLAYING after 14 s. (ytd-app was not checked on the trial: `scripts/deskplay/dp.sh` launches Firefox
  without `--marionette`; checked after the refresh via $SP/laptopsync/dpm.sh, see below.)

### Userland refresh
- Source: `userland/target/x86_64-unknown-none/release` (what scripts/mkfs-f2fs-populated.py stages into /bin; build-userland.sh copies
  the x86_64-unknown-leandros outputs there, and netctl exists only there). Compared md5 of every name in the mkfs `bins` list against /bin.
- 59 differed. Refreshed 50 (all non-critical): aplay chroottest dbusprobe drmsmoke epolltest evsplit evtest2 exectest f2fstest fdmaxtest forktest fsbench hello i915test idletest jobtest lsblk lspci meminfo memtest netcheck netctl netctltest nettest pcachetest permtest ping polltest privtest ps pstest pthreadtest ptytest racetest scmtest sigchldtest sigtest sigtest2 smpwaketest syscalltest timertest tput udptest uptrtest venustest vfstest vttest waittest wakepolltest xattr 
- Backups: /var/backup/userland-2026-10-25/bin/ (50 files, md5-verified against the old /bin before deploying).
  Rollback: `sh /var/backup/userland-2026-10-25/rollback.sh`. Deployed with `remote.py deploy-files`, post-deploy md5 mismatches 0.
- Left STALE on purpose (boot/login/greetd path): fstab greeter-launch loadkmap login lsusb mount poweroff(+reboot/halt) shell umount.
  Also stale, not repo-userland (out of scope): musl-built repo crates (sshd, espctl, greetd-login, blanktest, ...) and ports.
- Not on the stick at all (newer tests; need a re-flash or a deploy-files): gpuqtest kexectest kparks listentag nettag2 pollnarrow proctest proctest-cold.bin shmtest strtest vfstag 

### Verification (after reboot into good b840210fc8c6 with the new userland)
- `ping -c 2` gateway 172.16.0.2 and 1.1.1.1: 2/2 replies each, rc 0, stderr empty (no ld_so lines).
- `netctl scan wifi0` twice 3 s apart: scan generation 8 -> 9 (USER flag reaches the kernel).
- 5-min heartbeat: 300 SSH beats, **3 gaps > 2 s (2.15, 2.02, 2.43 s, all within guest uptime 173-192 s)**, ping 297/299 (0.7% loss). SSH stayed responsive.
  iwlwifi scans take ~4.1-4.6 s each ("scan complete in 4xxx ms"); the 120 s background scan while associated is the likely cause
  (klog has no timestamps, so the correlation is not proven). Pre-refresh 120 s window on the same kernel: 0 gaps, 0% loss.
  Fallback if this matters: `iwlwifi.scanconn=0` on the good cmdline (NOT applied; the user decides).
- Desktop login again ~6 s; YouTube PLAYING after 18 s; Marionette: ytd-app defined, h1 = Big Buck Bunny title. No regression, rollback not run.

### Open
- dp.sh lacks `--marionette` (use $SP/laptopsync/dpm.sh or add it).
- Background-scan blips (above): an A/B with `iwlwifi.scanconn=0` would settle the cause.
- Guest has no grep/cmp/which: remote checks must parse on the Mac side.
