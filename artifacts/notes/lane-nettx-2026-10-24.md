# lane/nettx — 2026-10-24 (L9.9 tail: net daemon passes during bulk TCP transfers)

Status: FIXED in QEMU (shaped Wi-Fi-like upload 31 k -> 3.3 k passes/s); laptop not re-measured (QEMU-only lane).

## Premise corrected
`netd_why timer_sleeps` counts sleeps whose *deadline* came from smoltcp (below the cadence), not
sleeps that *ended* on that deadline, and it also counted `poll_delay == 0` re-polls (sleep_ns 0 <
cadence). New counters: `immediate` (poll_delay 0 re-polls), `tx_held`, `timer_expired` (timer-limited
sleeps that ran to the deadline, no kick), `timer_hist` (<100us/<1ms/<10ms/>=10ms).
- Unshaped virtio (2.8 Gbit/s RX, 0.4 Gbit/s TX): every pass is kick-driven (RX IRQ). Download 16–19
  passes/MB (64 KB per pass), upload 349–366 passes/MB = one pass per received ACK; timer_expired ~1.
  Delayed-ACK / RTO deadlines exist (RTO always pending while data is in flight, hence ">=10 ms"
  "timer sleeps") but almost never fire. No sub-ms smoltcp timer storm. These passes are needed:
  upload is ACK-clocked.
- Wi-Fi-like link (`netshape=eth0:20000:20000:64:256:2`, the driver TX queue backpressure iwlwifi has):
  upload 30967 passes/s, `immediate` 364984 in 11 s (15 k/MB). smoltcp keeps reporting poll_at = now
  (segments to send) while `tx_ready()` is false, so the daemon spun 8 back-to-back empty passes, then a
  200 us anti-spin sleep (timer_expired 27.6 k), repeat. This is the "short timer sleeps" signature.

## Fix (servers/net/src/lib.rs)
When a NIC stack's `poll_delay` is 0 and that NIC is not `tx_ready`, the stack's next poll becomes a
10 ms fallback (`TX_HELD_FALLBACK_NS`) instead of "now": a pass then sends nothing; both drivers that can
refuse frames kick on drain (iwlwifi TXQ_LOW/TX_HELD, netshape half-empty), and that kick starts the
next pass. RX, kicks, other timers and the netshape due-release wake are unchanged.

## Evidence (aarch64 HVF, nettxtest.py)
| case | build | passes/s | passes/MB | immediate | Mbit/s | netd CPU |
|---|---|---|---|---|---|---|
| shaped upload 20 Mbit | before | 30967 | 15353 | 364984 | 18.2 | 9.8 % |
| shaped upload 20 Mbit | after | 3293 | 1674 | 20 | 17.8 | 4.5 % |
| upload unshaped | before | 6396 | 349 | 0 | 385.7 | 24.6 % |
| upload unshaped | after | 6216 | 366 | 0 | 391.7 | 22.1 % |
| download unshaped | before | 3577–5567 | 16–19 | 0 | 1620–2861 | 73–94 % |
What is left on the shaped upload is ~1.6 k kicks/s (ACK RX, drain kicks) plus ~1.7 k/s netshape
TX releases (the emulator meters frames out on daemon passes; real iwlwifi drains in hardware).
New guard: nettxtest `--tx --shape ...` (in net-e2e, suite `nettx`): immediate re-polls < 100/MB and
passes < 3000/MB; the unfixed build measured 15 k/MB immediate (FAIL), fixed 1/MB (PASS).
nettest 17.4 s, udptest 21.2 s (netidle lane: 17.2/18.3, 21.1/21.1) — no latency loss.
Shaped-upload throughput: before 18.2 (one run), after 17.8 / 17.3 Mbit/s (link cap 20): within
run-to-run noise as far as 3 samples show; repeat if a laptop pull A/B disagrees.
Tests: regress PASS except wifisimtest (own boot hung pre-kernel; PASSED on rerun) and evgkeytest
(held_btn_bit_cleared/held_key_seen_down, then RC=1 again on rerun; input path, untouched here: check on
main). net-e2e all PASS incl. netidletest 1.0/s and nettxtest (17.3 Mbit/s, 3424 passes/s, immediate 0/MB).
x86_64 build OK.

## Open
- Laptop: re-measure scp pull with `netd_why` immediate/tx_held/timer_expired (expected: immediate
  near 0 now; the 1.5–2 k/s there matched the immediate-spin signature, unproven on hardware).
- Shaped *download* (`nettxtest.py --shape eth0:20000:20000:64:256:2`) gets only 0.6 Mbit/s (0.8 MB in
  10 s, tx_held 0, so this lane's code path never runs there): pre-existing, worth a lane (netshape RX
  queue vs host HTTP over SLIRP, or a real RX-window problem). Repro: the command above.
- Boots of a `cp -c` image with a test cmdline hung in Limine "Loading module" 3 of 6 times
  (the known pre-kernel own-boot hang).
