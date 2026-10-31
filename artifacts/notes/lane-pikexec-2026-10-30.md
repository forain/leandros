# lane/pikexec 2026-10-30 -- aarch64 kexec straggler CPU

Bug (real Pi 5): a secondary CPU (cpu3) sometimes never reached the park page;
final_cpu0 logged "never reached the park page; jumping anyway" and the purgatory
overwrote the old kernel under the still-running CPU. Most likely cause: the CPU
spins with IRQs masked in `uart::getc`'s RX_LOCK acquire loop, so the park SGI never lands.

## Changes
- A (`kernel/src/kexec/exec_aarch64.rs`): final_cpu0 re-sends the park SGI every 100 ms
  during the 2 s wait. A CPU still missing gets one diagnostic line (last INTID, IRQ count,
  interrupted ELR from `sched::wedge::trail`, `uart_rx_lock_held`), then kexec ABORTS
  ("cpuN still in the old kernel; aborting kexec, system reset"): rpi5 `wdt_kick_nolock(1)` +
  wfe, otherwise PSCI SYSTEM_RESET. The purgatory is never entered over a live CPU.
  (INTID/count/ELR read `-`/0 unless the wedge probes are enabled.)
- B: `exception::divert()` (ack+EOI pending IRQ, run IRQ_DIVERT target). `uart::getc` acquire
  loop checks IRQ_DIVERT and diverts, and gives up after ~10 ms (returns None).
  `wedge::PARK_HOOK` (set by stop_and_jump) is called every 256 iterations of every
  `wedge::Spin::tick` (lockwatch tracked-lock spin, OUTBOX spin), so masked lock spinners park too.
- C: knob `kexec.test_straggler=<cpu>`: stop_and_jump leaks RX_LOCK and the named CPU, on its
  first divert, loops `uart::getc()` with IRQs masked. Case added to
  `.claude/skills/run-leandros/kexecfdttest.py` (`runtests aarch64 <tag> --suite kexecfdt`):
  gen3 loads gen4 with the knob, gen4 kexecs to gen5; requires 4 CPUs in gen5 and fails on
  "never reached the park page" / "never came online" / "still in the old kernel".

## Evidence (aarch64 QEMU/HVF)
- With B: kexecfdt PASS (gen5 4 CPUs, straggler path exercised, no bad strings).
- Without B (getc check + bound removed): FAIL -- serial: `[KEXEC] cpu3 never reached the park page;
  irq#- n=0 elr=0 uart_rx_lock_held=1`, then A aborts with system reset, gen5 never comes up.
- Not proven on the real Pi 5 (needs a trial): the real spinner may be another lock; a Spin-based
  path is covered by PARK_HOOK but not by a QEMU case.

## Pi 5 trial (orchestrator, 2026-10-31, 598fe79b = lane + main)
X=598fe79b -> good, 3 runs: all 3 ended on the card kernel by a clean reset (no corruption, no wedge, good's serial never ran on a broken image).
Runs 1 and 3: `[KEXEC] cpu2 never reached the park page; irq#27 n=2516 elr=ffff8000004c0c60 uart_rx_lock_held=0` then
`cpu2 still in the old kernel; aborting kexec, system reset`. elr = sched::kthread::sleep_until IRQ window (msr DAIFClr; nop; msr DAIFSet):
cpu2 takes timer IRQs (INTID 27) but never parks -> a second straggler class (not the uart RX_LOCK one). Open: lane pikexec2.
