# lane/synlat 2026-11-13 - first DNAT connect latency
FIXED. Temporary ms-stamped traces in fwd.rs showed: SYN parked at t, the ARP request was only drained (drain_in) 446 ms later, then
ARP reply -> unpark -> replay -> SYN-ACK in <2 ms. Root cause: hair()'s learn-miss branch pushed the ARP request on FWD.inq and returned
without drivers::netdev::kick(), so drain_in waited for the next net housekeeping tick (~450 ms). Fix: drop(st); netdev::kick().
Measured Mac aarch64 HVF: first connect 447 ms -> 0 ms. brtest nftnat_cross_netns now CHECKs first connect < 100 ms (was KNOWN-OPEN print).
Traces removed. Box regress results: see merge commit / report.
