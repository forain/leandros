# lane/piwedge 2026-10-27 — Pi 5 whole-machine wedge after wifi0 carrier up: make it speak

Status: PARTIAL (instrumentation + bisection knobs delivered; root cause needs one Pi run).

## Findings from code inspection
- REFUTED by code: SDIO card-interrupt / sdio2 IRQ storm. sdio_host never sets the SDHCI
  signal enables (REG_NORMAL/ERROR_SIG_ENABLE = 0 in host_init and on release); the card
  interrupt is only a polled status bit inside `wait_card_irq`. No sdio2 SPI can reach the GIC.
  (So `sdioirq=poll` is already the only mode; no knob added.)
- REFUTED by code: unbounded SDHCI waits. command/wait_int/cmd53_pio/reset/clock waits are all
  deadline-bounded (monotonic = CNTVCT, not tick-derived). Only an MMIO access that never
  completes (bus hang) could stall there; the new `sdio=` marker names it.
- FOUND: why the existing tick watchdog stayed silent. `[WDOG] cpuN took no timer tick` prints
  through `arch_serial_putc` -> per-CPU stage -> `OUTBOX` (plain unbounded spin lock) ->
  framebuffer mirror. If the stuck CPU holds anything on that path, or every CPU ends up behind
  it, the report wedges the observer too. With 4 CPUs online the old watchdog should otherwise
  have printed within 2 s, so the silence itself says: all CPUs stall, or the print path stalls.

## Changes
- `sched/src/wedge.rs` (new): lock-free diagnostics, on by default on rpi5 (`wedge=0` off),
  `wedge=1` opt-in elsewhere; one relaxed load when off.
  - raw printer `arch_wedge_putc` (kernel/src/main.rs: PL011/16550 data register, bounded
    FIFO wait, no klog/stage/lock/fb).
  - per-CPU IRQ trail (last INTID, IRQ count, interrupted PC) from aarch64 `handle_irq`.
  - SPI storm detector: one SPI 50 000x back to back (timer PPIs/IPIs ignored) within 1 s ->
    `disable_spi` + one `[WEDGE] irq storm` line.
  - stage markers: brcmf (tx/poll/rx/ctl), sdio (cmd idx+arg / cmd53 data bytes),
    net (netd recv/send in the brcmf glue). A non-idle marker older than 3 s ->
    `[WEDGE] <slot> stage stuck N ms on cpuX [cpu trail] | markers`.
  - `Spin` probe in TrackedMutex contended loops (RUN_QUEUE, EPOLL, ...) and the console
    OUTBOX: a CPU spinning > 2 s prints `[WEDGE] cpu spinning >2 s for <lock>` + every CPU's
    trail, so even "every CPU is stuck behind one lock" speaks.
  - existing `[WDOG]` line goes raw when wedge is on and gains the trail + markers.
  - boot banner `[WEDGE] armed: ... cpus=4` proves the raw path works.
- brcmf dp knob: `brcmfctl dp on|off|rx [yield|noyield]` (or `brcmf.dp=off,yield` cmdline).
  off = keep association, hold wifi0 carrier down, drain+drop data frames; rx = carrier up,
  drop every transmit; yield = msleep(1) every service pass. Shown in /proc/net/nic `datapath:`.
- Tests: wedge unit tests (storm with interleaved timer PPIs, interleaved SPIs not a storm,
  slow runs not a storm, spin report once, stuck slot once, formatting) — storm guard fails on a
  variant without the PPI exemption; brcmf-tests 70/70 (dp parser incl. `seq=` prefix);
  sdio-tests 19/19; check-rpi5 rpi5+raspi4b clean; aarch64 QEMU regress (see report).

## Next Pi session
See the operator script in the lane report (orchestrator) — summary:
1. boot: expect `[WEDGE] armed: ... cpus=4`.
2. `brcmfctl dp off` then connect: wedge => brcmf/SDIO data RX itself; no wedge => net side.
3. `brcmfctl dp rx` (carrier comes up, no TX): wedge => RX into the net stack; else TX/DHCP.
4. `brcmfctl dp on yield`: no wedge => starvation by the non-preemptive kthread.
5. On wedge, the `[WEDGE]`/`[WDOG]` lines name cpu, last IRQ, PC, lock and stage.
