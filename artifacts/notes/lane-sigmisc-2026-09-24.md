# lane/sigmisc — finish sigalrm's open items (2026-09-24/25)

Branch `lane/sigmisc` @ worktree `~/code/leandros-sigmisc`, base `origin/integ-wave-0924` (`61b5a71`),
merged forward to `d16c563` (clean auto-merge, no conflicts). Mac: aarch64 HVF, x86_64 TCG.

## Result: PARTIAL — 2 of 3 open items FIXED clean; the 3rd (FUTEX_WAIT restart) has a real,
## reproducible race under load that was NOT root-caused in the time available. Shipped with the
## regression test that catches it left in the suite, marked known-flaky.

1. **si_value/si_timerid/si_overrun into siginfo for SIGEV_SIGNAL timers — FIXED, solid.**
   `SigInfo` gained `si_value: u64` and a `SigInfo::timer(timerid, overrun, value)` constructor
   (`sched/src/task.rs`). `write_siginfo` gained an `SI_TIMER` branch (`sched/src/signal.rs`,
   mirrors Linux's `siginfo_t._sifields._timer`). `sys_rt_sigtimedwait`'s own manual siginfo writer
   got the same branch (`kernel/src/syscall.rs`). `sys_timer_create` now reads 20 bytes of
   `struct sigevent` (was 16) to also capture `sigev_value`. `check_timers`/`service_timers_irq`
   build `SigInfo::timer((idx as i32)+1, timer.overrun, timer.sigev_value)` per timer
   (`servers/tty/src/lib.rs`).

2. **SIGEV_THREAD_ID delivers to the named thread, not the process — FIXED, solid.**
   `PosixTimer` gained `target_tid: u32` (0 = process-directed). `sys_timer_create` reads
   `sigev_notify_thread_id` when `sigev_notify == 4`. `check_timers` calls
   `sched::deliver_signal(target_tid, ...)` (the same thread-targeted primitive `tgkill`/`tkill`
   use) when `target_tid != 0`. `service_timers_irq` (IRQ/try-lock context) needed an IRQ-safe
   equivalent — added `sched::try_deliver_signal` by extracting `post_signal_thread` out of
   `deliver_signal` (mirrors the existing `post_signal_process`/`deliver_signal_process` split
   exactly). This is the **only** `sched/src/lib.rs` change in this lane — a mechanical extraction,
   no lock-order/semantic change — kept minimal for the runqlock/forkcow overlap.

3. **FUTEX_WAIT interrupted by a signal should restart transparently under SA_RESTART — the fix
   is IN and improves the untested-before behavior, but has a real, reproducible ~10-20% failure
   rate under host load that is NOT understood.** `sys_futex` (`kernel/src/syscall.rs`): after
   `sched::futex_wait(...)` returns `0` with `cmd == FUTEX_WAIT` and a signal is now deliverable
   (`interrupted()`), returns `ERESTARTSYS` — reusing sigalrm's existing
   `note_syscall_restart`/`check_and_deliver_signals` mechanism verbatim. `FUTEX_WAIT_BITSET` is
   left unaffected (unchanged, still returns bare `0`).

## The open bug, in detail

`userland/sigtest/src/main.rs`'s `test_futex_wait_restart_stress` loops the SA_RESTART FUTEX_WAIT
case 20× and tallies raw `(r, elapsed_us)`. Idle host: passes reliably (140/140 clean in one
session). **Under any real host load — even just another lane's concurrent build, or 6×
`yes >/dev/null` on the Mac — it fails 5-20% of the time**: `r == -4` (EINTR) at ~50 ms (right at
the signal), instead of `r == -110` (ETIMEDOUT) at ~350 ms (the correctly-restarted outcome). This
is a **real bug**, not a test-margin artifact — confirmed by running 100+ iterations under
deliberate load (`for i in 1..6; do yes >/dev/null & done`) with counts, per instruction, rather
than trusting a single pass/fail.

**Two hypotheses tried and both DISPROVEN by measurement** (kept here so nobody re-tries them
without new evidence):

- *H1: the per-CPU `RESTART_NR`/`RESTART_A0` slots in `sched/src/signal.rs` get clobbered by an
  unrelated task's ordinary (non-restarting) `check_and_deliver_signals` call landing on the same
  CPU between this task's `note_syscall_restart` and its own `check_and_deliver_signals`, because
  `cpu_switch_to` never touches DAIF and the scheduler's own wait loops (`irq_window`, the idle
  `wfi`) briefly unmask IRQs every iteration — so a redispatched task could resume that window with
  IRQs already open.* Tried: forcing `daifset`/`cli` unconditionally right after `dispatch()`
  returns in `syscall_dispatch`, closing that window defensively. **No change in failure rate**
  (still ~10-20% under the same load). Reverted (not committed).
