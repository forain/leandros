# lane/fwdtcp (L4 of design-podnet3) - FIXED (TCP NAT, port reservation, expiry, reap); ICMP error translation NOT done

- fwd.rs: TCP NAT (SYN-only flow start, lite FSM: SYN, SYN-ACK, ESTABLISHED, one FIN, both FINs/RST = TIME_WAIT-lite; TTLs 60 s / 2 h / 60 s / 10 s), full TCP checksum rewrite.
- NAT ports (UDP and TCP) are reserved in the ns-0 port table: lib.rs `bind_conflict` asks `fwd::reserved` (a bind to an in-use NAT port on 0.0.0.0 or a NIC address gets EADDRINUSE);
  the NAT pick inserts its entry first, then asks `host_port_busy` (SOCK_TABLES read, no FWD held) and retries on a host-held port.
- Conntrack expiry timer: `drain_in` (every daemon pass) runs the 1 s throttled sweep; `reserved` also ignores expired entries.
- Reap: vif.rs `reap` and `free_vifs` collect the MACs of the freed vifs and call `fwd::forget_macs` (entries by container MAC).
- brtest: fwd_tcp (TCP DNS over NAT, plus HTTP GET with `-h IP[:PORT]`), fwd_port_reserved, fwd_reap.
- Fail-first (box aarch64, test-only commit a6bdd73e): fwd_tcp, fwd_port_reserved, fwd_reap FAIL. After: brtest PASS.
- Open: ICMP errors for tracked flows are not translated; the NAT-never-picks-a-held-port direction is covered by the pick/verify loop but not by a test (fd cost).
