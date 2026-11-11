# lane iwlgaps 2026-11-11 - REFUTED (no code change)

Item: iwlwifi gaps (TODO). Laptop 840 G5 / 8265, AP "Anvara" ch 1 (2.4 GHz), same spot, Linux 7.2.9 baseline vs main d831fc3f.

- Already implemented (TODO text was stale): WMM parameter updates from later beacons (`wmm_new`/`wmm_updates`,
  `ht:` line "wmm set N updates M"), STATISTICS_NOTIFICATION v13 (`fw_stats:` in /proc/net/nic), antenna comparison
  (lane wifi2), rate-control column search incl. MIMO/SGI on 5 GHz (`iwlwifi.rscol`, default on).
- Sparse ping loss (gateway, 1 pkt/s, 300 s): EndeavourOS iwlwifi 4/300 lost (1.33 %, max rtt 2.7 s); LeandrOS 0/300.
  Dense (0.2 s x 1000) Linux 0 %. So the 1-4 % is link/AP behaviour (power save + crowded channel), not a LeandrOS driver bug.
- 2.4 GHz on LeandrOS: MCS7 SGI SISO, A-MPDU 28447 txed / 25988 acked (8.7 % subframe loss, BAR recovered), 40 MB push 15.5 s
  (~20.6 Mbit/s), pull 9 s (~35 Mbit/s). fw_stats cts_timeout 791 -> 937, ack_timeout 26 -> 265 over a 5 min idle ping:
  channel contention, same as lane wifi2's ch 6 numbers. No driver-side cause found; Linux exposes no per-station retries
  without iw/debugfs rc_stats here, so no retry A/B.
- Not measured: 5 GHz MIMO/STBC/SGI (the stick has only the "Anvara" profile; the 5G passphrase was not available; Linux's
  profile is user data and was not read).
- Left open in TODO: 5 GHz TX MIMO A/B once a 5G profile is on the stick.
