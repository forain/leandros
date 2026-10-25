# lane/nmtkiptest — 2026-10-24 — PARTIAL (TKIP expectations fixed; separate legacy-autoconnect regression found)

- Cause of the nmrealtest first failure: SimTkip is a WPA/WPA2 mixed AP (group TKIP, pairwise CCMP+TKIP), which lane tkip made joinable. Stale expectation, not a kernel bug.
- wifi_sim: new RsnVar::TkipOnly (`k`, pairwise TKIP only) + default AP SimTkipOnly (not joinable; selftest check bss-tkip-only-not-joinable). nmtest expects SimTkip listed, SimTkipOnly hidden.
- netroamtest: TkipNet now `k` (pure TKIP); rejection text "no CCMP pairwise".
- Result (aarch64, net-e2e): nmrealtest PASS (0 fails). netroamtest 13 -> 10 fails; rsn_log, join_col, sec_msgs now PASS.
- STILL RED, not TKIP: netroam_legacy_auto and everything downstream (wifi0 never autoconnects to NetA). Cause: b6b8edda lifted `wifi.psk=` off the stored kernel cmdline; the only consumer of cmdline_secrets::take is iwlwifi.rs, while servers/net/src/netctl/mod.rs (CMDLINE, legacy wifi.ssid/wifi.psk profile for wifi_sim) still reads the scrubbed cmdline, so no legacy profile has a PSK. Fix: netctl legacy profile must get the psk from cmdline_secrets (or the test must use a root-only wifi.conf).
- linkmontest tx_bulk also failed in the same run (not investigated).
