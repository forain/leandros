# lane rekeychk - Anvara 60 s GTK rekeys vs ping gaps and "undecrypted" (openbugs L12 #54), 2026-10-25

Verdict: PARTIAL / gaps REFUTED as rekey-caused; "undecrypted" ROOT-CAUSED to missed group-key handshakes (cause of the miss open).

## Instrumentation (commits on lane/rekeychk)
- iwlwifi: every protected frame the firmware did not decrypt is classified in /proc/net/wifi:
  `rx_undecrypted: uc k0..k3 mc k0..k3 (CCMP key index from the header) sec none/mic/err/other foreign last`.
  `foreign` = the frame would also fail the address check (counted apart).
- wifisim selftest `undecrypted-classified` guards the classification and the proc line (green on aarch64).

## Trials on the laptop (kernel rekeychk-ea22c4fc = main incl. scanfrag 160c7add + the counters; iwlwifi.mgr=1)
Unicast ping from the Mac at 5/s, 4500 pings each; 1 s on-laptop samples of uptime/epoch + gtk/rx_undecrypted (t2).
| run | rekeys | loss | gaps > 1 s | undecrypted |
|---|---|---|---|---|
| t1 (15 min) | 18 | 1.6 % | 0 | 218, all `mc k1`, sec=mic, one burst |
| t2 (15 min) | 15 | 1.0 % | 0 | 160, all `mc k2`, sec=mic, 1792912625-...685 |
- 33 rekeys, zero ping gaps > 1 s: the tkr3 gaps (54 up to ~5 s) were the 120 s connected scans (#49, now gone), not the rekeys.
  Max RTT ~1.3 s spikes remain (RF/power save, separate item).
- t2 install timeline (epoch, key, installs): ...564 key1 #30, then NOTHING until 684 key1 #31 (a 120 s interval; all
  others 59-61 s). The AP switched to key index 2 at ~624: from 625 to ~684 every group frame under k2 failed the MIC
  against our stale key-2 slot (3-8/s). klog shows `group key 1 installed` twice in a row, no `EAPOL:` error, no scan,
  rx filtered dup 1 / replay 0 / foreign 0. So group message 1 of that rekey never reached the supplicant, and the AP
  did not deauth (it does not enforce the group handshake). Rate: 1 miss in 15-18 rekeys (tkr3: ~3 in 25, with scans).
- Key slot / PN hypotheses REFUTED: slots alternate offset 2 (idx 1) / 1 (idx 2), the old key stays live, host PN is
  per key index and reset on install (replay 0); every installed epoch decrypted fine.

## Open (next lane)
1. Where msg 1 is lost. Candidates in code: (a) `log_async` silently discards (0xff, 1) EAPOL packets popped by
   `wait_notif` in scan() (up to 20 s), the TIME_EVENT wait and wait_mgmt - a real latent bug while connected
   (fragmented/roam scans, applet scans), likely what made tkr3 worse; (b) the RX BA reorder `Fate::Old` drop
   is silent - an EAPOL frame with an out-of-window SN would vanish; (c) plain RF loss with no AP retry.
   Add counters for (a)/(b) first (EAPOL rx at rx_eth_frame, EAPOL discarded by log_async, BA Old/Dup per TID).
2. Fix (a) regardless: keep EAPOL packets popped during those waits and re-queue them after the wait.
3. Not promoted (instrumentation only; good stays the scanfrag kernel).
Measurement scripts: $SP/rkc/{sampler.sh,measure2.sh,analyze.py} (laptop has no grep: sampler uses case).
