# lane tforkhang — 2026-10-27 — FIXED (relibc)

`/bin/epolltest churn 25 tfork` hung to the 300 s timeout (3/3, aarch64 HVF -smp 4). Root cause: relibc,
not the kernel.

## Evidence
- Snapshot at +45 s (brush builtins reading /proc/*/task/*/stat): the parent (pid 40) was still forking
  from its forker thread; the main thread waited on child 4667, started at ~7 s, which had exactly
  **2 threads**: leader 4667 `S` with utime=stime=0, and 4668 `R` (an epoll waiter, looping).
  `child()` creates 2 waiters, so the leader was stuck inside its FIRST `pthread_create`, after the clone.
- relibc `pthread::create` inserts into `OS_TID_TO_PTHREAD` (a futex Mutex) after the clone, and the
  BTreeMap insert allocates: lock order map -> allocator (thread teardown's `dealloc_thread` too).
  `fork()` only took the allocator lock (its atfork prepare hook). With `tfork` the forker thread holds the
  allocator lock across a ~57 ms fork; the spawner blocks on it while holding the map lock; the main
  thread's fork then acquires the allocator next and snapshots the map LOCKED by a thread that does not
  exist in the child. The child's first `pthread_create` waits forever. Nothing in the kernel log because
  nothing in the kernel is wrong.

## Fix
- relibc `88bbaa57` (master): `fork()` runs under `pthread::with_thread_map_locked` — the map lock is
  taken first (keeps map -> allocator order), released in both parent and child (the lock is not
  owner-tracked).
- leandros: regress gains `/bin/epolltest churn 20 tfork` (next to `churn 20`).

## Results
- After the fix: churn 20 tfork PASS ×2 (20.8 s, 20.9 s), churn 25 tfork PASS (25.6 s); 1525–2395 forks.
- aarch64 regress (box TCG, ef3d9f1c + relibc 88bbaa57): 55 suites, 0 bad, overall PASS.

## Open (not fixed here)
- The churn parent grows without bound while it runs: VSZ ~33 GB and RSS ~125k pages (~490 MB) after 45 s
  (`[FORK] tgid=40 private_rw_pages=133k clone_us=57 ms holds=66`). Looks like joined thread stacks or
  thread memory are not returned; it makes each later fork slower. Worth its own lane.
- Under `tfork` thread spawns drop from ~12k to ~2k per 20 s: every fork quiesces all siblings for the
  whole (growing) clone.
