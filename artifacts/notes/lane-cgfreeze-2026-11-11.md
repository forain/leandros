# lane/cgfreeze 2026-11-11 - FIXED (freezer part)

- `cgroup.freeze` now parks tasks: `sched::freeze_group/thaw_group/group_all_stopped` (sched/src/lib.rs), new `Task.frozen`.
- Freezer reuses the job-control park (`stop_pending` + `Stopped`) but notifies no parent / no wait4 report.
- SIGCONT does not thaw a frozen task (on_signal_generated skips it); SIGKILL (and cgroup.kill) clears it.
- `cgroup.events` reports `frozen 1` only when requested AND every thread of every tgid in the subtree is Stopped.
- Thaw skips groups that are also job-control stopped (leader stop_signal != 0).
- Test: cgtest `cg_freeze_real` (counter in shared mmap stops, SIGCONT no-op, thaw resumes, kill works).
  Fail-first on box x86_64 at test-only commit fab050ad: FAIL line 764 (counter kept running). Green aarch64 Mac (HVF).
- Open: tasks forked/migrated into an already-frozen cgroup are not frozen; podman pause/unpause not exercised in-guest.
