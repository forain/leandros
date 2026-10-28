# lane/lapwifitx: design and handoff (2026-10-27)

Worktree `/Users/forain/code/leandros-lapwifitx`, branch `lane/lapwifitx` (based on main a775dba7). Not merged, not pushed.
Laptop data was never collected for this lane: the laptop lock was held by another lane all session and SSH timed out.

## Branch state
- `d9b80497` "iwlwifi: BAR only for real Block Ack holes, ignore BAR-answer BAs in rate control (untested WIP)".
  This is the Part B code below. It is not compiled yet: the tree did not build while Part A was half done.
- Part A (VHT TX rates) is in progress with a fast-worker in the same worktree. It is uncommitted or in a later commit.
  Check `git log` and `git status`. The worker briefing is the Part A spec below.

## Part B: Block Ack accounting (drivers/src/iwl_agg.rs)

### Root cause (strong hypothesis, laptop check pending)
The good kernel shows `ba 120819 acked 3247 partial 120265 bars 120266`. The counters say:
- bars == partial + 1. Every partial BA caused exactly one BAR, and no two partials ever merged inside the
  50 ms BAR rate limit (`BAR_MIN_INTERVAL_MS`). Real data BAs arrive about every 1 ms under load. If those were the
  partials, BARs would merge and bars would be much smaller than partial.
- acked/ba is 0.027, so `txed_2_done` is almost always 0 on the "partial" BAs.

The loop: `TxAgg::on_ba` (old iwl_agg.rs:271-280) marks a BA as partial when `txed_2_done < txed` and wants a BAR at
scd_ssn. The firmware also sends a BA_NOTIF for the AP's BA that answers our BAR. That notif has txed > 0 and
done = 0 and no A-MPDU before it, so it counts as partial and asks for the next BAR. The result is a BAR every 50 ms
forever, about 20 BAR/BA exchanges per second, even when idle.

The same notif also goes to `rc.on_ampdu(rate, txed, 0)` in `Dev::on_ba_notif` (iwlwifi.rs ~930). That feeds
rate control a failed A-MPDU at the current rate 20 times a second, which drags the TX rate down. This is a likely
cause of the low upload on both bands.

A second flaw: old `on_ba` sent a BAR for *any* partial BA. The firmware itself retries unacked subframes while they
are still at or above scd_ssn. A BAR is only needed when a frame the firmware has finished (below scd_ssn) is
missing from the AP's bitmap. That is the case lane bssidbar hit.

### Fix (already in d9b80497)
- `AggStats` gains `ba_unsolicited` and `ba_holes`. New `BaRec` ring `TxAgg::recent` (`BA_RECS` = 6) records
  ba_ssn, bitmap, scd_sn, txed, done and solicited.
