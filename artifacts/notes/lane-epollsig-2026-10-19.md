# lane epollsig — 2026-10-19

Bug list items L2.4, L2.3, L2.1 (openbugs-2026-10-19.md, group 2 items 4, 3, 1).

## L2.4 epoll_pwait / epoll_pwait2 ignored the sigmask — FIXED

Both were routed to the plain `epoll_wait` body with only four arguments.
They now take `sigmask`/`sigsetsize` (a4/a5) and run the wait inside
`masked_timed_wait`, the ppoll/pselect6 frame: the temporary mask is
installed for the wait; EINTR parks the caller's mask with
`stash_sigsuspend_mask`, so the signal is delivered under the temporary mask
and the handler frame restores the old one; any other result restores it at
once (a signal only the temporary mask unblocked stays pending). NULL sigmask
= no change (sigsetsize unchecked, as Linux); sigsetsize != 8 = EINVAL. No
timeout write-back (Linux has none for epoll).

Test: `sigtest pollmask` / `test_ppoll_pselect_sigmask` now loops over
ppoll, pselect6, epoll_pwait, epoll_pwait2 (arrives, pending, ready+pending,
timeout, NULL mask; write-back checks only for the first two).

## L2.3 signalfd — FIXED (two parts)

1. **Re-mask on the open file description.** `VnodeKind::SignalFd` now
   carries its ofd id; `signalfd4(fd, mask)` stores the new mask in
   `SIGFD_MASKS` (servers/vfs, keyed by ofd id, dead ids swept on insert,
   entry cleared when a recycled id makes a new signalfd). Read and poll use
   `signalfd_mask(ofd, creation_mask)`. dup / fork / SCM_RIGHTS copies see the
   re-mask. Test `signalfd_remask_reaches_copies` (dup + fork child).
2. **Process-directed signals queue on the shared set.** `post_signal_process`
   used to drop a kill(pid)/SIGCHLD straight into the chosen thread's private
   `signal_pending`, so another thread's signalfd (or sigtimedwait) never saw
   it, and it was stranded if that thread then blocked it. It now always
   parks on the leader's `shared_signal_pending` (Linux `shared_pending`),
   kicks the group's sigfast hints and wakes the chosen thread; the existing
   return-to-user claim delivers it. SIGKILL/SIGSTOP still go directly to the
   chosen thread (a stale mask must never hold them on the shared set); if the
   leader slot is gone it also falls back to the chosen thread.
   Test `signalfd_sees_process_signal_of_other_thread`: the only unmasked
   thread sits in an (uninterruptible) vfork wait while the masked main
   thread reads its signalfd — deterministic.

## L2.1 TFD_TIMER_CANCEL_ON_SET — ALREADY FIXED

`60c17ec8` (lane nowake, on main): `step_realtime_ns` →
`vfs::timerfd_clock_was_set`; ECANCELED on read; `timertest
timerfd_cancel_on_set`. Nothing to do.

## Test evidence (aarch64 / HVF)

- After: `sigtest pollmask`, `sigtest sigfd`, full `sigtest`, `epolltest`,
  `timertest` all RC=0; ep_pwait/ep_pwt2 "arrives" EINTR at 60 ms, handler
  under the temporary mask; `remask=3 dup=12 usr1_gone=1 fork_child_status=0`;
  `signo=10 handler_runs=0`. Regress 37/37 green.
- Before (branch base 252246c3 kernel + new tests): pollmask FAIL
  (`ep_pwait arrives r=0 el_ms=1000 BAD`, same for pending and for
  epoll_pwait2); `sigtest sigfd` RC=140 (the SIGUSR2 the dup could not read
  stayed pending and killed the process on unblock).

## Open

- A re-mask wakes only the caller's tgid signal tag; a forked holder's
  pollers parked on its own tgid tag see the wider mask at their next probe.
- Standard-signal coalescing: a thread-directed instance pending on the
  leader plus a process-directed one of the same signal now share one payload
  slot (first writer wins), as the all-masked path always did.
