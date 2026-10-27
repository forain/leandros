# lane/piepollnull — 2026-10-27 (PARTIAL: wedge FIXED, corruption source OPEN)

Real Pi 5, kernel a9a61b2b: `/bin/sh` (pid 151, tgid 149, 5 threads) in epoll_pwait took an
EL1 data abort (ESR 0x96000046, FAR 0, W), was killed by the user-pointer fault path, and the
exit path re-took EPOLL — all four CPUs spun on it.

## Symbolized (picard1 build-aarch64-direct kernel, a9a61b2b)
- 0xffff8000003d8fec = `memset` (`str x11, [x12]`, first store of the destination).
- 0xffff800000451510 = `sched::scheduler_run_loop` (IRQ sample of the spinning CPUs; noise).
- Only memset callers that run under EPOLL_INSTANCES with a heap destination: `ep_diff`
  (`ents.resize(hi, EpEnt::NONE)`, 72-byte stores at the vector's buffer). FAR == 0 means the
  kept wait snapshot (`EpCache.ents`) had a NULL buffer with a capacity >= hi: the per-thread
  `EpCache` box was torn/freed memory. Not a user pointer — the "user-pointer" classification
  only came from FAR < USER_SPACE_END.

## Fixes
1. `force_release_held()` in sched/src/lockwatch.rs: every TrackedMutex acquisition records
   (per CPU, per lock id) the mutex and a type-erased force-unlock; the EL1 fault-kill paths
   (aarch64 exception.rs, x86_64 idt.rs) release every tracked lock this CPU holds before
   `exit_group_signal`, and log the mask. Plain spin::Mutex / sleeping locks are not covered.
2. `epoll_wait_ready`: a kept snapshot whose vectors are implausible (len > cap, cap huge,
   buffer not in the kernel half) is logged `[EPOLL] corrupt wait snapshot dropped` and leaked
   instead of used — turns the Pi crash into a diagnostic line.
3. `epoll_wait_ready` ignored copy-out failures (`let _ = write_user…`) and returned n events
   with nothing written; it now stops and answers EFAULT (or the events already delivered),
   the entry stays ready. (The legacy `epoll_wait_until` probe loop already did this.)

## Tests (privtest, aarch64, root, each with a second thread parked in epoll_wait)
- `epoll_pwait_unwritable_events_efault`: ready pipe, events buffer on an unmapped page → EFAULT
  x4, then the event is still delivered.
- `kernel_fault_under_epoll_lock_releases_it`: SYS_SECPROBE op 4 stores to the null page with
  EPOLL_INSTANCES held → child SIGSEGV, parent's epoll_wait still works.

## Open
- WHO tore the EpCache. Candidates read but not proven: slot ep_cache ownership across
  `take_over_leader` (exec from a non-leader thread), CURRENT_SLOT staleness. Watch Pi logs for
  `[EPOLL] corrupt wait snapshot` / `[EXC] released locks` lines to catch it again.
