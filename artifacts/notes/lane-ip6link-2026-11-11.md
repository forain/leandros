# lane/ip6link (design-ipv6 L4) 2026-11-11: FIXED

- servers/net/src/ip6.rs (new): EUI-64 fe80::/64 on each NIC stack while it exists, is admin up and disable_ipv6=0 (all or iface), synced once per daemon pass;
  v4 lease code keeps v6 addrs (replace_v4); scope id (= ifindex = sid+1) -> stack, EINVAL for scope 0, ENODEV for unknown index (bind/connect/UDP sendto/ping sendto);
  on-link prefix / default-router tables with route(); holder_of for fe80; /proc/net/if_inet6 (lo + NICs).
- sched ns.rs ipv6_disabled(); vif.rs ifnames hook now lists NICs in netns 0 so /proc/sys/net/ipv{4,6}/conf/eth0/* exist.
- ip6test +5 cases: ll_present, ping6_ll_router (slirp answers fe80::2 NS and echo, 4/4), ll_noscope, ll_bind_scope, disable_ipv6.
Evidence: red on tests-only commit (box a64): 5 FAIL. Green: ip6test a64 TCG + x86 KVM; full box regress both arches 84 suites / 0 bad.
Open: prefix/router tables are populated by nobody until L5; fe80 own-address delivery on a NIC stack; scope id missing in recvfrom/accept sockaddrs;
multicast reception on macb/brcmf (R5) not verified here; UDP socket stays on its first interface; ping fe80 over multiple NICs only by scope at first send.
