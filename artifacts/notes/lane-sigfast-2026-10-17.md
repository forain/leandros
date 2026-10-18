# lane/sigfast — 2026-10-17

## Question

lane deskbusy2's `/proc/rqhold` on the laptop (YouTube playing): RUN_QUEUE
195 k acquisitions/s, 1126 ms/s spun. ~150 k acq/s and ~490 ms/s of the spin
were signal bookkeeping that found nothing: the syscall-return claim
(signal.rs:261) and delivery loop (:325), 52.5 k/s each, and
`interrupted()` → `has_deliverable_signal` (45.7 k/s, 279 ms/s spin).

## Change

A per-task hint, `Task::sig_hint: AtomicU32` ("this thread may have signal
work"), readable without RUN_QUEUE through a per-CPU pointer to the running
task's hint (`CURRENT_SIGHINT`, published at dispatch next to `CURRENT_AS`,
cleared at switch-back; same validity argument: the running task's boxed
Task is never freed or moved while it runs, readers run with IRQs masked).

- `deliver_pending_signals` (every syscall/exception return): hint 0 ⇒
  return with no lock (both holds skipped).
- `has_deliverable_signal` (`interrupted()`, tty): hint 0 ⇒ `false` with no
  lock. Its slow path clears the hint when the full predicate is false
  (a thread that never leaves epoll_wait would otherwise lock forever after
  one masked arrival).
- Safety net: one fast syscall return in 1024 re-checks under the lock; work
  found while the hint reads 0 *under that hold* is a missing writer kick —
  counted, its pid kept, hint healed. `/proc/rqhold` ends with
  `# sigfast ret_fast N ret_slow N intr_fast N intr_slow N miss N miss_pid P`.

### The predicate

`sig_work_locked(T)` = exactly what makes the return-to-user check do
anything (a superset of `has_deliverable_signal_locked`):

    T.stop_pending || T.saved_sigmask.is_some()
    || T.signal_pending & !T.signal_mask != 0
    || leader.shared_signal_pending & !T.signal_mask != 0
    || leader.group_exit_owner ∉ {0, T.pid}

Dispositions are not in it (a pending SIG_IGN'd signal must still be
dequeued), so `sigaction`/`reset_handlers_on_exec` need no kick.

### Invariant and race argument

At every RUN_QUEUE release: `predicate(T) ⇒ T.sig_hint != 0`.

1. Every write that can turn the predicate true happens under RUN_QUEUE and
   stores 1 (Release) in the same hold (`Task::sig_kick`,
   `RunQueue::sig_kick_group`).
2. The hint is cleared (store 0) only under RUN_QUEUE, in a hold that has just
   evaluated the whole predicate false (end of `deliver_pending_signals`'s
   loop when nothing is pending and no sigsuspend mask is being restored; the
   slow path of `has_deliverable_signal`). No writer can interleave with that
   hold, so any later write re-sets it. A stale 1 costs one locked check; a
   stale 0 cannot exist at a lock release.
3. Reader: hint load (Acquire) with no lock. Seeing 0 means no write ordered
   before the read (happens-before) made work appear: the sender's RUN_QUEUE
   release precedes our dispatch's acquire when it woke us; a pipe/futex/
   eventfd hand-off after a kill carries the order; a self-signal is program
   order. A post racing the check is caught at the next check — exactly the
   window the lock-based check already had (it too could lock just before the
   post). Parks that must not sleep through a signal still test
   `has_deliverable_signal_locked` inside the parking hold (futex), and a post
   still makes a Blocked target Ready under the lock; poll parks are woken by
   the signal tag. None of that changed.

### Writer enumeration (every store to the predicate's fields, `sched/` is the only writer crate)

