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

Phase 1 fix (f178c3cc): the first x86 TCG run (b98607bf images) timed out
procmax: the watchdog's "a CPU is idle" was `CURRENT_PID == 0`, which every
busy CPU passes through at each switch, so all ~2000 waiting tasks of the
fork storm were flagged, each with a serial line printed under RUN_QUEUE
(71k lines). Now "idle" = found nothing to run for ≥ 50 ms (`IDLE_SINCE`,
set in the idle branch, cleared at dispatch) and at most one console line a
second. Re-run (p1b) green.

| arch | holds/1000 yields | ns/yield | sigfast miss | mirror_miss | state_miss | hung_ready | futex pp ns/rt 1/2/4 pairs |
|---|---:|---:|---:|---:|---:|---:|---|
| aarch64 HVF | 1002 | 1498 | 0 | 0 | 0 | 0 | 16.3k / 19.0k / 28.4k |
| x86_64 TCG | 1013 | 14452 | 0 | 0 | 0 | 0 | 62.4k / 90.1k / 150.6k |

Tests (p1b): regress incl. schedstress short, schedstress full, sigtest
sigfast 20, killmt, net-e2e, epolltest + bench with `epoll.ready=0` — PASS
both arches. schedstress fair (global queue): ±4–6 %.

## Phase 2 (f1b36afb)

Per-CPU `CpuQueue`s inside `RunQueue`, all still under RUN_QUEUE.

- **Wake → queue.** `Task::set_state(Ready)` from anything but Running marks
  the slot in `slot::WOKEN` (atomic bitmap, only touched under RUN_QUEUE):
  this catches every Ready transition without restructuring the ~12 wake
  sites that hold a `&mut Task`. `RunQueue::flush_woken[_sync]` places the
  marked tasks; the runqueue wake methods and futex wake/requeue flush in
  the same hold, and **every pick drains first** (so in phase 2 no wake can
  be missed by a pick). Phase 3 must replace the drain-at-pick by the
  waker locking RQ[target] (spec R3): pick under RQ[me] cannot read WOKEN
  entries for other CPUs' tasks safely.
- **Placement** (`make_ready_on`): home if idle with empty queue; else an
  idle CPU with empty queue, whole idle core first; else home. vruntime
  renormalised: `dst_min + (vruntime - src_min)` then `place(dst_min)`.
  Single futex wakee of a user thread → waker's CPU (sync).
  New tasks → least loaded (queued + running).
- **Stale entries**: blocking/stopping/killing a queued task does not
  dequeue it; `eevdf()` drops non-Ready entries (`stale`). `remove()`
  dequeues and clears WOKEN and `curr`.
- **Pick** (`pick_next(me)`): checker once per tick → drain WOKEN → count
  balance → hog balance → EEVDF over own queue → new-idle steal (busiest
  first, then any). Dispatch sets `curr`, `SCHED.cpu`.
- **Balance (deviation: pulled forward from phase 4, needed for schedstress
  `fair`).** With placement + new-idle steal only, `fair` measured ±83 %:
  a CPU with one spinner never steals (it is never idle). Count balance
  alone (move one when a queue is 2 longer) still ±40–90 %: a sched_yield
  loop counts like a spinner. Final form:
  - hog = EWMA (×3/4) of "this dispatch ran ≥ 1 ms" (0..256, settles at
    253 for a spinner), in `SchedSlot::hog`, per-queue sums `hog_q` +
    `hog_curr`;
  - hog balance: a CPU whose hog load exceeds ours by ≥ 1.5 hogs gives us
    its most hog-like queued task (2 full hogs never fired: 3×253 < 253+512);
  - count balance moves only the least hog-like task (else the two undo
    each other: count pull put the spinner back).
  Result `fair` (8 spinners + 2 yielders, 4 CPUs): ±3.7 … ±12.6 % over 6
  runs on aarch64 (global queue ±4–6 %). The residual is the yielders'
  CPU use landing on whichever CPUs host them; a global-virtual-time or
  tick rotation (phase 4) would remove it. Short mode (regress) allows
  ±15 %; full mode keeps the spec's ±10 % and can flake on this.
- **Checker** (`check_queues`, once per tick): `rqmiss` (Ready ∧ off-CPU ∧
  not queued ∧ no pending wake → count + heal), `rq_bad` (on_rq vs bitmap,
  per-queue counts), `gen_miss` (queued gen ≠ slot gen), plus phase-1 slot
  checks. Debug line `# cq cpuN:nr=,hq=,hc= hog_hist [...]` (CPUs 0–7).
- `unquiesce_thread_group` kicks idle CPUs holding queued work (their
  filtered tasks stayed queued).

Results (f1b36afb kernel, schedstress at 161a24d8):

