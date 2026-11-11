# lane/ip6icmp (design-ipv6 L3) 2026-11-11: FIXED

Commits: 60b2bae0 fail-first ip6test, 4ae49137 implementation, 111b9622 smoltcp traffic class.
Red on 60b2bae0: ping6_dgram_lo, ping6_raw_lo, pktinfo6 fail. Green on tip: ip6test, nettest, udpselftest,
scmtest, nettag2test, sshsmoke, `ping -6 -c 2 ::1` all pass (aarch64, Mac).
The vendored smoltcp did not carry the IPv6 traffic class (egress or ingress); fixed in iface/interface/mod.rs and ipv6.rs.
Open: v4-mapped destination on a v6 ping socket EINVAL; hop limit 0 reads back as the default; no ICMPv6 cmsgs.