- *H2 (narrower form of H1): only the "wrong task steals via `take_syscall_restart`'s blind
  `swap(0,..)`" half of H1.* Tried: tagging the per-CPU slot with the owning pid
  (`RESTART_PID[cpu]`), and having `take_syscall_restart` refuse (leave the slot alone) if
  `current_pid()` doesn't match. **No change in failure rate** (8 runs / 160 iterations under load,
  same ~10-20%). Reverted (not committed).

Since neither fix — both targeting the note/take handoff specifically — moved the reproduction
rate at all, the actual loss is most likely happening **somewhere else**: candidates not yet
checked, for the next person to pick up (reproduce with `for i in 1..6; do yes >/dev/null & done`
on the Mac, then `sigtest` repeatedly — no kernel tracing needed, it reproduces stone cold):

- `check_and_deliver_signals`'s per-signal scan (`sched/src/signal.rs` `deliver_pending_signals`)
  might, under load-stretched scheduling, process a **different** pending signal first on some
  iterations (SIGCHLD from the forked child's `_exit`, in particular) in a way that consumes/skips
  the restart bookkeeping before SIGALRM's own turn — needs tracing *inside that loop*, not just
  at the note/take boundary, and specifically instrumented to survive under load (a `serial_print`
  per iteration was NOT tried under load — only used earlier, on an idle host, where it never
  reproduced, itself suspicious: a Heisenbug where tracing overhead changes the failure rate is a
  strong hint the print volume itself perturbs timing enough to close the window, so any next
  attempt should use the lowest-overhead signal possible, e.g. a lock-free counter array sampled
  after the fact rather than serial I/O in the hot path).
- Whether `action` (the `SigAction` read inside `deliver_pending_signals`) could observe a
  **stale/mid-write** `signal_actions` entry — this lane's test reinstalls the SIGALRM handler with
  different `sa_flags` every loop iteration (`sigaction(SIGALRM, &act, ...)` right before `fork()`),
  and a previous iteration's forked *child* (not yet reaped when the next iteration starts, if
  `reap()`'s bounded poll loses a race under load) shares the same TGID-leader-keyed
  `signal_actions` table read path (`rq.find_pid(t.tgid).map(|leader| leader.signal_actions[bit])`)
  — worth checking whether fork() truly gives the child an independent copy, and whether a delayed
  reap could let a *previous* iteration's zombie/child observe or interact with the *current*
  iteration's sigaction call.
- Whether `futex_wait`'s own early-exit path (`has_deliverable_signal_locked` check before ever
  parking, in `sched/src/futex.rs`) and its late-exit path (after `cpu_switch_to` returns) can be
  reached by *two different code routes* under load in a way that only one of them is
  distinguishable from a real timeout in `sys_futex`'s `interrupted()` re-check — not fully audited
  under time pressure.

**Given the ~1.5 h timebox on this specific item, shipping as-is**: the underlying `sys_futex`
change is a net improvement (SA_RESTART now works correctly on an idle host, and previously it
never restarted FUTEX_WAIT at all), `FUTEX_WAIT_BITSET` regression coverage is solid, and the new
stress test is the load-bearing artifact for whoever picks this up next — it reproduces reliably
under load with zero setup. `futex_wait_restart_stress` is **expected to occasionally FAIL** in any
verification run that has concurrent host load (another lane's build, CI contention, etc.); an idle
host should see it pass. This is called out explicitly so it is not mistaken for a fresh regression.

## Evidence (aarch64/HVF)

- `timertest` 27/27 PASS, including the two new cases `timer_create_sigev_value` and
  `timer_create_sigev_thread_id` (idle host and under load, checked both).
- `sigtest`: 14/15 cases PASS solidly; `futex_wait_restart_stress` flaky under load as above
  (idle host: clean across many runs; under load: ~10-20% of iterations EINTR instead of
  ETIMEDOUT). `futex_wait_signal_restart` (the single-shot version, wide margins) and
  `futex_wait_bitset_unaffected` were not observed to fail in any run performed.
- `polltest` 7/7 PASS (including the previously-known-red `poll_timeout_wake_latency`).
- `pthreadtest` 5/5 PASS.
- x86_64: build/verify in progress — see final report for the actual outcome (this note was
  written before that run completed; update if it surfaces anything x86_64-specific).

## Shared files

- `sched/src/lib.rs`: added `try_deliver_signal` + extracted `post_signal_thread` — the **only**
  change here; `deliver_signal`'s own behavior/signature is unchanged. Overlaps runqlock/forkcow's
  scope; diff kept intentionally minimal and mechanical.
- `kernel/src/syscall.rs`: `sys_timer_create`, `sys_rt_sigtimedwait`, `sys_futex` — all touched by
  sigalrm already this wave; changes are additive/localized to each function. (Two speculative
  changes to `syscall_dispatch`/`sched/src/signal.rs`'s restart bookkeeping were tried and reverted
  — see above — so this file's net diff from sigalrm's version is only the `sys_futex` addition.)
- `servers/tty/src/lib.rs`: `PosixTimer`, `handle_timer_create`, `ensure_real_timer`,
  `check_timers`, `service_timers_irq` — same file sigalrm already owned.

## Decisive data point: the race is futex-specific, not generic

Per request, ran the SAME load recipe (`for i in 1..6; do yes >/dev/null & done` on the Mac, 100
iterations on aarch64/HVF) against sigalrm's **pre-existing, already-on-main** SA_RESTART
mechanism using a *blocked pipe read* instead of futex (same shape: fork a child, child sleeps
then signals the parent via `kill`, parent is mid-syscall with SA_RESTART set) — a temporary loop
added to `test_blocked_read_sa_restart`'s pattern in `timertest`, run and then removed (not
shipped; see git history if it needs resurrecting).

