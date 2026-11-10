# lane/fwdudp (L3 of design-podnet3) - FIXED (UDP + ICMP echo)

## What landed
- servers/net/src/fwd.rs: forward core. Bridge-ingress divert (called from tunbe::poll_all for a bridge's rx queue, before its stack runs),
  forward gate = `sched::ns::forwarding(0, <bridge name>)`, SNAT of UDP (reserved port 61000..64999) and ICMP echo (id = port), TTL decrement,
  full IP/UDP/ICMP checksum rewrite, conntrack (UDP 30 s / 180 s once replied, ICMP 30 s, swept at most once a second), ARP cache snooped on the NIC rx hook
  plus ARP request on a miss (up to 4 held frames per pending next hop, dropped after 1 s), NIC fwd txq drained in the NIC pass, un-NAT in the NIC rx hook
  (before smoltcp) into an in-queue that the daemon hands to the bridge switch after the NIC loop.
- iface.rs: `nic_route` (NIC-only route: on-link else default gateway), `is_nic_addr`. lib.rs: `mod fwd`, rx hook, NIC pass drain, `drain_in`. l2::forget -> fwd::forget(bridge).
- brtest: fwd_udp (UDP DNS query to /etc/resolv.conf nameserver), fwd_ping (ICMP echo to the default gateway from /proc/net/route), fwd_off (ip_forward=0: no reply).

## Isolation
- Only frames addressed to the bridge MAC are considered, only when the gate of THAT bridge is open (default off; ip_forward fan-out sets it).
- Never forwarded: destination that is any ns-0 address (NICs, bridge/veth addrs, loopback), multicast/broadcast, the bridge's own subnet (stays with the bridge stack);
  source must lie in the bridge's network and not be the bridge address (anti-spoof); fragments, TTL<=1, protocols other than UDP/ICMP echo are dropped.
- Return path only on a conntrack hit (src = recorded remote, dst = NIC addr + NAT port); the frame goes to the recorded container MAC through the bridge switch.
  Nothing is delivered into another netns or a host socket outside the bridge/NAT path. Outside -> container new flows: no hit, smoltcp handles it as before.

## Evidence
- Fail-first (box aarch64, test-only commit 8e1a7da9): fwd_udp and fwd_ping FAIL, fwd_off passes trivially (nothing forwards).
- After: brtest 85 passed, 0 failed (aarch64, Mac HVF).

## Open
- NAT ports are not reserved in the ns-0 port table (fwdtcp/L4 owns the bind_conflict seam); the range sits above Linux's ephemeral range 32768..60999.
- Conntrack entries of a dead container are not reaped by netns death, only by timeout. ICMP errors (time exceeded/unreachable) are not translated.
- Checksum offload on real NICs untested (frames carry full software checksums).
