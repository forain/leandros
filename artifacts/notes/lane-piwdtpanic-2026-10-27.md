# lane piwdtpanic — 2026-10-27: Pi 5 resets itself after a kernel panic

Status: FIXED in code (rpi5 build + aarch64 regress); not yet run on the real Pi.

## Root cause
The rpi5 PM watchdog is fed by cpu0's `watchdog_scan` (IRQ context, every 0.5 s) whenever
every busy online CPU's tick age is under 45 s (`rpi_misc::wdt_feed_hook`). The panic
handler (`kernel/src/main.rs`) never told the watchdog anything and ended in
`loop { spin_loop() }` with DAIF left as it was. So after
`--- KERNEL PANIC --- buddy free list corrupt`:
- if the panicking CPU had IRQs unmasked it kept taking timer ticks -> its age stayed 0;
- if it was cpu0 with IRQs on, cpu0 itself ran the scan and fed;
- a CPU that panicked while flagged idle is excluded from the stuck test.
In every one of those cases the feed continued forever: no reset in 6 min matches.
Not disarmed (nothing on the panic path wrote RSTC), and not a dead countdown:
- the arm sequence matches Linux `bcm2835_wdt_start` exactly (WDOG = PASSWORD | ticks&0xfffff,
  then RSTC = PASSWORD | (RSTC & WRCFG_CLR) | WRCFG_FULL_RESET); ticks are 1/65536 s
  (`SECS_TO_WDOG_TICKS(x) = x << 16`), 15 s = 0xf0000 fits the 20-bit field;
- the serial reset (5x Ctrl-], `pm::restart` = the same `arm()` with 10 ticks) works on
  the Pi, which proves WDOG/RSTC writes take effect and the counter runs to expiry.
Wedge-type stalls (CPU stuck with IRQs masked, non-idle, not cpu0) were covered; panics were not.

## Fix
- `fatal_begin()` first thing in the panic handler: rpi5 `rpi_misc::panic_begin` sets
  PANICKED (the feed hook returns immediately from then on) and re-arms the full 15 s
  lock-free, so a panic path that hangs later (log flush) still resets in 15 s.
- `fatal_end()` after the message + `klog::flush_panic`: `panic_end` prints
  `[WDT] panic: full reset in 5 s (panic.reset=0 halts instead)`, waits on CNTVCT
  (re-kicking 15 s each second as a backstop), prints `[WDT] panic: full reset now` and
  writes the RSTC full reset. `panic.reset=<s>` sets the delay (Linux `panic=` semantics,
  cap 3600); `panic.reset=0` stops the watchdog and halts (`[WDT] panic: halted ...`).
- aarch64 EL1 fault halts (`exc_el1_sync_handler` fallthrough, `exc_unexpected_handler`)
  now call `kernel_fatal_halt()` = the same policy (they used to spin with IRQs masked:
  cpu0 there -> 15 s reset, idle-flagged CPU -> never).
- Boot countdown proof: after arming, `[WDT] countdown ok: wdog f0000 -> ... in 10xxx us
  (expect ~655 ticks), rstc ...` or `countdown NOT RUNNING`.
- `wdog.selftest=2`: at tick 3000 cpu1 stalls in its timer IRQ (IRQs masked) forever, marked
  busy (IDLE_SINCE cleared). Expect `[WDOG] selftest: cpu1 stalls ... FOREVER`, then
  `[WDT] cpu1 stuck 45s: letting hardware watchdog reset`, reset ~60 s after the stall.
- All of it is rpi5-only (`cfg(feature = "rpi5")`, PM block required); other boards keep spinning.

## To verify on the Pi (orchestrator)
1. Boot: the countdown line says `ok` with ~655 ticks.
2. `wdog.selftest=2`: board reboots by itself ~90 s after boot (30 s tick 3000 + 60 s).
3. Any panic: `[WDT] panic: full reset in 5 s` then reboot.

## Open
- Which CPU hit the buddy panic, and with which DAIF, is not known (log lacks it); the fix
  does not depend on it. The buddy corruption itself is a separate bug.
