# Time

The kernel has exactly one monotonic clock — the architecture's free-running counter (CNTVCT on AArch64, the TSC on x86-64) — and one wall clock defined as that counter plus an offset. Every timed wait (`nanosleep`, `poll`/`select`/`epoll_wait`, futex timeouts, timerfds, POSIX timers) is an absolute nanosecond deadline on the monotonic clock, released by a per-CPU one-shot timer with the 100 Hz tick as a fallback, so a wait is never early and is late by microseconds rather than up to a tick.

## Overview

| Clock id | Source |
|---|---|
| `CLOCK_MONOTONIC`, `CLOCK_MONOTONIC_RAW`, `CLOCK_MONOTONIC_COARSE`, `CLOCK_BOOTTIME` | `sched::monotonic_ns()` = `arch_monotonic_ns()`: the counter against the boot epoch, sub-tick, never decreasing. Nothing suspends and nothing slews, so these are all the same clock. |
| `CLOCK_REALTIME`, `CLOCK_REALTIME_COARSE` | `sched::realtime_ns()` = `monotonic_ns() + REALTIME_OFFSET_NS` |
| `CLOCK_PROCESS_CPUTIME_ID`, `CLOCK_THREAD_CPUTIME_ID` | per-task `cpu_ns` accounting; the process clock sums the group's *live* threads (exited threads are not folded in, so it is a floor) |
| anything else | `EINVAL`, as Linux |

- **The wall clock** is seeded at boot by `time_init()` in [kernel/src/main.rs](../../kernel/src/main.rs) from the board's battery clock: the PL031 RTC on the AArch64 `virt` machine ([arch/aarch64/src/rtc.rs](../../arch/aarch64/src/rtc.rs), mapped next to the GICv3 frames) and the CMOS RTC on x86-64 ([arch/x86_64/src/rtc.rs](../../arch/x86_64/src/rtc.rs): BCD and 12-hour aware, waits out update-in-progress, double-reads). It prints `[RTC] epoch seconds: …`. Boards without an RTC (Raspberry Pi builds) start the wall clock at the boot instant.
- `clock_settime(CLOCK_REALTIME)` and `settimeofday` step it (root only; the monotonic and CPU clocks are `EINVAL`). `gettimeofday`, `time` and every `CLOCK_REALTIME` deadline derive from the same `realtime_ns`, so they cannot disagree. Filesystem timestamps (`UTIME_NOW`, `stat`) use it too, through `sched::register_clock_ns`, so `stat` agrees with `clock_gettime(CLOCK_REALTIME)`.
- `clock_getres` reports the arch timer's resolution.
- **x86-64 TSC frequency** is resolved once at boot (`resolve_tsc_khz` in [arch/x86_64/src/timer.rs](../../arch/x86_64/src/timer.rs)): a frequency stated by CPUID leaves 15H/16H or the hypervisor is preferred, and a PIT channel-2 window confirms it or stands in for it; the result is printed as `[TSC] … MHz`. Every kernel clock on x86-64 derives from it, including the audio driver's microsecond clock (`ff3dd7e`).

## How it works

### The tick and one-shot deadlines

**The 100 Hz tick** drives scheduling, the per-CPU watchdog and tick hooks. It is programmed as an **absolute grid**: on AArch64 deadline *k* is `EPOCH + k × interval` in CNTVCT units written to `CNTV_CVAL_EL0`; on x86-64 the LAPIC runs periodic against a TSC grid. When the handler finds more than one grid point has passed (a long IRQ-masked section, a stalled vCPU, a lost timer edge later repaired by the alive check) it does *not* fire the missed ticks back to back: it skips to the next future grid point and credits the elapsed count to `TIMER_TICKS`, so `sched::ticks()` stays a faithful 10 ms count without an interrupt storm. See [arch-aarch64.md](arch-aarch64.md) and [arch-x86_64.md](arch-x86_64.md) for the per-arch timer hardware.

