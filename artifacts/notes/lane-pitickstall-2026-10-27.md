# lane/pitickstall — 2026-10-27 — FIXED (pending hardware confirmation)

Bug: real Pi 5 (rpi5 build 4ad83920), heavy f2fs SD I/O (pi_sync extract: `head -c` cutting
large files out of a ~64 MiB bundle, splice), tick watchdog:
`[WDOG] cpu0 took no timer tick for ~12 s ... pid=11 ... [cpu0 irq#1 n=680378 elr=0xffff80000044a260]`
and cpu2 ~22 s (pid 1014 /bin/head). Wi-Fi AP deauthed the Pi afterwards (reason 6).

## Symbol
Rebuilt 4ad83920 `--rpi5` (tag 4ad839207a-20261027T1758Z; same commit as the Pi's 4ad839207a-20261027T1517Z,
only the timestamp differs). In `kernel-direct`, 0xffff80000044a260 = `sched::scheduler_run_loop`,
the `msr DAIFSet,#2` right after the idle `msr DAIFClr,#2; wfi`. Same function exists on main 4b975f56.
irq#1 = SGI_RESCHED. So the LAST IRQ each CPU took was the resched IPI waking it from idle; it then
dispatched a task (pid 11 / head) and took NO interrupt at all for 12-22 s: it ran kernel code with
IRQs masked the whole time (not a dead timer: no IPI either; not a lock-free WFI: it was dispatched).

## Root cause
`drivers/src/sdhci.rs` (rpi5/raspi4b SD driver) is polled PIO, one 512-byte sector per CMD17/CMD24,
run in syscall/kthread context with IRQs masked, and never opens an IRQ window. Each handshake wait is
bounded only by SPIN_LIMIT = 20M MMIO reads (~10-20 s on BCM2712 — matches 12 s / 22 s and the
self-recovery). A long read span (read_blocks holds DEVICES across the whole span) or a slow card
(write busy / inhibit) keeps the CPU deaf; waiters on DEVICES (spin::Mutex) on other CPUs spin deaf too.
Wi-Fi brcmf work on the deaf CPU starves -> AP deauth.

## Fix (commit on lane/pitickstall)
- `tick_window()` (same contract as virtio_blk::irq_window_if_masked: only with IRQs masked, pid != 0,
  not in IRQ context, preempt disabled around it so no task switch while DEVICES is held):
  once per sector in do_io and every 1024 spins in wait_normal_int / reset_cmd_dat / wait_cmd_inhibit_clear.
- read_blocks takes DEVICES per 4 KiB block, not across the span (write_blocks already did).
Inert elsewhere: sdhci is compiled only for rpi5/raspi4b. rpi5 drivers crate compiles.

## Tests
aarch64 QEMU regress (virt; sdhci not in that build): overall PASS, boots failed 0/1.
No QEMU repro: the stall needs the BCM2712 MMIO latency + a real card.

## What the next Pi run should show
Same pi_sync extraction: no `[WDOG] cpuN took no timer tick` lines; if the card really stalls,
SD timeouts now show as the driver's own `W<sector>p..N..E..~` / `!` / `i` dbg lines and
`[TIMER]`-free progress, and Wi-Fi stays associated. If WDOG lines persist with elr != the idle wfi,
the deaf loop is elsewhere (send the new elr).
