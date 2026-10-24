# lane/killexec2 2026-10-24 — openbugs L5.3 (x86_64 killmt exec_kill "child not reaped", intermittent)

Status: FIXED (one window shown red and fixed; the original exec_kill hit was not reproduced in 1200+ iterations).

## Root causes (sched)
1. Dequeue/claim gap (signal.rs check_and_deliver_signals → lib.rs exit_group_signal). A default-kill signal was
   cleared from the pending set in one RUN_QUEUE hold, and the group-exit claim came in a later hold. If an execve
   de-thread claimed in between, it marked the thread group_killed. Its resched IPI then preempted the thread inside the gap
   (preempt_check on IRQ return), and the dispatch loop reaped it. The signal was gone from the pending set and had not been
   forwarded, so the exec'd image ran on. This fits L5.3: exec_kill sends SIGKILL with kill() at it 46. That SIGKILL picks
   the Blocked parked sibling, which wakes, dequeues it and enters the gap.
   Fix: `claim_fatal_signal_locked` runs in the dequeue hold. Either this thread owns the group exit (a racing exec loses
   its claim and dies), or the owner is a non-leader exec and the kill is set as its forced_exit before the lock drops.
2. A SIG_DFL terminate/core signal sent with tgkill at a sibling the de-thread had already marked group_killed (Kicking, still
   on CPU) was dropped while tgkill returned 0. `sigkill_target` now redirects such a signal (unblocked, SIG_DFL, target
   group_killed) to the group-exit owner, the way it already handled SIGKILL. Linux complete_signal group-exits here.

## Guards (userland/killmt proclife, both in `killmt` / `killmt proclife`, min 300 iters)
exec_tidkill / exec_tidterm: 6 spinners + 2 parked threads report their tids. The exec'ing thread delays execve by
(it%64)*16 us after its marker. The parent tgkill()s every sibling, and once any tgkill succeeds the child must die by that signal.
- Unfixed, x86_64 TCG: exec_tidterm FAIL in 3/3 runs (it 2, it 155, it 253: "survived 1 of 8 tgkill(15)", 1 task, R).
  exec_tidkill stayed green unfixed (5×500): gap 1 is too narrow for this guard to hit. Its fix rests on the code
  argument above. Plain exec_kill 2×500 and full killmt ×2 were also green unfixed.
- exec_kill failure path now dumps /proc/<pid>/status fields, task list and stat.

## Results (fixed, ccb719ed)
- x86_64 TCG: killmt 500 exec_kill ×3, exec_tidterm 500 ×2, exec_tidkill 500, full killmt ×3: all PASS.
- aarch64: full regress + full killmt + exec_tidterm 300: PASS.

## WCOREDUMP: not set, deliberately
This kernel has no core-dump path. The policy in ExitStatus::wait_status (lib.rs) and task.rs CLD_DUMPED never sets
0x80, and that matches Linux, which sets it only when a core file was actually written (coredump_finish). forced_exit goes
through exit_group_signal, so its status is the same as a direct signal death.

## Open
- The synchronous-fault exits (SIGSEGV via exit_group_signal straight from the fault path, rseq) still claim in a later hold.
  Same gap in principle, not covered.