**Deadlines** do not wait for the tick. Every timed park funnels through `sched::register_poll_deadline` (or `block_on_poll_prepare_*`, which records the deadline in `Task::poll_deadline` and calls it), which folds the deadline into the lock-free `NEXT_POLL_DEADLINE` hint and arms the **current CPU's one-shot timer** (`arch_timer_arm_deadline`). The arch keeps each CPU's earliest pending instant and only reprograms when the new one is sooner.

- **AArch64**: `CNTV_CVAL = min(next grid point, deadline)`. A pure deadline interrupt (no grid point passed) returns before the BSP's polling, tick hooks and `timer_tick_irq`.
- **x86-64**: the tick stays LAPIC-periodic; a deadline, and the "realign to the next tick" that follows it, are **one-shot** countdowns, in a per-CPU `ST_PERIODIC` / `ST_DEADLINE` / `ST_REALIGN` state machine. The count is the TSC delta scaled to APIC ticks, with a +1/256 late bias. `arch_timer_check_alive` (once a no-op on x86) restores periodic mode if a one-shot interrupt is lost for 4 ticks.

When a CPU's one-shot fires, `sched::timer_deadline_irq()` services the kernel's high-resolution timers (`sched::hrtimer`) and then the registered deadline hook, `poll_deadline_service(now)` in [kernel/src/syscall.rs](../../kernel/src/syscall.rs): it wakes every parked waiter whose deadline is due or whose interest set names an expired timerfd (`service_poll_deadlines`, under one `RUN_QUEUE` try-lock hold that also recomputes `NEXT_POLL_DEADLINE` exactly), services POSIX timer expiry (`tty_server::service_timers_irq`), and returns the next pending deadline for the arch to re-arm. Everything in IRQ context is try-lock only: if `RUN_QUEUE` cannot be taken within a bounded 50 µs spin, the service retries 200 µs later. The BSP's 100 Hz tick still runs the same service as the fallback, so a lost one-shot costs at most one tick of lateness.

The recompute-under-the-lock is load-bearing. The old service woke due waiters and then stored `u64::MAX` into the hint; that store raced `register_poll_deadline`'s lock-free `fetch_min`, and a deadline published in the window was wiped, stranding a `nanosleep` or finite `epoll_wait` (which have no edge source) until an unrelated deadline happened to fire — seconds late, or never.

### Converting user time

All timespec parsing goes through `read_user_timespec` (`EFAULT` for a bad pointer, `EINVAL` for a negative field or `tv_nsec ≥ 10⁹`, the checks POSIX requires of every consumer), and relative timeouts become absolute deadlines with `deadline_after_ns(rel)` = `monotonic_ns().saturating_add(rel)`; a saturated `u64::MAX` deadline means "no deadline" (a 584-year timeout is one). `poll`'s and `epoll_wait`'s millisecond `int` timeouts are read from the low 32 bits, so a `-1` that arrives zero-extended still means "forever".

