# lane/sysctlif (S3 of design-podnet3), 2026-11-10

- sched/src/ns.rs: IFCTL table, ip_forward fan-out to per-interface forwarding, `pub fn forwarding(ns, ifname)`.
- servers/net/src/vif.rs: ifnames hook. servers/vfs/src/lib.rs: netctl_slot uses the reader's netns.
- Tests (nstest): procsysnet_ifdirs, ip_forward_fanout, ifdir_isolated.
- Fail-first on lane/sysctlif-base (aarch64): procsysnet_ifdirs and ip_forward_fanout FAIL (errno 22); ifdir_isolated passes on base (isolation holds trivially without the dirs).
- Fixed: nstest all PASS on aarch64.
- GAP (not done): `ls /proc/sys/net/ipv4/conf` does not enumerate per-interface directories of the reader's netns, and stat on the dynamic files is not wired; open/read/write by path work.
