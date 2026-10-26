# lane pi4uart — raspi4b PL011 RX INTID (openbugs #100) — FIXED (compile-checked)

- gic.rs: SPI_PL011 = 32 + 121 (INTID 153) under `raspi4b` (BCM2711 uart0, DT `interrupts = <0 121 4>`); was 33.
- FEN/IFLS/RSR init and OE/error counters from piuartrx are already unconditional in uart.rs (FEN forced on the
  non-rpi5 path too), so nothing else needed. INTID stays a per-board constant: no DTB parse exists at that point.
- scripts/check-rpi5.sh takes CHECK_FEATURE=raspi4b. rpi5 and raspi4b both check clean.
- No regress run: change is cfg-gated, default build untouched. Not verified on QEMU raspi4b/hardware.
