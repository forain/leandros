# lane/ip6slaac (design-ipv6 L5) 2026-11-13

- servers/net/src/ip6.rs: internal raw ICMPv6 socket per NIC stack (`attach`, in init_nic); `slaac_pass` per daemon pass: RS x3 (4 s apart, link-local source, ff02::2) while there is no router/address, RA parse (router lifetime, PIO A/L flags), SLAAC address = advertised /64 + EUI-64 IID, on-link prefix (L), default router (smoltcp default v6 route + ROUTERS table), expiry timers, withdrawal on admin down / carrier loss / disable_ipv6 / accept_ra=0; `autoconf=0` keeps the router but skips the address.
- sched ns.rs `ipv6_conf()` (per-iface accept_ra/autoconf); `/proc/net/ipv6_route`; if_inet6 lists the global address.
- ip6test: `slaac_addr` (address with eth0's EUI-64 IID, any non-fe80 prefix: this slirp advertises a random fd.. ULA prefix on the Mac, fec0::/64 elsewhere), `ping6_global` (<prefix>::2). `unreach6` now accepts success when a router exists.
- Evidence: red on tests-only commit (Mac a64): slaac_addr, ping6_global FAIL. Green after: slaac_addr, unreach6 pass on Mac a64; ping6_global and ping6_ll_router fail on Mac only (below).
- Mac HVF side issue (ping6_ll_router 0/4 errno 11): not root-caused. Same symptom for ping6_global on the Mac, while the multicast RS -> RA exchange works there and both cases are green on the box. Suspect NS/echo handling of the newer libslirp in QEMU 11.1.1 (needs a pcap). Left open in TODO.md.
- Open: DAD, RDNSS, RA MTU option, accept_ra_off/dns6/tcp6_out cases, scope id in recvfrom/accept sockaddrs (left in TODO).
