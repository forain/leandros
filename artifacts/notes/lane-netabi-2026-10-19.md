# Lane netabi — 2026-10-19

Bug list L9 (P2) items 2, 3, 4, 6, 9, 10, plus L2.2 (AF_UNIX listener close).

## L9.10 — nettag2 flaky: FIXED (kernel)

Root cause: a blocking send/recv/connect on a socket parked with
`PollTag::bits(tag)` (`net_block_hint`), i.e. bloom bits with no keys. An
unkeyed bit is never refined, so every *keyed* wake in the system
(AF_UNIX/pipe/eventfd data edges) whose key landed on the same one of 64
bits woke the waiter. nettag2's own "unrelated" AF_UNIX/pipe round trips
use ~4-6 keys, so each case had a few-percent chance per boot, and which
case failed followed the socket's key — exactly the "2/3 runs fail,
varying case, ~20-50k wakes/s" seen by lane sotimeo. The poll() path was
already keyed; only the blocking park dropped the keys.

Fix: `net_server::fd_wake_tag` returns the full `PollTag` (`sock_tag` /
`unix_end_keys`, the same answer `handle_poll` gives), and
`net_block_hint` passes it through. Wildcard UDP with more than 3 stack
keys still parks on bits only (`keys_tag`), as before.

## L9.3 — ephemeral ports: FIXED

`alloc_ephemeral_port` searched from `ticks % range`, so every allocation
within one 10 ms tick began at the same port and a port freed a moment ago
came straight back (UDP connect/close, or TCP when the server closed first
so no TIME_WAIT reservation existed). Now a cursor seeded from the kernel
CSPRNG at net init (`seed_ephemeral`, never under SOCK_TABLES) moves past
every port it hands out (Linux's incrementing `__inet_hash_connect`). Range
32768-60999 and the TIME_WAIT snapshot are kept.

## L9.4 — AF_UNIX connector closed before accept: FIXED
## L2.2 — closing an AF_UNIX listener: FIXED (SO_ERROR part)

The backlog was "every table entry in `UnixPendingAccept` for this
listener", so a connector that closed took its connection out of the
backlog (and the `UnixConn` was freed). Now the backlog lives on the
connection: `UnixConn::pending_sid` (listener id while unaccepted) and
`pending_ord` (arrival order; accept takes the oldest — FIFO like Linux,
not table-scan order). Closing the last connector reference of a pending
connection only marks end A closed; accept returns it, reads see the data
sent before the close and then EOF. `unix_backlog_head` serves accept and
the listener's poll; the GC derives its embryo list from it; a listener
that goes away frees the orphaned ones and resets the rest as before.
`pending_sid` is set under SOCK_TABLES after the entry exists, so an
accept cannot see a connection whose entry is not yet pending.

L2.2: POLLHUP/POLLERR and the ECONNRESET-then-EOF read were already done
(`unix_listener_gone`, `reset_a`); missing was SO_ERROR, which now answers
ECONNRESET once (and clears it, so POLLERR goes and reads see EOF).

## L9.6 — keepalive spacing, TCP_NODELAY push: FIXED

Keepalive (vendored smoltcp, see its LEANDROS-PATCH.md "Lane netabi"):
first probe KEEPIDLE after the peer last spoke (upstream sent one the moment
keep-alive was enabled), then every KEEPINTVL, reset after KEEPCNT
unanswered; an answer restarts the count; a new KEEPIDLE re-arms. The net
server no longer emulates the deadline with smoltcp's timeout (which probed
only every KEEPIDLE). TCP_NODELAY (any TCP-level change on a connected
socket) now kicks the net daemon so Nagle-held data goes out at once
instead of at the next ACK/timer.

## L9.2 — delayed duplicate SYN hits LISTEN: FIXED

smoltcp's `process_tcp` gave a segment to the first socket in slot order
that accepted it; a LISTEN socket accepts any SYN, so a duplicate SYN of a
connection whose socket sat above a re-armed listener started a second
handshake. Now two passes: the connection (4-tuple) first, LISTEN after.
A SYN in a synchronized state gets an RFC 5961 challenge ACK.

## L9.9 — AF_UNIX datagrams: FIXED

- SCM_RIGHTS on a connected datagram/seqpacket socket: `sendmsg` parses
  them and `unix_dgram_send_fds` pins the batch to the record's first byte;
  `unix_dgram_read_locked` delivers it with that record (recvmsg) or
  closes it (recv/read), peek clones.
- MSG_TRUNC: recvmsg sets it in msg_flags; recv/recvmsg with MSG_TRUNC
  answer the full length. A zero-length datagram now reads as 0 (it used to
  look like an empty queue: EAGAIN) and wakes its reader.
- SO_SNDBUF: stored like Linux (doubled, min 4608, max 2*wmem_max), read
  back; on AF_UNIX datagrams `len > sndbuf-32` is EMSGSIZE and a send waits
  while queued bytes + 576/record (skb truesize stand-in) reach it. AF_UNIX
  SO_SNDBUF/SO_RCVBUF default reads 212992 (was the TCP buffer size).

## Tests

`userland/udptest/netabi.c` (in udptest → regress): eph_port_spread,
unix_accept_closed, unix_listener_reset, tcp_nodelay_push,
unix_dgram_rights, unix_dgram_trunc, unix_dgram_sndbuf. All 7 PASS on the
Linux desktop's host kernel (7.2.7, gcc) first. `udptest -o PREFIX` runs
only the cases with that prefix.

smoltcp host tests (crates.io Cargo.toml): 598/598; the new
`test_tcp_dup_syn_prefers_connection_over_listener` fails on the old
`process_tcp`.

aarch64 (HVF):
- udptest 48/48 (the 7 new cases included); 3/3 more on slirp.
- nettag2 under the qemu lock, 5 boots: 5/5 PASS, 9/9 cases each; of 129
  1 s windows 120 saw 0 wakes, 9 saw 1 (best-of-3 is 0 everywhere). Before:
  2 of 3 boots failed with 20-50k wakes/s (lane sotimeo, main 42f5c03b).
- regress (parallel with other lanes' QEMUs): 36/37; udptest failed only
  udp_mcast_loop / udp_broadcast with *two* copies of each datagram — the
  fixed ports 47011/47013 on the shared vmnet subnet, so another lane's
  concurrent udptest's broadcast/multicast arrived too. Re-run on slirp
  (`LEANDROS_NET=user`) 3/3 PASS. Worth giving those cases per-run ports.
- After the rebase onto main c71ed0c6, regress on slirp: 38/38 RC=0.

## Open / notes

- The TCP_NODELAY push cannot be told apart from an ACK-released Nagle
  segment on loopback (the peer ACKs at once); the case checks the bytes
  arrive and the options read back.
- No in-guest test can inject a duplicate SYN (no raw TCP sockets); L9.2 is
  covered by the smoltcp tests.
- listen.rs still keeps the arm above queued sockets in slot order; it is
  now redundant with the two-pass demux but harmless.
