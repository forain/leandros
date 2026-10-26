# lane piuartrx — Pi 5 serial RX drops input at line speed (openbugs #99) — FIXED (code review; needs a card swap to confirm)

## Root cause
- `gic::SPI_PL011` was 33 on every board. Pi 5 uart10 (0x10_7D001000) is `interrupts = <0 121 4>`
  in bcm2712-rpi-5-b.dtb, i.e. INTID 153. The Pi enabled the wrong INTID, so RX was only drained by
  the cpu0 tick fallback (timer.rs, 100 Hz). 10 ms at 115200 is ~115 bytes; the FIFO holds 32
  (or 1 if the firmware left LCRH.FEN clear — we kept the firmware's LCRH verbatim). One char per
  20 ms fits in a tick, a pasted line does not: exactly the observed symptom.
- The drain itself (evdev serial_rx_drain) already reads until RXFE, with a 4 KiB backlog and
  RX+RTIM interrupts unmasked; that part was fine.

## Fix
- gic.rs: SPI_PL011 = 32 + 121 under `rpi5`.
- uart.rs: force LCRH.FEN on rpi5, program IFLS (RX 1/2, TX 1/8), clear RSR/ECR at init;
  getc() counts DR.OE into UART_RX_OVERRUNS and FE/PE/BE into UART_RX_ERRORS.
- iostat: prints `[uart] rx overruns N errors N throttled N` on the console when they change.
- Periodic stats (`[fslock]`, `[pcache]`, fsync report, `[FORK]`) go through `kernel::stat_line`:
  klog-only on rpi5 (they cost ~25 ms of 115200 TX each and buried the shell), console elsewhere.
  `[FORK]` is now one formatted line instead of per-byte putc (same text; 12708 lines in regress,
  same as pihw2).

## Guard
- QEMU cannot reproduce: its PL011 applies chardev backpressure (pl011_can_receive returns false
  while the FIFO is full), so a host burst is never overrun, and virt already used the right INTID.
  No QEMU guard; the `[uart] rx overruns` line is the on-hardware check.
- aarch64 regress: PASS (tests-piuartrx). check-rpi5.sh: OK.

## Open
- dmesg on the Pi image: there is no kernel interface to read the in-memory klog (no syslog(2) /
  /dev/kmsg); espctl klog reads a persistent partition the SD layout may not have. Not cheap here.
  Boot lines like `[early] nomap` still go to the serial console, so capture from power-on.
- raspi4b (QEMU) also uses SPI_PL011 = 33; BCM2711 uart0 is SPI 121 too. Not touched.
