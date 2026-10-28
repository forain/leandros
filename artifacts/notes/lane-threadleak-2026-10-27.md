# lane threadleak — 2026-10-27 — FIXED (relibc)

Follow-up to lane tforkhang ("churn parent grows without bound").

## Root cause
relibc never unmapped an exited thread: not the 1 MiB pthread stack (`exit_thread` on Linux/LeandrOS had a
TODO), and not the TCB/TLS mapping from `Tcb::new` (abi page + TLS + TCB page). Two VMAs and ~1 MiB of
address space per thread, forever. The kernel is fine: munmap and the clear-child-tid exit path work.

## Measurement (aarch64 HVF, `pthreadtest churn 5000`, 100 warm-up threads first)
| | threads | VMAs | VmSize | VmRSS |
|---|---|---|---|---|
| before (`ff695179`) | 0 → 5000 joined | 208 → 10208 (+2000/1000) | 113 MB → 5.29 GB (+1036 MB/1000) | 3.1 → 143 MB (+28 MB/1000) |
| after | 0 → 5000 joined, + 2500 detached | 8 → 8 | 9544 kB flat | 288 kB flat |

`epolltest churn 25 tfork`: before (tforkhang note) 1.5–2.4k forks, ~2k spawns, `[FORK] private_rw_pages=133k
clone_us=57 ms`; after 11252 forks, 10869 thread spawns, max `private_rw_pages=512 clone_us=161`.
churn 5000 itself: 0.9 s (before: >300 s, it was still crawling through the growing map).

## Fix — relibc `5ec78736` (master)
- `new_thread_shim` registers `Pthread::exit_tid` via `set_tid_address`; the kernel zeroes + futex-wakes it at exit.
- `pthread_join` waits for `exit_tid == 0`, then unmaps the stack (only if `create` mapped it) and the TCB mapping.
- Exited detached threads push themselves on a lock-free `ZOMBIES` list; the next `pthread_create`/`join`
  frees the entries whose `exit_tid` is 0.
- `PthreadFlags::EXITING` closes the detach-after-exit race (the second of exit/detach frees).
- A caller-provided stack is no longer munmapped when `create` fails.
- Not freed (small, noted): the per-thread DTV vector (ld.so allocator, see the XXX in tcb.rs). Redox path unchanged.

## Regress
`/bin/pthreadtest churn 5000`: 5000 joinable (sampled per 1000) + 2500 detached threads; fails if VMAs grow > 16,
VmSize > 16 MB or RSS > 2 MB. Unfixed: +10000 VMAs / +5.2 GB at 5000 threads.

## Issue 2 — fork pauses siblings (not changed)
`quiesce_thread_group` (sched/src/lib.rs) stops every sibling for the whole CoW clone, so the pause is
~clone_us + IPI: ~0.43 µs per private page — 57 ms at 133k pages before, ≤0.2 ms now that the parent stays small.
That (fixing the leak) was the cheap improvement; spawns under tfork went ~2k → ~11k per 25 s.
Further option, unverified: the clone is already chunked (2048 pages, AS lock dropped between chunks); doing a
range TLB shootdown per chunk would let siblings run between chunks (writes then fault on the downgraded PTEs
and wait for the AS lock), shrinking the stop-the-world to one chunk. Needs care with the corruption the
quiesce was added for (brush/tokio Process struct).

## Validation
aarch64 full regress on the box (TCG, lane/threadleak + relibc 5ec78736): 56 suites, 0 bad, overall PASS.
