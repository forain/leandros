# lane/rxwin — 2026-10-24 (L9.9 remainder: fixed 2 MiB receive window overshoots shallow queues)

Status: FIXED in QEMU aarch64 (shaped 20 Mbit/s download: RX-queue drops 694-726 -> 0, throughput unchanged).
Laptop Wi-Fi not measured.

## Cause
smoltcp advertised the whole free receive buffer (2 MiB, wscale 6) from the first ACK. The sender's
slow start was then limited only by its cwnd, and it overshot the 256-frame netshape RX queue: queue
pinned at 256 (standing depth 214-231), ~4.4 % of frames tail-dropped, ~1700 out-of-order segments and
~197 too-many-holes per 10 s run. SACK (lane shapedl) made recovery cheap, but the drops remained.

## Fix (commits 7acc70bd, 6c2b4822)
- Vendored smoltcp `socket/tcp.rs`: opt-in `set_rx_autotune(Some(initial))`, Linux DRS-style
  (`tcp_rcv_space_adjust` + `tcp_rcv_rtt_measure`). Advertised window = min(free, clamp - buffered),
  i.e. right edge <= remote_seq_no + clamp; clamp starts at 64 KiB and only grows, to
  min(cap, 2 * read-per-receive-RTT + 16 * 1460), so the right edge never moves left (RFC 7323).
  Receive RTT = minimum time to receive one advertised window. wscale still negotiated from the full buffer.
- Net server: every TCP socket starts autotuned at `TCP_RX_WIN_INIT` = 64 KiB; `/proc/net/nic` line
  `tcp_rxwin: autotuned N grows G max_window M`.
- Guard: nettxtest shaped download must drop <= 1 % (`DROP_SHARE`) of its frames in the RX queue
  (in net-e2e and suite `nettx`); the shaped copy honours `LEANDROS_DISK_IMAGE`. Host tests: 4 new
  `test_rx_autotune_*` (right edge never moves left; fast reader reaches 2 MiB; 20 Mbit/s link stays
  <= 256 KiB; slow reader does not grow). Crates.io Cargo.toml copy: 602/602 pass.

## Evidence (aarch64 HVF, nettxtest.py)
| case | main 2fba3874 | lane |
|---|---|---|
| shaped 20 Mbit/s: Mbit/s | 18.4 / 18.0 / 17.5 | 18.2 / 18.6 / 18.7 / 18.7 |
| shaped: RX queue dropped (of ~16 k) | 716 / 694 / 726 (guard FAIL) | 0 / 0 / 0 / 0 (guard PASS) |
| shaped: RX queue max_depth | 256 | 102-105 |
| shaped: tcp_rx out_of_order / too_many_holes | 1688 / 197, 1722 / 197 | 102 / 0, 0 / 0 |
| shaped: max advertised window | 2 MiB | 158-330 KiB |
| unshaped bulk, Mbit/s (1 GiB) | 2654 2657 2449 2664 2038 | 2654 917* 2440 2341 2648 (window reaches 2 MiB) |
\* single outlier while another VM ran; main had a 2038 outlier in the same series.

Retransmits are not visible guest-side (host is macOS); out-of-order and too-many-holes segments are
the proxy and fall to ~0.

## Tests (aarch64)
regress 45/45 PASS; net-e2e all PASS (incl. both nettxtest shaped cases).

## Open
- Laptop: measure a real Wi-Fi download with `tcp_rxwin:` and AP retries.
