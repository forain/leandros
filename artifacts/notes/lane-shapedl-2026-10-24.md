# lane/shapedl — 2026-10-24 (L9.9 tail: shaped download 0.6 Mbit/s)

Status: FIXED in QEMU aarch64 (shaped 20 Mbit/s download 0.6 -> 18.4 Mbit/s). Not netshape: a smoltcp
bug that hits every outbound TCP connection, so it applies to real Wi-Fi downloads on the laptop too
(unmeasured there).

## Root cause
smoltcp records the peer's SACK-permitted only on the passive open (`Listen`, SYN). On an active open
(`SynSent`, SYN-ACK) `remote_has_sack` stayed false, so our client offered `sackOK` in its SYN and then
never sent a SACK block. pcap (QEMU `filter-dump` on net0, host = macOS over vmnet): the 2 MB receive
window lets the host's slow start overshoot into the 256-frame netshape RX queue (rx dropped 260, tail
drops interleaved -> dozens of holes); with no SACK the host repaired one hole per RTO (~0.4 s): one
host segment per 250-500 ms for the rest of the run, ACKs carrying no options. tx_held 0 throughout:
the TX/backpressure path (lane nettx) is not involved; netshape behaves like a tail-drop AP queue,
which is what it is meant to model.

Secondary: segments beyond the assembler's 32-hole limit were dropped without an ACK (`tcp_rx:
too_many_holes 199` in one 10 s run), and `ack_reply` sent only one SACK block.

## Fix (servers/net/vendor/smoltcp/src/socket/tcp.rs, LEANDROS-PATCH.md)
- `(SynSent, Syn)`: `self.remote_has_sack = repr.sack_permitted`.
- too-many-holes segment: answer with an immediate duplicate ACK (RFC 5681 4.2) instead of `None`.
- `ack_reply`: SACK blocks 2 and 3 carry the next-lowest assembler ranges (RFC 2018).
- New counters `RX_STATS` -> `/proc/net/nic` line `tcp_rx: in_order out_of_order too_many_holes out_of_window`.

## Evidence (aarch64 HVF, `nettxtest.py aarch64 --shape eth0:20000:20000:64:256:2`)
| build | MB in 10 s | Mbit/s | rx dropped | tcp_rx in_order / ooo / too_many_holes |
|---|---|---|---|---|
| main cb0f5189 | 0.0 / 0.8 / 0.8 (1.5 in 6 s) | 0.0 / 0.6 / 0.6 (2.1) | 260-381 | 511 / 183 / 199 |
| lane | 22.0 | 18.4 | — | 14068 / 1726 / 198 |

Guard: nettxtest download with `--shape` now requires >= 50 % of the shape's RX rate (`RATE_SHARE`);
added to net-e2e and suite `nettx`. Main's runs (0.0-2.1 Mbit/s) are all below its 10 Mbit/s bound
(red); the lane passes at 18.4.

## Open
- Laptop: re-measure a real Wi-Fi download (expected better recovery after AP drops; 27 % AP retries).
- smoltcp still has no receive-window autotuning: the 2 MB window invites slow-start overshoot into
  any shallow queue; SACK now makes the overshoot cheap, autotuning would avoid it.

## Tests (aarch64 HVF)
regress: all PASS except wifisimtest (RC=1 in 8 s, ran beside net-e2e; PASSED on rerun). net-e2e: all
PASS incl. both nettxtest cases (upload unchanged, download 18.4 Mbit/s) except netroamtest (RC=1 beside
regress; PASSED on rerun, 146 s). x86_64 build OK.
