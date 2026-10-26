# lane piuartrx2 — Pi 5 serial RX still overruns (openbugs #101) — PARTIAL (code fix; needs a card swap)

## Code audit (main c89fcf6d)
- GIC: SPI 153 is enabled through `enable_spi` (level, IRQ_PRIORITY, GICv2 ITARGETSR = cpu0's banked iface mask /
  GICv3 IROUTER = BSP); NUM_IRQS 512. Looks right; QEMU virt proves the dispatch path (below).
- PL011: IMSC = RXIM|RTIM, handler `uart_irq` drains until RXFE (evdev serial_rx_drain), clears ICR,
  and wakes readers itself (`flush_pending_wake`), no tick wait.
- LIKELY CAUSE: the kernel runs syscalls with IRQs masked (DAIF.I set; only `sched::irq_window` opens them),
  and the RX SPI targets cpu0 only. A console `write` (brush redraws the whole line on every key) spins in
  `putc_patient` for TX room at ~87 us/byte with IRQs masked. Anything past ~64 bytes of output = >2.8 ms
  = 32 RX byte times with no drain -> one overrun per line, cut at ~FIFO depth: the pihw4 symptom.

## Fix (aeee3bc0)
- uart.rs: while `putc` waits for TX room and RXFE is clear, `rx_rescue` moves RX bytes into a 1 KiB ring
  (IRQ-masked try-lock) and software-pends the UART SPI (`gic::pend_uart_irq`, GICD_ISPENDR) so `uart_irq`
  delivers them once IRQs open; `getc` pops the ring first (order kept); `has_data` counts the ring.
- IFLS RX trigger 1/2 -> 1/8 (4 bytes), leaving 28 FIFO slots of headroom.
- Counters on the `[uart]` line: `irqs` (uart_irq entries), `tick` (bytes only the cpu0 tick fallback drained),
  `rescued` (bytes pulled out during TX waits). The line now also prints once when the first RX IRQ arrives.

## Evidence
- QEMU aarch64 regress PASS (tests-piuartrx2); log: `[uart] rx overruns 0 errors 0 throttled 0 irqs 16 tick 0 rescued 0`
  -> the RX IRQ fires and drains everything, the tick fallback took 0 bytes.
- QEMU cannot overrun (chardev backpressure) nor block TX, so `rescued` stays 0 there; the ring path is unexercised.

## On the Pi (read the `[uart]` line after pasting a 52-char line)
- `irqs 0`, `tick > 0`: the SPI never fires on BCM2712 -> GIC routing/INTID still wrong.
- `irqs > 0`, `rescued > 0`, overruns flat: fixed by the TX-wait drain.
- `irqs > 0`, overruns still rising, `rescued 0`: another long IRQ-masked section on cpu0 (not console TX).
