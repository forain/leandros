# lane/affinity 2026-10-23 (L10.8 follow-ups of lane/netrest)

FIXED
- Placement honours sched_setaffinity. `SchedSlot.aff` mirrors `Task.affinity` (set in `enqueue`, refreshed by
  `RunQueue::set_affinity`). Honoured in: new-task `least_loaded`, wake `select_cpu`/sync-wake preference,
  EEVDF pick (`allowed` filter next to the quiesce filter), `most_lagging(dst)` for tick balance / new-idle steal,
  and `cpuq::switch_back` (a task requeued on a CPU its mask excludes goes to an allowed CPU's queue via try_lock,
  else stays queued and the allowed CPUs are kicked to steal it). `reaffine`: queued-on-wrong-CPU is moved at once,
  running-on-wrong-CPU gets a preempt so its switch-back migrates it. A task pinning itself off its CPU yields
  in `sys_sched_setaffinity` so it is already on an allowed CPU when the syscall returns.
- getcpu already returned the real CPU id and node 0 (sampled 1000x in the test: not "always 0"); now guarded.
- relibc (commit 14f69773): sched_getcpu, sched_setaffinity, sched_getaffinity, waitid (+ P_* constants).

EVIDENCE (aarch64, QEMU HVF, several CPUs)
- syscalltest affinity_pin_cpu0 on the unfixed kernel: FAIL 1000/1000 samples off CPU 0 (all on CPU 1);
  after the fix PASS, affinity_pin_cpu1 / affinity_pin_other (blocked child pinned by parent, woken) PASS.
- waittest case j relibc_waitid_getcpu PASS (waitid(P_PID, WEXITED) siginfo, sched_getcpu, set/getaffinity).
- regress: everything green except pstest (tty_nr/tpgid/ps tty cases; unrelated, intermittent in other lanes'
  logs too, e.g. tests-sevr2); schedstress, pthreadtest, waittest, syscalltest pass.

OPEN
- A task whose mask excludes the CPU it is queued on while all allowed CPUs are busy waits for their balance/steal
  (no direct push when try_lock fails). x86_64 not run (per-lane policy).
