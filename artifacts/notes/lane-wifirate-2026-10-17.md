# lane/wifirate — 2026-10-17

The problem, from the laptop logs (HP EliteBook 840 G5, Intel 8265): Firefox
video stutters. About 40 % of TX attempts fail at -57 dBm, ping ranges from
19 to 752 ms, and the AP retransmits 9 % of its unicast frames (our `dup`
counter), so our ACKs and our TX are received poorly.

Base: local main a3f226cc.

## What changed

| Area | Change | Where |
|---|---|---|
| Rate control | New `iwl_rs::RateCtl`, a cut-down Linux mvm `rs.c` for legacy rates. The initial rate comes from the beacon RSSI (rs_get_initial_rate tables: -57 dBm gives 54 M). Each TX_RSP charges the retry chain: attempts 0..failure_frame are read off the table the firmware had, the last attempt gets the frame's verdict. Each rate keeps an EWMA of its success ratio; a window counts as valid after 8 successes or 3 failures and goes stale after 3 s. The up/down/stay decision is `rs_get_rate_action` on expected throughput (success ratio x Linux expected_tpt_legacy). LQ chain: current rate x2, each lower rate x1 down to the lowest basic rate, which fills the rest. LQ is resent at most every 100 ms, async, from the TX_RSP path, and a monitor tick flushes any pending table. Antenna A/B toggles after 8 outright failures in a row, or when the lowest rate fails; at most once per 2 s, and only if valid_tx_ant has both antennas. Only 0x82/0x83 count as failures; postponed, PS and dropped frames are ignored. A response whose initial rate does not match the current table (sent before the latest LQ, or EAPOL at a fixed 12 M) is counted as `mismatched` and skipped. | `drivers/src/iwl_rs.rs`, `iwlwifi.rs` (`on_tx_resp`, `send_lq_if_due`, `connect_steps`) |
| Diagnostics | TX_RSP now parses `bt_kill_count` (p[1]), `failure_rts` (p[2]) and `reduced_tpc` (p[22]). New `TxStats` counters: `enq`, `ok`, `fail_fw`, `busy`, `notready`, `capped`; status histogram (01 02 4x 82 83 88 8a 90 other); retries histograms for ok and failed frames; final-rate histograms for ok and failed frames; bt_kill, rts_fail, tpc_reduced, mgmt ok/fail. Per-queue used/read/write and the age of the last TX_RSP. The first 20 data-queue failures of each association are logged. | `iwl_rs.rs`, `iwl_assoc.rs`, `iwlwifi.rs` |
| Output | Every 60 s: `wlan stats` (old line; `tx` = ok, `failed` = all failure kinds), then `wlan tx 60 s: <deltas>` and `wlan tx state: rc <rate> ant <A/B> ... sr <per-rate %/attempts>; q4 ...; q5 ...; holds N`. In `/proc/net/wifi`, after the scan table: a blank line, `# TX (...)`, then `tx:`, `tx_status`, `tx_retries`, `tx_rate`, `tx_coex`, `tx_mgmt`, `tx_hold`, `tx_queues`, `tx_rate`, `erp` lines. Readers checked: wifiscan only `cat`s the file. netroamtest and wifisimtest grep for SSID/JOIN rows, and with the sim the file is unchanged. No NM-shim or netctl code parses it. `/proc/net/nic` keeps `tx_frames`/`tx_failed` and adds `tx_counts:` and `tx_rate:`; `stack_tx` gains `N held`. | |
| Backpressure | netdev has a new `tx_ready` op (wired NICs and wifi_sim always return true). `NicDevice::transmit()` returns None while it is false, so smoltcp holds the frames. A `send()` that returns false now counts as `tx_drop` instead of a sent packet. iwlwifi reports ready below 64 frames in the BSS ring and refuses data above 96 (`capped`; this catches replies sent from the RX path). Once a held queue drains below 32, iwlwifi kicks netd. | `drivers/src/netdev.rs`, `servers/net/src/lib.rs`, `iwlwifi.rs` |
| ERP | `Bss.erp` is parsed from beacons. At association, `use_cts_prot` = ERP Use_Protection and Barker mode forces the long preamble. Tracked beacons are watched for ERP changes, and a change re-sends MAC_CONTEXT MODIFY. Short slot now comes from the association-response capability (it used to come from the scan beacon). | `wifi_frame.rs`, `iwlwifi.rs` (`update_erp`) |
| Knobs | `iwlwifi.rc=0`: the old fixed table. `iwlwifi.txant=a\|b`: forced antenna, no toggle (applies with rc=1). `iwlwifi.maxrate=<Mb/s>`: caps both the rc list and the fixed table. `iwlwifi.btcoex=wifi`: BT_CONFIG mode 3 (BT_COEX_WIFI) with no modules, i.e. Linux's `bt_coex_active=0`. The boot log prints a `TX: rate control ..., antenna ..., max rate ..., BT coex ...` line. | |

