# lane/proclife2 (2026-10-23): L5.12a, L10.6a

Status: FIXED (aarch64; both arches left to the per-batch run on main).

## L5.12a
- `wait_scan` (sched/src/lib.rs): stopped/continued reports now also come from a delayed-zombie leader
  (`unsettled` = live, or zombie whose group is not settled). Settled-zombie path unchanged.
- `do_signal_stop` (sched/src/signal.rs): the parent to notify is read from the group leader's `ppid`;
  a non-leader thread's `ppid` names the sibling that created it, so SIGCHLD/CLD_STOPPED went to the child itself.
- Tests (in proctest.c, C with pthreads, instead of the no_std waittest): `stop_leaderless`, `stop_thread_sigchld`.

## L10.6a
- `thread_children_on_exit(pid, tgid, last)`: runs for the leader too (G1); reparents to the leader if alive,
  else the first live thread (Linux find_alive_thread, G4); pdeathsig is read from every live thread of each child (G2,
  `collect_pdeath`, also used by `reparent_children`); for the process's last thread only the ppid fix-up happens and
  `reparent_children` delivers, so it is once (G3).
- Tests: `pdeathsig_leader_thread`, `pdeathsig_chain`, `pdeathsig_child_thread`, `pdeathsig_once` (RT signals, counted).
  Each parent waits ~30 ms so the child has called prctl before the parent thread leaves (otherwise a real race).

## Evidence (aarch64, proctest)
Before the fix: 5 failures (leader_thread got 0, chain 1/2 vs 2/3, child_thread 0, stop_leaderless "stop not reported",
stop_thread_sigchld si_code 0). pdeathsig_once passes before and after (guard).
After: all pass.

## take_over_leader side note
Already handled: it rewrites `ppid == old_pid -> tgid` for children (lib.rs, "Children forked by this thread name its old tid"). No change.
