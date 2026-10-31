# lane pikexec3 (2026-10-31)

Pi 5 straggler: cpu2 never parked while the brcmf radio loop called msleep(1) holding a spin::Mutex.
sleep_until's atomic busy-wait branch (and udelay) never reached the kexec park hook.

Fix: `wedge::park_check()` (one atomic load of PARK_HOOK) called in sleep_until's busy-wait branch and udelay.
Guard: `kexec.test_straggler_mode=sleep` (kernel/src/kexec/exec_aarch64.rs): the straggler CPU takes a spin::Mutex,
atomic_inc and loops msleep(1). kexecfdttest.py gained gen5->gen6 for it.

Evidence (aarch64 QEMU, --suite kexecfdt):
- without fix: `[KEXEC] cpu3 never reached the park page; ...` / `cpu3 still in the old kernel; aborting kexec, system reset`, suite FAIL
- with fix: RESULT PASS
- First attempt (no atomic_inc) passed without the fix: that context was "no current task", i.e. not the same branch; a stale kernel also masked one run.

Not done: drivers/src/brcmf/netdev.rs:~670 (drop lock around msleep) is not trivial; follow-up.
