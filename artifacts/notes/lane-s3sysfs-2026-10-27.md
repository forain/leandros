# lane s3sysfs (2026-10-27)

Scope: userspace-visible sleep knobs, no ACPI/real S3 (s3aml, s3core not started).

- `kernel/src/pm.rs`: `pm::init()` (called from init.rs after lappower) registers ONE delegating
  `sysclass::Ops` table (all arches). power_supply/backlight go to `drivers::power_sysfs` unchanged;
  new classes `power` and `rtc`.
- `/sys/power/{state,mem_sleep,pm_test,wakeup_count}`: served by `servers/vfs/src/sysclass.rs` as a
  pseudo class (index 3, files directly under /sys/power; split()/dir()/attr()/write()/dirent()
  special-case it, ~25 lines).
  - `state` reads `freeze` (plus `mem` iff pm.s3=1 and S3_VALID, which s3core must set).
  - `echo freeze` runs the existing `freeze_cycle` (hold 500 ms) when pm_test is `devices`; `freezer`
    is a logged no-op; other levels EINVAL.
  - `echo mem` is EINVAL without pm.s3=1. With it: at level none and pm.s3_require_rtc (default on)
    it needs an armed alarm <= 300 s; levels platform/processors/core/none return ENOSYS until s3core.
  - `pm_test` reads `none core processors platform devices freezer` with `[sel]`; boot knob `pm.test=<level>`.
  - `wakeup_count`: write back of a stale value is EBUSY (counter never bumps yet).
- `/sys/class/rtc/rtc0/{wakealarm,since_epoch}` (x86_64 only; no rtc0 on aarch64, wakealarm write is ENOENT/ENODEV):
  `+N` / epoch / `0`; target must be in the future and < 24 h. `arch/x86_64/src/rtc.rs`
  `set_alarm/clear_alarm/alarm_regs` write only CMOS 0x01/0x03/0x05 and the AIE bit of 0x0B
  (BCD/binary, 12/24 h aware). Logged: `[PM] rtc alarm armed ... regB=0x22` (AIE set).
  NOT done: FADT DAY_ALRM/MON_ALRM handling (needs FADT; s3aml/s3core), AIE clear after resume.
- Knobs: `pm.s3=1`, `pm.s3_require_rtc=0`, `pm.test=`. Defaults inert.
- `userland/suspendtest/suspendtest.c`: new `--sysfs` mode (state/pm_test/freeze/alarm checks).
- Caveat: the freeze cycle runs inside the VFS server thread for sysfs writes (all fs I/O stalls for the
  hold time). It resumed fine on x86 QEMU virtio-blk; revisit if s3core moves the cycle elsewhere.

Tests (release, TCG): x86_64 `suspendtest --sysfs` all PASS (incl. freeze_cycle, alarm set/readback/clear),
plain `suspendtest` PASS; aarch64 both PASS (no rtc: wakealarm write fails). Laptop not used.
