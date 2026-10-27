# lane piwdt — 2026-10-27: Pi 5 recovers from a wedge by itself

Status: DELIVERED (code + host tests + aarch64 regress; NOT yet run on the real Pi).

## Why
The Pi 5 is powered from a Mac USB-C port that cannot be power-switched. In the EPOLL
self-deadlock, CPUs 1-3 spin with IRQs masked, CPU0 keeps taking IRQs, ping answers,
[WDOG]/[WEDGE] print, the serial shell is dead, and only an unplug recovers it.

## What
1. **Hardware watchdog (on by default on rpi5).** The BCM2712 PM watchdog
   (`brcm,bcm2712-pm` @ 0x10_7d200000, PM_WDOG/PM_RSTC, password 0x5a000000,
   RSTC WRCFG_FULL_RESET) is armed at boot with 15 s (the hardware maximum). cpu0's tick
   watchdog scan (`sched::watchdog_scan`, every 0.5 s, IRQ context) calls
   `rpi_misc::wdt_feed_hook`, which feeds it (try_lock on the PM lock) only while every
   non-idle online CPU's `WD_AGE[0][cpu]` is under 45 s. A stuck CPU stops the feeding, so the
   board resets ~60 s into the stall. A stuck cpu0 stops it too (no scan).
   Boot line: `[WDT] bcm2712-pm armed 15 s, fed while all CPUs tick; reset ~60 s into a stall (wdt.off=1 disables); serial reset: 5x Ctrl-] or BREAK b`
   Stop line: `[WDT] cpuN stuck Ns: letting hardware watchdog reset`; if the CPU comes back
   before the reset: `[WDT] all CPUs ticking again: feeding resumed`.
   Knob: `wdt.off=1` (also stops a firmware-armed watchdog). Inert unless the DTB has a
   bcm2712/bcm2835 PM node, and only compiled into the rpi5 path (`rpi_misc::boot`).
   Replaces the old opt-in `pi.wdt=<s>` kthread kicker (it kicked unconditionally).
2. **Serial reset.** `uart::rx_hw_byte` (every byte read from the PL011 FIFO, any CPU, under the
   tiny RX_LOCK only) calls `RX_BYTE_HOOK` = `rpi_misc::serial_magic`, which resets via
   `rpi_pm::restart()` (lock-free) on 5 consecutive Ctrl-] (0x1d) within 2 s, or a BREAK then
   'b' within 2 s. Send from the Mac: `printf '\035\035\035\035\035' > /dev/cu.usbserial-1410`.
   If the console reader is wedged (backlog full, RX throttled), cpu0's tick reads and drops
   the FIFO after 1 s of continuous throttling so the sequence is still seen
   (`UART_RX_DISCARDED`; only when the hook is set, i.e. never in QEMU).
3. **kexec.** `final_cpu0` re-arms the full 15 s before jumping, so the next kernel must reach
   `rpi_misc::boot` within 15 s; one that hangs earlier is reset to the card's kernel.img.
   Older kernels stop the watchdog in their `wdt_init`.

## Evidence
- Host tests: `cd drivers/rpi-misc-tests && cargo test --target aarch64-apple-darwin` 15/15
  (new: `wdt_feeds_only_while_every_busy_cpu_ticks`, `wdt_serial_magic`).
- aarch64 regress (QEMU virt) PASS, no `[WDT]` output there; `scripts/check-rpi5.sh` clean for
  rpi5 and raspi4b; `build-all.sh --arch aarch64 --rpi5` builds.

## Open / caveats
- Not exercised on hardware yet: confirm the boot line, then `wdog.selftest=1` (cpu1 stalls
  with IRQs masked) should give the stuck line and a reset ~60 s later; and the printf reset.
- Idle CPUs are excluded from the stuck test (a CPU wedged while flagged idle would not stop
  the feeding; [WDOG] still reports it).
- A kexec'd kernel whose boot to `rpi_misc::boot` (after the v3d probe) exceeds 15 s resets.
- PM base stays the fixed 0x10_7d200000 (DT is used as a presence gate, not parsed for reg).
