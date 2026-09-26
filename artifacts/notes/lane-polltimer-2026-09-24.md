# lane/polltimer — one-shot timer armed to each deadline (2026-09-24)

Branch `lane/polltimer` @ `78ae75d` (base `fc5a6bc`). Machine: Mac (aarch64/HVF, x86_64/TCG).

## Result: FIXED

`polltest poll_timeout_wake_latency` (1000 × poll(idle pipe, 10 ms), overshoot µs):

| | p50 | p90 | p99 | max | early |
|---|---|---|---|---|---|
| main (wave-0918 verify), aarch64/HVF | 9728–9944 | 10072–10145 | ≤11.2 ms | ≤19 ms | 0 |
| polltimer, aarch64/HVF ×4 (greeter up) | 316–481 | 1339–1793 | 2684–4473 | 3.6–54.6 ms | 0 |
| polltimer, x86_64/TCG steady state ×7 | 201–250 | 258–310 | 321–879 | ≤9.8 ms | 0 |
| polltimer, x86_64/TCG during `[WDOG]` cosmic-comp mmap stall ×2 | 255–688 | 2186–3622 | 116–124 ms | 0.29–3.97 s | 0 (FAIL) |

The two x86 FAILs each landed in the one run per boot that overlapped greeter startup's
`[WDOG] cpuN took no timer tick for ~2 s … cosmic-comp last syscall 9 (mmap)` — the
pre-existing IRQ-masked stall (sessmisc's item), not the timer path. `early` is now
strict (any return before the timeout), was "> one tick early".

Knock-on (timertest, before → after on aarch64/HVF): nanosleep 500 µs min 1.12 → 0.53 ms,
FUTEX_WAIT 3 ms min 6.3 → 3.2 ms, poll 1 ms min 4.2 → 1.26 ms, pselect6 1 ms → 1.11 ms.
x86_64/TCG: nanosleep 0.56 ms, futex 3.05 ms, poll 1.15 ms, epoll 1.06 ms.

## Design

- `sched::register_poll_deadline` is the single funnel (poll/select/epoll prepare,
  futex, timerfd arm); it now also calls `arch_timer_arm_deadline(ns)` (new extern) to
  arm the CURRENT CPU. Arch keeps a per-CPU earliest pending deadline.
- On the timer IRQ, any CPU whose deadline is due calls `sched::timer_deadline_irq()` →
  registered hook `kernel::syscall::poll_deadline_service(now)` (the deadline half of
  `poll_deadline_tick`, split out; try-lock only) → returns the next pending deadline
  (min of `NEXT_POLL_DEADLINE`, timerfd pool); if the RUN_QUEUE try-lock lost, retry in
  200 µs. BSP 100 Hz tick still services deadlines as the fallback.
- aarch64: CNTV_CVAL = min(next grid point, deadline). A pure deadline IRQ
  (`passed == 0`) returns before BSP polling/tick hooks/`timer_tick_irq`.
- x86_64: LAPIC periodic for the tick; deadline + "realign to next tick" are ONE-SHOT
  countdowns (state machine PERIODIC/DEADLINE/REALIGN per CPU). APIC count =
  TSC delta × TICKS_PER_IRQ/TSC_PER_TICK, +1/256 late bias. `arch_timer_check_alive`
  (was a no-op on x86) restores periodic if a one-shot IRQ is lost for 4 ticks.

## Hazard found (x86_64/TCG wedge) — keep in mind for any LAPIC work

First x86 attempt kept periodic mode and just rewrote INIT to a short count. QEMU's
`apic_timer` re-arms a periodic timer from its previous `next_time`, so a µs-scale
period that falls behind makes the main loop spin in `timerlist_run_timers` under the
BQL forever: guest froze right after `login:`, monitor + serial sockets refused
(host `sample`: 2241/2310 samples in timerlist_run_timers/apic_timer, all vCPUs in
`qemu_mutex_lock`). Fix 1: one-shot mode for short countdowns. Fix 2: the restore to
periodic must write INIT (full) BEFORE flipping LVT to periodic — the reverse order left
a one-MMIO window of "periodic + tiny count" and reproduced the wedge ~10 min in.

## POSIX timers / setitimer (the "if cheap" part) — PARTIAL

`servers/tty` timer table and `setitimer`/`getitimer`/`alarm` now keep ns vs
`monotonic_ns()` instead of 100 Hz ticks: a 5 ms it_value used to floor to 0 ticks =
silently DISARMED; sub-tick remainders fired early. Invalid fields → EINVAL. New
timertest `itimer_subtick_arms_never_early` (itimer 5 ms → 6.07/5.57 ms; timer_settime
3 ms → 4.2/3.4 ms, aarch64/x86). NOT done: delivery is still only at the owner's syscall
return (`check_timers`), so a process parked in pause()/sigsuspend() with no other wake
never sees SIGALRM — needs a kernel-side expiry → `deliver_signal` from task context.

## Other suites (both arches, this build)
timertest 21/21, epolltest 11, wakepolltest 17, sigtest 11, idletest 2/2, pthreadtest 5,
drmsmoke failed=0 (18 KMS skipped: compositor is master), polltest other 6 cases PASS.
aarch64: greeter painted, 0 `[WDOG]`/panic. x86: 1 `[WDOG]` per boot (cosmic-comp mmap,
pre-existing).

## Host environment note (Mac)
Host build scripts failed to link (`tapi error: unknown architecture arm64e.x1` in
MacOSX27.0.sdk `libSystem.B.tbd`, CLT ld-1267). Workaround: `export
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX26.5.sdk` before `build-all.sh`.

## Shared files touched
`sched/src/lib.rs` (register_poll_deadline + new deadline hook, ~35 lines; runqlock
overlap — no RUN_QUEUE / pick_next changes), `kernel/src/syscall.rs`
(poll_deadline_tick split; itimer ns), `kernel/src/init.rs`.
