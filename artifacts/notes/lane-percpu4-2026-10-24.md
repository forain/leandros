# lane/percpu4 — 2026-10-24 — openbugs L3.1: FIXED (the post-wake take hold is gone)

## What changed (commit 0f1f3b4c)
A woken listener's wake ring is handed over in an owner-read mailbox in its `SchedSlot`
(`lm_full` + `lm`), so the post-wake `poll_listen_take` RUN_QUEUE hold (poll() and epoll_wait)
is replaced by the lock-free `sched::poll_listen_woken(lid)`. `poll_listen_park` keeps its hold.
- Writer: `Task::set_state` on Blocked -> Ready of a park that `listen_park` armed
  (`PollListen::parked`), under TASKS, before the state is published (Release on `lm_full`).
  Any other park (nanosleep, futex, non-listener poll) clears `parked`: those wakes leave the ring
  alone because nothing reads the mailbox after them.
- Multiple wakers: only the Blocked -> Ready transition writes; later wakes land in the ring and the
  next `listen_park` takes them in its own hold (`Taken`). A still-full mailbox for the same id is
  merged into, never overwritten.
- Timeout/signal racing a waker: `block_on_poll_cancel` is now poll-specific; in its TASKS hold it
  takes the mailbox (written by its own Ready transition or by a waker that beat it) back into the
  ring. A committed park whose wake recorded nothing (signal, deadline without keys) gets an empty
  mailbox and re-probes everything, as before.
- Thread exit: the mailbox is plain data (no allocation); `on_insert` and `listen_park` clear it.
  /proc readers do not read the ring.

## Guard
First version armed the hand-over on ANY Blocked -> Ready of a listener on the poll channel; the
waiter's `usleep` (a timed poll-channel park that keeps the listener) handed 40 recorded wakes to a
mailbox nobody read: epolltest `wake_ring_overflow` FAILED (first=1, expected 2^40-1). Fixed by the
`parked` arming flag; now PASS.

## Measurement (aarch64 HVF -smp 4, --virgl, Firefox ffv.html loop, 3 x 10 s windows)
| build | TASKS acq/s | held ms/s | cont/s | spin ms/s | RQ* acq/s | RQ* spin ms/s | ff+cosmic CPU % |
|---|---:|---:|---:|---:|---:|---:|---:|
| percpu2 after | 35562 / 35776 / 34606 | 43–47 | 10.5k–12.3k | 30.9–31.5 | ~39.5k | 3.5–5.8 | 39 / 44 / 46 |
| percpu4 run 1 | 20761 / 20769 / 20772 | 27–30 | 2.8k–3.0k | 8.5 / 7.9 / 7.7 | ~27.9k | 4.3–5.0 | 35 / 37 / 32 |
| percpu4 run 2 | 23767 / 23860 / 23125 | 36–40 | 4.2k–4.4k | 13.6 / 13.3 / 14.4 | ~31.6k | 5.1–6.3 | 43 / 44 / 47 |
- `poll_listen_take` (old lib.rs:3929) is gone from the site list. TASKS spin 31 -> 8–14 ms/s,
  contended 11k -> 3–4.4k/s. Park rate fell too (9.6k -> 4.6k/s): wakes after the hand-over are
  `Taken` at the next park without sleeping. Run-to-run spread (run 1 vs 2) is large; CPU% is within
  the known ±25 % playback noise. Video currentTime stays 0 in this page in both old and new runs
  (scroll animation is the load), same as percpu2/3.
- Remaining top TASKS spin: lib.rs park (~2.6 ms/s), futex sites, net server 4857.

## Tests
aarch64: full regress PASS; schedstress full, sigtest sigfast 20, killmt, epolltest (default, stat,
drain, xwake, kctl, all) PASS. x86_64: build + full regress PASS.
