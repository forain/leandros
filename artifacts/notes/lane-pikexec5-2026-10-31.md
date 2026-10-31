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
