# lane/leakres (L1.9) 2026-10-23: FIXED

The "~27 pages/death lazy-fault residual" is REFUTED (<0.01 page/death); the real
residue were per-death heap objects, found with a temporary frame-pointer tracer (removed).

1. sys_execve leaked `kpath` (1-2 small Vecs) on every successful exec: `replace_address_space`
   never returns. Fix: `drop(kpath)` before it.
2. Per-death epoll leak (one 160 B EpCache ents + one 288 B EpCache Box + one 8 B `cand`
   Vec<bool> per brush death): `epoll_wait_until` held the thread's EpCache and its scratch
   vectors in locals across the sleep. A sibling thread killed by exit_group while parked is
   reaped without unwinding its kernel stack, so they were never freed. Fix: the cache (with
   `cand`/`recheck`/`changed` moved into `EpCache::scratch`) is parked in `Task::ep_cache`
   (`sched::ep_cache_put/get`) for the duration of `block_on_poll_commit`; Task drop frees it.
3. Guard: memtest `death_residual` now also fails when any heap size class grows by >= N/2 objects
   over batch 1 (a per-death leak grows it by exactly N): `heap_leaks=` in its output.
   `memtest leak /bin/sh -c:` before the epoll fix: heap 160/288/8 +200 each (FAIL); after: heap_leaks=0 PASS.
   (Default run uses /bin/true and only sees the exec leak; the brush variant sees the epoll one.)
