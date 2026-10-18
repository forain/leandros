# lane/percpu — 2026-10-18

Branch `lane/percpu` from main b6e6f363. First agent: phases 0, 1, 2 (pick and
switch-back stay under the global lock); phases 3a/3b/4 are for a follow-up
agent — see "Hand-off" at the end.

## Spec (synthesized from designs A + B, 2026-10-18)

Both designs agree: RUN_QUEUE stays as the global **TASKS** lock (task table + pid index, per-task process
state, the whole wait/park registry incl. PollListen/pk_*/listening/bloom, signals, process tree, cred).
New per-CPU **RQ[c]** locks own ONLY the ready set + dispatch state. Do NOT build per-object waitqueues,
wake lists, or the refuted per-instance wait channel (TODO.md:344).

### Census on main b6e6f363 (lines moved; laptop data from 63bcebe2, ~95k acq/s, 305 ms/s held, 595 ms/s spin)
| site | function (main) | acq/s | held ms/s | spin ms/s |
|---|---|---:|---:|---:|
| lib.rs:5299 | scheduler_run_loop switch-back (+carried pick) | 24k | 100 | 135 |
| lib.rs:5208 | uncarried pick (after idle / reap) | 13k | 24 | 203 |
| lib.rs:~3026 | service_poll_deadlines_keyed, try_lock on EVERY CPU tick | 11k | 71 | 0 |
| lib.rs:2530 | block_on_poll_prepare_set | 9.6k | 6 | 91 |
| lib.rs:384 | process_exit_state (pidfd probes) | 8.8k | 2 | 49 |
| lib.rs:2520 | block_on_poll_prepare_masked | 6.7k | 2 | 32 |
Pick + switch-back = 57% of spin. epollready (merged after this census) changed shares → re-census.

### Data layout
- `static SCHED: [SchedSlot; MAX_TASKS]` by slot, OUTSIDE Box<Task> (avoids &mut Task aliasing UB between
  lock domains): state AtomicU8, on_cpu AtomicU32 (NONE=MAX), cpu AtomicU32 (home rq), on_rq (under RQ[cpu]),
  sched entity vruntime/vdeadline/weight/lag (UnsafeCell, only under RQ[cpu]), cpu_ns/sys_ns AtomicU64,
  sb_slow AtomicU8 ("switch-back needs TASKS": Zombie / stop_pending / group_killed), gen counter,
  dispatch mirror (ctx ptr, kstack top, page_table, as_ptr, user_gs_base, reply_port, rseq ptr, sig_hint ptr,
  pid, tgid) written next to every write of its source field (enumerate write sites, sigfast-style).
- `RQ: [TrackedMutex<CpuQueue>; MAX_CPUS]`: queued set (bitmap+summary or dense entries {slot,gen,pid,tgid,
  weight,vruntime,vdeadline}), nr_queued AtomicU32 (peekable), curr, per-rq min_vruntime. Pick touches no Task.

### Lock order & rules
FUTEX_TABLE → TASKS → RQ[i] → RQ[j] (i<j, or second via try_lock). RQ sections are leaves: no alloc, no TLB
shootdown, no other locks, RQ holder never takes TASKS. irq_window never inside any of them. IRQ paths:
try_lock TASKS then may spin on RQ (safe: RQ holders never wait on TASKS). RQ spin must service shootdowns
like the TASKS spin. lockwatch level below L_RUN_QUEUE asserting the order.
- R1 Ready↔Running only on owning CPU under RQ[cpu].
- R2 Running→Blocked by the task itself under TASKS (prepare step unchanged).
- R3 wake (under TASKS): Blocked/Stopped→Ready; then lock_task_rq (load cpu, lock, recheck): if on_rq or
  on_cpu set → nothing (switch-back enqueues anything it finds Ready under the same RQ[c]); else choose
  target CPU (prev if idle, else idle core w/ SMT pref from wake_up_an_idle_cpu, else prev; sync wake → waker
  CPU), set cpu, renormalize vruntime by lag vs target min_vr, enqueue, IPI only if target idle or wakee
  deadline beats curr. Broadcast wake scans batch: lock each RQ[c] once per TASKS hold. sig_kick BEFORE
  set_ready in the same hold.
- R4 others moving a task to Zombie/Stopped hold TASKS + RQ[t.cpu]: queued&off-CPU → dequeue; on-CPU → set
  sb_slow and kick (killer that sees on_cpu set gets Kicking and retries, as today).
