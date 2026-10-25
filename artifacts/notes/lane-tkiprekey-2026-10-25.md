# lane/tkiprekey — 2026-10-25 (L6.6 HW half) — PARTIAL: 75 min on TKIP-group LEANDRO2026, no GTK rekey occurred

Laptop (good kernel 97ffd47688) stayed joined to LEANDRO2026 (mixed mode, pairwise CCMP, group TKIP) for
75 min (00:35-01:50 -03) using the tkipjoin method (self-reverting nohup script, profile saved --no-auto,
PSK file deleted right after the join; no PSK in any log, 0 hits when grepped).

## Evidence
- klog: exactly one `group key 1 (TKIP) installed` and one `4-way handshake complete` for the join; no second
  group key install, no key-index change, no `rx: TKIP Michael MIC failure` lines in 75 per-minute klog snapshots.
- Mac broadcast ping 172.16.255.255 (about 1/s) ran the whole window: the laptop answered 4512 of 4651 seqs (97%),
  last reply at seq 4650, i.e. group-addressed TKIP frames kept decrypting for the full 75 min.
- No loss-of-connectivity revert; script returned to Anvara itself (`back to Anvara done`).
- Cleanup: laptop on Anvara, LEANDRO2026 profile forgotten (2 user profiles, untouched), /root/tkr and PSK copy removed.

## Conclusion
No rekey was observed: the router's GTK rekey interval is longer than 75 min (or rekey is disabled).
The group-handshake code path stays covered only by crypto_tests (full_handshake_and_rekey,
tkip_group_handshake_installs_32_byte_gtk). To observe one on HW would need a >2 h window or an AP with a
configurable rekey interval (e.g. hostapd wpa_group_rekey=60 on a Linux box, see lane tkipap).
Note: /proc/net/wifi has no RX filter counters in the connected view, and the laptop shell lacks grep.
