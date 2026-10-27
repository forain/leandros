# lane piuartrx3 — Pi 5 serial RX loses ~25 chars mid-line (openbugs #101) — PARTIAL (fix + guard; needs one Pi boot)

## Pi evidence (before)
`[uart] rx overruns 1 errors 0 throttled 0 irqs 22 tick 35 rescued 41`, one `[uartirq] rx-pending` capture.
INTID 152 works (piirq3). `rescued 41` = the TX-wait drain already ran; `tick 35` = 35 bytes sat until the
cpu0 tick (timer PPI wins over SPI 152 when both pend) = IRQs masked across tick boundaries.

## IRQ-off windows found (code reading)
1. Framebuffer console, under KERNEL_FB, IRQs masked (syscall context): `scroll_px` = one ~8 MiB memmove
   of the shadow, then the FlushBatch drop blits the whole screen (~8 MiB) into write-combining scanout.
   No RX service anywhere in it. Every line wrap/newline at the bottom of the screen scrolls; brush redraws
   the line on every key, so the wrap point of a long typed line hits it: ~5 ms = 2.8 ms FIFO + ~25 bytes.
   (Same for `clear` on ESC[2J.)
2. putc TX wait: already rescues RX (piuartrx2) — not the cause.
3. RX trigger 1/8 + RTIM already set (piuartrx2); IRQ ack order (drain to RXFE, then ICR) is fine.

## Fix
- drivers/src/framebuffer.rs: scroll memmove and clear in 32-row chunks, blit polls every 8 rows, each
  calling an RX poll hook; the kernel registers `arch_aarch64::uart::rx_poll` (FR read; rx_rescue into the
  1 KiB ring + pend the UART SPI if RX non-empty).
- kernel serial_write_raw: rx_poll every 8 bytes.
- Counters: `txgap_us` = longest RX-unserviced stretch of a console write; `irqlate_us` = longest cpu0
  tick lateness (bound on ANY IRQ-masked section). On the `[uart]` line and `/proc/sysstat` `# uart`.

## Evidence
- host:uartgaptest.py (aarch64 virt, gpu=none): `# uart txgap_us 177 irqlate_us 2478 rescued 128 overruns 0` PASS.
  Limitation: gpu=none has no fb console, so the guard bounds the putc path only; the scroll fix is
  exercised only on the Pi (QEMU raspi4b has no display either). irqlate 2.5 ms on HVF = some other
  masked section (not attributed; on the Pi, irqlate_us > 2800 with txgap_us small = a non-console culprit).

## Next Pi boot
Type/paste a 100+ char line at the brush prompt (several times, at the screen bottom), then `cat /proc/sysstat | head -3`.
Expect `[uart] rx overruns 0 ...` unchanged (no new line), and `# uart txgap_us <~500 irqlate_us <2800 rescued >0 overruns 0`.
If overruns rise with txgap small and irqlate > 2800: another masked section on cpu0 (not console output).
