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
