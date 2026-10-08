# lane/pollmask — ppoll/pselect6 sigmask, select-family timeout write-back, FUTEX_WAIT_BITSET EINTR (2026-09-27)

Branch `lane/pollmask` from `origin/integ-wave-0924` da5a4f00. Head `5a95a189` (tests `b7023992`, kernel `5a95a189`).
Mac aarch64/HVF, desktop x86_64/KVM (`--no-gpu` headless, own worktree, since removed).

## Verdict: all four items FIXED (each premise confirmed on the base first)

Base = da5a4f00 kernel + the new sigtest, aarch64/HVF (`sigtest pollmask`):

| case | base | lane (aarch64 / x86_64) |
|---|---|---|
| ppoll/pselect6, SIGUSR1 blocked, mask unblocks it, signal at 60 ms, timeout 1 s | r=0 at **1000 ms**, handler not run | EINTR at 60 to 62 ms, handler ran under the temporary mask, caller's mask restored |
| same, signal pending before the call | r=0 at **1000 ms** | EINTR at 0 ms |
| fd ready + pending signal the temporary mask unblocks | r=1 | r=1, mask restored at once, signal still pending (Linux do_poll order) |
| remaining time written back (EINTR at 60 ms of 1 s) | **1000 ms** (untouched) | 938 to 939 ms |
| written back after a 40 ms timeout | **40 ms** | 0 |
| x86_64 select(2) timeval, EINTR / timeout | not measured on base | 939 ms / 0 |
| FUTEX_WAIT_BITSET + handler: untimed+SA_RESTART / untimed / timed+SA_RESTART / timed | r=**0** at 50 ms in all four | 0 at 300 ms (restarted to the real wake) / EINTR / EINTR / EINTR at 50 ms |
| FUTEX_WAIT_BITSET absolute 500 ms, stop/cont 150/250 and 300/700 | r=**0** at 251 / 701 ms (spurious) | ETIMEDOUT at 500 / 700 ms |
| ppoll with a temporary mask, stop/cont (restart reinstalls it) | new case | 0 at 500 / 700 ms, mask restored |
| x86_64 poll (nr 7), stop/cont | new case | 0 at 500 / 700 ms |

## Change
- `kernel/src/syscall.rs`
  - `sys_ppoll` takes `sigsetsize`, and `sys_select` takes pselect6's `{ss, ss_len}`. A size other than 8 gives EINVAL. SIGKILL and SIGSTOP are removed from the mask.
  - The shared `masked_timed_wait` (Linux `set_user_sigmask` + `poll_select_finish`) installs the mask for the wait.
    - Interrupted: the old mask goes to `saved_sigmask`, the sigsuspend mechanism. The handler runs under the temporary mask, and `uc_sigmask` restores the old one.
    - Any other result: the old mask is restored at once.
  - Timeout write-back: every return writes the remaining time back, except for a zero timeout. It uses a timeval for select and a timespec for pselect6/ppoll. poll(2) does not write back.
  - The restart block carries the mask and the write-back pointer. `restart_syscall` resumes through `poll_masked`/`select_masked`, which reinstalls the mask.
  - With nothing ready, a deliverable signal now wins over an expired deadline, as in Linux do_poll/do_select. A zero-timeout ppoll whose mask unblocks a pending signal therefore gives EINTR.
  - FUTEX_WAIT_BITSET goes through the same path as FUTEX_WAIT: timed uses `futex_wait_timed` (restart block), untimed gives ERESTARTSYS.
- `sched/src/signal.rs`: `saved_sigmask` used to be cleared whenever any signal was picked. Now only a handler frame clears it.
  - Latent bug this fixes: an ignored signal or a stop (a SIGSTOP of a masked ppoll, or of sigsuspend) cleared it without restoring it. That left the temporary mask installed for good.
- `sched/src/{task,lib}.rs`: `restart_block` grows from `[u64; 7]` to `[u64; 8]`.
- `userland/sigtest`
  - New `test_ppoll_pselect_sigmask`.
  - `test_futex_wait_bitset_unaffected` is replaced by `test_futex_wait_bitset_signal`. It used to assert the spurious 0.
  - stoprem gains `futex_bs`, `ppoll_msk` and (x86_64 only) `poll`.
  - New mode `sigtest pollmask`.

## Deliberate difference from Linux
ppoll/pselect6/select restart after stop/continue on the ORIGINAL absolute deadline (futexrem's restart block). Linux re-executes the call with the remaining time it wrote back when the signal arrived. The stopped interval therefore extends a Linux wait (500 ms wait, stop 150 to 250: Linux about 600 ms, here 500 ms). This is kept on purpose: it matches POSIX better, and the tests assert it.
A FUTEX_CLOCK_REALTIME BITSET deadline is converted to monotonic once. A clock step during a stop is not re-applied on the restart.

## libc impact
- relibc implements poll/ppoll/select/pselect on epoll, so its wrappers never reach these syscalls. Only raw callers (and musl/glibc-built binaries) do.
- relibc's `futex_wait` treats EINTR the same as 0 (`Waited`). Rust std retries on EINTR.
- musl and glibc pass a copy of the timeout to pselect, so their callers never see the write-back.

## Tests (release): all RC=0 on both arches
sigtest pollmask, sigtest, timertest, pthreadtest, polltest, sigtest2, jobtest, smpwaketest.
- aarch64/HVF: 0 WDOG/panic lines.
- x86_64/KVM, first boot: one `[WDOG] cpu1 took no timer tick ... pid=2 local_ticks=1`. It appeared at boot during init's `/data` mount, before any test ran, while lane hostgpufault's GPU QEMU was loading the host.
- x86_64/KVM, second boot (sigtest pollmask + polltest): clean, 0 lines. Not related to this lane.

## Shared files
kernel/src/syscall.rs (futex, poll, select, restart_syscall: futexrem/futexsig/sigmisc territory), sched/src/signal.rs (delivery pass), sched/src/{task,lib}.rs.
