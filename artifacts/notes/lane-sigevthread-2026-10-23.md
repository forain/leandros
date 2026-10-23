# lane/sigevthread 2026-10-23 (L10.9) — FIXED

## SIGEV_THREAD timers
relibc master 90e7ce78 runs SIGEV_THREAD callbacks on a helper thread (musl style); a follow-up
commit fixes the `sigset_t` import path and the `pthread_attr_t` cfg so it builds for leandros.
timertest `sigev_thread_notify_thread` and `sigev_thread_callback` PASS (aarch64).

## Session leader exit SIGHUPs the foreground group
Before: jobtest `session_leader_exit_hup` FAILED at step 6 (no hangup at all).
Fix: `sched::register_session_leader_exit_hook` (called from `exit` when the group's last thread
leaves); the kernel registers `tty_server::pty::session_leader_exited(sid=tgid)`, which detaches the
pty's session and sends SIGHUP then SIGCONT to its foreground group (non-VT ptys only).
Case is now in the default jobtest run.
ptytest `ctty_setsid` queried the master after the leader exited; that now (correctly, as on Linux)
sees no session, so the child stays alive until the parent has queried.

## Evidence (aarch64)
timertest 0 fails, jobtest 0 fails incl. session_leader_exit_hup, full regress: see merge commit.
