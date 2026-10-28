# lane threadleak2 — 2026-10-27 — FIXED (relibc af24d27c)

Follow-up to the threadleak review (review-threadleak-2026-10-27.md).

- MEDIUM: `exit_current_thread` now disables cancellation and blocks all signals first (as musl's pthread_exit),
  and skips the zombie push / waitval post if `EXITING` was already set. No re-entry from the cancel signal.
- LOW: `pthread_detach` of an already-exited thread pushes it on `ZOMBIES` (shared `push_zombie`) instead of blocking.
- LOW: fork child resets the `ZOMBIES` head (`forget_zombies_after_fork`, called in `fork_locked`).
- Regress: `pthreadtest churn` gains a cancel-racing-exit phase (n/5 threads, alternately joined and detached,
  cancelled right after create); fails if VMAs/VmSize/RSS grow. aarch64 HVF `churn 5000`: PASS, flat 8 VMAs / 9544 kB / 288 kB.
- Not done: reaping from detach (review item 3 optional), items 4-6 unchanged.

## Validation
x86_64 box regress (KVM): overall PASS (also the x86 check threadleak owed). aarch64 box regress (TCG): one red,
`futextest kwake_scan_indexed` (slot-scan stat, unrelated to pthread exit); futextest re-run twice on aarch64 HVF: PASS.
aarch64 `pthreadtest churn 5000`: PASS.
