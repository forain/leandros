# lane/wifirate — 2026-10-24 (L6.1 + L9.9 laptop re-measure) — FIXED (rate control), L6.1 narrowed

Laptop EliteBook 840 G5, Intel 8265, 'Anvara 5G' ch 40 / 40 MHz, -76..-79 dBm. Commit ec8414e0.
Laptop trial 741538499293 (build wifirate-t2 = main 752673a6 + fix) PROMOTED as good, cmdline `iwlwifi.mgr=1`.

## Root cause: rate control read healthy A-MPDUs as failing
The per-rate success ratio was an EWMA with 1/8 weight per attempt. `on_ampdu` records an A-MPDU's
acked subframes first and the failed ones last, so the failures dominate: 24 sent / 18 acked read as
42 % instead of 75 % (new guard `ampdu_success_ratio_is_the_window_ratio`: unfixed 422 permille, FAIL).
Under bulk TX the controller therefore stepped down to MCS0/40 and then legacy 9M/12M, although the
A-MPDU ack ratio stayed 73-74 % — this is the "6M/12M legacy rate" of L6.1. Fix: the rs.c window
(`_rs_collect_tx_data`): last 62 attempts, plain ratio, `valid` on window counts. The single-frame test
`lossy_top_rate_steps_down_and_settles` was timing-dependent on its final state; it now asserts the
time share at 36 M (unfixed: 534/1000 with 54 ups/56 downs; fixed: >= 800, 19/20).

## Laptop A/B, same session (50 MiB fresh random each way; pull = laptop TX, push = laptop RX)
| run | kernel | pull Mbit/s r1/r2/r3 | push Mbit/s | rc at end of pulls | pull A-MPDU acked | netd passes / pull | tx_held / pull |
|---|---|---|---|---|---|---|---|
| good1 | 647fb4a8 (i915wc, no shapedl) | 8.2 / 6.7 / 10.3 | 19.7 / 20.3 / 19.9 | MCS0/40, MCS4/40, 9M | 73-74 % | 19.8-25.5 k | 11.6-17.6 k |
| t2 | main + fix | 10.4 / 14.8 / 15.0 | 19.3 / 19.5 / 16.8 | MCS3/40 x3 | 72-74 % | 16.1-16.8 k | 8.3-8.7 k |
| t2 tx40=0 | same, HT TX at 20 MHz | 10.1 / 11.8 / 12.1 | 17.9 / 17.4 / 18.7 | MCS4 (20) x3 | 73-75 % | 18.1-19.7 k | 9.6-10.4 k |
| good2 | 647fb4a8 again | 7.6 / 6.8 / 6.9 | 16.1 / 6.9 / 8.7 | MCS1/40, 9M, 9M | 73-75 % | 23.0-24.8 k | 13.8-15.4 k |

- Laptop TX median 7.2 -> 14.8 Mbit/s (2x); RX unchanged within noise (good2's push dip is session noise).
- `iwlwifi.tx40=0`: worse than 40 MHz (median 11.8 vs 14.8). Keep the default.
- netd_why `immediate` 0-4 per transfer on all kernels (nettx holds on HW); passes per 50 MiB pull -30 %.
- shapedl on HW: tcp_rx during pushes in_order ~40 k / out_of_order 1.8-5.3 k / too_many_holes 0 per run —
  SACK recovery in use, no hole-limit drops. Push throughput did not change (already ~19 Mbit/s).
- t2: heartbeat 300 beats, max gap 1.26 s, 0 > 2 s. Idle CPU (20 s greeter): good 5.3/5.1 %, t2 5.2 %.
- Tests: iwlwifi host tests 141 pass; aarch64 regress PASS (sshsmoke flake passed on rerun); x86_64 build.

## Open (L6.1 narrowed)
- `regulatory: 00` on every kernel this session (wifirate2 saw 'BR' applied after the first scan):
  the MCC update is not taking effect any more, or the chub notification no longer arrives. Check the klog.
- The radio link itself: MCS4/40+ deliver < 45 % at -77 dBm, so ~15 Mbit/s TX is near this position's
  ceiling; 2.4 GHz retries and "2-stream AP at 1.3 Mbit/s" not re-measured (band rule keeps 5 GHz).
- No rs.c column search (SISO/MIMO/SGI switching, stay-in-column timeout); windows still go stale at 3 s.
