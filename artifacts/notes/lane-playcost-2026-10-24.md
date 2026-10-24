# lane/playcost (+playcost2) 2026-10-24 — TASKS cost during playback (L3.1, L3.7)

Status: PARTIAL (lock-cost fixes in; per-key waiter index / deadline heap still open).

## Changes
- `runqueue.rs widen_port_masks`: skip a parked slot unless its park mirror matches (port + tag), no Task deref per parker (was 15 us/hold, 1018/s).
- `futex.rs wake_key`: FUTEX_WAKE with no claimable waiter in the bucket returns under FUTEX_TABLE, never takes RUN_QUEUE.
- `futex.rs`: wake/requeue that finds the waiter Blocked also clears `rt_wait` and marks the slot `tidy`; the woken waiter then frees its slot without a second RUN_QUEUE hold (was futex.rs:472, 3480/s). The two holds cannot be folded into one (they straddle the context switch); this removes the second for the common path.
- Counters in `/proc/rqhold`: `# playcost widen_skip widen_deref fwake_empty fwake_locked fwait_tidy`, `# fdclose`/`# fddup` per fd kind.

## Evidence (aarch64 HVF)
- futextest guards: `wake_empty_no_tasks` fwake_empty +2000 / fwake_locked +0; `wait_woken_single_tasks_hold` fwait_tidy +401 of 401.
- Same guards with both fast paths disabled: FAIL (fwait_tidy +0 of 402; wake_empty_no_tasks FAIL).
- regress aarch64: PASS.

## Open (measured cost, deskplay2 baseline)
- `service_poll_deadlines_keyed` lib.rs:4418, 2000/s x 9 us = 17.8 ms/s held: O(parked) scan; needs a cached earliest deadline / heap.
- `wake_poll_key` (unix send/recv, eventfd read) 5-7 us per hold, O(parked): per-key waiter index.

## Laptop (x86_64, main a847fcf9, trial d7cb59dfcfaf, PROMOTED good), deskplay phase1, 3 runs, video playing (exec_vcs 60/s, flips 53/s)
| | deskplay2 baseline | playcost r1 / r2 / r3 |
|---|---|---|
| TASKS acq/s | 30.4-31.1k | 28.6k / 28.8k / 27.2k |
| held ms/s | 93.5-95 | 92.0 / 121.6 / 90.8 |
| spin ms/s | 28.5-31.7 | 28.2 / 131.6 / 24.4 |
| RDD sys % | 9.1-9.2 | 8.8 / 8.7 / 8.9 |
- r2 outlier: sched lib.rs:5018 (wait4 scan under RUN_QUEUE) 224/s x 160 us, max 996 us per hold, right after the r1 Firefox kill: contention spin spread to every TASKS site. Not present in r1/r3.
- Counters per s: fwait_tidy 3470 (= every locked wake; futex.rs:472 hold gone), fwake_empty only ~150 (wakes mostly have waiters), widen_skip 290k vs deref 20k (93 % skipped) yet lib.rs:4158 still 1021/s x 13 us.
- fd kinds (system-wide, ~60 frames/s): close eventfd 1625 + tmp 1640, dup eventfd 1385 + tmp 1090 -> ~27 eventfd and ~27 tmp (memfd/shm) closes per frame.
- Heartbeat 690 beats, max gap 2 s, 0 > 2 s.
- Next by cost: lib.rs:4419 deadline scan 18.9 ms/s; wait4 scan (lib.rs:5018) up to 1 ms per hold; widen 13.5 ms/s; unix send wake_poll_key lib.rs:4891 12.8 ms/s; futex wake_key 8.5 ms/s; unix recv POLLOUT 8.1 ms/s.
