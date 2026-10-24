# lane/playcost3 2026-10-24 — TASKS cost: deadline scan + wait4 scan (L3.1)

Status: FIXED (items 1-2); item 3 (per-key waiter index for wake_poll_key, widen_poll_masks) NOT STARTED.

## Changes
- `runqueue.rs`: deadline index. Every finite `pk_deadline` write pushes `(deadline, slot)` onto a min-heap
  (`dl_hd/dl_hi`, cap 2 x MAX_TASKS). `wake_due_poll_deadlines_keyed` pops only due entries (lazy stale
  check: still parked and same mirror deadline), drops superseded tops so the published hint equals the
  old exact minimum (no extra passes). Overflow compacts; still full -> one fallback full scan + rebuild.
  Timerfd passes still walk the parked set but skip words whose `wd_mask` (superset OR of pk_mask,
  recomputed exactly per walked word) misses the expired tags.
- `lib.rs wait_scan`: per-slot parent-tgid mirror `pt_tgid` (Pid::MAX for threads), refreshed by
  `note_ppid` at insert, reparent-to-init, thread-exit reparent, de_thread and leader take-over. The
  phase-1 scan dereferences only slots whose mirror equals the caller; a filtered scan that returns
  NoChildren re-runs unfiltered (safety net, counted). `group_settled` is skipped once moot.
- `/proc/rqhold`: `# dlscan pass slots full`, `# w4scan calls slots fallback`.

## Evidence (aarch64 HVF)
- futextest `deadline_scan_indexed` (64 long timed waiters parked, 200 1 ms timeouts):
  unfixed 79 slots/pass (FAIL) -> 1 slot/pass, 0 full scans (PASS).
- futextest `wait4_scan_children_only` (64 own threads + 1 child, 300 WNOHANG): unfixed 84 slots/scan
  (FAIL) -> 1 slot/scan (PASS).
- futextest, polltest, epolltest, timertest, sigtest, jobtest, `schedstress full`, regress: PASS.
