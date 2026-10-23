# lane/pstty 2026-10-23 — FIXED (test bug, not a kernel bug)

pstest tty cases (stat tty_nr, tpgid, ps -o tty, ps -t) failed since the session-leader-exit
hook (sigevthread merge 823f837d). Root cause: the test made the session leader exec /bin/ps;
when ps exited, the leader (pid == sid) exited, so the hook correctly hung up the pty and
detached the session (Linux disassociate_ctty), leaving the sleeper with tty_nr=0, tpgid=-1.
The kernel hook is correct (matches only p.sid == exiting tgid). Fix: userland/ps/src/test.rs
test_tty keeps the leader alive as the sleeper and runs ps in a forked child.
Evidence: pstest/jobtest/ptytest all RC=0 on aarch64; regress green except pollnarrowtest
(flaked once, passed on rerun).
