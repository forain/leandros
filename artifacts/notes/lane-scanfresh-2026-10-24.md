# lane/scanfresh (L12 #44) - FIXED (sim-verified; laptop not run)

Cause: `netctl scan` sent flags=0, so the kernel coalesced it for 10 s (SCAN_COALESCE_MS) and
returned the cache (scan_gen unchanged); additionally iwlwifi had scan-while-associated off by
default (iwlwifi.scanconn=1 opt-in), so even a non-coalesced scan while associated was answered from cache.
Fix: netctl sets scan_req_flags::USER (=FORCE, 2 s rate limit); iwlwifi.scanconn now default on
(iwlwifi.scanconn=0 opts out). Connect path unchanged.
Guard: wifi_sim selftest `scan-user-twice-connected` (gen 4 -> 5 -> 6). Failed with USER=0 (FAIL 1 of 41), passes 41/41.
aarch64 regress PASS. Open: laptop verify (background scan every 120 s while associated is now active on iwlwifi).
