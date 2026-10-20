# lane/proclife — 2026-10-19

Process lifetime, bug list L5.3–L5.5 (`openbugs-2026-10-19.md`). aarch64/HVF, -smp 4.

## L5.3 — FIXED: a killed group was waitable before it was gone
Root cause: `wait_scan` reported a leader as soon as it was marked Zombie. When a
non-leader thread ran the group kill, the leader was marked (or reaped and logged as a
process exit) while the killer was still in its loop, holding the process fd table
(`run_exit_teardown(tgid)` ran after the loop) and an `Arc` on the address space. In
addition, a sibling's CPU drops its `Task` box (and its `Arc`) *after* it releases
RUN_QUEUE, so even the last thread's `release_exiting_address_space` saw
`strong_count > 1` and left the free to that CPU, after the parent's wait.

Fix (`sched/src/lib.rs`):
- `group_settled(tgid)`: every member still in the run queue is a Zombie with
  `Task::mm_released` (set by `exit()` for the group's last thread, before the Zombie
  mark), and no CPU has a removed member's drop in flight (`AS_DROP_TGID[cpu]`, set
  under RUN_QUEUE in the reap hold, cleared after the box is dropped).
  `wait_scan` (phase 1 and the exit-log phases) and `process_exit_state` (pidfd) treat
  an unsettled group as still running.
- The last thread waits out in-flight drops before it frees the space itself.
- SIGCHLD goes out when the group settles: from the last thread's `exit()`, or, for
  two threads exiting at once, from the reaping CPU via the system workqueue.
- The process fd table, sockets and epoll sets go with the group's *last* thread, not
  the leader. `exiting` is set under the lock that decides who is last. Before this
  change a leader's `pthread_exit` closed them under live siblings: the EXIT syscall
  called `vfs_close_all_current()` (keyed by tgid) first.

## L5.4 — FIXED: a SIGKILL during a non-leader execve takeover
Reproduced on main: `killmt exec_kill` FAIL at iteration 6, "child not reaped". A
process-directed SIGKILL goes to the thread's private set and prefers a Blocked
thread. When that thread was a parked sibling, the takeover reaped it with the signal
still pending. The exec'd image then ran on, unkillable by that kill.
- A SIGKILL (process- or thread-directed) now lands on the group's teardown owner
  (`sigkill_target`). After the exec, the image dies with SIGKILL.
- A thread that ends the process (`exit_group`, a fatal signal) but loses the claim to
  an exec owner forwards its status (`Task::forced_exit`). The de-thread then exits the
  process with that status instead of completing the exec (`exit_if_forced`). The
  signal is stamped on the group only by the claim winner, so a race can no longer
  relabel an `exit_group(42)` as "killed by 9".
- Dying threads park for the owner (`exit_dying_thread`, and `exit()` when a foreign
  owner exists) instead of becoming Zombies themselves. A leader that turned itself
  Zombie mid-takeover was reaped by its CPU as a *process* exit. The parent got a bogus
  exit, and the image continued under the exec'ing thread's own pid.

## L5.5 — ALREADY FIXED (lane/execfd pid exchange)
`killmt exec_state` passes on main as well. After a non-leader execve the image keeps
the pid/tgid (`/proc/self/status`), ppid, cwd, umask, SIG_IGN, a handler reset to
SIG_DFL, the pending alarm, the inherited fds (also via `/proc/self/fd` + readlink),
loses the cloexec fd, and its new fds work from a new thread.

## Tests (killmt, `killmt N proclife`, added to the regress suite)
- `exit_settle`: 128 MiB child, three kill shapes, parent spins on WNOHANG and then
  checks pipe EOF and free memory. main: FAIL ("waitable with 128 MiB still
  allocated", and "fd table still open" before the rebase). Branch: PASS 60/60.
- `exec_kill`: SIGKILL (kill / tgkill at the leader, ± parked sibling) jittered across
  the exec. main: FAIL. Branch: PASS 100/100 (killed before the image 45, after 55).
- `exec_exit`: the leader's exit_group races the exec. Never reproduced the bug on
  main (the exit wins only ~4/100 times). PASS on both.
- `exec_state`: see L5.5.
Branch: full `killmt 50` battery 17/17. regress 37/38 before the rebase: polltest
`poll_linux_semantics` nval failed, which is the POLLNVAL case that lane/epollabi fixed
on main. After the rebase onto main da51b45e: regress 40/40 PASS (aarch64/HVF, -smp 4),
`killmt 40 proclife` 25 s.

## Open
- A non-leader execve whose leader exited *earlier* (pthread_exit, then exec) still
  "promotes in place". The image runs under the exec'ing thread's pid, and the old
  leader's exit record is reported. Fixing it needs the caller to take pid == tgid,
  including a reply port owned by the tgid (ipc). Not done.
- `/proc/<pid>/fd` does not exist (only `/proc/self/fd`).
- A group whose leader has exited keeps no leader slot, so leader-only state
  (sigaction table, creds) has no home for the remaining threads. This predates the lane.
