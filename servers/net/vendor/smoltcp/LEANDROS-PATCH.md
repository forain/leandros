# smoltcp 0.11.0, patched for LeandrOS

Vendored from crates.io (`smoltcp-0.11.0`), used through `[patch.crates-io]`
in the workspace `Cargo.toml`. `Cargo.toml` is the published one with the
examples, benches, dev-dependencies and profile removed (a workspace member
needs only the library). Every change in `src/` is marked `LEANDROS`; all of
them are in `src/socket/tcp.rs` (lane wifirx, 2026-10-17).

Why: upstream 0.11 TCP recovers from loss by go-back-N. Over the laptop's
Wi-Fi (1 % air loss, RTT swinging 15-170 ms) one lost segment resent the whole
window in flight. In QEMU with `netshape=eth0:8000:8000:64:256:2:10` (8 Mbit/s,
1 % loss) a 4 MB transfer from the guest put 1.46x the payload on the wire
and ran at 0.51 MB/s instead of 0.90. Patched: 1.07x, 0.90 MB/s (the
link rate).

1. **RTO restart on ACK of new data** (RFC 6298 5.3). Upstream kept the
   retransmission timer armed from the first send while data stayed
   outstanding, so a long transfer over a queued link hit the RTO while ACKs
   were still arriving.
2. **RTO floor 200 ms** (Linux `TCP_RTO_MIN`; upstream 10 ms).
3. **Fast retransmit sends one segment** (RFC 5681 3.2) and then resumes at
   the highest sequence already sent; upstream rewound to SND.UNA. A partial
   ACK during the recovery retransmits the next hole at once (NewReno,
   RFC 6582, `recover_seq`). An RTO still goes back to SND.UNA.
   The timer, the recovery point and the counters change only once the
   segment is really out: under the iwlwifi/netshape TX backpressure the
   device often refuses a frame, and the attempt is simply repeated on the
   next poll. The recovery point is set when recovery starts and never moved
   by a retransmit inside it (else every later ACK looks partial). A fast
   retransmit clears the RTT sample (Karn) but does not inflate the RTT
   estimate the way an RTO does.
4. **A window update does not end a run of duplicate ACKs** (it is not
   counted either). Upstream reset the count, so a peer whose receive window
   moved during a loss never reached three.
5. `reset()` also clears the duplicate-ACK state.
6. `RECOVERY_STATS` (fast retransmits, RTOs, partial ACKs, duplicate ACKs),
   shown by the net server as the `tcp_recovery:` line of `/proc/net/nic`.

Tests: `test_fast_retransmit_after_triple_duplicate_ack` was rewritten for
the new behaviour (one segment, partial ACK retransmits the next hole),
`test_ack_of_new_data_restarts_rto` and `test_recovery_ends_at_recover_point`
are new, `test_rtt_estimator` expects the 200 ms floor and
`test_fast_retransmit_duplicate_detection_with_window_update` that a window
update keeps the count. To run them, copy the crate with the crates.io `Cargo.toml`
(dev-dependencies) and `cargo test --lib socket::tcp` on the host (582/582
pass).

Not done: congestion control (0.12 has Reno/CUBIC), SACK-based recovery.
