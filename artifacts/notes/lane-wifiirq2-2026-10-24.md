# lane/wifiirq2 — 2026-10-24 (L9.9 follow-up) — PARTIAL

Commits: 27d20ba2 (interrupt wakes only the net daemon while associated, deferred
re-arm, quiet housekeeping park), 274e8a5a (quiet park keyed on TX, last handed-off
notification in /proc/net/nic). Merged because the laptop's promoted good kernel
ff42eeb457aa (build wifiirq2-q2) was built from this branch: main now matches the laptop.

## What changed
- ISR kicks only the net daemon while LINK_UP; a re-arm that finds DEV held inside
  `with_dev` sets RX_DEFER and the holder kicks the daemon on release (SeqCst Dekker
  on WITH_DEV_HELD), instead of a spinning daemon pass.
- Quiet housekeeping park (QUIET_PARK_MS = 1000, `iwlwifi.quiet=0` disables) and
  `Mgr::set_waker` so manager commands end the park.
- /proc/net/nic `irq:` line: quiet parks, deferred re-arms, beacons, last_notif.

## Laptop (EliteBook 840 G5, greeter idle on Wi-Fi), vs good f40d175b8cae
| | f40d175b8cae | ff42eeb457aa |
|---|---|---|
| net daemon passes/s | 113 | 50–60 (69 in the first trial) |
| empty IRQs/s | 30 | 2–3 |
| idle CPU | 6.8 % | 6.1–6.3 % |
| scp pull/push 50 MiB | 9.4 / 22.0 Mbit/s | 12.6 / 25.7 (10.5 / 21.5 rerun) |
| SSH heartbeat 300 beats | — | max gap 1.15 s, 0 gaps > 2 s |

IRQs ~25/s, beacons of the tracked BSS ~10/s, rearm passes ~2/s. Trial promoted.

## Tests
QEMU aarch64 regress incl. wifisimtest PASS on the main-merged tip (1c4d7be1); x86_64 built
clean at 27d20ba2.

## Left open (taken by lane/wifiirq3)
- quiet parks stayed 0: a needs_tick term is always true at idle.
- the driver thread still drains the RX ring while associated.
- ~10 beacons/s reach the host (no firmware beacon filtering).
