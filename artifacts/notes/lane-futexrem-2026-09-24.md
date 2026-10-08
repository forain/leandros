# lane/futexrem — timed waits resume the remainder after a no-handler restart (2026-09-27)

Branch `lane/futexrem` from `origin/integ-wave-0924` a575c908. Head `d4b765b`. Mac aarch64/HVF, desktop x86_64/KVM (`--no-gpu` headless only).

## Verdict: FIXED (premise confirmed and measured, plus the same bug class in nanosleep/ppoll/pselect6)

Test: a forked waiter does a 500 ms relative wait. The parent sends SIGSTOP at 150 ms and SIGCONT at 250 ms, then repeats with stop at 300 ms and SIGCONT at 700 ms.
Linux behaviour: the restart keeps the original deadline, so the waits end at about 500 ms and 700 ms.

| aarch64/HVF, stop/cont | base a575c908 | lane |
|---|---|---|
| futex 150/250 | ETIMEDOUT at **753 ms** | ETIMEDOUT at 501 ms |
| futex 300/700 | ETIMEDOUT at **1201 ms** | ETIMEDOUT at 700 ms |
| nanosleep 150/250 | **EINTR** at 250 ms | 0 at 500 ms |
| ppoll / pselect6 150/250 | **EINTR** at 251 / 252 ms | 0 at 500 ms |
| nanosleep / ppoll / pselect6 300/700 | **EINTR** at about 700 ms | 0 at 700 to 701 ms |

x86_64/KVM gives the same lane numbers: 500 ms and 700 ms, all ok. The base build was only measured on aarch64. The old code paths are shared between the arches.

## Change
- A per-thread `Task::restart_block: [u64; 7]` (`sched/src/task.rs`), set and taken through `sched::set_restart_block` / `take_restart_block` (`sched/src/lib.rs`).
- A new `restart_syscall` (x86_64 219, aarch64 128) resumes it. The dispatcher maps the new internal -ERESTART_RESTARTBLOCK (-516) to a NOHAND restart whose rewind targets `restart_syscall`. `rewind_syscall` now also writes x8 on aarch64 (Linux does the same).
- Timed FUTEX_WAIT, relative nanosleep/clock_nanosleep, poll/ppoll and select/pselect6 store their ABSOLUTE deadline and return -516 when interrupted.
  - When a handler runs, the result is still EINTR, as before.
  - When no handler runs (stop/continue, or a sibling took the signal), the call restarts on the same deadline.
  - A futex restart whose deadline has already passed does the value check first and returns EAGAIN or ETIMEDOUT without parking.
- Absolute clock_nanosleep returns ERESTARTNOHAND, so it restarts with the same absolute time.
- poll/ppoll and select bodies were split into `poll_wait` / `select_wait`, which take a deadline.
- Behaviour change: nanosleep, poll and select no longer fail with EINTR after a stop/continue. This matches Linux (since 2.6.24). select still does not write the remaining time back to the user timeval (Linux does). That was already the case.

## Not changed (Linux also gives EINTR here, so they match)
epoll_wait/epoll_pwait2, rt_sigtimedwait, sigsuspend. FUTEX_WAIT_BITSET still gives a spurious 0 on a signal. Its deadline is absolute, and every libc here re-checks it.
ppoll/pselect6's sigmask argument is still ignored (this predates the lane).

## Tests (release builds): all RC=0 on both arches
sigtest (with the new `timed_wait_stop_resumes_remainder` case, also available as `sigtest stoprem`), timertest, pthreadtest, polltest, sigtest2, jobtest, smpwaketest. The serial logs show 0 WDOG/panic lines.
x86_64 `poll` (nr 7) goes through the same `poll_wait`. It is covered by polltest, but the stop case has no test of its own.

## Files
kernel/src/syscall.rs, sched/src/{task,lib,signal}.rs, userland/sigtest/src/main.rs. Shared with futexsig/sigmisc: the `signal.rs` rewind and the syscall.rs futex, poll and select code.
