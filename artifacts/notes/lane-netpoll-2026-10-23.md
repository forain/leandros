# lane/netpoll (L9.8) - 2026-10-23 - FIXED

Commits: 83b11451 (netctl event wakeups, virtio-net config-change IRQ, async TX slots), netflaptest guard.

## What changed
- netctl worker waits on a wait queue (5 s backstop, 250 ms poll only until the boot profile loads) instead of an
  msleep(250) loop; kicked from iface::set_admin and the Wi-Fi register hook. `netctl_wakes` in /proc/net/nic.
- virtio-net: link/config-change IRQ (INTx ISR bit 1; MSI-X config vector shares entry 0).
- virtio-net TX: 32 pre-allocated copy slots, lazy reclaim at next send, no completion spin (queue stays NO_INTERRUPT).
- tests: `netflaptest.py` (QMP set_link flap), added to the `net-e2e` suite.

## Evidence
- Idle wakes, aarch64: netctl_wakes 6 -> 12 over 30 s = 0.2/s (before: 4/s by construction).
- x86_64 TCG after nettest: `rx_irq on`, virtio_rx_irqs 346 vs stack_rx 366 packets (IRQ coalescing; all RX via vector 0x43).
- aarch64 main shows `[NET] INTx armed` + `first RX interrupt` (tests-npa64); the suspected irq23 regression is REFUTED
  (tests-irq23reg also has INTx armed; only "first RX" is absent there because no traffic ran).
- Throughput proxy, x86 TCG, main vs lane: nettest 18.0 vs 18.3 s, udptest 22.6 vs 23.0 s (within noise; no dedicated TX bulk
  tool exists, netcheck only measures downloads).
- Flap guard (aarch64 and x86_64): carrier-down event 0.6-0.7 s after set_link off (includes monitor latency); carrier up and
  `ipv4 bound` 0.7 s after set_link on. One aarch64 run once missed the up events (not reproduced in 4 reruns; watch window widened).
- Regress aarch64 PASS; x86_64 nettest/netctltest/udptest/sshsmoke PASS.

## Open follow-ups (not done)
- net daemon 10 ms idle loop (servers/net/src/lib.rs ~3333, ~100 passes/s idle) is now the dominant idle wake source; stale
  "virtio-net has no receive interrupt" comment.
- e1000e / r8169 still run their own 250 ms link-monitor threads.
- No TX bulk throughput tool.
