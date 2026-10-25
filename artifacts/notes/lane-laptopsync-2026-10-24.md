# lane/laptopsync2 2026-10-24 - laptop sync to main + netwatch watchdog test

DELIVERED. Built x86_64 release at main 97ffd476 (code identical to 714ee4fa validated by integ13), trialed on the laptop and promoted.

- Trial cmdline carried `f2fs.rf=0 netwatch=180` and `iwlwifi.mgr=1`; no secrets.
- Health: SSH ok, on Anvara 5G (wifi0 up), 120 s heartbeat 120 beats / 0 gaps > 2 s (max 1.12 s), ping 0% loss; desktop login ok (~6 s); YouTube PLAYING after 16-20 s; ytd-app defined via Marionette (dp.sh does not pass `--marionette`, so the first mncheck said "no marionette"; rechecked with a --marionette launch on the promoted kernel: ytd true, SW controller true).
- Promote: good = kernel id cf0a2883940e (uname `#1 97ffd47688`), good cmdline ends `iwlwifi.mgr=1`, netwatch knobs stripped, default=good, armed=0.
- Watchdog: one-shot trial of the same kernel with `netwatch.host=192.0.2.1` (never answers). The laptop self-rebooted back into the good slot with SSH still up on the trial; boot to reboot about 300 s (new boot's uptime was 604 s when sampled at t0+898 s, t0 = first SSH success on the trial; my SSH-loss detector missed the short outage so the figure is derived, +/- 20 s). Final: booted_slot=good, default=good, armed=0.
- Open: dp.sh launch has no Marionette option, so ytd-app needs a separate --marionette launch.
