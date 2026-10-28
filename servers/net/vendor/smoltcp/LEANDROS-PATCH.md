# smoltcp 0.11.0, patched for LeandrOS

Vendored from crates.io (`smoltcp-0.11.0`), used through `[patch.crates-io]`
in the workspace `Cargo.toml`. `Cargo.toml` is the published one with the
examples, benches, dev-dependencies and profile removed (a workspace member
needs only the library). Every change in `src/` is marked `LEANDROS`: the TCP
loss recovery below (lane wifirx, 2026-10-17) and the UDP/ICMP/IPv4 changes
of lane udp2 (2026-10-18, second section).

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

Not done: SACK-based recovery.

## Lane pitcp / pitcp2 (2026-10-28): congestion control

`Cc` in `socket/tcp.rs` (`tcp.cc=none|reno|cubic`, default cubic; unknown values log a warning). After the review:

1. cwnd grows only when cwnd-limited (flight + MSS >= cwnd; slow start: 2*flight >= cwnd) and is clamped to max(peer window, send buffer). Loss uses the flight size for `w_max`/`ssthresh`.
2. CUBIC is integer-only (`icbrt`, ms units, i128); K is computed once per epoch. `w_max` changes only on loss; with no loss in the epoch K = 0. Reno-friendly region (`w_est`, alpha 0.53) added.
3. A repeated RTO (`rto_count > 0`) keeps ssthresh/w_max (RFC 5681 3.1). After an RTO `rto_recover` records the highest sent sequence so dupacks of the resend do not reduce again (RFC 6582).
4. Restart after idle > RTO: cwnd = IW and the CUBIC epoch is shifted by the idle time (RFC 5681 4.1).
5. `/proc/net/tcpsnd` `t_ms` is signed (negative when the timer is overdue).

Tests: `Cc` unit tests plus `test_dupacks_after_rto_do_not_reduce_again`; legacy tests that assume no CC pin `TEST_CC_MODE` to 0 (thread-local). Not done: fast-recovery inflation, fast convergence.

## Lane udp2 (2026-10-18): UDP demultiplexing, ICMP errors, IPv4 fragments

The net server enables `proto-igmp`, `proto-ipv4-fragmentation`,
`reassembly-buffer-count-16` and `iface-max-multicast-group-count-32`.

1. **UDP demultiplexing like Linux** (`process_udp`, `udp::Socket::score`).
   Broadcast and multicast go to *every* socket that accepts them (upstream:
   the first in slot order). Unicast goes to the best match (bound address
   +4, connected peer +8), the most recently bound on a tie, or within a
   SO_REUSEPORT group (`set_reuseport`) the one an FNV hash of the 4-tuple
   picks. A socket bound to an address no longer hears broadcast/multicast.
2. **Connected UDP** (`set_peer`): only the peer's datagrams are accepted, at
   demultiplexing time, so datagrams queued before a (re)connect stay.
   `unbind_port` gives up the port but keeps the queue (connect(AF_UNSPEC)).
3. **UdpMetadata** carries, received: the destination address, TOS byte and
   TTL (IP_PKTINFO, IP_RECVTOS, IP_RECVTTL); sent: a source address and a TTL
   per datagram. Equality still compares only endpoint and packet meta.
   `PacketMeta::tos` sets the IPv4 TOS of a sent packet (also on fragments).
4. **ICMP errors reach sockets** (`icmpv4_error`): destination unreachable
   and time exceeded are mapped like Linux's icmp_err_convert. A connected
   UDP socket whose peer the quote names gets a *fatal* one as a pending
   error (`error`/`take_error`, wakes the reader); a TCP socket in SYN-SENT
   whose SYN (ports and sequence number) it names closes with the errno
   (`process_icmp_error`, `icmp_errno`). `Icmpv4Repr::parse` no longer
   refuses a quoted packet whose total length exceeds the quote (every real
   error about a datagram longer than 8 bytes was dropped).
5. **Fragmentation**: every non-last fragment carries a multiple of 8 payload
   bytes (upstream used the whole MTU); the fragmenter buffer is a growing
   Vec (no 64 KiB array inside `Interface`); a packet being fragmented is not
   overwritten by the next one (the socket waits as for a full device), and
   all fragments go out in one poll. Reassembly refuses a datagram ending
   past 65535 bytes, drops the whole datagram on any fragment error, and
   frees a large buffer when its slot is reused or expires.
6. `Interface::ipv4_multicast_groups()` lists the joined groups
   (/proc/net/igmp).

Tests: `src/iface/interface/tests/leandros.rs` (9 tests: broadcast and
multicast to every socket, unicast best match, SO_REUSEPORT spreading,
ICMP port unreachable on connected/unconnected UDP, TCP SYN-SENT ended by
host unreachable, fragmented broadcast round trip, fragment offsets, TOS and
TTL per datagram, a port-unreachable round trip), plus `send_pending` (a closed socket waits for its queue). With the crates.io Cargo.toml: 591/591 pass.

## Lane sshlate (2026-10-19): neighbor cache entries no longer lapse under traffic

Upstream fills an ARP entry only from ARP and drops it 60 s later however
busy the peer is. The first packet to it after that is dropped and an ARP
request goes out instead (`NeighborPending`), and for 1 s (`SILENT_TIME`,
one limiter for the whole cache) every packet to any unresolved neighbor is
dropped too. On the laptop 1 Hz pings from the Mac lost exactly one reply
every 60 s (seq 43, 103, 163-165, 225, 285 of 300: 2.3 %), and a busy TCP
peer lost a segment a minute (an RTO stall, longer while the ARP exchange
itself is lost on a loaded Wi-Fi link).

