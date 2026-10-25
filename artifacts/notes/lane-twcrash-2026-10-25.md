# lane/twcrash 2026-10-25 — iwlwifi teardown TIME_EVENT firmware crash (openbugs L12 #66)

**FIXED** (laptop-verified, promoted).

## Root cause
Session protection is added with TIME_EVENT_CMD ADD, duration 2000 TU (~2 s). Its END
notification was ignored (log_async drops it). `soft_teardown` later sent TIME_EVENT_CMD
REMOVE with that unique id, typically minutes after the event had ended. Firmware
8265 -36 answers a REMOVE of an ended event with SW_ERR: UMAC NMI_INTERRUPT_DATA_ACTION_PT,
LMAC 0x301d, last hcmd 0x..29 (TIME_EVENT_CMD). That was 4 of 5 teardowns in boots 179/180,
each followed by a firmware restart of about 4 s. A switch made within 2 s of connecting
(event still running) was the one that did not crash.

## Fix (commit 7c2073dd)
- `asc::TeTrack` (iwl_assoc.rs) follows Linux's `iwl_mvm_time_event_data` running/uid. It is
  fed by every TIME_EVENT_NOTIFICATION in Dev dispatch. END, or START with status != 1, clears it.
  An END that arrives before the ADD response is remembered. It is reset on firmware restart.
  REMOVE is sent only for an event that is still running.
- `asc::teardown_plan` is the explicit iwlmvm order. Flush, then wait up to 200 ms until the
  queues drain. Disable the queues (this ends the BA session). Set the MAC unassociated and
  clear the quota. Remove the AP station (Linux removes the AP STA only after the MAC is
  unassociated). Remove the time event if it is running. Unbind last.
- The old order put the TE removal first and the queue disable after the MAC unassoc, with
  no drain wait.

## Guard
drivers/iwlwifi-tests/src/teardown_tests.rs has 5 tests. They failed 4/5 on the old code
(red commit a5035773) and pass 162/162 with the fix.

## Laptop trial (twcrash-f91f1186 = main 2f3cd221 + lane, slot 0d0c5e8be1dd)
- 10 soft switches (5 AP2 74:83:c2:ba:21:df -> ctl 84:78:48:d8:7e:79, 5 back), dwell 15 s each,
  then a reconnect with no BSSID set.
- Results: 0 firmware restarts, 0 SW_ERR, 12x "session protection already ended; not removed",
  and 12x "left the BSS without a firmware restart" (9-13 ms each).
- Mac reachable (on-laptop ping) 0.2-6.2 s after each connect, all within 10 s. Max 6.2 s, on a ctl->AP2 switch.
  Caveat: `netctl connect` returns once the request is accepted, so the fastest values may
  include a ping that went out over the old link.
- PROMOTED via remote.py.

## Open
- The aarch64 regress harness reports "boot failed" for this lane, for bssidbar and for
  integ15-x86 on 2026-10-25. The serial log reaches `login:`, so the failure is in the
  harness or login and is not caused by this change. iwlwifi is not loaded in QEMU.
- This lane did not touch the BA/BAR on partial BA work (lane bssidbar).
