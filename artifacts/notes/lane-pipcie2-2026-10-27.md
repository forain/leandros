# lane/pipcie2 — pcie2/RP1 bring-up made bisectable on hardware (2026-10-27)

Status: PARTIAL (instrumentation + knobs delivered; the hanging access is not yet identified — needs 1–3 Pi kexecs).

## Symptom (Pi 5 D0, main 4ad83920 kexec'd with `pcie.rpi5=1`)
After `[pi5] act led ...` cpu0 takes no timer tick forever (`[WDOG] cpu0 took no timer tick ... pid=0 not in a syscall`,
seen from cpu1). No `[pcie]` line, no SError. Without the knob the kernel boots fine.

## Analysis
- "No tick, no SError, not spinning in a bounded loop" = a load that never completes (stalled AXI read). Every poll in
  `brcmstb_pcie::init` was already bounded (rescal 1 ms, MDIO 1 ms, link 300 ms with msleep — msleep would tick).
- The old code's first print (`[pcie] rc rev`) came *after* rescal, bridge SW_INIT assert, PERST# RMW (first RC read,
  MISC_PCIE_CTRL 0x4064, done while the bridge is held in reset), bridge release and the HARD_DEBUG RMW. Its prints
  went through the per-CPU console stage/OUTBOX, so a stall can also swallow an already-"printed" line: the log
  cannot tell which access hung.
- Prime suspects (old drivers/src/brcmstb_pcie.rs:154-166): the RC reads right after `bridge_reset(swinit, true)`
  (perst RMW, HARD_DEBUG RMW, MISC_REVISION) with the firmware-trained RP1 link live — upstream Linux carries
  CFG_QUIRK_AVOID_BRIDGE_SHUTDOWN for BCM2712 for bridge-reset trouble; second: the rescal block read; third (only if
  the output was swallowed): the first config TLP to bus 1 or the first RP1 BAR1 read in rp1::probe.
- DTB (bcm2712d0-rpi-5-b): pcie2 `reg 0x10_0012_0000/0x9310`, resets rescal + bcm_reset 44 ("bridge") only — no swinit
  line, so the driver's reset set matches the DTB.

## Change (d5586d13)
- 17 numbered stages (`brcmstb_hw::STAGES`). Before each stage's first access: `[pcie] stage N name` written straight
  to the UART FIFO (`sched::wedge::raw_str`, no stage/lock) + wedge slot `pcie` = N, so the `[WDOG]`/`[WEDGE]` line from
  another CPU shows `pcie=<name>/...`.
- New read-only stages ahead of any reset: 2 `swinit-read` (prints bridge reset in/out), 4 `rc-read`
  (`[pcie] rc rev=.. status=.. ctrl=.. link=up|down`).
- `pcie.stop=N`: stages 1..N run, then `[pcie] stop before stage N+1 name (pcie.stop)`, state=stopped, nothing else
  touched. `pcie.reuse=1`: if stage 4 sees the firmware link up, skip resets/SerDes/PLL/link-params/PERST# (5–8, 11).
- Host tests `drivers/brcmstb-tests` 11/11 (new: stop knob/stage names, reuse gating, brcmstb-reset bank math).
- check-rpi5.sh rpi5 + raspi4b clean; aarch64 QEMU virt regress: see merge.

## Stage table
1 map, 2 swinit-read, 3 rescal, 4 rc-read, 5 bridge-assert, 6 bridge-release, 7 serdes, 8 pll, 9 misc, 10 windows,
11 link-params, 12 link-poll, 13 bus-numbers, 14 rp1-id, 15 rp1-bars, 16 rp1-probe.
- aarch64 QEMU virt regress: all suites RC=0 except sigtest2 sigtimedwait_parks (timing); rerun /bin/sigtest2 PASS. New code runs only on the rpi5 build with pcie.rpi5=1.
