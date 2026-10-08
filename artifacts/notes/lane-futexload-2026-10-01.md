# lane/futexload: the FUTEX_WAIT + SA_RESTART race under load (2026-10-01)

Branch `lane/futexload` from `origin/main` cbe9947. Head `323c9a3`. Tests only, no kernel change.
Machines: Mac aarch64/HVF; desktop x86_64/KVM. The x86_64 images were built on the Mac and copied to the desktop's `/tmp` (tmpfs), because the desktop's samsung970pro512 disk was full (see below).

## Verdict: REFUTED on current main. 0 EINTR in 200+ loaded iterations on both arches.

The TODO.md "Open work (2026-09-24 reconciliation)" first bullet is **stale**. `lane/futexsig` (2f7d101/8002838) and `lane/futexrem` (a3f0353) were merged through integ-wave-0924. They already contain the fix and the correct semantics. Nobody had updated the bullet.

1. **The expected result in the premise does not match Linux.** The task expects a timed FUTEX_WAIT interrupted by an SA_RESTART handler to restart and return ETIMEDOUT at about 350 ms. Linux never does that. A timed futex wait returns -ERESTART_RESTARTBLOCK, and that becomes EINTR whenever a handler runs, with or without SA_RESTART. I re-measured this on the desktop host (Linux 7.2.7). The test is `futextimed.c`: 350 ms timeout, tgkill at 50 ms, 20 runs per row.

   | signal at 50 ms | EINTR | ETIMEDOUT | mean elapsed |
   |---|---|---|---|
   | handler + SA_RESTART | 20 | 0 | 50 ms |
   | handler, no SA_RESTART | 20 | 0 | 50 ms |
   | SIGCHLD at SIG_DFL (ignored) | 0 | 20 | 350 ms |

   LeandrOS main already behaves this way. `sys_futex` gives an untimed wait ERESTARTSYS and a timed wait `futex_wait_timed`, which returns ERESTART_RESTARTBLOCK.

2. **Where sigmisc's "10–20 % EINTR" came from.** It was a SIGCHLD handler without SA_RESTART, left installed by earlier sigtest cases. That signal landed on the correctly restarted wait. futexsig showed this, and I reproduced it on current main with `sigtest futexab 100` under load:

   | SIGCHLD disposition | aarch64 | x86_64 |
   |---|---|---|
   | handler (no SA_RESTART) | 45–51 % EINTR | 10–40 % EINTR |
   | SIG_DFL | **0/100 EINTR** | **0/100 EINTR** |

   The EINTR with a handler is the expected answer, and Linux gives the same.

## Measurements on main's kernel (release builds)
Load on both arches was 6× `yes >/dev/null` in the guest, confirmed by `jobs`: 6 Running on 4 vCPUs. Extra host load:
- Mac: other lanes' builds, load average 6 to 14.
- Desktop: 24× host `yes` that I started and killed myself.

| `sigtest futex N` under load | aarch64/HVF | x86_64/KVM |
|---|---|---|
| restart_stress (untimed SA_RESTART → 0 at the genuine wake) | 200/200 + 2×100 + 3×20, **0 EINTR** | 200/200 + 2×100 + 20, **0 EINTR** |
| timed_stress, handler → EINTR at the signal (new case) | 200/200 + 2×100 | 200/200 + 2×100 |
| timed_stress, ignored SIGCHLD → ETIMEDOUT ≥ 350 ms (new case) | 200/200 + 2×100 | 200/200 + 2×100 |
| wake_race lost / bad | 0 / 0 | 0 / 0 |

One false failure turned up, on x86_64 in a full `sigtest` under load before the test fix: `restart_stress[6] r=0 el_us=84979`. This was a test artifact, not EINTR. The helper's 20+80 ms sleeps start inside `pthread_create`, but `t0` was read after it returned. Under load the main thread lost the CPU for about 15 ms in that gap. Fixed in 323c9a3:
- `t0` is now taken before `pthread_create`.
- A restart now passes only if the futex word is already 8 when the wait returns. This catches a spurious early 0 directly, rather than inferring it from elapsed time.

All runs after the fix were clean.

## Changes (userland/sigtest/src/main.rs only)
- New `futex_timed_signal_stress`, in `sigtest` (5 iterations) and `sigtest futex N` (N iterations). It is strict: no flaky mode, any other result fails. It checks:
  - a handled signal gives EINTR at the signal, with the handler run once;
  - an ignored signal gives ETIMEDOUT at ≥ 350 ms, with no handler.
- `futex_signal_round_op` starts its clock before the helper exists, and records `FWORD` at return (`FW_AT_RETURN`). `futex_wait_restart_stress` requires `r==0 && word==8 && el>=100ms && handler==1`.
- The futex cases were already strict on main (`report(name, good == iters)`). Nothing in sigtest was marked known-flaky any more.

## Regressions: all RC=0, 0 WDOG/panic lines in every serial log
- aarch64/HVF: sigtest, polltest, timertest, pthreadtest, sigtest2, smpwaketest.
  - One earlier smpwaketest run failed only its latency gate: max 116–225 ms against a 100 ms threshold, lost=0. Host load average was 14.6 at the time, from other lanes' builds. A rerun at load average about 6 passed: max 10/3/20 ms. The kernel is identical to main.
- x86_64/KVM: sigtest, polltest, timertest, pthreadtest, sigtest2, smpwaketest, all RC=0.
- COSMIC session smoke:
  - aarch64/HVF, virgl: greeter → typed leandro's password → panel, clock and 3-icon dock over the Orion wallpaper.
  - x86_64/KVM, Venus+Zink, via `zinkbench.py`, which starts the session from serial: the same desktop at 1920×1080.
  - zinkbench recorded 0 `[DRMSTAT]` samples. Its stats parser no longer matches the current log; the session itself was fine.

## Housekeeping / for the orchestrator
- **The desktop's `/run/media/forain/samsung970pro512` hit 100 % (24 KiB free)** while my first x86_64 build was writing its f2fs image. Other lanes' builds there will fail with ENOSPC. I removed my worktree and outputs, which brought it back to 2.2 GB free. That is still very low. The biggest sibling dirs are mame 43G, leandros-artifacts 32G, coreutils 25G, brush 23G and old lane worktrees of 7–13G each (term20, wgpuapps, ctrlq, applets, integ, lazystack). I left them alone.
- Suggested TODO.md change: delete the first "Open work (2026-09-24)" bullet, or replace it with "CLOSED 2026-10-01 (lane/futexload): fixed by futexsig/futexrem; timed FUTEX_WAIT + handler = EINTR, as on Linux 7.2; 0 EINTR in 200 loaded iterations per arch".
- Scripts are in the session scratchpad and not committed: `guestrun.sh`, `futextimed.c`, `macsmoke.py`.
