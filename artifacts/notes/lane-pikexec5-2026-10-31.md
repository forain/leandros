# lane pikexec5 (2026-10-31)

Cause: on aarch64 kexec a CPU parks holding a plain spin::Mutex (brcmf lock around msleep); another CPU then spins in spin's relax() with IRQs masked, never reaches a park point -> "cpuN still in the old kernel".

Changes:
- vendor/spin: held::set_park_hook / park_poll; Spin::relax calls park_poll first (one relaxed load; unset on x86).
- exec_aarch64.rs: stop_and_jump also sets spin park hook; FINAL guard so cpu0 inside final_cpu0 ignores park_hook (no re-entry via log -> serial spin).
- Test: kexec.test_straggler_mode=lock (holder forgets a spin::Mutex and parks; designated CPU spins on it masked); kexecfdttest.py leg gen6->gen7.
- Proven both ways: relax hook reverted -> leg FAILS (gen6->gen7, "still in the old kernel"); with it PASS.
- Not done: (e) per-CPU last-lock diag (relax() has no lock address; would need per-CPU index in the lock path, not cheap); (f) brcmf guard-drop across msleep (not verified safe).

## pikexec7 (follow-up)
- aarch64 arch_tlb_service_pending() now calls sched::wedge::park_check(): every masked cross-CPU wait loop (14 sites) is a park point. FINAL guard in park_hook still keeps cpu0 in final_cpu0 from re-entering.
- Added tlb_service_pending() to: sched AS old.busy spins (switch/exit), hrtimer cancel_sync spin branch, klog flush_final spin branch.
- Test: kexec.test_straggler_mode=rq (holder forgets a run-queue lock, straggler spins on it masked); kexecfdttest.py leg gen7->gen8.
- Proven: kexecfdt PASS with hook; FAIL with hook reverted.

## pikexec9 (Pi diagnostic build)
- Per-CPU stage words in the park page at +0x980 + 8*cpu (cleaned to PoC): 1 divert entry (on_ipi / straggler_entry / park_hook), 2 after gic kexec_cpu_off, 3 just before the `br` in enter_idmap, 4 first store at the purgatory park entry.
- "never reached the park page" line now ends with `stage=<n>` and `clean=<sleep_forbidden_reason at divert>` (note: usually "in IRQ context"; the recorded reason is for the interrupted divert).
- Success prints `[KEXEC] park stages: cpu1=4 cpu2=4 cpu3=4`.
- Knob `kexec.icache=is` (cmdline of the kexec'ing kernel) selects `ic ialluis` instead of `ic iallu` in prepare().
- QEMU kexecfdt PASS, stages 4 for all CPUs.

## Addendum pikexec10: SDIO power-off serialization + knob

Pi evidence: straggler CPU at stage=0, last IRQ in brcmf msleep window; I-cache knob no effect.
Hypothesis: MMIO stall on a clock-gated SDIO host while power_off ran from the kexec CPU.

- `drivers/src/brcmf/sdio_host.rs`: new `GATE: Mutex<bool>` (the bool is HOST_DEAD). Every `Mmio`
  accessor (r8..w32) takes it, returns all-ones/drops writes when dead. `power_off` takes the same
  gate (try_lock, bounded 100 ms; on timeout returns "sdio busy, power-off skipped" and does not
  gate), sets dead under it, then resets/gates with raw writes, then drops WL_REG_ON.
  `probe_with` clears the flag (re-runnable).
- `drivers/src/brcmf_boot.rs::shutdown`: waits up to 500 ms for the driver kthread's SHUTDOWN_DONE
  (the netdev TX/RX path runs on that same kthread). On timeout: logs
  `[REBOOT] brcmf: thread busy, power-off skipped` text and SKIPS the gating (previously it gated
  anyway, "forced"). The next kernel's `probe_with` quiesces the host and pulses WL_REG_ON low
  itself, so a chip left powered is reset there.
- Knob `brcmf.kexec_poweroff=0` (kernel cmdline, parsed in `brcmf_boot::start`): skips SDIO
  clock/bus power-off and the WL_REG_ON drop; shutdown line says so.
