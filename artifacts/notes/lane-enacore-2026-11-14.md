# lane/enacore (EC2 design lane L4) - 2026-11-14

FIXED/DELIVERED: ENA driver core, host placement, polled.
- `drivers/src/ena_hw.rs`: EnaBus-generic core (readless MMIO, reset, admin SQ/CQ + AENQ by phase, feature negotiation, CREATE_CQ/SQ, TX/RX copy rings, link_gen, keep-alive/fatal/admin-timeout watchdog + re-init).
- `drivers/src/ena.rs`: PCI probe (1d0f:ec20/ec21/0ec2/1ec2), BAR0 map, DMA glue, watchdog thread, netdev `ethN` ops, kexec/poweroff reset; `ena.disable=1`. Kconfig `ENA` (default y). No device -> one log line.
- `drivers/ena-tests`: Rust mock ENA (21 tests incl. readless rejection of plain reads, 10k-frame loopback with ring wrap, admin/AENQ phase wrap, link_gen, keep-alive reset, fatal reset, retry backoff). Run: `cd drivers/ena-tests && cargo test --target aarch64-apple-darwin`.
- Regress: x86_64 85 suites 0 bad; aarch64 48 run, only timertest realtime_is_not_uptime (known flake, passes on rerun).

OPEN / UNPROVEN (needs real EC2, gate G1): descriptor/command field offsets follow the public ENA headers from memory of the Linux/DPDK drivers, encoded identically in driver and mock, so a misreading would pass the tests; HOST_ATTR_CONFIG is not sent; GET_FEATURE control-buffer length (128) is a guess; LLQ (L5) and MSI-X (L8) not done.
