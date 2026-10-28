# lane pibtauto 2026-10-28: BT autostart (init stray sweep) + Pi root sync

FIXED. Root cause: sweep_strays (run when greetd gives up with DM_EXIT_NO_GPU) SIGKILLed
btd, bthid and `netctl watch --stamp`, whose pids init never recorded.

## Change (userland/init/src/main.rs, arch-neutral)
- DAEMONS table (btd, bthid, netctl watch): pids exempt from sweep_strays.
- Reap path logs `<name> exited (status S) after T s`, schedules respawn with the sshd
  backoff (1,2,4,8,16 s; give up after 5 quick exits < 60 s); exit 127 = no respawn.
  Respawn happens from the idle tick (no usleep in the reap path), gated on
  /dev/hci0 + bt=0 (btd), bthid=0 (bthid), /dev/netctl (netctl).
- Netctl exec failure now exits 127 (was 0).

## Pi evidence
- pi-sync (root, 2 MiB bundles, LEANDROS_PISYNC_BUNDLE=2097152): 46/46 changes, 22.2 MB, 96 s
  (was ~25 min with one 64 MiB bundle); second dry run: 0 changes.
- Trial 9c9810f4: `btctl show` ready with no manual start; btd + bthid + netctl alive after
  the NO GPU banner and >5 min; no `killing stray pid` lines in the console log;
  `kill -9 btd` -> `btd exited (status 9) after 88 s, restarting in 1 s`, bthid reconnected.
- 8BitDo was off (`connection to E4:17:D8:82:A2:0A failed status 0x04` = page timeout), so
  auto-connect/event node not verified. Pi returned to good 1f82dbdc, not promoted.
- Note: $SP/piirq/boot.log was not being written during this session; used `remote.py run dmesg`.

## Not done
- Optional item 4 (skip greetd without /dev/dri/card0) and the regress grep for
  `killing stray pid .* (btd|bthid|netctl)`.
