# lane pidfdflake 2026-11-15 — syscalltest pidfd_wake_conmon flake

Cause: a test bug, not a pidfd/poll/exit bug.
- Every recorded failure DETAIL tripped only `dt < 480` (dt=445, 472 ms):
  revents was exactly POLLIN and waitid already reported the exit, so the
  pidfd became readable at the right time. dt was taken AFTER fork() returned
  in the parent; when the parent resumes late (child-first scheduling on a
  loaded box), the child's 300+200 ms life looks 30-55 ms short. Full-regress
  context only matters as box/guest load, which a few CPU hogs did not mimic.
- Second test bug (likely the RC=139 seen once in lane utmpread, x86_64):
  pw_signaller checked pw_tid_ok then called pthread_kill, racing with the
  watcher's exit+join (pthread_kill on a freed pthread_t).

Fix (userland/syscalltest/syscalltest.c):
- time from before fork (strict bound: the child cannot exit before +500 ms
  unless its own sleeps are short); readiness is checked against exit times
  the child and grandchild write to a MAP_SHARED page; failure DETAIL prints
  the full timeline (fork return latency, nanosleep rc/errno, grandchild fork,
  exits, pid). A non-failing "note: parent resumed N ms after fork" line marks
  runs where the old check would have fired.
- signaller uses tgkill(getpid(), kernel tid); fatal SIGSEGV/SIGBUS/SIGILL/
  SIGFPE prints "syscalltest: FATAL signal ... after case <name>".

Evidence: 12 full box regress runs on 5bdd919b (6 aarch64 TCG, 6 x86_64 KVM)
pidfd_wake_conmon clean, no note fired (the old trigger is rare). Not a
fail-first reproduction: the old failure was not reproduced in 12 runs; the
diagnosis rests on the two recorded DETAILs. If the new check ever fails, the
timeline names the culprit. Unrelated: brtest nftnat_masq_ok/fwd_udp/
fwd_icmperr failed together in 2 of 6 aarch64 runs (TODO item added).
Open question: the old DETAIL showed si_pid=4/5 although global pids are
never reused; the new DETAIL prints pid= next to si_pid to check.
