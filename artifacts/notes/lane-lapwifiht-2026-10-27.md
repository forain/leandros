# lane/lapwifiht — 2026-10-27 — DELIVERED (VHT 80 MHz); HT/WMM/BA were already done

Laptop EliteBook 840 G5, Intel 8265 (fw API 36).

## The premise was stale
The "legacy 54 Mb/s, 27 % retries" state was from before 2026-10-17. Earlier lanes had already
done it: WMM/QoS, HT caps, HT station flags, RX A-MPDU reorder (wifiht), TX A-MPDU (iwl_agg.rs),
HT40 + driver rate control (wifirate/wifirate2/rscol). The 8265 uses driver rate scaling (LQ_CMD).
TLC_MNG_CONFIG is 9000+ only, so there was no firmware TLC to configure.
On the shipped good kernel 7a0d1c759331 ('Anvara' ch 1, HT20, -56 dBm) wifibench 3x16 MB measured
**down 45.3 / up 39.5 Mbit/s**, RX MCS12 (2 streams), ap_retry 9.2 %.

## What was missing: VHT. Now done (commit 515a6fc1)
- `wifi_ht.rs`: parses VHT Capabilities (191) and VHT Operation (192). `VhtOp::center80` covers
  80 MHz, 160 MHz in both encodings and 80+80, each used as its primary 80. `vht_cap_ie` advertises
  what Linux iwl_init_vht_hw_capab does for the 8000 family, without beamforming: 3895-byte MPDUs,
  SGI80, RX LDPC, RX STBC1, TX STBC, A-MPDU exponent 7, and MCS 0-9 on 2 streams.
- `iwl_cmd.rs`: `phy_ctxt_cmd_80` (PHY_VHT_CHANNEL_MODE80 plus the iwl_mvm_get_ctrl_pos control
  position) and `sta_vht80_flags` (FAT_EN_80MHZ, MIMO2 from the VHT streams, max(HT, VHT) A-MPDU
  exponent).
- `iwl_assoc.rs`: `rate_vht`. `rate_r500k` no longer reads VHT rates as legacy. VHT MCS5 has
  low byte 0x05, which is the 12M OFDM PLCP.
- `iwlwifi.rs`: 80 MHz is used when the link is already HT40, the AP sends VHT capabilities
  and an 80 MHz operation, the NVM SKU has VHT, and all 4 channels carry NVM MHZ80. Knob
  `iwlwifi.vht=0` turns it off. New log line `BSS VHT: ...`. In /proc/net/nic, `ht:` shows
  `width 80 vht` and `rx_quality` gains `vht VHTmcs x nss` counters and `80 MHz frames`.
- TX still uses the HT40 rate ladder (MCS0-15 at 40 MHz inside the 80 MHz channel). There are no
  VHT TX rates in iwl_rs.rs yet.
- Host tests: `drivers/iwlwifi-tests/src/vht_tests.rs` (5 tests). Run them with
  `cargo test --release --target aarch64-apple-darwin`: 178/178 pass.

## Evidence
- Box x86_64 full regress of lane/lapwifiht: 54 suites, 0 bad, wifisimtest PASS.
- Laptop trial 3c83c75894ce (build 515a6fc1), switched live to 'Anvara 5G'
  (8a:78:48:d8:7e:7a ch 40, -53 dBm). Link showed `width 80 vht`, RX VHT5x2/VHT6x2/VHT7x2,
  and all 80 MHz frames were VHT.
  wifibench 3x32 MB: **down 170.9 Mbit/s (171.3/170.9/149.4), up 44.0 (62.6/37.1/44.0)**,
  ap_retry 10.1 %, tx_fail 0 %, ping 3.9 ms avg. That is 3.8x the HT20 downlink above. Earlier
  5 GHz HT40 runs measured 11-20 Mbit/s, but at -76 dBm, so they are not a same-spot baseline.
- Not promoted. The laptop was rebooted back to good 7a0d1c759331.

## Open
- VHT TX rates in rate control (VHT MCS0-9 x 1-2 streams at 80 MHz). This is the main gain still
  available for upload, currently ~44 Mbit/s.
- The laptop autoconnects 'Anvara' (prio 10) instead of 'Anvara 5G' (prio 0). That is a profile
  priority, not a driver issue.
- The good kernel shows `tx_ba ... ba 120819 acked 3247 partial 120265 bars 120266`: almost every BA
  notification counts as partial and sends a BAR. The ratio looks wrong; check iwl_agg BA accounting.
- No same-session HT40 vs VHT80 A/B was run (needs a trial with `iwlwifi.vht=0` added to the full good cmdline).
- aarch64 was built and tested only through host tests. The code is arch-neutral, iwlwifi is x86 laptop only.
