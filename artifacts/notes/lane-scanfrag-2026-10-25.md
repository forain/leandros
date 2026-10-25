# lane/scanfrag (L12 #49, #42) - 2026-10-25: FIXED (scan cost); residual link loss is baseline, not scans

## Change (03f20e68)
- iwlwifi: while associated, SCAN_REQ_UMAC v7 carries the iwlmvm MILD timing (max_out_time 120 ms,
  suspend_time 120 ms, both LMACs), so the firmware returns to the operating channel between dwells. SCAN_CFG_CMD is
  re-sent with the same timing when the assoc state changes (iwl_mvm_config_scan behaviour). Unassociated scans
  keep 0/0. Dwells (active 10 / passive 110 / fragmented 44), adaptive dwell and scan_priority EXT_6 unchanged
  (passive 110 fits inside one 120 ms excursion).
- wifi_mgr: no periodic 120 s scan while connected; `connected_bg_scan_due()` needs signal < -75 dBm
  (ROAM_SCAN_DBM, a roaming trigger like NM bgscan "simple"), still at most every 120 s. User scans (USER/FORCE) unchanged.
- scripts/deskplay/dp.sh: Firefox launched with `--marionette` (DP_MARIONETTE=0 disables) (L12 #42).

## Guards (red first)
- iwlwifi-tests `scan_timing_associated`: FAILED with timing forced to UNASSOC (156/157), passes 157/157.
- wifi_sim selftest `no-periodic-scan-connected`: FAIL on the old rule ("good true unknown true"), ok after; 45/45 on merged main.
- aarch64 regress: all green except nettest icmp_poll_timeout/icmp_recv_eintr (a stray ICMP reply arrives early;
  3/3 reruns, slirp side, unrelated to Wi-Fi; not investigated - new item).

## Laptop (Intel 8265, trial built at lane head 0940ba86 = main f8973069 + this lane, `uname -v` #1 0940ba86d9-scanfrag)
- 5-min heartbeat with 2 `netctl scan wifi0` (rel 60 s and 180 s): 300 beats, 0 gaps > 2 s, max gap 1.90 s at rel 168 s
  (outside both scan windows); 2/298 ping lost (0.7%), seq 153 and 228, also outside the scan windows. User scans now
  take ~7.1 s wall (fragmented, was 4.1-4.6 s in one off-channel stretch); both ran fresh ("last scan" reset).
- Baseline, same kernel, 5 min, NO scans: 3/298 lost (1.0%), max gap 2.10 s, 17 beats > 1.3 s (scan run: 8).
  => the remaining loss/jitter is the radio link itself (signal ~-61..-75 dBm area, crowded), not scans; the
  target (<= 0.3%, no gap > 1 s) is not reachable by scan changes. No background scan in 524 s of idle.
- Health: desktop login ~6 s, YouTube PLAYING after 12 s, Marionette ytd-app true, Big Buck Bunny h1.
- PROMOTED: good = 90cff489eba6 (kernel sha256 d74f7a67...), default=good, armed=0.

## Open
- Wi-Fi baseline loss ~0.7-1.0% / 2 s jitter with no scans (power save? retries? 27% AP retries seen earlier).
- nettest ICMP cases red on aarch64 (above).
- klog not fetched via `dmesg` (guest returned nothing matching "scan complete"); use `remote.py logs` next time.