- R5 free (rq.remove) only under TASKS + RQ[t.cpu] with !on_rq && on_cpu NONE; bump gen; pick/steal verify gen.
- R6 cpu changes only while unqueued & off-CPU (R3), or in a steal holding both rqs with task queued, not running.
- Switch-back fast path RQ[me] only (sb_slow clear, not Zombie): charge time, clear on_cpu, enqueue if Ready,
  carry RQ[me] guard into next pick (keeps sigfast2's merge). Slow path: KEEP on_cpu claim, drop RQ, lock
  TASKS→RQ[me], run today's code unchanged (log_exit before remove, release_quiesce_if_owner, klocks rules),
  release claim in that hold.
- quiesce_thread_group: after storing QUIESCE_TGID/EXCEPT, lock+unlock every RQ[i] (rq barrier), then poll
  on_cpu with Acquire; pick & steal apply the filter under RQ; unquiesce IPIs CPUs holding filtered tasks.
- TASKS-only readers treat Ready/Running as one class (audit). Signal thread choice, group exit, cred,
  wait_scan, /proc read atomics; stale "prefer Blocked" choice is harmless.
- Balancing: wake placement (R3), idle/new-idle steal (peek nr_queued, try_lock busiest, take one queued,
  not running, not quiesce-filtered, renormalize), tick: pull when lengths differ ≥2 (~4 ms) or kick an idle
  CPU to steal. No periodic load tracking/NUMA. Fork/clone → least-loaded CPU. No affinity to honour.

### Phases (each: commit, regress + net-e2e on BOTH arches, one VM at a time, before the next)
0. Small independent wins + instrumentation: /proc/rqhold per-lock-id rows (TASKS, each RQ[c]);
   deadline service on ONE CPU (CAS servicer atomic) instead of every tick; process_exit_state lock-free
   (per-tgid live/zombie/reaped atomic written under TASKS); sync-wake (waker about to block doesn't IPI an
   idle CPU that then loses the race).
1. SchedSlot refactor (pure): state/on_cpu atomics behind helpers (set_ready, set_blocked_self, set_zombie…),
   dispatch mirror + write-site table, pick reads mirror and compares to Task (mirror_miss counter must be 0).
   Always-on release checks: run_guard CAS NONE→cpu at dispatch (fail ⇒ panic: double run), freed Box poison +
   gen bump (use-after-reap), hung-ready watchdog (queued >1 s while some CPU idles ⇒ log+count).
2. Per-CPU CpuQueues inside RunQueue, all STILL under the global lock: placement, steal, renormalize,
   enqueue-at-switch-back, per-rq min_vr. Keep a once-per-tick full scan as checker: Ready ∧ on_cpu NONE ∧
   ¬queued ⇒ rqmiss++ and heal. Gate rqmiss=0.
3a. Pick under RQ[me] only; switch-back TASKS→RQ[me] carrying RQ[me] into pick; wakers lock RQ[c] nested;
   quiesce rq barrier. 3b. switch-back fast path RQ[me] only + slow path.
4. Targeted kick replaces wake_up_an_idle_cpu-on-every-wake; tick balance.
(5) Only if re-census demands: split the wait index off TASKS — NOT in this lane.

### Tests
Existing: regress, net-e2e, epolltest (both modes), sigtest sigfast 20 (miss=0), killmt, forktest, exectest,
pthreadtest, polltest latency, idletest, smpwaketest/racetest if present.
New `schedstress` (add to regress, with a shorter mode): 1 futex/pipe/eventfd ping-pong 2×N threads w/
latency histogram (timeout = lost wake); 2 sigsuspend/kill ping-pong + signals at parking threads;
3 SIGSTOP/SIGCONT storms on spinning/blocking/klock-holding threads; 4 exit_group/fatal signals while siblings
spin/block/poll; 5 fork from MT process while siblings write memory (checksums: quiesce); 6 non-leader exec
while siblings run; 7 spinners > CPUs + yielders, all progress within a balance period, fairness ±10%;
8 pidfd/waitpid reaping storm 10k short children.
aarch64 HVF is the priority race-finder (parallel, weak ordering); x86_64 TCG: confirm MTTCG (thread=multi).
Measure per phase: /proc/rqhold holds per 1000 yields (now ~1002), ns/yield (sigtest sigfast_yield_holds),
cross-CPU futex ping-pong scaling 1/2/4 pairs. Expected after phase 3b: ~0 TASKS holds per yield; on laptop
pick/switch-back spin (~340 of 595 ms/s) gone; gain mostly latency, less in process CPU%.

---

## Environment

- x86_64 QEMU 11.1.1 TCG on this Mac runs **MTTCG**: 4 vCPU threads each with
  minutes of CPU time in `ps -M` (no `thread=` given; the default is multi).
  aarch64 runs HVF.
- `MAP_SHARED|MAP_ANONYMOUS` is private across fork in this kernel
  (syscall.rs mmap: "MAP_SHARED is not supported ... silently treat as
  private"). schedstress first relied on it (stopcont false FAIL), now uses a
  pipe. Not fixed here (out of scope).
- Scratch: `scratchpad/percpu/` — `bin/run.sh` (waits for no QEMU, unique run
  id), `bin/chain.sh <phase>` (per arch: regress + rqhold + sigtest sigfast 20
  + killmt + schedstress full; net-e2e; an `epoll.ready=0` image copy with
  epolltest + bench + schedstress pingpong), `bin/check.sh` (cargo check).
  Logs in `scratchpad/percpu/<phase>/`.

## Phase 0 (f8a2a94c, schedstress fix 636570f7)

- `/proc/rqhold` gains `# lock TASKS holds N ns_held N cont N ns_spun N`
  (lockwatch rows: row 0 = TASKS, row 1+c = RQ[c] — phase 3 wires its RQ
  locks to `row_note_hold/row_note_spin(lockwatch::row_rq(c), ..)`) and one
  `# percpu ...` counter line (`sched::SchedStatsLine`; later phases append).
- Deadline servicer (`percpu::deadline_service_enter/exit/release`,
  kernel/src/syscall.rs `poll_deadline_service`): one CPU owns the pass; a
  CPU that finds it taken ORs `DL_AGAIN` into the same word and returns
  without re-arming for the global deadline; the owner runs another pass
  (≤4, then releases and its own re-arm retries). Counters: deadline_pass /
  skip / again.
- `process_exit_state`: per-tgid cache (256 entries) tagged with
  `PEXIT_GEN`; writers bump after writing, under RUN_QUEUE: every Zombie
  set/unset (now inside `Task::set_state`), `enqueue`/`remove`,
  `wait_reported`, EXIT_LOG insert/consume (pool and spilled ring),
  `take_over_leader` exchange. Readers load the gen BEFORE deriving. 1/256
  fast answers re-derived under the lock: `pexit_miss` (0 on both arches).
- Sync wake: single-waiter FUTEX_WAKE from a user task defers the idle kick
  (`SYNC_KICK[cpu]`), paid at `deliver_pending_signals` (every return to
  user), the local tick, or a switch-back that leaves the waker runnable;
  dropped if the waker blocked, but paid after the pick if other candidates
  remained. Laptop-relevant observation: in the test suites sync_drop is
  rare (18 of 13903): most futex wakers return to user first.
- schedstress (userland/schedstress, `/bin/schedstress short` in regress).

| arch | holds/1000 yields | ns/yield | sigfast miss | pexit_miss |
|---|---:|---:|---:|---:|
| aarch64 HVF | 1002 | 1500 | 0 | 0 |
| x86_64 TCG (MTTCG) | 1014 | 15310 | 0 | 0 |

Tests: regress (all suites), net-e2e, killmt, sigtest sigfast 20, epolltest
both modes — PASS both arches. schedstress: stopcont FAIL both arches =
test bug (MAP_SHARED, above), fair_share ±11 % in the 2 s short mode on
aarch64 → short mode now 3 s / ±15 %, full mode keeps ±10 % (passed ±3–4 %).
Futex ping-pong ns/rt (aarch64 HVF) 1/2/4 pairs: 15.4k / 18.1k / 24.8k;
x86 TCG: 64.6k / 84.0k / 148.1k.

## Phase 1 (b98607bf)

`sched/src/slot.rs`: `static SCHED: [SchedSlot; MAX_TASKS]` (64-byte
aligned, all-zero → .bss). `Task::slot` (NO_SLOT until enqueued),
`Task::magic` (poisoned in `Drop`).

- State mirror: `Task::set_state` is the only writer of `Task::state`
  (27 sites converted, `grep '\.state = '` in sched/src is empty except the
  constructors). The full scan compares (`state_miss`).
- Run guard: `slot::claim` CAS 0 → cpu+1 at dispatch (panic = double run),
  `slot::release` CAS back at switch-back (panic on mismatch), `on_remove`
  panics if a claimed slot is freed. Poison check at both.
- Dispatch mirror write sites (all call `slot::publish(t)` after the write):

| field | post-enqueue writers |
|---|---|
| ctx / rseq / sig_hint (addresses) | none (box never moves; set at `on_insert`) |
| kernel_stack | none after enqueue |
| page_table, address_space | `exit` (last thread drops its address space: `t.page_table = 0; t.address_space.take()`), exec `replace_address_space` |
| user_gs_base | `set_user_gs_base` (arch_prctl), exec |
| reply_port | `set_current_reply_port`, `take_over_leader` |
| pid / tgid | `take_over_leader` (both slots; leader promotion) |
| rseq contents | rseq.rs (3 sites) — pointer unchanged, published anyway |
| constructors / clone / spawn | before `enqueue` → `slot::on_insert` publishes |

  Phase 1 still dispatches from the Task and counts `mirror_miss`.
- Hung-ready watchdog in the once-per-tick full scan (Ready, off-CPU, not
  quiesce-filtered, >1 s since `ready_since`, another CPU idle) → serial
  line + `hung_ready`/`hung_pid`.
