# Lane futex — 2026-10-19 (bug list L10.3)

## What

1. **Shared futex keys.** The 2026-09-24 fix (`key_matches`) only split
   *private* futexes by thread group; non-private ones still matched by
   virtual address alone, so a `MAP_SHARED` object mapped at two addresses
   (or in two processes at different addresses) never woke, and two
   processes' *private* pages at the same address did cross-wake through a
   non-private wake. Now every futex has a `FutexKey` computed once per call
   (`sched::futex::futex_key`, Linux `get_futex_key`):
   - FUTEX_PRIVATE, or a shared futex in a non-`MAP_SHARED` mapping:
     (thread group, VA) — identical for both, as Linux's (mm, address);
   - `MAP_SHARED` anonymous: (`ShmObj`, byte offset);
   - `MAP_SHARED` file through the page cache: (file key, file offset);
   - other shared (memfd/tmpfs frames from `map_shared_frames`, uncached
     files, devices): the physical byte (pinned while mapped); an absent page
     is touched with no lock held and looked up again;
   - kernel addresses (`mutex.rs`): global.
   `mm::vmm::AddressSpace::futex_backing` does the lookup. Cost: a
   non-private futex call (all of relibc's) now takes the address-space lock
   and a linear VMA scan.
2. **Robust lists.** `set_robust_list`/`get_robust_list` (aarch64 99/100,
   x86_64 273/274; were ENOSYS). `sched::robust` walks the list (Linux
   `exit_robust_list`/`handle_futex_death`: OWNER_DIED | WAITERS, owner
   cleared, one waiter woken; `list_op_pending`; 2048-entry limit; garbage
   pointers fault-tolerantly skipped) at thread exit (before the
   clear_child_tid wake) and at exec. Threads that never run their own exit
   (reaped by a group kill, the old leader in a non-leader exec, an
   already-dying thread) have their head stashed under their TID and the
   surviving thread walks it while the address space is still theirs.
   Not inherited by fork/clone; cleared on exec.
3. **CLOCK_REALTIME FUTEX_WAIT_BITSET restart.** The restart block now keeps
   the original wall-clock deadline and `restart_syscall` re-translates it,
   so a clock step while the waiter is stopped moves the deadline.
4. **PI hand-off.** `FUTEX_UNLOCK_PI` (`sched::futex::futex_unlock_pi`)
   claims the first FUTEX_LOCK_PI waiter (`handoff = 1`: it stays parked
   even if its deadline/signal fires), writes `its TID | FUTEX_WAITERS` into
   the word with no lock held, then commits (`handoff = 2`) and wakes it;
   its LOCK_PI returns 0 owning the lock. A claimed waiter killed before the
   commit gets the lock handed on to the next one (or released).

## Tests

New `userland/futextest` (std/musl, in `regress` after pthreadtest):
shared_memfd_two_maps, shared_cross_process, shared_anon_fork,
private_not_shared, robust_exit / _exit_waited / _killed / _group_exit /
_thread_exit / _pi_exit (EOWNERDEAD then pthread_mutex_consistent),
pi_handoff (word names the waiter right after UNLOCK_PI, user cas cannot
steal), realtime_restart (root; SIGSTOP, step the clock +10 s, SIGCONT:
times out at once instead of at the old 3 s monotonic deadline).

aarch64 (HVF), rebased on main fe5126c6: futextest 12/12 PASS, pthreadtest
11/11 PASS; full regress 37/38 RC=0, the one red was kparkstest
`empty_poll_sleep` (1 stray poll wake in a 100 ms empty poll), then 3/3
green on re-run: flaky, unrelated to futexes. No before-run of futextest on
the old kernel (it needs an image rebuild); on the old code set_robust_list
was ENOSYS, shared non-private futexes matched by VA, UNLOCK_PI released to 0.

Rebase: lane/vmreuse2 (e97bd8aa) forced `private` for words in private
mappings in sys_futex; `futex_key` does that itself now (same (tgid, VA)
key), so that override and `futex_word_private` are removed.

## Open

- A CLOCK_REALTIME deadline is still translated once per (re)start: a clock
  step while the waiter is parked (not stopped) does not move it (Linux
  re-arms realtime hrtimers on clock_settime). Same for LOCK_PI timeouts.
- No priority inheritance proper (no RT class), unchanged.
- Shared futex lookup is a linear VMA scan under the AS lock per
  non-private call; if relibc-heavy workloads show it, add a VMA index or
  make relibc pass FUTEX_PRIVATE_FLAG.