Not done: STATISTICS_NOTIFICATION is still ignored, because its layout is version-dependent. Antenna "search" in the Linux column sense is also left out: we only toggle on failures, and never compare antennas while the link is healthy.

## Tests

- Host: `cd drivers/iwlwifi-tests && cargo test --target aarch64-apple-darwin` passes 82 tests, 17 of them new in `rs_tests.rs`. They cover:
  - TX_RSP fields, ERP, rate-code round trip, LQ bytes, retry chain, initial rate.
  - `rate_action` rules.
  - The state machine:
    - A good link stays at 54 M.
    - The 60/75/97 % link settles at 36 M.
    - LQ rate limiting and mismatch handling.
    - Charging the retry chain.
    - Antenna toggle, forced or invalid antenna.
    - Recovery after stale windows.
  - Counter splitting and deltas, BT_COEX_WIFI bytes.
- QEMU: regress and net-e2e on x86_64 and aarch64 (results in the lane report). The real 8265 cannot be exercised in QEMU. These runs show the netd backpressure change and the shared netdev/wifi_frame changes don't break anything (wifi_sim).

## Laptop A/B test plan (no deploy done; kernel `wifirate-<hash>` built)

Use the laptop-deploy-kernel skill and add the knobs per boot. Take credentials from the existing profiles, never from the cmdline. Run each case on **Anvara (2.4 GHz)** and on **Anvara 5G**, with the laptop in the same spot as the -57 dBm logs.

| Boot | Knobs | Purpose |
|---|---|---|
| A | `iwlwifi.rc=0` | Baseline: old fixed table, now with the new counters. The histograms show *why* frames fail. |
| B | (none) | Rate control, automatic antenna. |
| C | `iwlwifi.txant=b` | Is antenna A the bad one? |
| D | `iwlwifi.btcoex=wifi` | Does BT coex cost us? Compare bt_kill and 8a. Bluetooth may suffer; test with BT idle and with BT audio. |
| E | `iwlwifi.maxrate=24` | A cap that rules out "high rates simply fail" independently of rc. |

Workload per boot: 2 min idle, then `ping -c 300 -i 0.2 <gw>` (min/avg/max/loss), then 3 min of a YouTube 1080p video in Firefox (watch for stutter), then `cat /proc/net/wifi /proc/net/nic`. Grab the `wlan tx 60 s` / `wlan tx state` lines from dmesg.

How to read the histograms:
- **status 83** (long limit, no ACK after all retries): genuine air loss. If **retries(ok)** is spread over 1–15, we are retrying a lot before success, which points at the rate being too high or at antenna/RF trouble.
- **status 82** with large `rts_fail`: CTS is not coming back. Look at protection and hidden nodes (`erp:` line, `cts_protection`).
- **status 8a / bt_kill > 0**: Bluetooth coexistence is killing attempts. Boot D should make these go to 0 and lower fail_fw.
- **status 4x**: the firmware postponed frames (quiet period, BT priority). **88**: the AP thinks we are dozing. **90**: the firmware dropped frames. Each points somewhere other than the rate.
- **rate(ok)**: with rc=0 it shows where frames actually got through. If most succeed at 24–36 M after failing at 54/48, the fixed table wasted 4–6 attempts per frame, and rc (boot B) should fix exactly that: fewer retries(ok), lower avg ping, fewer AP dups (`rx_filtered dup` in /proc/net/nic).
- **wlan tx state `sr`**: the per-rate success % and attempts in the window. A healthy link sits where sr ≥ 85 %. If rc keeps toggling the antenna (`toggles` climbing), the RF on both antennas is poor; compare with boot C.
- **busy / capped / tx_hold episodes / stack_tx held**: before this lane, busy meant silent drops at a 240-frame ring. Now the stack is held at 64 and `capped` should stay near 0. A ping max far above about 64 frames × the per-frame airtime means queueing outside the driver.
- **tpc_reduced > 0**: thermal/SAR power cut, not a rate problem.
- **mismatched**: frames that were in flight across an LQ change. A few % is normal.

Expectations:
- On 2.4 GHz at -57 dBm, rc should settle at the highest rate with ≥85 % success, likely 36–48 M. Overall failures should drop from about 40 % to under 10 %.
- Ping max should fall from about 750 ms to below about 100 ms under load, thanks to backpressure.
- Anvara 5G should start at 54 M and stay at a high rate if the signal is better there.
- If boot A already shows mostly 8a/bt_kill, BT coex is the culprit rather than rate. Boot D is then the real fix candidate (a default change would need the BT-audio test).
