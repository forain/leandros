# lane/pidmisc 2026-11-08 (ffsbreview2 G5, G6) - FIXED

- G5: `NiceTarget::matches` (sched/src/lib.rs) now requires the task to be visible in the caller's pid ns
  (`is_ancestor_or_eq(pidns::cur(), t.pidns)`); covers set/getpriority for PRIO_USER and PRIO_PGRP (who already translated by `nice_who`).
- G6: timer_create SIGEV_THREAD_ID tid goes through `pidns::from_cur` (kernel/src/syscall.rs); the same-thread-group check stays.
  TIOCGPGRP/TIOCSPGRP/TIOCGSID on the console (servers/tty/src/lib.rs) and pty (pty.rs): out() on reads (0 when invisible),
  arg() on TIOCSPGRP (ESRCH when not in view, then EPERM if not in the session). pty translation is done outside the PTYS lock
  (RUN_QUEUE inversion).
- Tests (userland/nstest): prio_user_scoped, timer_thread_id, tty_pgrp_view. Unfixed build: all three FAIL
  (outer nice changed; timer_create EINVAL for tid 1; TIOCGSID EINVAL/raw ids). Fixed: nstest, jobtest, timertest PASS aarch64.
- Note: unfixed TIOCGSID on a pty slave returned EINVAL (errno 22) in the pid ns, now OK.
