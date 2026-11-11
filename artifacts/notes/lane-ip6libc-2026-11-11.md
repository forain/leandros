# lane/ip6libc -- IPv6 resolver/libc (design-ipv6 L7) -- 2026-11-11

REFUTED PREMISE / NO LIBC CODE CHANGE. The design targets relibc, but relibc was retired 2026-11-02 (lane/relibcretire): all
guest userland is musl (zig's static musl for tests, Alpine musl for the rest). relibc's inet_pton/getaddrinfo (v4-only) are no longer shipped.
Instead L7 became a regression guard on musl's resolver running on our kernel: ip6test +14 cases (all musl-linked):
pton6, ntop6, pton6_mapped, pton6_bad, ntop6_loopback, ntop6_enospc, gai_localhost6/4/_unspec/_addrconfig, gai_numeric_loopback6,
gai_numeric6_vs_inet, gai_v4mapped, gai_numeric6_scope (fe80::1%lo -> if_nametoindex), gni6_numeric, gni6_name_localhost,
localhost_fallback_v4only (design R1: getaddrinfo("localhost") ::1-first falls through to a 127.0.0.1-only listener in <2 s).
Evidence (Mac aarch64 HVF): ip6test 36/36 on the unfixed tree (only my test's EAI code expectation was wrong: musl returns EAI_NODATA,
fixed) -- so no fail-first exists; the kernel already satisfied the resolver contract (AI_ADDRCONFIG probe, /etc/hosts ::1 line, ECONNREFUSED on ::1).
Open: AAAA DNS over a real network (needs L4/L5 NIC v6); v6-first ordering with a real v6 route.
