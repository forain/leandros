# lane/piepollsrc — 2026-10-27 (PARTIAL: corruptor NOT reproduced; owner-tag diagnostics + churn test; new hang found)

Goal: find who tore the per-thread epoll wait snapshot (`EpCache`) seen on the Pi 5 (lane piepollnull).

## Code reading (no ownership bug found)
- `SchedSlot::ep_cache` is touched only via `ep_cache_swap_current` (CURRENT_SLOT of this CPU, IRQs masked)
  and `slot::on_remove` (run guard clear, checked; claim/release CAS are AcqRel). CURRENT_SLOT is written
  only at dispatch/switch-back, together with CURRENT_PID; no other switch path.
- `take_over_leader` keeps the caller's slot (pids exchanged, slots not); the leader is parked first and its
  slot cache goes into its own Task box. Single `RunQueue::remove` path, single `enqueue` path.
- `epoll_wait_ready` handles the box in safe code only. FAR 0 = `ents.ptr == 0 && len == 0` with cap kept:
  looks like freed memory (slab freelist link 0) rather than a safe-code state => heap UAF/double free
  somewhere (possibly unrelated to epoll) is as likely as an ownership bug.

## Changes
- `EpCache` carries `magic` + `owner` (= `sched::current_slot_tag()`: slot index + slot gen). Checked on
  entry and after every park. Pi log lines now say WHICH failure it is:
  `[EPOLL] torn wait snapshot (magic)` (heap corruption/UAF) vs `[EPOLL] foreign wait snapshot owner slot a/g here b/h`
  (ownership bug) vs `bad wait snapshot vectors`, and `[EPOLL] wait snapshot changed across park: pid=N torn|foreign`.
- `epolltest churn [secs] [tfork]` (userland/epolltest/src/churn.rs): 3 waiters + thread spawner (slot reuse) +
  pipe poker; main forks children that (a) exec from a non-leader thread while siblings wait in epoll,
  (b) exit_group with waiters parked, (c) leader exec. In regress as `/bin/epolltest churn 20` (~21 s).

## Evidence (aarch64 HVF, -smp 4)
- churn 40 on unmodified main: PASS, 1723 forks, 12462 thread spawns, no `[EPOLL]`/`[EXC]` lines.
- churn 20 with owner tags: PASS, 1190 forks, 7344 spawns, no snapshot diagnostics; epolltest/pthreadtest/exectest green.

## NEW: hang (open)
`/bin/epolltest churn 25 tfork` (adds a non-leader thread that forks + waitpid's in a loop, children only
`_exit(0)` or execve) hangs to the 300 s runtests timeout, 3/3 runs, with or without a SIGCHLD handler.
No kernel diagnostics in the serial log. Next: run it, then `ps`/`/proc/<pid>/task/*/stat` + wchan of the
parent's threads (waitpid vs fork vs exec), relibc fork-from-thread lock state.

## Open
- The Pi corruptor: deploy main to the Pi and run the hash script; the new lines classify it.
