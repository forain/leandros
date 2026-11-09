# lane/nsudp 2026-11-08 (follow-up to nsnet2)

FIXED (aarch64). servers/net/src/lib.rs: a UDP bind in a non-initial netns (sock netns != 0) no longer
calls udp_materialize, so no host-stack smoltcp socket exists (unreachable from host/other netns);
recv on such an InetBound dgram socket answers EAGAIN (blocking waits on its sock_state key).
Test nstest netns_udp_bind: child netns binds UDP 0.0.0.0:41031, host sends to 127.0.0.1:41031,
child recv (SO_RCVTIMEO) -> EAGAIN, host can still bind 41031. FAILED before the fix (child received
the datagram), PASSES after. nstest + nettest + default regress: see report.
Note: ping (ICMP) sockets in child netns left unchanged (bind() on them is a no-op already, state created at first send which is gated ENETUNREACH).