| field / event | site | kick |
|---|---|---|
| `signal_pending \|=` thread-directed (kill to tid, tgkill, tkill, raise, pidfd_send_signal to a thread, POSIX timer SIGEV_THREAD_ID — IRQ try-lock variant too) | `post_signal_thread` | the target |
| `signal_pending \|=` process-directed to an unmasked thread (kill, killpg via `kill_pgrp`, kill(-1), SIGCHLD, setitimer/alarm/timer_create, pidfd_send_signal, tty SIGINT/SIGTSTP/SIGHUP/SIGWINCH, IRQ try-lock variant) | `post_signal_process` | the chosen thread |
| `shared_signal_pending \|=` (all threads mask it) | `post_signal_process` | every thread of the group |
| `signal_pending \|=` synchronous fault | `fault_signal` | self |
| `signal_pending \|=` claim of leader-parked bits | `deliver_pending_signals` | self (already set: slow path) |
| `signal_pending/shared \|=` merge at non-leader exec | `take_over_leader` | self |
| `signal_mask` sigprocmask / rt_sigprocmask / pthread_sigmask (all `how`) | `sys_sigprocmask` | self |
| `signal_mask` rt_sigreturn restore (both arches) | `restore_signal_frame` | self |
| `signal_mask` sigsuspend / sigtimedwait / ppoll / pselect6 temp mask and its restore | `replace_signal_mask` | self |
| `signal_mask` sigsuspend saved-mask restore on the no-handler path | `deliver_pending_signals` | self |
| `signal_mask \|=` handler blocked set | `deliver_pending_signals` | none needed (only adds bits) |
| `saved_sigmask = Some` | `stash_sigsuspend_mask` | self |
| `stop_pending = true` SIGSTOP/TSTP/TTIN/TTOU default action, terminal job-control stop | `do_signal_stop` (also reached from `stop_now_if_pending`) | every group thread it marks |
| `stop_pending = true` on a leader still on a CPU during a non-leader exec | `take_over_leader` | the leader |
| `group_exit_owner = pid` (exit_group, fatal signal, non-leader exec dethread) | `claim_group_exit` | every thread of the group |
| `tgid`/pid change (leader promotion / pid exchange) | `take_over_leader` | self |
| new task (fork, clone, kernel thread, exec image) | constructors | starts at 1 |
| only-decreasing writes: dequeue, `accept_pending_signal` (signalfd read, sigtimedwait), SIGCONT/SIGKILL withdrawing stops and stop signals, `stop_now_if_pending` claim, `saved_sigmask = None`, `group_exit_owner = 0`, `stop_pending = false` | — | none needed |

Not present in this kernel: ptrace, seccomp (no signal-posting paths).

## Switch-back CPU charge (22.5 k/s, 99 ms/s spin) — merged with the next pick (lane sigfast2)

The charge itself (vruntime, cpu_ns, sys_ns) is a few adds; the hold exists
to release `on_cpu`, resolve Ready/Stopped/Zombie and reap in one critical
section — scheduling state another CPU's pick reads under the same lock.
Moving it off RUN_QUEUE needs a lock-free `on_cpu` release protocol (pick
would have to tolerate a task whose switch-back is still in flight), not a
per-task atomic. Not done.

Done instead (lane sigfast2): `scheduler_run_loop` keeps the switch-back
guard (`held: Option<TrackedGuard<RunQueue>>`) and the next iteration's pick
uses it instead of re-locking. One context switch = one RUN_QUEUE
acquisition, not two.

Race argument:

1. **Only interleavings are removed.** Before, another CPU could take the lock
   between H1 (switch-back) and H2 (pick + claim). Now it cannot. "H1 then H2
   with nothing between" was always a legal schedule (the lock gives no
   fairness), so every invariant that held for all old schedules still holds.
   Nothing on another CPU waits for this CPU to *release* the lock between the
   two holds. The quiesce loop waits for `on_cpu == None`, which is set inside
   H1, and spinners still service TLB shootdowns while they wait.
2. **What this CPU did between the holds.**
   - *Zombie teardown* (exit hook, `clear_exe_path`, freeing the kernel stack)
     must run unlocked. A cycle that reaped does not carry the guard. It drops
     it as before, and the next pick locks afresh.
   - *IRQ window at the top of the loop.* It must never open with RUN_QUEUE
     held: an IRQ handler on this CPU taking the lock would self-deadlock. So
     it moved to just before H1, after the per-CPU `CURRENT_*` slots are
     cleared and the kernel page table is loaded. The top-of-loop window now
     runs only when no guard is carried (first cycle, after a reap, after
     idle). Every cycle still opens exactly one window. In the moved window
     the switched-out task still holds its `on_cpu` claim. That is the state
     it was always in from `cpu_switch_to`'s return until H1, and other CPUs
     could always act on it in that state. An IRQ handler on this CPU now
     sees the same state. `CURRENT_CTX` is null, so a tick only sets
     PREEMPT_NEEDED, exactly as at the top of the loop. No IRQ-context path
     waits for an `on_cpu` claim to clear. The only waiter is
     `quiesce_others` in task context, which kicks and returns.
   - Nothing else ran between the holds. The idle path (`pick_next` → None)
     drops the guard before `hlt`, as before.
