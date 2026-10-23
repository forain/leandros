# lane/netctlbssid (L10.13) - FIXED

Cause: `netctl connect SSID [--bssid MAC]` always sent an INLINE request (akm NONE when no
passphrase), never consulting saved profiles, so a WPA2 AP failed `unsupported-security`
(`--uuid` used the saved profile). Fix (userland/netctl/src/main.rs): with no secret and no
`--akm`, look up a saved profile by SSID via GET_PROFILES and connect by its UUID (BSSID still pins the AP).
Test: wifisimtest.py now saves SimHome, disconnects, `connect SimHome --bssid 0a:57:53:a0:00:01`.
Before: FAIL unsupported-security; after: PASS (connected, akm psk). netctltest 0 fails (aarch64).