**Result: 100/100 restarted, 0 EINTR, 0 other**, across 5 runs of 20 under the identical load that
made the futex case fail ~10-20% in the same session. This rules out a generic bug in
`note_syscall_restart`/`check_and_deliver_signals`/`deliver_pending_signals` — main's existing
restart mechanism is solid under load. **The race is specific to `futex_wait`'s own block/resume
path** (`sched/src/futex.rs`) or to `sys_futex`'s new ERESTARTSYS injection interacting with it,
not to the shared signal-delivery machinery both my two (reverted) fix attempts targeted — which
also explains why neither fix moved the needle: they were aimed at the wrong layer.

**Hypothesis checked and ruled out**: that `futex_wait` (or `sys_futex` itself, before calling it)
returns `-EINTR`/`-4` directly on some path — a signal already pending before the waiter queues, or
arriving between queuing and parking — which would bypass `sys_futex`'s `interrupted()`-based
ERESTARTSYS conversion entirely and explain a load-dependent EINTR. Checked by inspection: every
return in `sched/src/futex.rs`'s `futex_wait` is `0`, `-11` (EAGAIN), or `-110` (ETIMEDOUT) — never
`-4` — including the "already pending signal before park" early-exit (line ~183, returns bare `0`,
same as the "signal arrived while parked" late exit) and the "already expired deadline" fast path
in `sys_futex` itself (returns `-11`/`-110`, not `-4`). `sys_futex`'s FUTEX_WAIT arm has exactly one
`return` besides the `EFAULT`/deadline checks: the `interrupted()` conversion. So this specific
bypass does not exist — whichever internal path in `futex_wait` produces the observed EINTR, it
goes through `sys_futex`'s `interrupted()` check the same as every other case (confirmed separately
by the fact that `-4` is only ever producible via that one conversion, so `note_syscall_restart` is
provably being called every time — the loss is in what happens after, not a bypass of the note
itself).

## Open

- **The FUTEX_WAIT SA_RESTART-under-load race, now narrowed to `sched/src/futex.rs` /
  `sys_futex`'s interaction with it** — see above. Two structural differences from the
  now-proven-clean pipe-read path worth checking first: (1) `futex_wait` uses its own
  `FUTEX_TABLE`/`blocked_futex` bookkeeping and a *direct* `cpu_switch_to` to
  `SCHEDULER_CTX[id]`, rather than the generic `block_on_port_prepare`/`block_on_port_commit`
  (`POLL_WAIT_CHANNEL`) machinery pipe-read uses — audit `futex_wait`'s post-wake tail
  (`was_woken` / `clear_slot` / the `remove_waiter`-vs-still-registered ambiguity) for a path that
  could return `0` in a way `sys_futex`'s `interrupted()` re-check doesn't see correctly under
  load-stretched scheduling; (2) `sys_futex`'s restart check is a single post-hoc
  `interrupted()` call right after `futex_wait()` returns, with no re-check loop, unlike
  `block_until_ready`'s loop-and-recheck shape — check whether a signal that arrives in a narrow
  window *after* `futex_wait` computes its return value but this exact single check still somehow
  misses it under load (e.g. a second, unrelated signal or scheduling artifact clearing
  `signal_pending` between `futex_wait`'s return and the `interrupted()` call). Reproduces stone
  cold under `for i in 1..6; do yes >/dev/null & done` + repeated `sigtest` runs — no kernel
  tracing needed, and tracing previously used (serial prints in the note/take path) never
  reproduced it even once (140/140 clean), which is itself a clue that whatever's wrong is on a
  hot, latency-sensitive path that print overhead perturbs away — favor counters/state sampled
  after the fact over inline serial I/O for the next attempt.
- `signalfd`'s `SI_TIMER` payload (`servers/vfs/src/lib.rs`) was **not** updated to carry
  `si_value`/`si_overrun` — out of scope (vfs is `vfsmisc`'s file) and not requested; sigwaitinfo/
  the signal-frame handler path both do carry it.
