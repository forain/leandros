# lane eapoldrop - group-key message 1 lost during driver waits (openbugs L12 #54), 2026-10-25

Verdict: FIXED (suspect (a)); (b) and (c) not observed.

## Root cause
`log_async` discarded every received frame (pseudo-notifications 0xff/0 management, 0xff/1 EAPOL) that
`wait_notif` popped while the driver thread waited for something else: scan() (up to 20 s), the TIME_EVENT
wait, wait_mgmt, the PHY-DB wait. A group-key message 1 (and the AP's retransmits) arriving during a
connected scan vanished, and the stale group key stayed for the next rekey interval (60 s of `mc kN` MIC failures).

## Fix (commit bd421a4c)
- `wifi_mgr::gk::Held<T>`: frames no wait claimed are held (EAPOL always, management only while associated,
  cap 64, overflow counted); iwlwifi's connected loop (`monitor_once`) and the 4-way loop drain it before
  waiting; the quiet park is skipped while it is non-empty; cleared on clear_link / firmware restart.
- Counters in /proc/net/wifi: `eapol: rx N held N dropped N qfull N held_overflow N last_tid T; ba_old t0..t7`
  (EAPOL received at rx_eth_frame, held, dropped, lost to a full notification queue; BA reorder Old drops per TID).
- wifisim `gtk-rekey-during-scan`: group message 1 delivered while a connected scan waits must install the key.
  Red before the fix (`installs 3->3 ... dropped 1`), green after (`3->4 key 1->2, held 1`). aarch64 regress PASS.

## Laptop trial (kernel eapoldrop-4c4dea6f = main 345bca30+ incl. this lane; iwlwifi.mgr=1), Anvara 60 s rekeys
| run | install intervals | undecrypted (mc) | eapol rx / held / dropped / qfull | ping loss, gaps>1 s |
|---|---|---|---|---|
| t1 15 min | 15 x 59-61 s | flat (0 new) | +16 / 0 / 0 / 0 | 2.2 %, 2 (2.0 s, 1.2 s; not at rekeys) |
| t2 15 min | 15 x 57-63 s | flat (0 new) | +21 / 9 / 0 / 0 | 0.9 %, 0 |
- t2: 9 EAPOL frames were popped by waits at a ~120 s cadence (epochs ...186, 305/307, 427/428, 546/547), each
  coinciding with a rekey; the old code dropped exactly these. All were installed (installs 20->41 = eapol rx +21,
  incl. AP retransmits). The intervals' +-3 s jitter is the hold delay. What waits every 120 s in t2 (not t1) was not
  identified (candidate: a connected scan from userland/applet) - harmless now.
- (b) EAPOL travels on TID 6, which has an RX BA session: ba_old t6 +2 (t1) / +4 (t2), 0-10 s away from installs,
  with every EAPOL accounted for (rx == installs + 4-way) - no EAPOL classified Old. t0 Old drops ~160/run (normal).
- (c) no unexplained miss in 32 rekeys.
- Promoted: good = bbb44e1565b0 (eapoldrop-4c4dea6f).

## Open
- Source of the 120 s waits in t2. Anvara rekey can go back to 3600 s.