| arch | holds/1000 yields | ns/yield | sigfast miss | rqmiss | rq_bad | gen_miss | mirror_miss | hung_ready | futex pp ns/rt 1/2/4 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| aarch64 HVF | 1002–1003 | 1604–2229 | 0 | 0 | 0 | 0 | 0 | 0 | 16.3k / 19.4k / 29.1k |
| x86_64 TCG | 1014–1015 | 16706–17166 | 0 | 0 | 0 | 0 | 0 | 0 | 62.6k / 96.0k / 207.1k |

(counters read after sigtest + killmt + schedstress full: aarch64 1.14 M
dispatches, 139 k steals, 102 k count pulls, 25 hog pulls, 360 stale; x86
0.90 M dispatches.)

- Holds per yield unchanged (1 TASKS hold per context switch — expected,
  everything is still under it). **ns/yield went UP** vs phase 1 (aarch64
  1498 → 1604–2229, x86 14452 → ~17000) and x86 4-pair futex ping-pong
  150 k → 207 k ns/rt: the pick now drains WOKEN, scans the other queues
  for balance and may steal, all under the global lock. Phase 3 moves this
  under RQ[me]; if it does not recover, make the balance scans cheaper
  (lock-free NR/hog peeks, balance only every Nth pick or at the tick).
- Tests: regress (incl. schedstress short) PASS both arches; net-e2e PASS
  both; epolltest + bench with `epoll.ready=0` PASS both; sigtest sigfast 20
  (miss 0) and killmt PASS both. `schedstress full` fails only `fair_share`
  (full mode, spec ±10 %): aarch64 ±7–15 %, x86 TCG ±14–19 % (see Balance);
  no starvation window, no hung-ready line on any run.

## Phases 3a + 3b + 4 (second agent, rebased onto main 5a398650)

Hand-off checklist from phase 2 done in one commit (`percpu phase 3`), plus
`schedstress` short bound back to ±12 %.

### Design as built
- `sched/src/cpuq.rs`: `static RQ: [RqLock; MAX_CPUS]` (own spinlock,
  owner = cpu+1, spins servicing TLB shootdowns, `# lock RQ<c>` rows via
  `lockwatch::row_rq`, watchdog id `L_RQ`). `CpuQueue` = queued bitmap +
  summary word, nr, monotonic `min_vr`, `curr`. Lock-free peeks
  `NR_QUEUED`, `CURR`, `MIN_VR`, `VT` (current min vruntime), `GVT`.
- `SchedSlot` grew the EEVDF entity (`vr`, `vd`, `weight`), `q_gen`,
  `sb_slow`, `cpu_ns`/`sys_ns`/`sc_nr`/`sc_ns`/`sc_t0` (procfix's
  `sc_saved_t0` lives here), rseq addr/sig mirror. `Task` lost `on_cpu`,
  `vruntime`, `vdeadline`, `cpu_ns`, `sys_ns`, `sc_saved_*` → accessors
  `on_cpu()`, `cpu_ns()`, `sys_ns()`, `sc_saved_nr()`, `sc_saved_t0()`
  (compile errors enumerated every reader). Still all-zero → .bss.
- `TaskState::Running` is never stored: a running task is `Ready` with a
  run-guard claim (display code maps claim → R/Running).
- **Wakes (R3)**: `set_state(Ready)` by a non-owner marks `WOKEN`;
  `RunQueue::flush_woken` places under TASKS + `RQ[target]` and kicks the
  target if it idles. Safety net that replaced the drain-at-pick: the
  RUN_QUEUE guard's `Drop` flushes (`lockwatch`), so every path (signals,
  SIGCONT, deliver_signal_process, ...) is covered without per-site flushes.
  Owner = the CPU holding the claim (`slot::owned_here`): its switch-back
  queues the task, no WOKEN.
