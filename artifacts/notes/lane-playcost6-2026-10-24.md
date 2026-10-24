# lane/playcost6 2026-10-24 — futex RUN_QUEUE (TASKS) holds (L3.1)

Status: PARTIAL. Holds are shorter; the hold COUNT per futex op is unchanged (1 per wait commit, 1 per FUTEX_WAKE
that finds a waiter) and cannot drop without a scheduler state-ownership change (below).

## Changes
- `futex.rs wake_key`: targets are picked from the bucket under FUTEX_TABLE before RUN_QUEUE is taken (batches of 16);
  RUN_QUEUE covers only the Blocked->Ready transitions and the wake placement. Site moved 595 -> 620.
- `futex.rs` wait commit (:379): one pid lookup (`RunQueue::task_at_mut`, `note_timed_wait_idx`); the full
  `has_deliverable_signal_locked` (task + 2 leader lookups) is skipped when the sigfast hint is clear — exact under
  RUN_QUEUE by the sigfast invariant (every writer that makes the predicate true kicks the hint in the same hold).
- futextest: `pingpong_holds` (bench: 2 threads x 20000 rounds, /proc/rqhold futex.rs deltas) and
  `futex_lost_wake_stress` (A: 8 timed waiters vs 2 wakers, sum of FUTEX_WAKE returns must equal 0-returns;
  B: 6-thread ring, no wait may time out or sleep >= 1 s; C: CMP_REQUEUE 8 waiters, all 8 woken).

## Evidence (aarch64 HVF, futextest pingpong_holds, ns held per hold)
| site | before (2 runs) | after (4 runs) |
|---|---|---|
| wait commit :379 | 183, 165 | 104, 112, 104, 98 (-40 %) |
| FUTEX_WAKE :595/:620 | 414, 418 | 386, 408, 394, 372 (-7 %) |
holds/op and ns/round vary 2x run to run (how many waits enter the kernel); ns/hold is stable.
- Guard shown failing: injected "count a wake on a not-yet-parked waiter without claiming it" -> FAIL
  `A claimed=30972 woken=30714`; reverted -> PASS. All runs: A equal, B lost=0, C 8/8.
- aarch64: futextest x5, regress, `schedstress full`, polltest, epolltest, sigtest, killmt PASS.

## Open — why the count cannot drop here, and the design that would
- The remaining wake hold is dominated by `flush_woken_sync`/`place_woken` (cpuq locks of the wakee's home and
  target CPU). Placement must stay in the hold: "Ready, off-CPU, unqueued: nothing but a TASKS holder can touch it"
  and lockwatch drains WOKEN at every TASKS release.
- `Task::state` (and blocked_futex/poll_deadline/rt_wait) are only written under TASKS; slot.rs forbids forming
  `&mut Task` outside it. Per-bucket futex locks would split FUTEX_TABLE, not TASKS.
- Follow-up design (multi-day lane, "FPARK"): a per-slot atomic `fpark` {0 none,1 parking,2 parked,3 released} in
  SchedSlot. Commit (FUTEX_TABLE only): store 1 SeqCst, then read sig hint (Dekker with sender: hint SeqCst, then
  CAS fpark 2->3); switch-back under RQ[cpu] CAS 1->2 = do not queue, else queue. Wake: swap fpark->3, old==2 =>
  place under RQ[target] (CAS winner is the exclusive placer instead of TASKS). Timed waits need a futex-owned
  timer list; kill/stop/ptrace/wait4/procfs/hungtask must treat fpark==2 as Blocked. Removes both holds per op.
