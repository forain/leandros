# lane/pibtrole 2026-10-28 - PARTIAL

Change: ACCEPT_CONN role 0x00 (central) is now the default in bt-host (`accept_central: true`) and btd.
Old behaviour: `--accept-peripheral`, `BTD_ACCEPT_PERIPHERAL=1`, `/etc/btd.accept-peripheral`.
`--accept-central`, `BTD_ACCEPT_CENTRAL`, `/etc/btd.accept-central` are accepted as no-ops (env/marker ignored, flag ignored).
Tests: cargo test -p bt-host 19/19 (existing incoming-channel test now expects role 00; new peripheral-knob test). rpi5 release build 0 errors.

Pi (trial f51a46029ffb, pi-sync bundle 2 MiB): removed /etc/btd.accept-central, killed btd; log shows `trace=true accept_central=true` from the default.
Reconnect: 0/3 verified. After the first restart the pad did not page back within ~2 min; btd's own outbound
connect failed with status 0x04 (page timeout), i.e. the pad was asleep. Per instructions I stopped (no button presses requested).
The pre-restart session did show event2 + psm 0x0011/0x0013 open under role 0x00.
Final state: /etc/btd.trace removed, btd restarted (no tracing).
