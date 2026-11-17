# lane conrx 2026-11-17: x86_64 KVM console input stall — FIXED

Symptom (TODO, lanes sysfshang/drmlinks/zigl2): on x86_64 KVM a 3-4 KB line typed into brush
over the serial console (the `sysfs` suite after `/bin/uinputtest`) echoed ~8 bytes, then
nothing for ~5 min; the suite timed out (361 s). aarch64 TCG fine. Hidden since 7c48a0d4 by
the harness staging long lines through a guest file.

Root cause: the kernel never wrote the COM1 16550 FCR, so the UART kept the firmware's
1-byte RX trigger level (boot line now prints `IIR before 0xc1`). QEMU's 16550 advertises
receive room only up to the trigger level, so it took one byte from the chardev per main-loop
pass, and x86 drains the UART only from the 100 Hz cpu0 tick. With the guest printing
nothing (reedline repaints only after input pauses) the main loop rarely ran, and input
crawled: an unpaced 4.4 KB line arrived at 14 B/s.

Evidence:
- Harness diagnostics on an unstaged full x86 regress (refs conrx-r1/r2): echo missing after
  10 s; three HMP RIP samples all in `sched::scheduler_run_loop` (guest idle, IF=1); the
  harness had queued 4467 -> 8935 -> 13403 typed bytes (the cancel-and-retype ladder) and was
  blocked 1.0-6.6 s on a full socket. So the guest was starved of input, not stalled.
- New suite `rawline` (uinputtest + a 4392-byte line typed unpaced and unstaged, gated on the
  end-to-end rate), x86_64 KVM box:
  red (conrx-red, no fix): timeout 311.3 s = 14 B/s, echo_ok=False.
  green (FCR=0xC7, 14-byte trigger): 0.9 s = 4880 B/s.
- `/proc/stat` `timerstat` field 5 = serial bytes drained (both arches).

Fix: `arch/x86_64/src/lib.rs` `uart_fifo_init` (FCR=0xC7 at arch init, skipped when no UART
decodes COM1). `rawline` runs in `regress` right after `/bin/uinputtest`.

usbstall: a separate cause. It failed on the unfixed tree too (conrx-red2), and on aarch64 TCG;
its slow echoes (3.5 s, 3.6 s into the window, both arches) coincide with the USB-MSC attempt-2
timeout and the Bulk-Only reset recovery. The fix did halve x86's echo baseline in usbstalltest
(median 1.18 s unfixed vs 0.51 s fixed, same box, same hour).