- `TxAgg::agg_pending` is set in `on_agg_rsp` (an A-MPDU TX response, frame_count > 1).
- `pub fn ba_hole(ba_ssn, bitmap, scd_sn) -> bool` takes d = (scd_sn - ba_ssn) & 0xfff:
  - d == 0 or d >= 0x800: no hole.
  - d >= 64: hole (the AP's window is stuck far behind).
  - otherwise: hole if any of the bitmap's low d bits is 0.
- `on_ba(&mut self, n, next_sn) -> bool`:
  1. Count the notif and take the solicited flag (agg_pending).
  2. Record it in the ring.
  3. If it is unsolicited: `ba_unsolicited += 1` and return false. It still reclaims (the caller does that), but it
     is not partial, triggers no BAR and is no rate sample.
  4. Otherwise: update txed and acked; partial is still `done < txed` but is statistics only.
  5. If `ba_hole(seq_ctl >> 4, bitmap, sn_of_index(next_sn, scd_ssn))`: `ba_holes += 1` and want a BAR at scd_sn.
     A pending bar_want is still retargeted as before.
  6. Return true.
- iwlwifi.rs `Dev::on_ba_notif`: `let ampdu = self.tagg.on_ba(n, next_sn);`. `txs.ok`, `txs.rate_ok` and
  `rc.on_ampdu` only run `if ampdu`.
- `/proc/net/nic` tx_ba line gains `unsolicited N holes N; last ssn:bitmap@scd/txed/done[u] ...`.
- `sim_bar_hole` now builds real BAs (seq_ctl = AP window start, bitmap from rx_got) and calls `on_agg_rsp` first.
- Tests in drivers/iwlwifi-tests/src/agg_tests.rs: `ba_hole_needs_a_finished_frame_the_ap_lacks`,
  `bar_answer_does_not_start_a_bar_loop`, `partial_ampdu_without_hole_sends_no_bar`, `lossless_flow_sends_no_bars`.
  The existing `ba_hole_resumes_within_1s` must still pass, because it guards the bssidbar router stall.
  Show that the loop test fails on a775dba7's on_ba (the old code returns a BAR on every BA).

### Risk and fallback
The assumption that the A-MPDU TX response comes before its BA_NOTIF follows iwlmvm (`iwl_mvm_rx_tx_cmd_agg` saves
the rate, the BA comes next). If the laptop shows `unsolicited` close to `ba` under upload load, the ordering
assumption is wrong. In that case, mark a BA as a BAR answer by `seq_ctl>>4 == SSN of the BAR sent within the last
BAR_STATUS_TIMEOUT_MS` instead of by agg_pending (keep `last_bar_ssn: Option<(u16,u64)>` in TxAgg, set in next_bar).
Use the `last` ring on the laptop to decide: BAR answers show `ba_ssn == previous BAR SSN`, txed small, done 0.

## Part A: VHT TX rates (drivers/src/iwl_rs.rs, iwl_assoc.rs, iwlwifi.rs)
This is the spec given to the worker.
- Widen the rate id to `pub type RateId = u16`. VHT id = `0x100 | 0x80 | SGI 0x40 | NSS2 0x10 | mcs(0..9) |
  width<<9` (0 = 20, 1 = 40, 2 = 80). Add `is_vht`, `vht_id`. Every HT helper (expected_tpt, expected_tpt_agg,
  nominal_r500k, rate_code, rate_label, rate_id, ladder, mimo_delim) branches on is_vht first.
- Add `asc::vht_rate(mcs, nss, sgi, ant, width)` with: BIT(26) VHT, mcs in bits 0-3, (nss-1) in bits 4-5, width in
  bits 11-12, SGI in BIT(13), antenna in bits 14-16, STBC in BIT(17), LDPC in BIT(27). NSS2 uses ANT_AB.
  `rate_id(rnf)` maps VHT back via `asc::rate_vht`, so `index_of`/`on_ampdu` do not count VHT as `mismatched`.
- Expected throughput comes from the rs.c tables (agg rows):
  - SISO80: 217,429,634,834,1220,1585,1760,1931,2258,2466 (SGI: 241,...,2691).
  - MIMO2-80: 411,834,1232,1625,2387,3066,3402,3733,4316,4723 (SGI: 460,...,5143).
  - 20/40 MHz reuse the HT tables for MCS0-7, plus the MCS8/9 entries.
  - Never generate MCS9 at 20 MHz.
- Config: `HtRates.vht: Option<VhtRates{width, max_mcs_1ss, max_mcs_2ss, sgi}>`. Fill it in iwlwifi.rs connect_steps
  (~2634-2650) only when the link runs VHT (`width 80 vht`). It is None with `iwlwifi.vht=0`, so behaviour is then
  byte-identical to today.
- Ladder: legacy rates below VHT MCS0, then SISO VHT, then NSS2 rates with higher expected throughput. Same column
  search (SISO/MIMO x SGI) and LQ fallback table as HT. The initial rate comes from rssi minus 6 dB at 80 MHz.
- Host tests go in a new `drivers/iwlwifi-tests/src/vhtrs_tests.rs`. All existing tests must pass (178 + new):
  `cd drivers/iwlwifi-tests && cargo test --release --target aarch64-apple-darwin`.

## Not done, by decision
Preferring 5 GHz in autoconnect: drivers/src/wifi_mgr.rs `pick_auto` / `auto_key` already prefer 5 GHz, but only
within equal priority. The profile priority (`Anvara` 10 vs `Anvara 5G` 0) comes first by design (NM semantics).
Overriding it would disobey the user's explicit priorities, so this was left alone.

## Test plan
1. Host tests as above. Then build: `INTEL=1 ./scripts/build-all.sh --arch x86_64` and aarch64. Box regress:
   `$SP/box/regress.sh lane/lapwifitx x86_64` (wifisimtest must PASS).
2. Laptop, one `$SP/lock.sh laptop` hold, trial built after `git merge main`. Use laptop-deploy-kernel; do not
   promote. Check `uname -v` shows the trial.
   - Idle check first: read the tx_ba line twice, 10 s apart. `bars` must not grow while idle. The old kernel would
     grow by about 200.
   - Switch live: `netctl profiles` (for the uuid), then `(sleep 1; netctl connect --uuid <5G uuid>) &`. Never print
     passwords. Wait for `ht:` to show `width 80 vht`.
   - Run `LEANDROS_REMOTE=172.16.149.179 python3 scripts/wifibench.py --mb 32 --runs 3` three times. Target: upload
     clearly above 44 Mbit/s. The tx_ba deltas must show partial and bars as a small fraction of ba, and
     unsolicited <= bars.
   - Switch to 'Anvara' (2.4 GHz HT20) and run wifibench 3x again. The baseline is down 45.3 / up 39.5.
   - Boot `good` back before releasing the lock.
3. Expected log lines:
   - `rate control: start at VHT<m>x<n>/80 ...` with a rates list ending in `VHT9x2/80`.
   - /proc/net/nic tx_ba: `... partial P bars B ... unsolicited U holes H; last ...`, with the `last` entries
     solicited (no `u` suffix) under load.
   - In the rc summary, `mismatched` stays near 0 at VHT rates.
