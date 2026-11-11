# lane/ip6dual -- IPv6 dual-stack (design-ipv6 L2) -- 2026-11-11

FIXED (L2; IPv6 TODO line narrowed, not struck: L3+ remain).
- servers/net: SockOpts.v6only + IPPROTO_IPV6/IPV6_V6ONLY get/setsockopt (EINVAL once bound; ENOPROTOOPT on AF_INET);
  sched ns.rs `ipv6/bindv6only` sysctl (default 0, read at socket()); v4-mapped bind on a V6ONLY socket EINVAL, connect ENETUNREACH.
- bind_conflict family-aware: `[::]` (dual) conflicts with 0.0.0.0/any v4 address, V6ONLY `[::]` coexists; listeners and UDP
  (incl. mirrors) get an IPv6 family filter when V6ONLY (listen::wildcard_family, udpany fam).
- sshd: `::` listeners are V6ONLY (tokio TcpSocket + setsockopt), 0.0.0.0 and `::` are separate listeners; sshsmoke checks
  the SSH banner on 127.0.0.1 and ::1 (`ip6test banner ADDR PORT` helper).
- Tests: ip6test +9 cases (v6only_default/set_get/coexist, dual_accept_v4, dual_conflict x2, v6only_after_bind,
  mapped_connect_v6only, udp_dual).
Evidence: fail-first on tests-only commit 85a900cc (box aarch64): 5+ FAIL. Fixed (Mac aarch64 HVF): ip6test 100%, nettest, nettag2, sshsmoke 9/9.
Open: sendto to a mapped dest on a V6ONLY UDP socket is not refused; IPPROTO_IPV6 options other than V6ONLY.
