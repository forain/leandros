# lane/killexec 2026-10-24 — openbugs L5.1 (killmt proclife exec_kill / exec_exit on x86_64)

Status: FIXED.

## Root cause
Both failures are one bug. integ7's diagnostics: `wait_bounded(183)` timed out with /proc/183/stat
state R, 1 task, utime 495 ticks, i.e. the exec'd `--exec-spin` image spinning, alive, after its SIGKILL.
The test then killed it again without reaping; that late zombie is what exec_exit's
"second wait returned 183 status 0x9" picked up (exec_exit itself was fine).

`post_signal_process` puts a process-directed SIGKILL on the *chosen* thread's private
`signal_pending`, preferring a Blocked thread (exec_kill's parked sibling, it % 4 >= 2). If the
execve de-thread (`kill_next_group_member_except`) reaped that sibling off-CPU before it was dispatched
to act on the kill, the SIGKILL was dropped along with it. The exec'ing thread became the sole thread and
the new image ran on. `sigkill_target` only redirects kills that arrive *after* `group_exit_owner` is set,
and `take_over_leader` only merges the *leader's* pending set. Slow TCG dispatch widens the window, which
is why x86_64 (TCG) hit it at it 6 every time.

## Fix (sched/src/lib.rs, kill_next_group_member_except)
When a member being reaped has SIGKILL pending, record it on the caller as `forced_exit` (signo 9).
`dethread_current_group` -> `exit_if_forced` then fails the exec with SIGKILL, which is what Linux does
(complete_signal makes a group exit and de_thread bails out). A dying exit_group owner never reads
forced_exit, so plain group exits are unchanged.

## Tests (userland/killmt/src/proclife.rs)
- New `exec_sibkill` mode (in `proclife`): 8 spinners plus a futex-parked thread; the leader calls
  kill(getpid(), SIGKILL) and then execve. It must die by SIGKILL within 5 s.
  Unfixed main, x86_64: FAIL at it 0 ("child 39 survived its SIGKILL (image ran: true)").
  Fixed: 40/40 and 100/100 (image started 0).
- exec_kill now reaps its child on the "not reaped" path, so a failure no longer spills into exec_exit.

## Results
- x86_64 TCG: exec_sibkill 40, then `killmt 40 proclife` twice, then full `killmt`: all PASS.
- aarch64: see below.
- aarch64 TCG: exec_sibkill 40, `killmt 40 proclife`, full `killmt`: PASS. Full regress: PASS except
  host:nettag2test icmp_unbound once (known nettag2test flake), which passed on a rerun under the lock.

## Open
- Only SIGKILL is forwarded. A thread-directed *other* fatal signal (tgkill of a default-terminate signal at a
  sibling the de-thread then reaps) is still dropped. Linux would group-exit at send time. Rare, not covered.
- Build note: relibc master eb01acd9 (another lane) briefly failed to compile (`syscall!` in sys_uio); fixed by 21c305aa.
