# lane/wifiht — 2026-10-17

Goal: get the laptop's Intel 8265 off legacy 11b/g association. Before this
lane we associated without QoS or HT. 'Anvara' (ch 6, -74 dBm, HT AP) sent
us `rate 18M:14061 24M:3564` with 27 % retries, and downloads ran at about
1.6 Mbit/s. Base: local main 73fbe8e1 (Merge lane/udp).

Done: steps 1-3 of the lane plan (WMM/QoS, HT 20 MHz, RX Block Ack with a
host reorder buffer). Not done: step 4 (TX aggregation), see the end.

## What changed

| Area | Change | Where |
|---|---|---|
| Shared 802.11 pieces | New hardware-independent module, used by both iwlwifi and wifi_sim. It handles: the WMM Information / Parameter elements and their EDCA mapped into the firmware's AC order (BK, BE, VI, VO; TXOP in µs, as iwl_mvm_set_fw_qos_params does); our HT Capabilities element (what Linux iwl_init_ht_hw_capab advertises for the 8265, restricted to 20 MHz: LDPC, SGI20, RX STBC 1, TX STBC with 2 chains, SM PS disabled, MCS 0-15, A-MPDU 64K / 4 µs density, 144 Mb/s highest); parsing of the AP's HT Capabilities / Operation; ADDBA request / response and DELBA action frames; the BAR control frame; `answer_addba` (mac80211's rules); and the **RX reorder buffer** (mac80211 `ieee80211_sta_manage_reorder_buf` / `ieee80211_sta_reorder_release`: 100 ms hole timeout, window moves, BAR, the "not started" pass-through, plus a copy-free fast path for in-order frames). | `drivers/src/wifi_ht.rs` |
| RX filtering split | `RxGuard::check` is split into `check_addr` (addresses / fragments, run before reordering) and `check_seq` (duplicate detection + CCMP replay, run in sequence order after the reorder). This is mac80211's order. A replay check made before reordering would reject the retransmitted subframes of an A-MPDU, since their PN is lower. `Bss` and `AssocResp` now keep their raw IEs. | `drivers/src/wifi_frame.rs` |
| Firmware encodings | MAC_CONTEXT gets `edca` (qos_flags UPDATE_EDCA + the AP's EDCA), `ht` (qos_flags TGN) and `ht_prot` (HT_PROT\|FAT_PROT from the AP's HT operation mode). ADD_STA gets the immediate-BA fields (`add_sta_rx_ba`: STA_MODIFY_ADD/REMOVE_BA_TID, SSN, rx_ba_window, as iwl_mvm_fw_baid_op_sta does), U-APSD fields, and `sta_ht_flags` (MIMO2/SISO from the AP's streams and SMPS, RTS_MIMO_PROT, A-MPDU size / density). TX_CMD for a QoS data header (26 bytes) clears TX_CMD_FLG_SEQ_CTL so the driver numbers the frame, adds 2 pad bytes with TX_CMD_FLG_MH_PAD, and sets offload_assist MH size (IV included) + PAD (iwl_mvm_tx_csum / gen1 pcie tx). HT rate_n_flags v1 (`ht_rate`, `rate_mcs`), and LQ with `mimo_delim`. Legacy encodings are byte-identical (host tests). | `iwl_assoc.rs`, `iwl_cmd.rs` |
| Rate control | One ladder ordered by expected throughput: with HT it is the legacy rates below MCS 0 (1, 2, 5.5, 6 M), then MCS 0-7 (short GI with txsgi=1), then (txmimo=1) the MIMO2 MCS that beat MCS 7. Expected throughput comes from rs.c's no-aggregation SISO/MIMO2 20 MHz tables. The initial MCS comes from rs_optimal_rates_ht (-74 dBm → MCS 3). Retry chain: the current rate and the two HT rates below it get 2 tries each, then lower rates 1 try each down to the basic floor. Histogram buckets now include MCS 0-15. The legacy ladder and its tests are unchanged. | `iwl_rs.rs` |
| Association | QoS / HT is decided from the beacon (HT needs WMM + HT cap + HT op + an HT SKU, as mac80211). The assoc request carries HT Capabilities + the WMM Information element. What the AP granted is read from the assoc response (WMM element → QoS, HT caps → HT) and then: ADD_STA modify with HT flags + U-APSD fields, LQ with MCS, MAC_CONTEXT with EDCA/TGN/HT protection, SF watermark 6144/8192 by AP streams. Data then goes out as QoS Data TID 0 with driver sequence numbers (EAPOL included, at the fixed rate). Beacons are watched for HT operation mode changes, which re-send MAC_CONTEXT (with the ERP watch). | `iwlwifi.rs` `connect_steps`, `tx_data`, `update_erp` |
| RX Block Ack | An ADDBA request (handled in the monitor loop and also during the 4-way handshake) is answered with: host reorder session first, then ADD_STA with the BA session, then the ADDBA response (immediate policy, window ≤ 64, A-MSDU echoed). If the firmware refuses, or the link is not HT, or iwlwifi.ba=0, the answer is status 37. A DELBA from the AP flushes the buffer and sends ADD_STA remove-BA. A BAR from the AP moves the window, if the firmware passes BARs up; if it does not, the 100 ms timeout covers holes. Reorder timeouts are checked on every RX ring service. QoS frames of a BA TID go through the reorder buffer; everything else is unchanged. | `iwlwifi.rs` `handle_action`, `rx_data`, `rx_ctl`, `service` |
| Diagnostics | `/proc/net/nic` (wifi0) gains two lines, `ht:` and `rx_ba:`. A `wlan ht:` line is logged every 60 s. The `rate` histograms show `MCSn`. The NM applet's bitrate comes from rate control (`tx_rate_kbps`). | `iwlwifi.rs` |
| Sim | Every sim AP is an HT AP unless flag `l` (beacon IEs: HT cap/op, hostapd's WMM params). After association the station's assoc request IEs are built and parsed with the shared code. The AP sends an ADDBA request; the station answers with `answer_addba`, and the AP reads the response. The downlink then goes through an A-MPDU emulation (bursts of 16; per-mille hold → retransmitted at the head of the next burst, dup, loss → BAR) and the station path that iwlwifi runs: RxGuard check_addr → reorder → check_seq → data_to_ethernet. Knobs: `wifisim.ht=0`, `wifisim.ba=0`, `wifisim.reorder=0`, `wifisim.ampdu=<hold>:<dup>:<loss>` (default 30:10:0, so every wifi_sim boot in regress / net-e2e exercises the reorder path). New selftest checks: `ht-addba`, `ht-reorder` (2000 synthetic frames, 30 % held, 5 % dup), `ht-reorder-loss` (2 % lost: BAR / timeout). | `wifi_sim.rs` |
| Bench | `wifihtbench.py`: one boot per case. It sends N MB host → guest over wifi0 (ssh `cat \| wc -c` through the wifi0 port forward) and prints the `ht:` / `rx_ba:` lines. | `.claude/skills/run-leandros/wifihtbench.py` |

Verified against Linux 6.6 (`~/.cache/leandros/linux-src`): the 8265
(family 8000, `mq_rx_supported` false) uses the non-MQ RX path
(`mvm/rx.c`). iwlmvm sets `SUPPORTS_REORDERING_BUFFER` only with the new RX
API, so mac80211 reorders in software for this device. iwl_mvm_sta_rx_agg
returns right after the firmware op when `!iwl_mvm_has_new_rx_api`, so no
BAID or firmware reorder buffer is involved. The host must reorder, which is
what this lane does.

## Knobs (kernel command line)

| knob | default | effect |
|---|---|---|
| `iwlwifi.ht=0` | | **Back-out switch**: legacy association exactly as before this lane (no WMM IE, no HT IE, non-QoS data, legacy MAC context / ADD_STA bytes) |
| `iwlwifi.ht=wmm` | | QoS (WMM) only, no HT and no Block Ack (bisect step 1) |
| `iwlwifi.ht=1` | on | WMM + HT 20 MHz + RX Block Ack |
| `iwlwifi.ba=0` | on | decline ADDBA (status 37): HT rates without A-MPDU |
| `iwlwifi.htrate=0` | on | HT association, but TX at legacy rates only |
| `iwlwifi.txsgi=1` | off | TX short GI when the AP accepts it |
| `iwlwifi.txmimo=1` | off | TX MIMO2 MCS 12-15 above MCS 7 (BT coex shares an antenna: off by default) |
| `iwlwifi.smps=static\|dynamic` | off | SM power save we advertise. With static the AP sends us one stream only: use it if MIMO RX looks bad |
| `iwlwifi.ldpc=0` | on | do not advertise LDPC RX |

The boot log line `[iwlwifi] QoS/HT: ...` shows which knobs are active.

## What to look for on the laptop

Boot log after association:
- `associated: AID n, AP capability ..; QoS (WMM) + HT 20 MHz: AP 2 stream(s), SM PS 3, A-MPDU 2^(13+3) density ..., HT op mode 0x....`.
  If this says `legacy (the response carries no WMM element)`, the AP did not take our WMM IE.
- `rate control: start at MCS3 on antenna A ...; rates 1M/2M/5.5M/6M/MCS0/.../MCS7`.
- `ADDBA request TID 0 (dialog .., SSN .., window 64, ...): accepted, window 64`.
  `the firmware refused the session` means ADD_STA BA failed: report the status.
- No `firmware error` / `REPLY_ERROR` after association. A firmware assert right after
  the first QoS frame would point at the TX_CMD pad / offload_assist. Back out with
  `iwlwifi.ht=wmm`, then `iwlwifi.ht=0`.

`cat /proc/net/nic` during a download (scp / YouTube):
- `ht: iwlwifi.ht=1; link qos 1 ht 1 tx_mcs 1; ap streams 2 sm_ps 3 ampdu 3 density ..; op_mode .. ht_prot ..`
- `rx_ba: sessions 1 [0:win64@NNNN] addba 1 accepted 1 declined 0 fw_refused 0 delba 0 bar N no_session N; reorder direct N buffered N released N old N dup N skipped N timeouts N bars N max_stored N stored N`
  - `buffered` > 0: A-MPDUs are arriving and holes are being closed (this is the download win).
  - `timeouts` high relative to `buffered`: subframes are lost for good. Each timeout costs 100 ms of
    stall. `bar` 0 with timeouts > 0 means the firmware does not pass BARs up, so the timeout is all we have.
  - `no_session` high: QoS data on TIDs without a BA session (fine for VO/VI ones).
- `rx_quality: ... rate MCS..:N`: the AP's rates to us. We hope for MCS 4-7 or 12-15 instead of `18M`.
  `retry %` should drop: with BA the AP retries only the missing subframes.
- `tx_rate: rc MCSn ant A ...; sr MCS3:..%/..`: our uplink rate control over MCS.

Comparisons (one boot each): `iwlwifi.ht=0` (old), `iwlwifi.ht=wmm`,
`iwlwifi.ba=0`, default. Measure an scp of a large file to the laptop and
`ping` under load, as in the wifirx notes.

## Tests

- Host: `cd drivers/iwlwifi-tests && cargo test --target aarch64-apple-darwin`: 115/115.
  33 tests are new: WMM parse / defaults / firmware mapping, HT cap bytes for 2x2 / 1x1,
  BSS element parsing, BA parameter bits, an AP's real ADDBA request, response / DELBA
  round trips, `answer_addba` rules, BAR, QoS data → Ethernet, the guard split, and the
  reorder buffer (in order, holes, dup / old, timeout, fresh hole, window move, BAR,
  4096 wrap, pre-session, sessions, 5000-frame random bursts, fast path against `rx`,
  48-frame window across the wrap). Also: ADD_STA BA / HT flags, MAC_CONTEXT QoS / HT
  bytes (legacy bytes unchanged), the QoS TX_CMD pad / offload, HT rate codes / LQ, the
  HT ladder and retry chain, and HT rate control climbing / settling. The legacy rate
  control tests are unchanged and pass.
- aarch64 (HVF): `--suite regress` PASS (26/26, including wifisimtest with the new
  `ht-addba`, `ht-reorder`, `ht-reorder-loss` checks); `--suite net-e2e` PASS (5/5:
  nmrealtest and netroamtest run their traffic through the default A-MPDU emulation).
- x86_64 (TCG): `--suite regress` PASS (26/26), `--suite net-e2e` PASS (5/5).
- wifi_sim measurements on aarch64 (`wifihtbench.py`): 8 MB host → guest over wifi0,
  5 runs. The first run of each boot is a cold start and is excluded below:

  | case | MB/s | stack saw |
  |---|---|---|
  | HT, BA, no perturbation (`ampdu=0:0:0`) | 62.4 | in order |
  | holes 15 % + dups 2 %, reorder on | 61.6 | 0 out-of-order (18 470 frames waited in the buffer, max 16 at once) |
  | same, `wifisim.reorder=0` | 49.2 (-20 %) | 3 245 out-of-order frames, 501 duplicates |
  | legacy (`wifisim.ht=0`), earlier run | ~70 | in order |

  The reorder buffer costs nothing measurable and hides the A-MPDU retransmission
  pattern from TCP. The airtime win (A-MPDU, MCS rates) only exists on real radio.
  With 0.2 % losses on top, both cases drop, as TCP loss recovery dominates; the
  reorder side handled the 38 losses with 38 BARs and 0 timeouts.
- Laptop kernel: `LEANDROS_KERNEL_TAG=wifiht-d4342399` built (`target/final-x86_64/`),
  not deployed.

## Not done / open

- **TX aggregation** (ADDBA request from us, SCD aggregation queue, BA_NOTIF
  reclaim, `tid_disable_tx` cleared): not started. Uplink stays one MPDU per
  channel access (ACKs of downloads are small; uploads gain little at -74 dBm).
- QoS Null frames: not needed. We never enter power save and we do not
  probe the AP with nullfunc (the firmware's missed-beacon notification
  covers link loss).
- WMM parameter set changes in later beacons are not applied (only the
  association's EDCA).
- 40 MHz: not advertised (20 MHz only, per the plan). TX STBC / LDPC in LQ
  `ss_params`: not used (0, as Linux when BT coex forbids it).
- The 2.4 GHz 20/40 coexistence IE is not sent (not needed without 40 MHz).
