# lane/regdom — 2026-10-24 (L6.1 follow-up) — FIXED (reporting bug, not a regression)

Commit a18fde0c. Laptop trial d549a3e40960 (build `regdom` = main 97390217 + fix) PROMOTED as good, cmdline `iwlwifi.mgr=1`.

## Finding
`/proc/net/nic` showed `regulatory: 00` on every kernel, but the country update DID apply:
- klog of the pre-fix good kernel (current and all stored boots, 4/4): `regulatory: firmware reports MCC 'BR'
  (source 0x5); applying` right after the first scan. The AP still sends a country (source 0x5 = WIFI), so no
  environmental cause; the firmware LAR learned it, no cmdline knob involved.
- Cause: the MCC_UPDATE sent for MCC_CHUB_UPDATE (eef870ef, wifirate2) went into `async_cmds`, whose replies are
  dropped. `Info.mcc` was only ever set from the init `GET_CURRENT` query (world '00'). So the field has never shown
  'BR' since wifirate2 either; wifirate2 saw 'BR' only in the klog line.

## Fix
`iwl_cmd::Regdom` tracks pending MCC updates by sequence number; the dispatcher parses the reply, logs status and
channel count, and reports an accepted country (status 0 NEW_CHAN_PROFILE / 1 SAME). Host guard
`regdom_follows_chub_update_reply` (cmd_tests.rs): FAILS with the reply ignored (simulated), passes fixed; 142/142.

## Laptop (trial d549a3e40960)
- klog: `MCC '00' (status 1, source 0x7)` -> `firmware reports MCC 'BR' ... applying` -> `MCC update -> 'BR'
  (status 0, source 0x5); 38 channels`. `/proc/net/nic`: `regulatory: BR`.
- TX scp pull, 50 MiB fresh random, 3 rounds: 11.0 / 14.1 / 17.4 Mbit/s (wifirate t2: 10.4 / 14.8 / 15.0), rc MCS4/40.

## Open
- Channel list from the 'BR' reply is not applied to the scan list (still the '00' list; same 38 channels here).
