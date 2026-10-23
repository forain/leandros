# lane/netirq 2026-10-23 (L9.8) - virtio-net RX interrupt

Status: FIXED for virtio-net RX (aarch64 proven; x86_64 MSI-X compiles, tested at merge batch).

- 951bda5d: RX queue interrupt (MSI-X on x86_64, shared INTx SPI line on aarch64); the aarch64 handle_irq now evaluates both the arch table and drivers::irq line sharers.
- f3993a9f (WIP at quota stop) was an accidental revert of 951bda5d (removed the handler and 149 lines); dropped.
- New: /proc/net/nic `netd:` line shows `rx_irq on|off` (now true for virtio once an IRQ fired) and `virtio_rx_irqs N`.

Evidence (aarch64, HVF): serial shows "[NET] INTx armed: INTID 0x23" and "[NET] first RX interrupt"; after /bin/nettest: virtio_rx_irqs 117, netd passes 3737. Regress: everything green except schedstress short and nettag2test (both pass on rerun; nettag2test is a known flake). nettest, udptest, netctltest pass.

Polling: the net daemon is kicked by the IRQ and keeps its 1 kHz / 100 Hz idle timer as the safety net (netdev.rs doc). The 250 ms `[netctl]` kthread (servers/net/src/netctl/mod.rs POLL_MS) diffs link/IPv4 state and scans for Wi-Fi backends, not RX; it is untouched. Making it event-driven needs link-change notifications from every driver and is left open.
TX completions remain polled. Guest `ping 10.0.2.2` gets no reply under QEMU user networking (unrelated; nettest RTT 2-3 ms).
