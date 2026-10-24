# lane/rscol — 2026-10-24 (L6.1 remainder: rs.c column search) — FIXED, laptop promoted

Laptop EliteBook 840 G5, Intel 8265 (2x2), 'Anvara 5G' 5 GHz / 40 MHz, -73 dBm this session.
Commit 097c2d91 (column search); trial a0ad7cd53e99 (build rscol-t1 = main 00a345a9 + lane) PROMOTED as good,
cmdline `iwlwifi.mgr=1`. Contains i915ddb 35c6790e, regdom e10ce203, taskshot ad95f7b0, deskplay 00a345a9.

## What
`drivers/src/iwl_rs.rs`: columns SISO / SISO-SGI / MIMO2 / MIMO2-SGI (rs.c rs_switch_to_column). Every 10 s
one other column is probed for 1.5 s (up to +3 s until its window is valid) with rate scaling inside it, starting
at its slowest rate whose ideal throughput beats the current measured one (rs_get_best_rate). Kept when its best
measured throughput (success ratio x expected tpt) >= 108 % of the old column's, else the old ladder + windows
are restored and that column backs off (20, 40 ... 160 s). Aborted on 8 consecutive failures. The MIMO2 ladder is
MCS0 then MCS8-15 (SISO MCS below MCS8's expected tpt). Capabilities: SGI only when the AP receives SGI at the TX
width; MIMO2 only with both TX chains, no single forced antenna, AP rx_streams >= 2, not SM-PS static, 5 GHz only
(BT coex shares a chain on 2.4). `iwlwifi.rscol=0` disables; klog prints `rate control: columns ...`;
/proc/net/nic tx_rate shows `col <name> colprobes N kept K`. Forced txmimo=1 / txsgi=1 keep their old ladders.

## Tests
New drivers/iwlwifi-tests/src/col_tests.rs (5 cases; all 5 FAIL with enable_col_search a no-op): capability
checks, MIMO2 ladder + chain encoding (both chains, no STBC on MIMO), switch to MIMO2 when it delivers (goodput
>= 1.3x SISO-only), revert + back-off when other columns are worse (<= 12 probes / 180 s, goodput >= 90 %), keep
SGI when it delivers as well. Host tests 147 pass (142 old). aarch64 regress PASS; x86_64 release build.

## Laptop A/B, same session (50 MiB fresh random, wifirate method)
| kernel | pull (laptop TX) Mbit/s r1/r2/r3 | push Mbit/s | rc / column at end of pulls |
|---|---|---|---|
| good d549a3e40960 (regdom) | 12.9 / 5.5 / 11.2 | 21.6 / 13.7 / 40.0 | MCS4, MCS4, MCS2 (/40 SISO) |
| trial a0ad7cd53e99 (rscol) | 14.7 / 16.4 / 16.1 | 24.2 / 18.7 / 13.8 | MCS4s SISO-SGI, MCS8s MIMO2-SGI, MCS3s SISO-SGI |
- Pull median 11.2 -> 16.1 Mbit/s; min 5.5 -> 14.7. Push within noise. After the pulls: col SISO-SGI, 18 probes / 7 kept.
- SGI is kept most of the time; MIMO2 wins only intermittently at this position (MCS8/40 MIMO2-SGI once).
- Heartbeat 300/300, max gap 1.77 s, 0 > 2 s. regulatory: BR.

## Open
- The column flips several times per minute between SISO-SGI and MIMO2(-SGI) under bulk TX (7 of 18 probes kept);
  harmless here but a stay-in-column timeout / hysteresis on re-entry may reduce probe cost.
- Windows still go stale at 3 s; no per-column stored history (rs.c keeps none either).
