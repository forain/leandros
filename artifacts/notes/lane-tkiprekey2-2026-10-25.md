# lane tkiprekey2 (finished by tkiprekey3) - GTK rekey soak on the real laptop, 2026-10-25

## LEANDRO2026 (TKIP AP) soak
- Joined a TKIP-group-cipher AP; exactly one TKIP GTK install, no rekey seen in ~85 min total.
  That AP never rekeys, so it proves nothing about rekey handling.

## Unexplained power-off
- ~05:28 UTC the laptop powered off by itself. After reboot: unclean f2fs; no panic and no ACPI
  line in the klog. Cause unknown (not reproduced during the later 25 min Anvara run).

## Anvara (router rekeys the group key every 60 s)
- Run 06:01:31-~06:26 UTC (190 samples at ~8 s), connected to Anvara bssid 84:78:48:d8:7e:79 ch1,
  -68..-71 dBm, group cipher CCMP.
- Rekey count: ~22 over ~23 min (klog "group key N (CCMP) installed" alternates key index 1/2 about
  once a minute; windows of the 400-line klog tail overlap so the count is derived from cadence,
  each tail window shows 2-4 installs).
- No MIC, deauth, disassoc, reconnect or "AP ended" line in any klog tail; the link stayed up with
  the same bssid and IP 172.16.149.179 at the end.
- Heartbeat (Mac broadcast ping 1/s, laptop replies only, 1100 s): 1027/1098 replies = 6.5 % loss,
  54 gaps, longest 5 consecutive missed (~5 s, at seq 477), latency p50 102 ms, p99 863 ms.
  Gaps are scattered, NOT periodic at 60 s, so not tied to rekeys; consistent with the weak
  signal and Wi-Fi power-save. Not proven either way (no per-line timestamps in klog).
  The "loss <= 0.3 % / no gap > 1 s" criterion is therefore NOT met on this link; a stronger
  signal (or wired reference ping) is needed to separate RF loss from rekey effects.
- Rx stats: "undecrypted" counter stayed at ~1 until 06:24, then rose to ~757 by the end of the
  run; unexplained, worth a look next time (could be frames under the old key index after a rekey).

## Cleanup
- netctl forget LEANDRO2026 done; /root/l26.psk absent; run3.sh, tkr, tkr3 removed;
  laptop back on Anvara, reachable. User's Anvara and Anvara 5G profiles untouched.
