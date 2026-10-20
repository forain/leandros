# lane/wifirate2 — 2026-10-19/20

Laptop (EliteBook 840 G5, Intel 8265, 'Anvara 5G' ch 40/40 MHz, -78 dBm). Items L6.1, L6.5, L6.7.

## What changed

| Area | Change | Where |
|---|---|---|
| Regulatory | `MCC_CHUB_UPDATE` notifications are now handled (Linux `iwl_mvm_rx_chub_update_mcc`): the firmware's new country is applied with `MCC_UPDATE_CMD`, except a WIFI-sourced one while associated. Before, the firmware stayed on the world domain '00' for good. On the laptop it now reports `'BR' (source 0x5); applying` right after the first scan. | `iwlwifi.rs` dispatch, `iwl_cmd.rs` `parse_mcc_chub` |
| TX coding | Linux rs.c parity for the 8000 family (`iwl8000_ht_params` stbc+ldpc; this firmware lacks API `LQ_SS_PARAMS`, so the rate carries the flag). Single-stream MCS go with STBC on both antennas when the AP receives STBC. HT rates use LDPC when the AP receives LDPC. In STBC mode the legacy part of the retry chain alternates A/B (`rs_fill_rates_for_column` toggle_ant), and antenna probes/toggles are off. The AP's HT cap is logged on the `rate control: start` line (Anvara 5G: 0x09ef = LDPC, RX STBC 1, SGI20/40). Knobs `iwlwifi.txstbc=0`, `iwlwifi.txldpc=0`; also `iwlwifi.tx40=0` (HT TX at 20 MHz on a 40 MHz link; untested on HW). | `iwl_rs.rs` `code`/`table`, `iwl_assoc.rs`, `iwlwifi.rs` |
| Multicast (L6.5) | `MCAST_FILTER_CMD` pass-all for the associated BSSID after association. Without it the firmware drops all group-addressed frames except broadcast (Linux: "if we don't have free ports, mcast frames will be dropped"). Knob `iwlwifi.mcast=0`. e1000e (RCTL.MPE) and r8169 (MAR all ones + AcceptMulticast) already ran all-multicast, so that part of the premise was wrong. | `iwlwifi.rs`, `iwl_cmd.rs` |
| Band choice | The 5 GHz preference is now relative, after wpa_supplicant's `wpa_scan_result_compar`. A 5 GHz BSS (>= -80 dBm) is preferred unless the strongest 2.4 GHz candidate leads by >= 7 dB of SNR (noise floors -89 / -92); at SNR >= 25 dB it always wins. Before, a fixed -80 dBm cutoff applied, whatever 2.4 GHz offered. | `wifi_pref.rs`, `wifi_mgr.rs` `pick_auto` |
| Weak host (L6.7) | Decision: Linux weak-host receive, strict per-interface ARP (arp_ignore=1 behaviour). A unicast IPv4 frame that arrives on one NIC for another interface's address is re-addressed to that NIC's MAC and injected into its stack (sockets live per stack). Replies leave through the owner. `/proc/net/nic` `stack_rx: ... N weak_host`. | `servers/net` `iface::weak_host_holder`, `NicDevice::receive` |

## Laptop measurements (`scripts/wifibench.py --mb 8 --runs 7`, medians; Mac wired)

| kernel / knobs | down Mbit/s | up Mbit/s | ap_retry | A-MPDU subframes acked |
|---|---|---|---|---|
| good 319792106a65, evening (5 runs) | 12.72 | 6.14 | 8.9 % | 46 % (since boot) |
| good, morning | 5.68 | 6.14 | 13.5 % | 73 % |
| good, morning, again | 11.15 | 9.73 | 11.4 % | - |
| trial, default (STBC+LDPC), morning x3 | 12.13 / 10.49 / 13.79 | 12.44 / 11.77 / 9.76 | 10.1 / 8.3 / 7.7 % | 82 / 84 / 82 % |
| trial, txstbc=0 txldpc=0, morning x2 | 10.20 / 10.40 | 11.50 / 10.25 | 10.5 / 9.9 % | 80 / 82 % |
| trial evening, all variants | 1.5-8.3 | 3.2-6.1 | 7-9.5 % | ~78 % |

Reading:
- The run-to-run spread is 2-3x, and the evening sessions were congested for every variant. Only
  same-session interleaved runs compare.
- trial vs good: uplink about +40 % (median of medians 11.8 vs 7.9), downlink about +30 %, AP
  retries 8-10 % vs 11-14 %, subframe success 82 % vs 73 %. Rate control no longer falls to 12M legacy:
  the old kernel spent most single frames at 12M (`tx_rate ok 12M=134550`); the trial sits on MCS1-3/40.
- STBC+LDPC on vs off on the same kernel: within noise (+5-10 % up). Most of the gain is common to
  both, so it is likely the regulatory fix (world '00' -> 'BR'). That is not isolated: no knob was
  made for it.
- 2.4 GHz retries (27-36 %) were not re-measured; the earlier lanes showed crowded channels. The band
  rule keeps 5 GHz here. The "2-stream AP at 1.3 Mbit/s" was the ch 1 2.4 GHz BSS, which autoconnect
  no longer picks.
- PROMOTED good cf0514ad32a7 (cmdline iwlwifi.mgr=1). The previous good slot was replaced; factory stays.

mDNS on wifi0 (laptop, the Mac sending to 224.0.0.251:5353 and 239.1.2.3:5000): good kernel 0/40 and
0/30 datagrams; trial 40/40 received.

## Tests

- Host: `drivers/iwlwifi-tests` 138 pass (new: `stbc_ldpc_retry_chain`,
  `stbc_tx_response_matches_table`, `mcast_filter_and_chub`, preference `relative_to_the_strongest_24`).
- New QEMU case `udptest peer` `peer_weakhost` (+ host peer command `WEAK <ip> <port>` in
  udppeertest.py): a datagram for eth0's address sent on the eth1 link must reach a 0.0.0.0 socket
  with IP_PKTINFO = eth0's address.
- aarch64 regress + net-e2e: see the lane report.

## Open

- STBC/LDPC benefit not shown beyond noise; defaults stay on (Linux parity). The MCC effect is not
  isolated by a knob.
- `iwlwifi.tx40=0` (20 MHz TX) untested on hardware.
- Found on the way: SO_RCVTIMEO ignored (already L9.1) — a test program blocked forever on recvfrom.
