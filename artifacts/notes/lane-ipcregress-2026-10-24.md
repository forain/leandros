# Lane ipcregress, 2026-10-24: desktop login livelock after ipcfast (FIXED)

## Symptom
integ12: coldstart-bench greeter login never completes on both arches after
480a9db2 (lane/ipcfast). Reproduced on main c15e1957 aarch64: three failed
login attempts, the serial root shell stops answering, QEMU at 422 % host CPU.

## Root cause
QMP `info registers -a` on the hung guest: all four vCPUs at EL1, three in
`KRawMutex::lock_deadline`'s spin branch, one in `KWaitQueue::wait_event_timeout`
spinning (netctl worker). Both spin only when `might_sleep()` is false, i.e.
every CPU looked atomic.

`TrackedGuard::unlocked` (new in ipcfast) releases UNIX_CONNS with
`spin::Mutex::force_unlock` around a long (>= 1024 B) AF_UNIX stream copy, then
re-takes it with `try_lock` and forgets that guard. The vendored spin crate's
held-count hook (lane pools) counts every acquire, and the guard drop releases,
but `force_unlock` released nothing: net +1 per long write/read on the CPU
running it. A CPU with a non-zero count is "holding a spin::Mutex" to
`sleep_forbidden_reason`, permanently, so every KMutex / wait-queue wait on it
spins with IRQs masked (and tick preemption is deferred). The desktop login moves
enough large unix-socket traffic (dbus, Wayland) to poison every CPU; the tests
did too few long copies on few CPUs to notice. net_block_lazy, the SOCK_TABLES
rwlock and the reserve/commit logic were not involved.

## Fix (3657fde3)
- vendor/spin `force_unlock` calls `held::released()` (the release half of a
  forgotten guard; also balances lock_api's try_lock+forget/unlock pattern).
- `/proc/lockstat` gains `spin_held <sum over CPUs> <switch violations> <sleep violations>`.
- scmtest `unlocked_copy_no_spin_leak`: 256 x 4 KiB send+recv on a socketpair,
  spin_held must not grow by more than 32. Unfixed kernel: grew by exactly 512 (FAIL);
  fixed: PASS.

## Verification
- aarch64: coldstart-bench (virgl, term) login first 0.66 s, launcher first 0.90 s
  (baseline 70b79902: 0.90 s); regress PASS (incl. nettest, sshsmoke); scmtest
  default, stress, pairbench PASS.
- x86_64: coldstart-bench login first 0.94 s, launcher first 1.25 s; regress PASS.

## Open
- Nothing from this lane. nmrealtest/netroamtest (integ12 regression 2, tkip) untouched.
