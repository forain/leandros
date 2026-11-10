# lane/flakefix 2026-11-10

## B brtest NAT precondition: FIXED
nat_scan probed bind() over 61000..64999, so TIME_WAIT ports from earlier cases or runs looked NAT-held.
The kernel's NAT picker already skips host-busy ports, so TIME_WAIT never collides with it.
Fix: /proc/net/nic gains `fwd_nat_tcp: <ports>` (live TCP conntrack NAT ports, same liveness as fwd::reserved);
brtest nat_scan reads it. fwd_port_reserved still asserts a host bind to the NAT port gives EADDRINUSE (nat_inuse).
bridge_l2 / fwd_hostlisten / hostlisten_not_in_ns host listeners now set SO_REUSEADDR (fixed ports 7778/7779/7781, TIME_WAIT on the 2nd run: errno 98).
Verified: /bin/brtest x2 in one boot on the Mac (aarch64): both RC=0, no FAIL lines. Before the listener fix run 2 failed bridge_l2 errno 98.

## A nettag2 connect_pending: NOT REPRODUCED
Six nettag2 runs in one boot beside 4 busy-loop processes: connect_pending passed 6/6 (0 wakes in 67k round trips per window).
Only the 20 ms latency cases (connect_lo, udp_recv, ...) failed, which is plain CPU starvation, not a stray wake.
Prior stray-wake fix is lane/wakecatch eeb22f7e. Failure text from the flaky runs was never captured; test left unchanged.
Next step: capture the exact FAIL line from the next full-regress failure (results.txt) before changing anything.