3. **Hold length.** The merged hold is H1 + `pick_next`, the same lock-held
   time minus one acquire/release. The census attributes it to the
   switch-back site (`lib.rs` `let mut rq = RUN_QUEUE.lock()` under
   `let zombie`). The pick site (`match held.take() …`) now counts only the
   uncarried picks. Read the laptop census with that in mind: the pick site's
   holds/s falls to about the reap+idle rate, and the switch-back site's
   ns/hold grows by one pick.

Diff: `sched/src/lib.rs` `scheduler_run_loop` only (+22/−2 lines). The dispatch
body and `gen_rqhold` are untouched.

## Measurements (QEMU, release, lane sigfast2 HEAD vs baseline d2f0eaee)

`sigtest sigfast` reads /proc/rqhold around 200 000 raw `gettid` calls and
(new, report-only `sigfast_yield_holds`) around 50 000 `sched_yield` calls.
Baseline = `sched` + `servers/vfs` checked out at d2f0eaee, same sigtest.

| arch | build | signal.rs holds / 1000 syscalls | all holds / 1000 syscalls | ns / gettid | all holds / 1000 yields | ns / yield | miss |
|---|---|---:|---:|---:|---:|---:|---:|
| aarch64 (HVF) | before d2f0eaee | 2000 | 2001 | 1127 | 4003 | 2163 | — |
| aarch64 (HVF) | after (sigfast + merge) | **0** | 1 | 859–968 | **1002** | **1399** | 0 |
| x86_64 (TCG) | before d2f0eaee | 2000 | 2002 | 1461 | 4017 | 15 599 | — |
| x86_64 (TCG) | after (sigfast + merge) | **0** | 1–2 | 883–1104 | **1011** | **13 037** | 0 |

A yield costs 4 holds before: 2 signal.rs, plus switch-back and pick. It
costs 1 after: sigfast removes the 2 signal.rs holds and the merge removes
1 of the scheduler's 2. "After" ranges are the two sigtest runs in one boot
(`sigtest sigfast 20` + the plain `sigtest` in regress). ret_fast ≈ 200 033
per 200 000 calls, ret_slow 0, `miss_total=0` after the full sigtest on both
arches.

## Tests (release builds, lane sigfast2 HEAD images)

`runtests.py <arch> sigfast2-<arch>-reg "/bin/sigtest sigfast 20" /bin/killmt --suite regress`
and `runtests.py <arch> sigfast2-<arch>-net --suite net-e2e`:
- aarch64 HVF: regress + sigtest sigfast 20 + killmt **all PASS** (56/56 RC=0
  lines); net-e2e **PASS** (netctltest, nmtest ×2, nmrealtest, netroamtest).
- x86_64 TCG: regress + sigtest sigfast 20 + killmt **all PASS** (56/56);
  net-e2e **PASS** (same list).

## Hand-off / next steps

1. Laptop: compare `/proc/rqhold` during YouTube with deskbusy2's 195 k acq/s.
   Expect about 150 k/s fewer from sigfast, plus about 22 k/s fewer from the
   merge: the pick site falls to the reap+idle rate. Check that
   `# sigfast ... miss 0` holds.
2. Pitfall: `pgrep -f qemu-system` matches the waiting shell itself. Use
   `pgrep -x qemu-system-aarch64`.

## Merge risk

Touches `sched/src/{signal,lib,task,runqueue}.rs` (hint field, per-CPU
pointer at dispatch/switch-back, kicks next to existing writes) and the
`/proc/rqhold` generator in `servers/vfs/src/lib.rs` (one extra line).
lane epollready (epoll/poll) should not overlap textually unless it edits
`gen_rqhold` or the dispatch block in `scheduler_run_loop`. Any NEW writer of
`signal_pending`/`signal_mask`/`shared_signal_pending`/`stop_pending`/
`saved_sigmask`/`group_exit_owner` added by another lane must call
`sig_kick` (or `sig_kick_group` for leader-held fields). The sampled verify
reports a missing kick as `miss` > 0 in /proc/rqhold.