- **R4**: a non-owner transition out of Ready goes through
  `slot::note_state` → `lock_slot` → state store + (off-CPU: dequeue |
  on-CPU: `sb_slow`). `RunQueue::try_park` for callers that must know the
  outcome (do_signal_stop siblings, take_over_leader's leader → Zombie).
  So queued ⇒ Ready (stale = 0 everywhere).
- **R5**: `remove` locks the slot's queue, dequeues, `on_remove` panics if
  claimed, gen bump.
- **Dispatch** under `RQ[me]` from the mirror only (`slot::dispatch_values`);
  magic checked through the mirror's ctx pointer.
- **Switch-back fast path** (`RQ[me]` only): state ∈ {Ready, Blocked,
  Stopped}, `sb_slow == 0`, mirror pid == CURRENT_PID → release claim,
  requeue if Ready, carry `RQ[me]` into the pick. Otherwise the slow path:
  TASKS, today's stop/group-kill/zombie logic, release under `RQ[me]`
  (inside TASKS), drop RQ, `remove` + `log_exit`, drop TASKS.
- **Lock order** asserted: TASKS taken while holding an RQ panics
  (`lockwatch::assert_no_rq`); wake placement/enqueue/remove/non-owner
  set_state panic under an RQ. Steal and balance use `try_lock` only; the
  tick checker takes all RQs in index order.
- **Quiesce**: lock+drop every RQ after storing the filter; pick/steal
  filter by mirror tgid/pid.
- **Checker** (`percpu_check`, once per tick, any CPU, scheduler context):
  TASKS + all RQs → `RunQueue::check_all` (rqmiss heal = re-mark WOKEN,
  woken_late, rq_bad, gen_miss, mirror/state mirrors, hung-ready, park
  hints).
- **Phase 4**: wakes and new tasks kick their target CPU only if it idles;
  `wake_up_an_idle_cpu` became a balance kick (only if a busy CPU or the
  caller has queued work). Balance runs once per tick per CPU
  (`BALANCE_DUE`, first pick after the tick): count pull (load ≥ ours + 2)
  and **global virtual time** pull.
- **Fairness**: vruntime is charged in real ns (not ticks) and is NOT
  renormalised on migration; the tick balance pulls the most lagging task
  of a queue whose `VT` trails ours by > 2 ticks. `GVT` (max queue min)
  floors queue minima/placements at `GVT − 32 ticks`. Hog EWMA/hog balance
  removed. Result: `fair` ±0.5–1.6 % aarch64, ±1.3–7.1 % x86 TCG (was
  ±7–19 % in phase 2, ±4–6 % with the global queue).

### Results (phase 3 build, chain p3; gate counters all 0 both arches)
| arch | holds/1000 yields | ns/yield | futex pp ns/rt 1/2/4 pairs |
|---|---:|---:|---|
| aarch64 HVF p0 | 1002 | 1500 | 15.4k / 18.1k / 24.8k |
| aarch64 HVF p2 | 1002 | 1604–2229 | 16.3k / 19.4k / 29.1k |
| aarch64 HVF p3 | **1** | 1480–1707 | 13.9–15.1k / 16.8–18.2k / 21.4–24.0k |
| x86_64 TCG p0 | 1014 | 15310 | 64.6k / 84.0k / 148.1k |
| x86_64 TCG p2 | 1014 | ~17000 | 62.6k / 96.0k / 207.1k |
| x86_64 TCG p3 | **~1** | 13583 | 63.5–66.5k / 84–86k / **100–101k** |

Gate (sched run after sigtest/killmt/schedstress full): rqmiss, rq_bad,
gen_miss, mirror_miss, state_miss, hung_ready, woken_late, stale,
sigfast miss, pexit_miss = 0; no panic / hung-ready line in any serial log.
Switch-backs: ~99.7 % fast (e.g. 2.27 M fast / 15 k slow over killmt +
schedstress). `r4_running` counts kill retries re-marking an on-CPU
Zombie (Zombie→Zombie, lib.rs kill_next_group_member_except) — expected.
Tests: net-e2e, epolltest + bench with `epoll.ready=0`, sigtest sigfast 20,
killmt, schedstress full (all cases) PASS both arches. regress PASS except
pstest's `/proc/sysstat e` lines — **not this lane**: the chain ran
schedstress full (10 k-child reap) + killmt before regress, overflowing
procfix's never-freed sysacct (tgid, nr) table (`# overflow 97584`);
pstest passes on a fresh boot with this kernel. chain.sh now boots
regress alone. (Open item for procfix: sysacct keys never freed.)

### Post-rebase verification (main 5a398650, chain p4)
- aarch64 HVF: regress (pstest included) PASS, sched run (sigtest sigfast
  20, killmt, schedstress full + 2× fair) PASS, net-e2e PASS, legacy
  epoll.ready=0 (epolltest + bench + pingpong) PASS. Gate counters all 0,
  sigfast miss 0; holds/1000 yields 1, ns/yield 1323; fair ±0.4–0.9 %.
- x86_64 TCG: regress PASS. sched / net-e2e / legacy runs NOT re-done after
  the rebase (stopped on wrap-up); they passed on the pre-rebase p3 build
  (the rebase brought only non-sched changes: blankplay ports/tests).

### Remaining steps (for a small follow-up lane)
1. x86_64 post-rebase: `scratchpad/percpu/bin/chain.sh p4 x86_64` minus the
   reg run (or just run it: ~45 min) — sched, net, legacy. Expect PASS.
2. Optional perf polish: ns/yield on aarch64 is only at par with phase 0
   (1.3–1.7 µs); candidates: drop the two clock reads per RQ hold
   (row stats) behind a flag, skip `percpu_check`'s tick test on the
   carried path, check `eevdf` cost with many queued tasks.
3. Laptop A/B (laptop-deploy-kernel skill) before merge: desktop idle and
   Firefox/YouTube playback, `/proc/rqhold` TASKS spin vs main.
4. Out of lane: procfix's sysacct (tgid, nr) table never frees keys —
   a 10 k-process run overflows it and pstest's `e` check fails afterwards.
5. Merge lane/percpu (9 commits on main 5a398650), not pushed.

## (historical) Hand-off to phase 3a

State: branch `lane/percpu`, everything committed, worktree
`/Users/forain/code/leandros/.claude/worktrees/agent-a716a84e7bd4e1bb0`.
Phase 2 keeps ALL scheduling under RUN_QUEUE (TASKS); the data is already
split so 3a can move it.

1. **Lift `CpuQueue` out of `RunQueue`** into
   `static RQ: [TrackedMutex<CpuQueue>; MAX_CPUS]` (new lockwatch id,
   level below L_RUN_QUEUE; release/spin notes go to
   `lockwatch::row_note_hold/row_note_spin(lockwatch::row_rq(c), ..)` so
   `/proc/rqhold` gets `# lock RQ<c>` rows). `q_gen`/`q_hog`/`cand` must
   become per-queue or per-slot-in-SCHED (a slot is in ≤1 queue, so
   per-slot fields in `SchedSlot` guarded by the owning RQ work).
2. **Pick reads no Task.** `eevdf()` uses `t.state`, `t.on_cpu`,
   `t.vruntime`, `t.weight`, `t.vdeadline`, `t.tgid/pid` (quiesce filter):
   move the sched entity (vruntime, vdeadline, weight) into `SchedSlot`
   (UnsafeCell, written only under RQ[cpu]; `charge_vruntime`/`place` move
   with it), and use `SCHED.state` / `SCHED.on_cpu` / `m_pid` / `m_tgid`.
   Dispatch must switch from the Task values to the mirror (phase 1 proved
   `mirror_miss` = 0 everywhere) — keep `check_mirror` as an always-on
   sampled check while TASKS is still taken.
3. **Replace the WOKEN drain-at-pick** (phase 2's safety net) with spec R3:
   the waker, under TASKS, locks RQ[target] (nested, TASKS → RQ) and
   enqueues. Today the wake sites only call `set_state(Ready)` and the
   runqueue/futex paths call `flush_woken`; signal paths (deliver_signal,
   SIGCONT, deliver_signal_process) rely on the pick drain — they need an
   explicit flush before the drain goes. Keep `rqmiss` as the gate.
4. Switch-back: TASKS → RQ[me], carry RQ[me] (not TASKS) into the next
   pick (sigfast2's merge argument applies unchanged to the RQ guard).
   `busiest()` reads `cq[c].curr`/`hog_*` of OTHER queues — make those
   atomics (peek) or try_lock the victim (spec: steal = try_lock busiest).
5. `check_queues` (tick checker) needs TASKS + every RQ[i] in index order.
6. Quiesce barrier (spec): after storing QUIESCE_TGID/EXCEPT, lock+unlock
   every RQ[i]; pick and steal apply the filter under RQ (they already do).
7. Do not touch: sigfast hint protocol (CURRENT_SIGHINT published at
   dispatch — take it from `m_sighint`), epollready listener recording
   (happens inside the wake scans under TASKS; unchanged as long as wakers
   keep TASKS).
8. Known open items: residual cross-CPU unfairness with yielders (phase 4:
   tick balance / rotation or a cross-CPU virtual clock); `rseq` CONTENTS
   are read at dispatch from the Task (`rseq::on_dispatch(&t.rseq)`) — with
   pick off TASKS, a concurrent `rseq.rs` write under TASKS races it
   (only the task itself registers, so reading under the run guard is
   probably fine, but document it); `sc_saved_nr/ns` are touched only by
   the CPU owning the run guard.

Test recipe (one VM at a time, the shared lock is mandatory):
`scratchpad/percpu/bin/chain.sh <phase>` runs, per arch, regress +
`/bin/sigtest sigfast 20` + `/bin/killmt` + `/bin/schedstress full` +
`cat /proc/rqhold`, then net-e2e, then an `epoll.ready=0` image copy with
epolltest/bench; `bin/run.sh` takes
`scratchpad/locks/lock.py acquire|release qemu percpu` around each VM.
Build: `./scripts/build-all.sh` (≈8–13 min; `--arch aarch64` alone ≈5 min).
Gate per phase: rqmiss = rq_bad = gen_miss = mirror_miss = state_miss =
hung_ready = sigfast miss = pexit_miss = 0.
