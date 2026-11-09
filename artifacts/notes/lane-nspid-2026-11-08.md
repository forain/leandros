# lane/nspid (P1) - real PID namespaces, core - 2026-11-08

Status: see bottom for test evidence.

## Delivered (2 commits)
- a: `sched/src/pidns.rs` (view/from_v/to_v/self_v/own_v, unshare_pid, fork_target, claim_init),
  `ns.rs` append (new_pidns, PID_INIT, PID_FLAGS, pid_dead), `Task.pidns/pidns_child`,
  per-CPU `CURRENT_PIDNS` (published from a new `m_pidns` slot-mirror field that fits the
  padding after `m_tgid`), unshare(NEWPID), clone(NEWPID) (rejects THREAD/SIGHAND/VM/set_tid,
  CAP_SYS_ADMIN in own userns unless NEWUSER is given), fork placement + ENOMEM after init death
  (flag set at the top of `reparent_children`), first child claims init, getpid/gettid/
  set_tid_address/getppid views, CLONE_CHILD_SETTID view for the vfork-style arm.
- b: kill/tkill/tgkill/rt_sigqueueinfo/rt_tgsigqueueinfo inputs via from_v; cred.rs visibility
  (invisible target = ESRCH, pgrp/-1 skip invisible, -1 also skips the caller ns init) and
  send-time `si_pid` translation for SI_USER/SI_TKILL; wait4/waitid inputs (ECHILD);
  futex LOCK_PI/UNLOCK_PI words use the caller's own view (waiter handoff writes the waiter's
  own view); robust-list walk compares the dying thread's own view; get_robust_list input.
- nstest: 15 new cases (ff_probe ... fork_enomem, plus siginfo_outside). NEWPID removed from the
  old "must be EINVAL" lists.

## Deviations / notes
- NEWPID with CLONE_VM (vfork-style) is EINVAL (like NEWUSER); no namespace plumbing there.
- user-supplied si_pid in rt_sigqueueinfo is passed through unchanged (only kernel-set
  SI_USER/SI_TKILL pids are translated).
- Not in P1 (P2): init signal protection, reaper_for, zap on init death, sid/pgid/priority/
  sched_*/prlimit/capget/pidfd_open/SO_PEERCRED sites. P3: /proc.

## relibc pid cache (design risk 5)
`getpid()` is a plain syscall (no cache). relibc caches `os_tid` per pthread
(`Sys::current_os_tid()` at thread start, `rlct_clone` return for new threads): a thread's
os_tid is the kernel tid and equals the in-namespace number except for the ns init's main
thread, whose cached value (read at process start by gettid) is 1 only if gettid ran inside
the namespace. A fork() child keeps its parent's stale os_tid (same as Linux/musl for a raw
clone(NEWPID) child); pthread_kill(pthread_self()) in a fork()ed init may target the wrong
tid. No fix applied; P2/P3 follow-up if Firefox needs it.
