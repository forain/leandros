# lane pikexec5 (2026-10-31)

Cause: on aarch64 kexec a CPU parks holding a plain spin::Mutex (brcmf lock around msleep); another CPU then spins in spin's relax() with IRQs masked, never reaches a park point -> "cpuN still in the old kernel".

Changes:
- vendor/spin: held::set_park_hook / park_poll; Spin::relax calls park_poll first (one relaxed load; unset on x86).
- exec_aarch64.rs: stop_and_jump also sets spin park hook; FINAL guard so cpu0 inside final_cpu0 ignores park_hook (no re-entry via log -> serial spin).
- Test: kexec.test_straggler_mode=lock (holder forgets a spin::Mutex and parks; designated CPU spins on it masked); kexecfdttest.py leg gen6->gen7.
- Proven both ways: relax hook reverted -> leg FAILS (gen6->gen7, "still in the old kernel"); with it PASS.
- Not done: (e) per-CPU last-lock diag (relax() has no lock address; would need per-CPU index in the lock path, not cheap); (f) brcmf guard-drop across msleep (not verified safe).
