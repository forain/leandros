# review: threadleak — 2026-10-27

Scope: leandros merge `d608e76c` (pthreadtest churn + regress line + note), relibc master `5ec78736`
on top of `88bbaa57` (tforkhang). Review only; nothing edited.

**Verdict: the fix is sound. Merge stands.** One medium follow-up (signals during exit), the rest low/info.
The relibc tree is clean (no uncommitted `src/ld_so/tcb.rs` edit is left). Both
`target/{x86_64,aarch64}-unknown-leandros/release/librelibc.a` were rebuilt after the last source change
(22:56 / 22:54), so **x86_64 compiles**. x86_64 has not been run yet: the note lists aarch64 regress only,
which is allowed per lane, but it is owed at the next merge-batch two-arch run.

## Checked and OK

- **No use-after-unmap on exit.** After the `ZOMBIES` push (detached) or `waitval.post` (joinable), the
  only remaining code is the `EXIT` syscall: `stack_base` and `stack_size` are already in locals, and nothing
  touches TLS. The kernel (`sched/src/lib.rs` ~8238) runs robust-futex release first, then
  `write_user_buf(clear_addr, 0)`, then `futex_wake`, then `release_exiting_address_space`. Its comment
  says that is the last user-memory access, so nothing reads or writes user memory after the clear.
  `write_user_buf` (`mm/src/vmm.rs:2429`) breaks CoW, so a fork between the thread's `EXITING` RMW and
  its `EXIT` cannot leave the word stuck at nonzero. The write holds the address-space lock, so a
  reaper's munmap cannot overlap it.
- **No reaping of a running thread.** `create` sets `exit_tid = u32::MAX` before clone, and
  `set_tid_address` only records the pointer. The word becomes 0 only through the kernel's exit clear.
  The main thread has `mem_len == 0` (the `init()` overwrite), so it is never freed. A fork or vfork
  child starts with `clear_child_tid = 0` (`sched/src/task.rs:1122`), so a child exiting cannot clear
  the parent thread's word.
- **No ABA or lost entries on `ZOMBIES`.** It is a Treiber push plus a whole-list `swap(null)` pop, with
  no single-node CAS-pop, so it is ABA-immune. `next` is read before the free, and re-pushes go back to
  the live head. Concurrent reapers get disjoint lists.
- **Detach/exit race.** `fetch_or` on the same `flags` word gives a total order, so exactly one side frees.
- **Mapping bounds.** `mem_base`/`mem_len` = abi page + TLS + TCB page, which is exactly the
  single `Self::map(..)` in `os_new` (`src/ld_so/tcb.rs:338`). On aarch64, TPIDR_EL0 points into the
  abi page; on x86_64, FS points at the TCB page. Both lie inside the one mapping, and no register or
  kernel state refers to it after exit. relibc registers no rseq, and a CLONE_VM thread starts with no
  sigaltstack (`sched/src/clone.rs:507`), so neither can make the kernel write into the freed range.
- **Caller stack.** `MmapGuard` gets size 0 and `owns_stack = false`, so a user stack is never unmapped.
- **Fork from a thread (tforkhang).** The new code takes no new lock. `reap_zombies` runs outside the
  thread-map lock, and `dealloc_thread` uses the same lock as before.
- **musl apps (Firefox etc.)** are unaffected: musl has its own pthread, which already uses
  CLONE_CHILD_CLEARTID and unmaps itself. They depend only on the same kernel clear and wake path,
  which this change does not touch.

## Findings (ranked)

1. **MEDIUM — re-entry into `exit_current_thread` from the cancel signal can double-push a zombie,
   giving a list cycle, then a SIGSEGV in the reaper and a double munmap.**
   `src/pthread/mod.rs` `exit_current_thread` (~343–380) and `cancel_sighandler` (~448).
   Scenario: thread A calls `pthread_cancel(B)` on a detached thread B with cancellation enabled. relibc
   sends `SIGRT_RLCT_CANCEL` whenever cancellation is enabled, deferred or not. The signal can land
   after B's CAS push and before its `EXIT` syscall. The handler calls `exit_current_thread` again: the
   destructors run again, `prev` has `DETACHED`, and B is pushed a second time. If B was the head,
   `zombie_next == B`. The reaper frees B, then follows `next == B` into unmapped memory (SIGSEGV), or
   munmaps B's range twice after a new thread reused it.
   For a joinable thread, the same re-entry calls `waitval.post` twice and runs the destructors twice.
   That hazard existed before this change, but it is now on the free path.
   Related: `pthread_detach(pthread_self())` from a signal handler after `EXITING` is set would make
   `detach` wait on its own `exit_tid`, a self-deadlock.
   **Fix:** at the top of `exit_current_thread`, set `has_enabled_cancelation` to false and block all
   signals (`sigprocmask(SIG_BLOCK, all)`, as musl's `pthread_exit` does). No handler can then run
   between the `EXITING` RMW or the push and the syscall. A cheap extra guard: if `prev` already has
   `EXITING`, skip the push and go straight to `EXIT`.

2. **LOW — `pthread_detach` blocks.** `detach` (~315): when the thread has already set `EXITING`, the
   detacher waits on a futex until the kernel clears the word. The wait is bounded in practice, but
   detach is not supposed to block, and it can deadlock with item 1's handler case.
   **Fix:** push the thread onto `ZOMBIES` instead of `free_thread_memory(thread, true)`, reusing the
   non-blocking path.

3. **LOW — some zombies are never reaped.** Reaping happens only on `create` and `join`. A process that
   spawns a burst of detached threads and then stops creating or joining keeps that last burst mapped.
   In a fork child, inherited `ZOMBIES` entries whose threads were still alive at fork keep
   `exit_tid != 0` forever, so they are walked on every `create` and never freed. Entries a sibling
   had swapped out mid-reap are simply lost in the child. All of this is bounded and leak-only.
   **Fix (optional):** also call `reap_zombies()` from `detach`. In the fork child hook, drop the list,
   or reset it after `munmap`ping the entries whose threads are known dead.

4. **LOW — misuse that used to be harmless now crashes.** `pthread_join`/`pthread_detach`/`pthread_kill`/
   `pthread_cancel` on a thread that has already been joined, or on a reaped detached thread,
   dereferences unmapped memory and SIGSEGVs. Before, it silently read stale memory. This is UB under
   POSIX and glibc behaves the same way, but buggy ports will now show it. Worth one line in TODO.

5. **LOW — the ld.so ↔ libc ABI depends on the Tcb layout.** `Pthread` grew by 5 fields inside the
   `Tcb`. Any dynamically linked relibc binary needs `ld.so` and `libc.so` from the same build. A
   kernel+initrd-only deploy (`deploy-kernel`) that mixes an old `/lib/ld*.so` with a new libc corrupts
   the TCB. Statically linked binaries are self-contained. Make sure the next full image ships both.

6. **INFO — known leftovers.** The per-thread DTV (ld.so allocator, the XXX in tcb.rs) is not freed. On
   a failed clone, the new TCB mapping still leaks; that predates this change. `MmapGuard::drop` lost
   its indentation; run `rustfmt` on it.

## Next steps
- Small relibc follow-up for items 1 and 2: block signals and disable cancellation in
  `exit_current_thread`, make `detach` non-blocking, and add a `pthreadtest` case that cancels detached
  threads in a tight loop.
- Run x86_64 KVM regress (`pthreadtest churn 5000`, `epolltest churn 20 tfork`) in the next merge batch.