| Call | Timeout semantics |
|---|---|
| `nanosleep`, relative `clock_nanosleep` | Relative ns. On interruption: writes the remaining time to `rmtp`, then `EINTR` if a handler ran, else resumes the *remainder* through the restart block |
| `clock_nanosleep(TIMER_ABSTIME)` | Absolute on the named clock; `CLOCK_REALTIME` is translated through the wall-clock offset; CPU-time clocks are `EINVAL`; a past deadline returns 0 at once; restarts with the same absolute time |
| `FUTEX_WAIT` / `FUTEX_WAIT_BITSET` | Relative / absolute (monotonic or `FUTEX_CLOCK_REALTIME`); see [threads-and-futex.md](threads-and-futex.md#timeouts) |
| `poll`, `ppoll`, `select`, `pselect6` | ms / timespec / timeval / timespec; ppoll, pselect6 and select write the time left back, poll does not; `poll(NULL, 0, ms)` is a real sleep |
| `epoll_wait`, `epoll_pwait`, `epoll_pwait2` | ms, ms, timespec |
| `rt_sigtimedwait` | Relative timespec |

Writing `rmtp` is not a nicety: `std::thread::sleep` passes the *same* timespec as both arguments and re-sleeps whatever it finds there, so an unwritten `rmtp` made every `EINTR` restart the full duration from scratch. The request is read out before anything is written back, since callers may pass `rqtp == rmtp`.

### POSIX timers and itimers

`timer_create`/`timer_settime`/`timer_gettime`/`timer_delete`/`timer_getoverrun`, `setitimer`/`getitimer` and `alarm` are backed by a per-process timer table in [servers/tty/src/lib.rs](../../servers/tty/src/lib.rs) (`TIMER_TABLES`, keyed by TGID, `MAX_TIMERS` = 8 per process, one table per process up to `MAX_PROCESSES`).

- **Handle encoding** — `timer_t` values are handed out as `slot + 1`, never the bare index, since a raw `0` cast to a pointer is indistinguishable from `NULL` and relibc rejects a `NULL` `timer_t` with `EFAULT`.
- **Nanoseconds, never ticks** — `it_value`/`it_interval` are kept in ns against `monotonic_ns()`. With 100 Hz ticks a 5 ms `it_value` floored to 0 ticks and silently *disarmed* the timer, and sub-tick remainders fired early. Invalid fields are `EINVAL`.
- **Overrun accounting** — a timer descheduled across more than one period has its deadline caught up in a single step; the skipped periods accumulate in a per-timer counter read (and reset) by `timer_getoverrun()`. An expiry that coalesces into an instance of the signal still pending in the group also counts as an overrun.
- **`alarm()`/`setitimer(ITIMER_REAL)`** share one reserved, idempotently re-armed slot (slot 0) rather than allocating a fresh one per call, so repeated use cannot exhaust the table. `ITIMER_VIRTUAL` and `ITIMER_PROF` are accepted and do nothing.
- **`sigevent`** — `SIGEV_SIGNAL` (process-directed, carrying `sigev_value` into `si_value` with `SI_TIMER`, `si_timerid` and `si_overrun`), `SIGEV_THREAD_ID` (delivered to the named thread), `SIGEV_NONE` (armed and readable via `timer_gettime`, never signals). `SIGEV_THREAD` has no kernel meaning and is accepted silently. A bad signal number is `EINVAL`; a `NULL` sigevent means `SIGALRM`.
- **Delivery** — expiry is serviced from the one-shot deadline interrupt (`service_timers_irq`, try-lock only, keeping an exact `NEXT_TIMER_DEADLINE` hint) and delivered with `sched::try_deliver_signal_process`, which also broadcasts the poll channel under the same `RUN_QUEUE` hold. The original mechanism, a check on every syscall return (`check_timers`), remains as a fallback. Before the IRQ path, a process parked in `pause()`/`sigsuspend()` with no other wake never saw its `SIGALRM`.
- **User memory** — the in-kernel tty server touches user pointers only through `AddressSpace::read_user_buf`/`write_user_buf`, for the same reason `wait`/`waitid`/`flock` do: a supervisor-mode fault on a not-yet-faulted CoW page has no recovery path.

Measured after the IRQ-context delivery landed (`bac8719`; aarch64/HVF, x86_64/TCG): `alarm(1)` + `pause` 1003.4 / 1000.8 ms; `setitimer` 20 ms + `sigsuspend` 20.3 / 20.9 ms; `timer_create` 15 ms + `sigwaitinfo` 15.06 / 16.4 ms; a 5 ms periodic timer ×10 via `sigsuspend` 55.4 / 50.7 ms with zero early expiries.

### timerfd

`timerfd_create`/`settime`/`gettime` live in the VFS ([servers/vfs/src/lib.rs](../../servers/vfs/src/lib.rs), a global pool of 256 refcounted slots): each entry holds `deadline_ns`, `interval_ns`, accumulated `expirations` and `clockid`. `timerfd_create` accepts `CLOCK_REALTIME` and `CLOCK_MONOTONIC` (`CLOCK_BOOTTIME` is `CLOCK_MONOTONIC`), `EINVAL` otherwise. `settime` resolves a relative value to `monotonic + value`, an absolute `CLOCK_MONOTONIC` value as is, and an absolute `CLOCK_REALTIME` value through the wall-clock offset; `gettime` reports the relative remainder in every case. Arming publishes the deadline (`register_poll_deadline`), and the deadline service wakes exactly the pollers whose interest mask carries that timerfd's tag. Missed periods are folded into `expirations` in one step (`timerfd_poll_expirations`), shared by `read`, `FIONREAD` and `poll` so none of them can under-report.

Three bugs shaped it: `TFD_TIMER_ABSTIME` was ignored and relative timerfds could fire early (`9848025`); a periodic interval under 10 ms truncated to 0 and turned the timer into a one-shot (`b7cf240`); and a tick whose `RUN_QUEUE.try_lock()` failed — which it did on more than half of ticks with a desktop compositor up — could drop the poll edge entirely. `TFD_NONBLOCK`/`TFD_CLOEXEC` must be recorded at creation: the `polling` crate (calloop's poller) reads its timeout timerfd expecting `EAGAIN`, and with the flag dropped the kernel's read path yield-spun on it, pinning a single-threaded compositor in one read so it stopped accepting Wayland clients.

### Kernel high-resolution timers

[sched/src/hrtimer.rs](../../sched/src/hrtimer.rs) gives kernel code one-shot and periodic timers with callbacks, driven by the same per-CPU one-shot interrupt (with the tick as fallback). A callback runs either in hard-IRQ context (`TimerMode::Irq`: short, must not sleep) or on the system work queue (`TimerMode::Worker`). A periodic timer re-arms by skipping missed periods rather than bursting, and a callback still running at its next period skips that period. `MAX_ACTIVE` = 128 armed timers system-wide.

## Design decisions

### A tick counter is not a clock

**Choice.** `monotonic_ns()` reads the free-running counter directly; the tick is kept on an absolute grid with catch-up rather than reloaded from "now" (`67c9ba1`).

**Why (measured).** Until 2026-09-16, `CLOCK_MONOTONIC` was `TIMER_TICKS × 10 ms` plus a sub-tick fraction, and the AArch64 tick reloaded `CNTV_TVAL` (a countdown from *now*) in its handler, so every tick's interrupt latency was added to the period: a tick taken 4 ms late — the cost of a `wfi` wake through Hypervisor.framework — scheduled the next one 14 ms after the previous, and "100 Hz" ran at 71 Hz idle and 85 Hz under load. x86-64's LAPIC was periodic, but TCG coalesces late expiries, so one interrupt meant two periods. Drift against the host:

| Accelerator | Before | After |
|---|---|---|
| aarch64 / HVF | −5…−7% (one run −22%) | ±0.3% |
| aarch64 / TCG | −12…−17% | ±0.3% |
| x86_64 / TCG | −1…−36%, wildly variable | ±0.3% |

The defect was first described as an HVF bug; it was worse on TCG — measure on every accelerator before attributing drift to one (see [../development/methodology.md](../development/methodology.md)). Every wall-time baseline recorded before 2026-09-16 was taken on a 5–36% slow clock and is stale. The desktop panel clock now tracks the host within 1 s over 205 s. `clockdrift.py` in the QEMU driver tooling measures it ([../development/testing.md](../development/testing.md)).

### Deadlines are absolute nanoseconds; the tick bounds lateness, never earliness

**Choice.** One monotonic source and one realtime source for the whole timespec family (`60c2495`).

**Why.** After the counter fix, every timed *wait* still expressed its deadline in 100 Hz ticks, with two consequences measured on the baseline: a relative timeout was floored to whole ticks, so any wait under 10 ms became 0 and returned at once (`poll(…, 1 ms)` returned in 17 µs, `FUTEX_WAIT` 3 ms released on the very next tick); and a deadline of "tick *N*" was released at *N*'s edge, up to 10 ms before the interval had elapsed on the clock userspace measures with (`sleep(200 ms)` measured 196.88 ms). The wall clock was two clocks — `CLOCK_REALTIME` from the counter, `gettimeofday`/`time` from `ticks/100` — up to 10 ms apart. `epoll_pwait2` was routed to `epoll_wait` and read its `timespec *` as milliseconds (a finite timeout waited forever), and `pselect6` read its timespec as a timeval (1000× too long). All fixed together; `timertest` gained nine never-early cases.

### Arm a one-shot timer to each deadline

**Choice.** Per-CPU one-shot deadlines (`78ae75d`), instead of a finer tick.

**Why.** Never-early deadlines plus a 100 Hz release made back-to-back `poll(10 ms)` loops re-arm just past a tick edge and overshoot a whole tick every time. `polltest poll_timeout_wake_latency` (1000 × `poll(idle pipe, 10 ms)`, overshoot in µs):

| | p50 | p90 | p99 | early |
|---|---|---|---|---|
| tick-only, aarch64/HVF | 9728–9944 | 10072–10145 | ≤ 11.2 ms | 0 |
| one-shot, aarch64/HVF | 316–481 | 1339–1793 | 2684–4473 | 0 |
| one-shot, x86_64/TCG steady state | 201–250 | 258–310 | 321–879 | 0 |

Knock-on: the minimum `nanosleep(500 µs)` went from 1.12 ms to 0.53 ms, `FUTEX_WAIT` 3 ms from 6.3 to 3.2 ms, `poll` 1 ms from 4.2 to 1.26 ms. A finer periodic tick would have cost every CPU interrupts all the time; a one-shot costs one interrupt per deadline that is actually pending.

**Hazard found on the way (keep in mind for any LAPIC work).** The first x86 attempt kept the LAPIC periodic and wrote a short initial count. QEMU's `apic_timer` re-arms a periodic timer from its previous `next_time`, so a microsecond-scale period that falls behind makes QEMU's main loop spin in `timerlist_run_timers` under the big lock forever: the guest froze right after `login:` with the monitor and serial sockets refusing connections. Fix one: one-shot mode for short countdowns. Fix two: the restore to periodic must write the full initial count *before* flipping the LVT to periodic — the reverse order left a one-MMIO window of "periodic with a tiny count" and reproduced the wedge about ten minutes in.

### Round up, never truncate, when converting time

**Choice.** Every relative interval becomes a deadline that is at least as far away as asked (`fb398c7`, since superseded by the nanosecond deadlines above).

**Why.** `sys_nanosleep` once computed `ticks = sec × 100 + nsec / 10_000_000` and returned at once on 0, so every sleep under 10 ms was a no-op. That passed every correctness test in the tree for months: a truncating sleep breaks nothing that checks *correctness*, only things that depend on *duration*. Mesa was the first program precise enough about its own timing to notice. Its Venus ring back-off (`vn_relax`) sleeps 160/320/640/1280 µs, all sub-tick, so all became no-ops; its 4096-iteration budget, which Mesa asserts takes 3.48 s, elapsed in 17 ms; and its ring-alive watchdog fired about 200× early and `abort()`ed. The symptom looked entirely host-side — QEMU's per-context render worker "died silently" with `proxy: failed to send message: Broken pipe` — and hours went into virglrenderer before it turned out that the worker's death was just the DRM fd closing behind a guest process that had already aborted. When a remote peer appears to die for no reason, check whether the local side quit first. `clock_nanosleep` was fixed in the same change: it dispatched straight to `nanosleep` and discarded `TIMER_ABSTIME`, turning "wait until this timestamp" into "sleep for that timestamp" — uptime-plus-seconds, or decades for a wall-clock deadline.

### Sub-tick CLOCK_MONOTONIC resolution

**Why.** A `vktest` hang under TCG was a deadlock between the guest and the host Venus ring thread: 96 bytes of a well-formed command sat unconsumed at the ring head because Mesa's notify throttle suppressed the wake-up — `clock_gettime` advanced in 10 ms steps, so the throttle never saw time pass. Giving `CLOCK_MONOTONIC` sub-tick resolution (`75b32e3`) fixed it; `vktest` now completes in 6–10 s on both TCG arches. The same reasoning moved evdev input timestamps and DRM page-flip timestamps off the 100 Hz tick (`05bb0fe`, `b8ff2f6`).

### One wall clock

**Choice.** `CLOCK_REALTIME` is `monotonic + offset`, and every realtime consumer — including absolute `CLOCK_REALTIME` deadlines, which are translated to the monotonic clock once — reads that one value. Stepping the wall clock changes only the offset; nothing that waits on the monotonic clock is disturbed.

## Limitations and open work

- **`timer_settime` ignores its flags**, so `TIMER_ABSTIME` is not honoured: an absolute `it_value` is armed as a relative interval — the same "absolute read as relative" class already fixed for `clock_nanosleep`, `FUTEX_WAIT_BITSET` and `timerfd_settime`. `timer_create` ignores its `clockid` (every POSIX timer runs on the monotonic clock).
- `TFD_TIMER_CANCEL_ON_SET` is accepted but a `clock_settime` step does not cancel the timer.
- `ITIMER_VIRTUAL` and `ITIMER_PROF` are no-ops.
- `sysinfo` reports uptime from the tick count and load averages of 0; `times(2)` reports zero CPU times.
- No NTP discipline (`adjtimex`/`clock_adjtime` are not implemented); the wall clock is the RTC at boot plus the counter.
- Wake latency is bounded by one-shot timer granularity plus `RUN_QUEUE` contention: tens of microseconds on KVM, coarser under TCG; a `[WDOG]`-class IRQ-masked stall still shows up as multi-millisecond lateness.

## Code and tests

| What | Where |
|---|---|
| `monotonic_ns`, `realtime_ns`, offset, `NEXT_POLL_DEADLINE`, `register_poll_deadline`, `timer_deadline_irq`, `service_poll_deadlines` | [sched/src/lib.rs](../../sched/src/lib.rs) |
| Kernel hrtimers | [sched/src/hrtimer.rs](../../sched/src/hrtimer.rs) |
| Arch timers, grid, one-shot state machines | [arch/aarch64/src/timer.rs](../../arch/aarch64/src/timer.rs), [arch/x86_64/src/timer.rs](../../arch/x86_64/src/timer.rs) |
| RTCs | [arch/aarch64/src/rtc.rs](../../arch/aarch64/src/rtc.rs), [arch/x86_64/src/rtc.rs](../../arch/x86_64/src/rtc.rs) |
| Clock and sleep syscalls, `poll_deadline_service`, timerfd syscalls, `timer_create` | [kernel/src/syscall.rs](../../kernel/src/syscall.rs) |
| POSIX timer table | [servers/tty/src/lib.rs](../../servers/tty/src/lib.rs) |
| timerfd pool | [servers/vfs/src/lib.rs](../../servers/vfs/src/lib.rs) |

Regression suites: `timertest` — POSIX timers, `alarm`/`setitimer`, real `SIGALRM` delivery, timerfd `TFD_TIMER_ABSTIME` on both clocks, sub-tick `clock_gettime` and timerfd intervals, and the never-early cases (`nanosleep_500us_never_early` across 40 tick phases, `futex_wait_relative_3ms`, `poll_1ms_never_early`, `epoll_wait_1ms_never_early` including `epoll_pwait2`, `select_1ms_never_early`, `gettimeofday_matches_realtime`, `realtime_is_not_uptime`, `timerfd_realtime_vs_monotonic`, `clock_nanosleep_realtime_abstime`, `itimer_subtick_arms_never_early`, `timer_create_sigev_value`, `timer_create_sigev_thread_id`, `timer_periodic_overrun`); `polltest poll_timeout_wake_latency`; `wakepolltest`; `idletest` (an idle system genuinely idles).

## See also

- [event-loop.md](event-loop.md) — the poll wait-channel that every timed park uses
- [threads-and-futex.md](threads-and-futex.md#futex_wait_bitset-is-an-absolute-deadline) — the futex side of "absolute read as relative"
- [signals.md](signals.md#syscall-restart) — how interrupted sleeps restart
- [scheduler.md](scheduler.md) — the tick, watchdog and idle
- [arch-aarch64.md](arch-aarch64.md), [arch-x86_64.md](arch-x86_64.md) — timer hardware
- [invariants.md](invariants.md) — round-up time conversion as a kernel rule
