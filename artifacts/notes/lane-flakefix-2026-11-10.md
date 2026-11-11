# lane/flakefix 2026-11-10

## B brtest NAT precondition: FIXED
nat_scan probed bind() over 61000..64999, so TIME_WAIT ports from earlier cases or runs looked NAT-held.
The kernel's NAT picker already skips host-busy ports, so TIME_WAIT never collides with it.
Fix: /proc/net/nic gains `fwd_nat_tcp: <ports>` (live TCP conntrack NAT ports, same liveness as fwd::reserved);
brtest nat_scan reads it. fwd_port_reserved still asserts a host bind to the NAT port gives EADDRINUSE (nat_inuse).
bridge_l2 / fwd_hostlisten / hostlisten_not_in_ns host listeners now set SO_REUSEADDR (fixed ports 7778/7779/7781, TIME_WAIT on the 2nd run: errno 98).
Verified: /bin/brtest x2 in one boot on the Mac (aarch64): both RC=0, no FAIL lines. Before the listener fix run 2 failed bridge_l2 errno 98.

## A nettag2 connect_pending: DIAGNOSED, NOT FIXED
Captured twice in a full box aarch64 regress (identical 9.4 s, identical text), never alone (box and Mac pass alone, also under 4 busy loops x6 runs):
`connect_pending: FAIL no pipe parks on a bit of mask 0x2000 (320 probed)`
So it is a test-side alias_pipe failure (userland/nettag2/nettag2.c), not a stray wake, and it is deterministic after the earlier suites, not random.
The waiter parked on a single bloom bit 13 (poll_key_bit = 1<<(key&63)); none of the 320 probe pipes parked on bit 13.
Tried: dropping probes that never parked (ef41b1f5): no change. Hypotheses left: pipe ring numbers allocated after the earlier suites do not cover all 64 residues (ring pool/stride), or the probe's mask for pipes includes a class offset. Next: print alias_bit[] histogram on failure, or fall back to a UDP/eventfd alias source when no pipe hits.
