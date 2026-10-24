# lane/percpu2 (+ percpu3 continuation) — 2026-10-24 — openbugs L3.1: PARTIAL

## What changed
The per-thread epoll snapshot (`EpCache`) moved off `Task::ep_cache` (under TASKS/RUN_QUEUE)
into an owner-only cell in `SchedSlot` (`sched/src/slot.rs`). `ep_cache_put`, `ep_cache_get` and
`poll_listen_store` no longer take TASKS; `poll_listen_enter` takes the cache lock-free and TASKS
only for the listener ring. An epoll_wait sleep went from 4 TASKS holds to 2 (park + take).

Correctness argument:
- Only the task touches the cell, while it holds its run guard, with IRQs masked
  (`ep_cache_swap_current`), so there is no preemption or migration mid-swap.
- `slot::on_remove` moves the cell into the Task box (freed by the reaper outside locks).
  It panics if the run guard is still claimed, so it never overlaps the owner, and the slot
  is clean when reused.
- `epoll_ctl` from another thread never reads the cell: staleness is detected through the
  epoll instance, as before. `/proc` readers (epollstat) do not read `ep_cache`.

## The "first WIP boot failure": REFUTED as a code bug
ffsession runs with `LEANDROS_RUN_ID=percpu2-aarch64` hung in Limine/early boot before any
kernel serial output, twice (a1, a2). The same build booted under `percpu3-aarch64`, both in
full regress and in ffsession. The cause is stale per-run-id host state, not this change.

## Measurement (aarch64 HVF, -smp 4, --virgl, Firefox ffv.html 720p VP9 loop), 3 x 10 s windows
| build | TASKS acq/s | TASKS held ms/s | TASKS cont/s | TASKS spin ms/s | RQ* acq/s | RQ* spin ms/s | ff+cosmic CPU % |
|---|---:|---:|---:|---:|---:|---:|---:|
| main 2950a0ea (before) | 40966 / 41215 / 40487 | 44.0 / 47.1 / 45.9 | 14296 / 14341 / 14172 | 30.4 / 31.6 / 31.3 | ~31.4k | 9.0–10.1 | 50 / 56 / 53 |
| percpu2 (after)        | 35562 / 35776 / 34606 | 43.3 / 45.2 / 47.1 | 12263 / 11458 / 10533 | 31.2 / 31.5 / 30.9 | ~39.5k | 3.5–5.8 | 39 / 44 / 46 |

- ep_cache_put/get (5.4k acq/s each) and poll_listen_store (1.1k) are gone from TASKS.
  TASKS acquisitions are down 14 % and contended acquisitions down 20 %.
- TASKS spin time did NOT go down. It moved to the two holds that remain:
  `poll_listen_take` (lib.rs:3929, 9.3k/s, 8.1 ms/s spin) and `poll_listen_park` (lib.rs:3936,
  9.6k/s, 7.4 ms/s spin). Their rates rose from ~6.4k/s to ~9.5k/s, because the loop iterates
  more often per second.
- RQ* spin is down 2–3x. Firefox+cosmic CPU is 39–46 % against 50–56 %, but single playback
  runs vary by about ±25 %, so this is suggestive, not proven.

## Open (next lane)
`poll_listen_take` at syscall.rs:4912 runs right after a woken `poll_listen_park`, so its ring
is usually non-empty: an "is the ring empty" flag would save little. The real fix is to hand
the ring over at wake time. Under TASKS, the waker that ends the park moves `listen.ring`
into an owner-read slot mailbox, which is safe because the owner is Blocked. Park's `Woken`
result then carries it, and the take hold disappears. Park itself has to keep TASKS, because
the ring-empty check and the block must be one hold.

## Tests
aarch64: full regress PASS; schedstress full, sigtest sigfast 20, killmt (full), epolltest
(default cases incl. drain/xwake; cost/abi/stat in regress) PASS. x86_64: build + full regress PASS.

