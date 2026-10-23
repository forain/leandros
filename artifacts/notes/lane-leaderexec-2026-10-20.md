# lane leaderexec (openbugs L5.12) 2026-10-20

A group leader that pthread_exits ahead of its threads now stays in the run
queue as a delayed Zombie (`Task::delayed_leader`, Linux delay_group_leader).
`group_settled` counts it once off-CPU; the path that settles the group
(`exit()` of the last thread, or the dispatch-loop reap) calls
`reap_delayed_leader` + `finish_reaped_leader` and logs the process exit under
tgid. exe/sysacct/reparent/orphan-pgrp cleanups moved from `pid == tgid` to
`last`. The existing `take_over_leader` now finds the leader slot, so a
non-leader execve after the leader's exit keeps the pid.

Tests (aarch64): killmt proclife leaderless_exit 20/20, exec_leaderless 20/20,
all other proclife cases and `/bin/killmt 40` pass; `--suite regress` overall PASS.
Regress serial log: no new [SCHED] warnings besides the known process/task-table
limit messages from the limit tests (check_all exemption not needed).
Open: the `/proc/<pid>/fd` clause of L5.12; WUNTRACED reporting of a delayed leader.
