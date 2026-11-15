# lane/ip6rest 2026-11-15
Rootless pasta IPv6 fixed: pasta_options no longer forces -4; tunbe rtnl_route6 accepts gateway-less on-link route adds; ping_allowed maps ping_group_range through the netns owner userns; vendored smoltcp NA neighbour cache keyed by NA target (LEANDROS-PATCH.md).
Own-address ICMPv6: servers/net/src/lib.rs local_delivery now answers NS for a local interface's address and loops unicast IPv6 frames addressed to a local MAC (v4 already did via ARP). ip6test ping6_own_global fail-first on aarch64, green after.
pasta_v6_ping targets the RA default gateway (busybox ip -6 route in the guest), fe80 gets %eth0; echo reply observed through pasta.
Gates: podpasta + podbridge aarch64 PASS pre-merge-main; regress both arches on box after merging main.
Open: Time Exceeded (fwd6::divert), aardvark-dns --rm teardown, tun6/DAD.
