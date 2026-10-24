# lane/fatalsig 2026-10-24 — openbugs L5.2 (fatal non-SIGKILL signal on a reaped exec sibling)

Status: FIXED.

## Change
sched/src/lib.rs `kill_next_group_member_except`: killexec forwarded only a pending SIGKILL. Now the reaped member's
unblocked pending set is scanned; SIGKILL first, else the lowest signal whose action on the group leader is SIG_DFL and
whose default is terminate/core (new `signal::default_terminates`: not default-ignore, not stop). It is recorded as the
caller's `forced_exit` (exit status 128+sig), so `exit_if_forced` fails the exec with that signal. Blocked, handled,
SIG_IGN, stop and default-ignore signals stay dropped with the thread (Linux: they were thread-directed).
Core flag is not set (forced_exit carries only signo/status).

## Guard
userland/killmt proclife `exec_sibterm`: tgkill(parked sibling, SIGTERM) then execve; parent must see WTERMSIG==SIGTERM.
Red on main (aarch64, 20 iters): FAIL. Fixed: `killmt 40 exec_sibterm`, `killmt 40 proclife`, full `killmt` PASS on aarch64.

x86_64 TCG: `killmt 40 proclife` PASS; full killmt PASS on rerun (first run once failed exec_kill it 46 'not reaped' + exec_exit follow-on: intermittent, 100x exec_kill passes after; unconfirmed whether pre-existing). aarch64 full regress PASS.
