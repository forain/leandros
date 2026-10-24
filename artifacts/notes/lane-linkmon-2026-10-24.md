# lane/linkmon 2026-10-24 — interrupt-driven link monitor + TX bulk tool (L9.9 remainder)

## FIXED (a) link monitor on link-change interrupts
- `drivers/src/netdev.rs` `LinkMon`: a KWaitQueue + event counter; ISRs `notify()`, the monitor
  thread `wait()`s with a timeout instead of `msleep(250)`. `/proc/net/nic`: `linkmon: irq_wakes N timer_wakes M`.
- e1000e: ISR notifies on ICR.LSC / ECCER. Cadence: 250 ms while RX/TX counters move, a TX-stall
  episode is open or no interrupt is wired; 2 s when idle.
- e1000e: aarch64 has no ITS (request_msi -> Unsupported), so it was fully polled there. New INTx
  fallback (shared line, `PciFunction::intx_line`) for link/errors; RX stays polled on INTx:
  with RX on INTx a bulk upload crawled at 0.03 Mbit/s (0.3 MB in 70 s) — not investigated,
  worth a look if RX-on-INTx is ever wanted (`irq_count ... empty 555` = sharers on line 35).
- r8169 [HW-unverified]: `note_status` notifies on IntrStatus LinkChg (every IntrStatus reader);
  same cadence rule (+ RX-stall episode keeps it fast). No QEMU model.

## FIXED (b) TX bulk tool
- `netcheck --put HOST PORT [MIB] [--secs S]`: streams zeros over one TCP connection, per-second
  lines, waits for the peer's FIN, prints `NETCHECK put: PASS ... = X Mbit/s`. Host sink: `nc -l PORT >/dev/null`.

## Evidence (aarch64 HVF, QEMU e1000e, `linkmontest.py`, in net-e2e; alone: `runtests.py aarch64 <tag> linkmon`)
- idle timer wakes 0.52/s (old: 4/s by construction), 0 irq wakes idle.
- `set_link net0 off`: seen in /proc/net/nic 0.59 s after the command (incl. serial loop latency), irq wake +1;
  `on`: 1.10 s (QEMU autoneg ~0.5 s), irq wake +1.
- `netcheck --put 192.168.105.1 PORT 32`: 32.0 MB in 0.26 s = 1033 Mbit/s, host got 33554432 B.

## Open
- r8169 path unverified on hardware; e1000e on x86_64 (MSI) not run in this lane (aarch64-only rule).
- RX on shared INTx (see above).
