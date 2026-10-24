# lane/wifiirq3 — 2026-10-24 (L9.9 follow-up) — PARTIAL

Commit: "iwlwifi: quiet park with a TX Block Ack session up, daemon owns the RX ring
while associated, firmware beacon filtering". Laptop trial c703e3a80dfb (build
wifiirq3-t1) PROMOTED as good.

## Root cause of "quiet parks = 0" — FIXED
`needs_tick` counted any TX aggregation state other than Off. Once the ADDBA succeeds
the session stays On for the whole association, so the 1 s housekeeping park could never
trigger. Only an outstanding ADDBA (Requested, it times out) needs the tick now; frames of
an On session are covered by the BSS-queue term. New `noquiet` counters per term in
/proc/net/nic show the remaining blockers at idle: TX movement and BSS queue/rate table
(20 s idle: tx 130, txq 37, rc 15, agg 0, addba 0); quiet parks 0 -> ~2.6/s.

## Driver thread no longer drains RX while associated — DONE
With MSI live and the link up the thread only pops handed-off notifications; it drains
the ring itself at most once per second (`thread_svc`, fallback against a missed hand-off).
`wait_event` stopped returning on a pending ring (the daemon's job), which would spin.

## Firmware beacon filtering — DONE
REPLY_BEACON_FILTERING_CMD 0xd2, 44-byte pre-v4 iwl_beacon_filter_cmd with
IWL_BF_CMD_CONFIG_DEFAULTS, beacon abort off; sent before LINK_UP, disabled in `leave`.
`iwlwifi.bf=0` turns it off. /proc shows `bf on`. Beacons reaching the host: ~10/s -> 0.2/s.

## Laptop (greeter idle, Wi-Fi) vs good ff42eeb457aa
| | ff42eeb457aa | c703e3a80dfb |
|---|---|---|
| IRQs/s | ~25 | 16 |
| beacons/s | ~10 | 0.2 |
| thread kicks/s | ~8 | ~9 |
| net daemon passes/s | 50–60 | 49 (39 after scp) |
| idle CPU | 6.1–6.3 % | 5.5–5.6 % |
| scp pull/push | 12.6/25.7, 10.5/21.5 | 14.9/24.6, 13.7/23.5 Mbit/s |
| heartbeat 300 | max 1.15 s, 0 > 2 s | max 1.17 s, 0 > 2 s |

## Tests
QEMU aarch64 regress incl. wifisimtest PASS; x86_64 builds (the trial kernel).

## Open — the <= 20 passes/s target is NOT reached
netd is kicked ~48/s but iwlwifi IRQs are only 16/s: ~30 kicks/s come from somewhere else
(netd_why `kicked`, tagged wakes ~7/s). Next: count netdev::kick callers by site (iwlwifi
ISR, rx_eth hand-off, RX_DEFER, socket/TCP timers) to find the other ~30/s. The ~16
IRQs/s left with beacons filtered are worth decoding too (CSR_INT bits, RX packet cmd ids).
