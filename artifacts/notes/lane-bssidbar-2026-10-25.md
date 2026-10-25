# Lane bssidbar (openbugs L12 #63), 2026-10-25: FIXED (laptop-verified, promoted)

Commit 589d1889 (+ merges of main). Files: drivers/src/iwl_agg.rs, iwlwifi.rs, iwl_assoc.rs, wifi_sim.rs, iwlwifi-tests/agg_tests.rs.

## Linux comparison
- iwlmvm sends NO BAR from a BA notification: iwl_mvm_tx_reclaim marks every reclaimed frame acked ("failures come as
  single frames"). mac80211 sends a BAR only for single failures (AMPDU_NO_BACK, SSN = SN + 1, any non-ACK except
  flushed FIFO_FLUSHED/DRAIN_FLOW and filtered DEST_PS). A failed BAR stays pending and is re-sent after the next
  successful TX (ieee80211_set_bar_pending / check_pending_bar). mac80211 itself never tears a session down for BAR failures.
- The handoff's "1653 unacked subframes" is cumulative over AP2 + ctl, and it counts retries. On ctl there were 231 subframes, 205 acked.

## Change
- Extra over Linux: after a partial BA (txed_2_done < txed), send a BAR with SSN = SN of scd_ssn (the first frame the firmware
  has not finished), so it never skips a frame still being retried. Rate-limited to 1 per 50 ms; deferred BARs go out from poll_tx_agg.
- Single failures: BAR on every Linux-BAR status (not just short/long limit). BAR TX status tracked by ring index; a failed or
  timed-out (1 s) BAR is retried; after 3 failures in a row: DELBA (initiator), ADD_STA tid_disable_tx, A-MPDU off, re-ADDBA after 10 s.
- tx_ba line: + partial, bar_fail, bar_teardown.

## Evidence
- Guard: sim_bar_hole (recipient holds its window at a hole for 300 s and drops frames beyond it; ACK-clocked sender).
  RED with the old on_ba: resume None, 804 delivered, 0 BARs (stalled). GREEN: resumes after 2 ms with 1 BAR. Failing BARs -> teardown after 3.
  Host tests 166/166; wifisim `ba-hole-bar` + `ba-bar-fail-teardown` ok; aarch64 regress PASS (after main b8a9ef77 harness fix).
- Laptop trial 9323598ad2 (main 9323598a), 3 tries: soft switch to 84:78:48:d8:7e:79 plus 3x 24 MiB pushes, switching back after 6 min:
  every push rc=0 (47-123 s). Max ping gap 5 / 8 / 4 s, first reply 2 s after the switch, no outage, no deauth. BAR log lines present
  (e.g. "BAR TID 0 SSN 39 (partial BAs 9, single failures 1)"). Back on 74:83:c2:ba:21:df. PROMOTED (good sha 74be66f3).
- Caveat: the old outage did not reproduce often (1 time out of several), so "fixed" means 3 of 3 clean tries.

## Open
- The ctl pushes are slow (~2-4 Mbit/s at -77 dBm). trial.sh's on-laptop grep fails (no grep on the laptop); read the BAR count from the pulled klog.
