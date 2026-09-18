# lane/timespec — one monotonic source, one realtime source (2026-09-18)

Machine: Mac. Iterated on aarch64/HVF, verified x86_64/TCG. Base `7a9ed31`.

## Root cause

`lane/hvfclock` gave the kernel an exact `monotonic_ns()` (CNTVCT / TSC), but every
*timed wait* still expressed its deadline in 100 Hz **ticks**: `sys_nanosleep`
(`sleep_ticks_from`), relative `FUTEX_WAIT`, `poll`/`ppoll`/`select`/`pselect6`,
`epoll_wait`, `rt_sigtimedwait`, the vfs timerfd pool, and every kernel-internal
park (`wait4`, console read, flock/F_SETLKW, net poll cadence, VT_WAITACTIVE, drm
fence/syncobj waits). Two consequences, both measured on the baseline:

- a relative timeout was **floored** to whole ticks, so any wait under 10 ms became
  0 and returned at once: `poll(…, 1 ms)` came back in 17 µs, `poll(NULL,0,5)` in
  14 µs (and `nfds == 0` short-circuited before the timeout anyway), `FUTEX_WAIT`
  3 ms released on the very next tick;
- a deadline of "tick N" was released at N's *edge*, up to 10 ms before the
  requested interval had elapsed on the clock userspace measures with:
  the pre-existing `sleep200ms_measured_ns` sample read **196 883 959** on the
  baseline (nanosleep returned 3.1 ms early).

The wall clock was two clocks: `clock_gettime(CLOCK_REALTIME)` was `monotonic_ns()`
(boot epoch) while `gettimeofday`/`time` were `ticks/100` — 0–10 ms apart, drifting
inside every tick. `timerfd_create` ignored its clockid. `epoll_pwait2` was routed to
`sys_epoll_wait`, which read the `timespec *` as milliseconds (a finite epoll_pwait2
timeout waited forever — the baseline run of the new test hung there). `pselect6`
read its `timespec` as a `timeval` (1000× too long).

## What changed

- **sched**: `Task::poll_deadline`, `NEXT_POLL_DEADLINE`, futex deadlines and
  `service_poll_deadlines(now)` are absolute `sched::monotonic_ns()` readings.
  New `sched::monotonic_ns()`, `realtime_ns()`, `set_realtime_ns()`,
  `realtime_to_monotonic_ns()` (the wall clock is `monotonic + offset`). Task dump
  prints `dl_ms=`.
- **kernel/syscall.rs**: `read_user_timespec`/`deadline_after_ns` helpers; nanosleep →
  `sleep_until_ns` (rmtp from the clock); clock_nanosleep ABSTIME honours
  CLOCK_REALTIME; FUTEX_WAIT relative in ns, FUTEX_WAIT_BITSET honours
  FUTEX_CLOCK_REALTIME; poll/ppoll/select/pselect6/epoll_wait/rt_sigtimedwait in
  ns; `poll(NULL, 0, ms)` sleeps; new `sys_epoll_pwait2` (timespec); `sys_select`
  takes a timespec-vs-timeval flag; `clock_gettime` REALTIME/REALTIME_COARSE from
  the wall clock, unknown ids EINVAL; new `clock_settime`/`settimeofday` (root);
  `gettimeofday`/`time` from `realtime_ns`; `timerfd_create` validates and forwards
  the clockid; `timerfd_settime` forwards `it_value` + absolute flag;
  `poll_deadline_tick` compares against `monotonic_ns()`.
- **servers/vfs**: timerfd pool in ns (`deadline_ns`/`interval_ns`/`clockid`);
  settime resolves relative → `monotonic + value`, absolute MONOTONIC as-is,
  absolute REALTIME through the offset; lock waits park 10 ms in ns.
- **servers/tty, servers/net, drivers/drm**: internal parks converted (drm's
  `syncobj_deadline_ns` now passes the absolute ns straight through — no tick
  rounding).
- **RTC**: `arch/aarch64/src/rtc.rs` (PL031 at 0x0901_0000, virt build only — mapped
  next to the GICv3 frames) and `arch/x86_64/src/rtc.rs` (CMOS, BCD/12h aware,
  UIP wait, double read); `kernel/main.rs::time_init()` seeds the wall clock after
  arch init: `[RTC] epoch seconds: 1789704065` on both QEMU targets. Without an RTC
  (Pi builds) CLOCK_REALTIME starts at the boot instant, as before.
- **userland/timertest**: 9 new cases (see below), late bound 50 ms (lateness is
  tick + RUN_QUEUE-contended-tick + hypervisor idle-exit latency; the cases are
  about *early*).

Wake latency is still tick-bounded (≤ 10 ms nominal): a sub-tick request parks
until the first tick at or after its deadline. That is POSIX-correct ("at least")
and the same cost as before; a one-shot timer would be the next step.

## Evidence

`timertest` **19/19** on both arches (10 pre-existing + 9 new):
`nanosleep_500us_never_early` (40 samples across the tick phase; min 1.12 ms/0.95 ms),
`futex_wait_relative_3ms` (min 6.3/5.4 ms, ETIMEDOUT), `poll_1ms_never_early`
(min 4.2/1.4 ms; `poll(NULL,0,5)` 9.99 ms), `epoll_wait_1ms_never_early` (incl.
epoll_pwait2 2 ms → 10.6/6.1 ms), `select_1ms_never_early` (pselect6 1 ms → ≥ 6 ms;
select(2) on x86_64 9.9 ms), `gettimeofday_matches_realtime` (worst skew 0 ns, `time()`
agrees), `realtime_is_not_uptime` (epoch 1789704188, offset drift 291 ns / 1 µs over
20 ms), `timerfd_realtime_vs_monotonic` (abs REALTIME +50 ms → 56.7/58.0 ms; abs
MONOTONIC → 59.6/57.8 ms; both report relative remaining),
`clock_nanosleep_realtime_abstime` (+20 ms → 20.6/29.4 ms).

- aarch64 / **HVF** (Mac): timertest 19/19, epolltest 10/10, wakepolltest all PASS,
  sigtest 11/11, idletest 2/2. `clockdrift.py` 20 s: err +0.46 % / −2.47 % (10 s) —
  the tool's serial-arrival timing is ±300 ms noisy with the desktop live; `date`
  reads the same counter as before this lane.
- x86_64 / **TCG** (Mac): timertest 19/19, epolltest 10/10, wakepolltest all PASS,
  sigtest 11/11, idletest 2/2, `sleep200ms_measured_ns=202605782`. `clockdrift.py`
  20 s: err −0.11 %.
- Baseline (before the fix, aarch64/HVF, same test build): `futex_wait_relative_3ms`
  FAIL, `poll_1ms_never_early` FAIL (17 µs), `sleep200ms` 196.9 ms,
  `epoll_wait_1ms_never_early` **hung** in epoll_pwait2.

## Pre-existing, not this lane

- `polltest` `pipe_epoll_pollout_reflects_ring_full` FAILs on main since `d7caad8`
  (lane/idlecpu) introduced the Linux PIPE_BUF POLLOUT rule: the case drains 256 B
  and expects EPOLLOUT, which now needs ≥ 4096 B free. Test expectation is stale.
- POSIX timers / `setitimer` (tty server) are still tick-based; `TFD_TIMER_CANCEL_ON_SET`
  is accepted but does not cancel on a `clock_settime` step.
- x86_64 `monotonic_us` scale (`snd.rs`) untouched — `lane/tsccal`.
