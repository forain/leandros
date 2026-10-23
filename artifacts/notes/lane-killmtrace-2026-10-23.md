# lane killmtrace 2026-10-23 — intermittent /bin/killmt proclife failures

Verdict: TEST BUG (timing under host load). No kernel race found or needed.

## Evidence
- The X0 failures carry the process's *correct final* status: exec_leaderless
  `0x2a00` (exit 42 = the exec'd image's success exit), leaderless_exit `0x700`
  (exit 7 = the worker's `_exit(7)` after writing OK). Those statuses only exist
  after the worker finished every check, i.e. the whole process legitimately
  completed. The parent only spins 10 ms before its WNOHANG probe while the
  worker spins 20 ms, so a parent descheduled for ~10+ ms under load sees a
  finished process. Kernel behaviour is right.
- Repro (aarch64/HVF, 6x `yes > /dev/null` in-guest): `killmt 40 proclife` x15
  -> exec_leaderless X0 4/15 runs (4/300 iterations).
- exec_exit under load (old code, 5x400 iterations): 1 failure "status 0x9,
  image marks []": the parent's fixed 500 ms probe expired before a slow exec
  wrote its mark, so it SIGKILLed a process whose exec was still in flight.
  The older "child not reaped" (it 18 / it 37) did not reproduce in 2000
  exec_exit + 2000 exec_kill iterations under load.
- The procfd1 `exec_state it 0 not reaped` was a separate FD_TABLE spin
  (procfd lane's WIP, [WDOG] lines), not this.

## Fix (userland/killmt only)
- leaderless_exit / exec_leaderless: the worker blocks on a CLOEXEC "go" pipe
  until the parent has done the X0 probe, so X0 is deterministic.
- exec_exit: wait for an outcome (reaped = exit won, or the image's mark =
  exec won) with a 20 s deadline instead of a fixed 500 ms; a process that does
  neither in 20 s is reported with its /proc stat (a real hang would show here).
  Post-kill reap bound 5 s -> 20 s.
- wait_bounded: on a >=1 s bound timeout or waitpid error, prints r/errno,
  elapsed, /proc/<pid>/stat and task list to stderr (diagnoses any future
  "not reaped" — kernel lost reap vs. slow).

## Results (aarch64/HVF, 6x `yes` hogs in-guest)
| | before | after |
|---|---|---|
| `killmt 40 proclife` runs with a FAIL | 4/15 (exec_leaderless X0) | 0/20 (120/120 mode runs) |
| gated X0 cases alone (20x exec_leaderless + 20x leaderless_exit) | — | 40/40 |
| exec_exit iterations | 1 fail / 2000 (0x9, no mark) | 0 / 2000 |
| exec_kill iterations (unchanged test) | 0 / 2000 | — |

Open: none in the kernel from this lane. If "not reaped" ever reappears,
the wait_bounded stderr line now says whether waitpid returned ECHILD (lost
reap) or the process was still alive (its /proc stat + threads).
