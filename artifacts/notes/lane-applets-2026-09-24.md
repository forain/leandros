# lane/applets: missing panel/dock buttons on x86_64/KVM Venus (2026-09-26)

Branch `lane/applets` (from origin/integ-wave-0924 f0879c5), fix `a19267d`. Pushed.
Desktop worktrees: `/run/media/forain/samsung970pro512/leandros-siblings/leandros-applets` (full build of d3b6494),
`.../leandros-applets-k` (kernel-only variants). Scratch and all session data: `/run/media/forain/samsung970pro512/applets-tmp/`
(scripts in `~/applets/` on the desktop: sess.sh, loop.sh, chk.py screenshot checker, ctrlt.py Ctrl-T dump grabber).

## Symptom and state
The affected processes are cosmic-panel-button instances: Workspaces, Applications, launcher, workspaces overview and app library all run from one binary.
They are **alive but hung**. They did not crash and none was skipped at spawn. greetd.log shows the same 64 startup lines for good and bad instances.
Ctrl-T shows the stuck main thread `Blocked futex=00007FFFFFFF7A64=0` (or `0x1416A10=0`, `...B9A4=0`) inside libwayland-client
`read_events` → musl `pthread_cond_wait`, whose waiter barrier word sits on the waiter's stack. The word is already 0: the signal happened and the wake was lost.

## Root cause
The kernel bug is in `sched/src/futex.rs`. The futex table was keyed by virtual address only, across ALL processes.
Instances of the same binary use identical stack, heap and libc-static addresses, so one process's FUTEX_WAKE(n=1) could claim another process's waiter.
Diagnostic kernel (`legacy2`, same keys, logs cross-tgid claims into the Ctrl-T dump): **33–50 cross-process private claims per session**.
Most were on musl's static locks (0x300B9820, 0x300BBFC8). In M4 they were also on `0x1416A10` (waker 298 → waiter 299), and pid 298 then sat blocked on 0x1416A10 with value 0.
The bug predates this wave (futex.rs is unchanged since d16c563). lane/cowtlb (46a7279) exposes it by making fork/exec fast, so the five instances start in lockstep.

## Fix (a19267d)
A private waiter matches only wakes from its own tgid, which matches Linux's (mm, addr) key. Non-private waiters on both sides still match by address.
The kernel mutex.rs and the clear_child_tid wake depend on that.

## Session counts (x86_64/KVM --venus, login as leandro, ~100–150 s)
| kernel | bad/total |
|---|---|
| d3b6494 (incl. diag-only variants) | 6/16 |
| d3b6494 − cowtlb | 0/5 |
| d16c563 kernel, new userspace | 0/3 |
| d3b6494 + fix (tested variant) | 0/6 |
| final commit | 0/3 |

Tests with the final kernel all passed with RC=0. x86_64/KVM: pthreadtest, smpwaketest, racetest, forktest and killmt 20.
aarch64 on the desktop's **TCG** (no HVF there): pthreadtest, smpwaketest (lost=0), racetest and forktest.
Shared files touched: `sched/src/futex.rs`, `sched/src/lib.rs` (re-export only), and `kernel/src/syscall.rs` (sys_futex passes FUTEX_PRIVATE_FLAG).
