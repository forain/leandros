# lane/posixtime — POSIX timers, RT signal queueing, wall-clock steps (openbugs L10.2, L10.9, L10.11 part)

## What changed
- **timer_create clockid** (kernel `sys_timer_create`): REALTIME/MONOTONIC/PROCESS_CPUTIME/
  THREAD_CPUTIME/BOOTTIME/TAI arm; MONOTONIC_RAW and the COARSE clocks give EOPNOTSUPP; the
  _ALARM clocks need root (EPERM); anything else EINVAL (Linux `clockid_to_kclock`).
- **timer_settime TIMER_ABSTIME** (tty server): the value is an instant on the timer's clock;
  on a wall clock it is translated through the realtime offset, and `timers_clock_was_set`
  re-programs absolute CLOCK_REALTIME timers when the clock is stepped.
- **CPU-time timers**: CLOCK_PROCESS/THREAD_CPUTIME_ID timers, ITIMER_VIRTUAL (user time) and
  ITIMER_PROF (user+sys) work, serviced on return to user (`sched::cpu_timer_check`, only
  for groups with an armed CPU timer). alarm()/setitimer have their own ITIMER_REAL slot,
  no longer slot 0 of the timer_create table.
- **Load averages**: Linux-style fixed-point avenrun (5 s samples, 1/5/15 min) in sched,
  reported by sysinfo(2) and /proc/loadavg.
- **RT signals** (`sched/src/rtq.rs`): SIGRTMIN..SIGRTMAX instances queue FIFO with their
  siginfo (private queue before shared, as Linux dequeues), standard signals still coalesce;
  a POSIX timer never queues a second instance of itself (overrun instead), and
  timer_getoverrun is `it_overrun_last` of the last dequeued expiry.
- **SIGEV_THREAD_ID** delivers to the named thread with its sigev_value; a raw SIGEV_THREAD
  reaching the kernel is SIGEV_SIGNAL, as on Linux (EINVAL without a signo).
- **signalfd**: ssi_overrun, ssi_ptr/ssi_int filled for SI_TIMER and SI_QUEUE.
- **rt_sigtimedwait** now fills siginfo with the signal-frame writer
  (`sched::signal::write_siginfo`): SI_QUEUE's si_value used to be dropped.
- **Clock step** (`step_realtime_ns`): FUTEX_WAIT_BITSET|FUTEX_CLOCK_REALTIME and
  clock_nanosleep(CLOCK_REALTIME, TIMER_ABSTIME) waiters re-translate their deadline
  (`sched::realtime_waiters_clock_was_set`), as do absolute realtime POSIX timers.
- **relibc** (`/Users/forain/code/relibc`, src/header/unistd/alarm.rs): alarm() is
  setitimer(ITIMER_REAL) on non-Redox targets (was a hidden timer_create timer, which only
  shared state with setitimer through the old slot-0 coupling).

## Already fixed on main (no change needed)
- "POSIX timers tick-based": expiry already ran from the one-shot deadline IRQ
  (`register_poll_deadline`) with ns deadlines.
- times(2) zeros: `sys_times` already reports process + waited-for children time.

## Tests
- timertest `posixtime.rs`: clockids, ABSTIME, CPU timers, SIGEV_THREAD_ID notify thread,
  clock step moves realtime waiters, times + loadavg. sigtest `rtsig.rs` (`sigtest rtq`):
  queue order, handler per instance, signalfd RT fields, SIGRTMAX usable.
- aarch64: timertest 0 fails, sigtest 0 fails, regress PASS before rebase; after rebasing on 578b5be4 all green but host:nettag2test, which failed only under host load and passes under the qemu lock.

## Open
- relibc SIGEV_THREAD (callback in a helper thread, musl-style) is not implemented on the
  leandros/linux platform: a C program using SIGEV_THREAD gets a plain signal. The kernel
  side (SIGEV_THREAD_ID) the helper needs is done and tested.
