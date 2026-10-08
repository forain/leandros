# lane/futexsig — FUTEX_WAIT vs signals (2026-09-27)

Branch `lane/futexsig` from `origin/lane/sigmisc` 8183717. Mac worktree `~/code/leandros-futexsig` (aarch64/HVF),
desktop worktree `/run/media/forain/samsung970pro512/leandros-siblings/leandros-futexsig` (x86_64/KVM).
Scratch on the desktop: `/run/media/forain/samsung970pro512/futexsig-tmp/` (x1/x2 test runs, s1..s5 sessions).

## Verdict: the "SA_RESTART lost under load" premise is REFUTED. Two real futex bugs found near it and FIXED.

### Why sigmisc's stress case showed EINTR
The earlier sigtest cases leave a **SIGCHLD handler installed without SA_RESTART** (`handoff_chld_handler`).
The stress case forked a child that did `kill(parent, SIGALRM); _exit(0)`. SIGALRM restarted the wait correctly.
When the host was loaded, the child's exit came late, so its SIGCHLD landed on the *restarted* wait.
That handler has no SA_RESTART, so the result is EINTR. Linux does exactly the same.
That also explains the arch skew and the sensitivity to load and timing (the keying fix only moved timing).
Evidence:
- **Linux 7.0 host**, the same shape (`futexab.c` in scratch): SIGCHLD handler gives ok=0 eintr=20; SIG_DFL gives ok=20 eintr=0.
- **LeandrOS `sigtest futexab 50` under load**: with the handler, aarch64 gives eintr=44/50 and x86_64 eintr=37/50. With SIG_DFL, both give eintr=0.
- **Linux also never restarts a *timed* FUTEX_WAIT once a handler runs**, SA_RESTART or not (it returns -ERESTART_RESTARTBLOCK).
  I measured this on the host: timed with SA_RESTART gives EINTR at 50 ms; untimed gives a restart.
  sigmisc's test asserted the opposite.

### Real bugs fixed
1. **A claimed wake could be lost.** Take a waiter that a signal has woken but that has not run yet. A FUTEX_WAKE claims it (the waker counts 1).
   The old `r==0 && interrupted()` check then turned that claimed wake into ERESTARTSYS, and the restarted wait slept again.
   Now `futex_wait_intr` decides woken / timed out / interrupted inside the FUTEX_TABLE hold that frees the slot. A claimed wake always wins and returns 0.
2. **An ignored signal made the wait return early.** SIGCHLD at SIG_DFL or SIG_IGN woke a Blocked futex waiter, and the wait returned a spurious 0.
   It now goes round register / re-check / park again, which is Linux's `goto retry`.

sys_futex mapping: an untimed FUTEX_WAIT gives ERESTARTSYS. A timed one gives the new ERESTARTNOHAND (-514): EINTR if a handler runs, restart if none runs.
With ERESTARTNOHAND the full relative interval is re-armed; Linux re-arms only the remainder.
FUTEX_WAIT_BITSET is unchanged (a signal still gives a spurious 0).

## Files
- `sched/src/futex.rs`
- `sched/src/lib.rs` (re-export only)
- `sched/src/signal.rs`: `note_syscall_restart` takes a nohand flag (bit 63 of the record)
- `kernel/src/syscall.rs`: dispatcher and sys_futex
- `userland/sigtest`: new futex cases; `sigtest futex N` and `sigtest futexab N` modes

## Evidence (release builds)
Load was N× `yes >/dev/null` on the HOST (Mac 8, desktop 30), started by guesttests.sh around the loaded runs.
| | aarch64/HVF | x86_64/KVM |
|---|---|---|
| restart_stress (untimed SA_RESTART), loaded | 400/400 ok, 0 EINTR | 400/400 ok, 0 EINTR |
| wake_race lost / bad | 0 / 0 | 0 / 0 |
| sigtest, timertest, pthreadtest, polltest, sigtest2, smpwaketest | RC=0 | RC=0 |
| WDOG/panic lines | 0 | 0 |
| x86_64/KVM --venus session as leandro, 5 boots | — | 5/5: panel shows 2 words and the dock 3 icons (same as the applets baseline), 0 WDOG/panic |

Branch head: `2f7d101`. Only sigtest's expectations changed: timed FUTEX_WAIT with an SA_RESTART handler is now expected to give EINTR, as on Linux.
