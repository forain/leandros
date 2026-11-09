# lane/nspidinit (P2) - pid namespace init semantics + remaining by-pid sites - 2026-11-08

Status: FIXED (see test evidence).

## Delivered
- pidns.rs (append-only): `arg` (0 stays 0, else from_cur), `out` (kernel pid -> caller view),
  `init_drops` (ns init without a handler drops user-sent signals; same-ns senders drop even
  SIGKILL/SIGSTOP, ancestors only SIGKILL/SIGSTOP pass), `reaper_for` (nearest live ns init up the
  chain, else global init; used by `reparent_children`), `zap` (init death: SIGKILL every live process
  in the ns subtree, bounded 5 s yield loop, then normal reparent; `on_process_exit` calls it after
  `set_pid_dead`, so later forks are ENOMEM).
- cred.rs: `init_drops` in kill_check/kill_thread/pgrp/-1 (a dropped signal returns 0).
- syscall.rs: SETSID/GETSID/GETPGRP/setpgid/getpgid views and inputs, get/setpriority (PROCESS, PGRP),
  sched_getparam/setscheduler/setparam/get+setaffinity, prlimit64, capget, pidfd_open (all via `arg`),
  pidfd_send_signal (kernel pid -> caller view before sys_kill; invisible = ESRCH).
- servers/net: SO_PEERCRED pid -> `to_v(pidns_of(caller), pid)` (outside peer = 0).
- nstest: 9 cases (init_ignores_inside, init_kill_outside, init_handler, orphan_to_init,
  init_death_kills, kill_minus1_scoped, sid_pgid, byid_calls, peercred).

## Evidence (aarch64, Mac HVF)
- Before the kernel change: 7 of 9 new cases FAIL (init_handler and kill_minus1_scoped already pass,
  they pin existing behaviour).
- After: nstest, sigtest, jobtest, waittest, killmt all PASS. Bug found by byid_calls: pidfd_send_signal
  handed a kernel pid to sys_kill, which now translates its input (fixed above).
- Full regress on the box: see merge commit message / report.

## Notes
- sched_getparam / SCHED_GETSCHEDULER ignore the pid except for the visibility check.
- No logging in zap on timeout (sched has no log macro); a timeout just proceeds to the reparent.
- /proc untouched (lane nspidproc).