1. **Refresh from traffic** (`Cache::refresh`, called by `process_ethernet`
   for every IPv4 frame): a unicast frame from hardware address M extends
   every entry that maps to M by `ENTRY_LIFETIME` (upstream 0.12's
   `reset_expiry_if_existing` covers on-link sources only; matching on M
   also keeps the router's entry alive for off-link traffic). Only ARP /
   NDISC replace a mapping.
2. **Stale entries stay usable** (Linux NUD_STALE): an IPv4 entry that
   expired less than `STALE_LIFETIME` (60 s) ago is still used by
   `lookup_hardware_addr` / `has_neighbor`, and a broadcast ARP request for
   it is queued (`want_probe`, at most one a second) and sent by
   `Interface::poll` (`neighbor_probe_egress`) with its own TX token. Past
   that, upstream's behaviour (drop + request). IPv6 is unchanged.

Tests: `test_stale_neighbor_used_while_refreshing`,
`test_neighbor_refreshed_by_ipv4_traffic` (tests/ipv4.rs). With the
crates.io Cargo.toml: 593/593 pass.

## Lane netabi (2026-10-19): Linux keepalive, duplicate SYNs

1. **Keepalive like Linux** (`set_keep_alive_probes`, `keep_alive_intvl`,
   `keep_alive_cnt`, `keep_alive_probes`): the first probe goes out
   `keep_alive` after the peer last spoke (upstream: the moment keep-alive
   was switched on, then every `keep_alive`); later probes every
   `keep_alive_intvl` (TCP_KEEPINTVL); after `keep_alive_cnt` unanswered
   probes the connection is reset when the next would be due
   (TCP_KEEPCNT). Any segment from the peer restarts the count. A new idle
   time re-arms an armed timer (TCP_KEEPIDLE on a live socket).
2. **A segment goes to its connection before any LISTEN socket**
   (`process_tcp`): two passes, non-LISTEN sockets first. Upstream took the
   first socket in slot order, so a delayed duplicate SYN of a connection
   already out of LISTEN started a second handshake on a listener that sat
   in a lower slot.
3. **SYN in a synchronized state gets a challenge ACK** (RFC 5961 4.2,
   rate limited by the existing challenge-ACK timer) instead of being
   dropped silently; the connection is untouched.

Tests: `test_keep_alive_idle_interval_count`,
`test_keep_alive_answer_restarts_count`, `test_keep_alive_new_idle_rearms`,
`test_established_dup_syn_challenge_ack` (socket/tcp.rs),
`test_tcp_dup_syn_prefers_connection_over_listener` (tests/leandros.rs; fails
on the old `process_tcp`). With the crates.io Cargo.toml: 598/598 pass.

## SACK on active opens, ACK for too-many-holes segments, 3 SACK blocks (lane shapedl)

- `(SynSent, Syn)` (the SYN-ACK of our own connect) now records
  `remote_has_sack = repr.sack_permitted`. Only the passive open did, so
  every outbound connection offered `sackOK` and then never sent a SACK
  block; after a slow-start overshoot into a tail-dropping queue the sender
  (macOS) repaired one hole per RTO: 0.6 Mbit/s on a 20 Mbit/s netshape link,
  18.4 with the fix.
- A segment the assembler cannot hold (too many holes) is now answered with an
  immediate (duplicate) ACK instead of silently dropped (RFC 5681 4.2).
- `ack_reply` fills SACK blocks 2 and 3 with the next-lowest assembler ranges
  (RFC 2018: report as many blocks as fit), not only the first.
- `RX_STATS` counters (in-order, out-of-order, too-many-holes,
  out-of-window data segments), shown as `tcp_rx:` in `/proc/net/nic`.

## Receive-window autotuning (lane rxwin)

- `Socket::set_rx_autotune(Some(initial))` (opt-in; the net server passes
  64 KiB for every TCP socket): the advertised window is `min(free buffer,
  clamp - buffered)`, i.e. the right edge is at most `remote_seq_no + clamp`.
  `clamp` only grows, so the right edge never moves left (RFC 7323 2.4).
- Growth after Linux's dynamic right-sizing (`tcp_rcv_space_adjust`): once
  per receive RTT the octets the application read are measured; when that
  beats the best RTT so far, `clamp = min(cap, 2 * read + 16 * 1460)`.
  The receive RTT is the minimum time to receive one advertised window
  (`tcp_rcv_rtt_measure`); no growth before the first sample.
- The window scale is still negotiated from the full buffer capacity.
- `RX_AUTOTUNE_STATS` (grows, largest clamp, autotuned sockets), shown as
  `tcp_rxwin:` in `/proc/net/nic`. `reset()` returns the clamp to `initial`.

Why: the fixed 2 MiB window let a sender's slow start overshoot the 256-frame
netshape RX queue (20 Mbit/s): 694-726 of ~16 k frames dropped, queue at 256.
Autotuned: 0 dropped, queue max ~105, same throughput (18.2-18.7 Mbit/s);
unshaped bulk download still reaches the 2 MiB window (2.3-2.65 Gbit/s).

Tests: `test_rx_autotune_starts_small_and_grows_for_fast_reader`,
`test_rx_autotune_slow_link_keeps_window_small`,
`test_rx_autotune_slow_reader_does_not_grow`,
`test_rx_autotune_off_advertises_whole_buffer` (both assert the right edge
never moves left). With the crates.io Cargo.toml: 602/602 pass.
