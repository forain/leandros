# lane/sigalrm — timer signals wake blocked targets (2026-09-24)

Branch `lane/sigalrm` @ `bac8719` (base origin/integ-wave-0924 `2e1d624`). Mac: aarch64/HVF, x86_64/TCG.

## Result: FIXED
- Expiry serviced from the one-shot deadline IRQ (`poll_deadline_service` → `tty_server::service_timers_irq`,
  try-lock only; `NEXT_TIMER_DEADLINE` hint folded into the re-arm). Arm path (`set_timer_ns`) calls
  `sched::register_poll_deadline`. Signal is process-directed (`sched::try_deliver_signal_process`, new),
  which also broadcasts the poll channel under the same RUN_QUEUE hold. Syscall-return `check_timers` stays as fallback.
- Coalesced expiry (instance already pending in the group) = overrun++.
- SA_RESTART: dispatcher maps internal ERESTARTSYS(-512) to EINTR + per-CPU (nr,a0) record;
  `check_and_deliver_signals` rewinds (x86 rax=nr, rip-=2; arm x0=a0, elr-=4) when handler has SA_RESTART or no
  handler ran. Restartable: block_until_ready (pipe/socket read/write/send/recv/readv), console read, wait4, waitid.
  Timed waits stay EINTR (Linux).
- pause() keeps current mask (was mask=0); rt_sigsuspend(NULL) EFAULT; sigtimedwait EINTR on other handled
  signals; timer_create SIGEV_NONE/THREAD → no signal, bad signo EINVAL.

## Evidence (timertest 26/26, sigtest, polltest 7/7, both arches; also waittest sigchldtest epolltest pthreadtest jobtest [+wakepolltest arm] RC=0; 0 WDOG/panic)
| | aarch64/HVF | x86_64/TCG |
|---|---|---|
| alarm(1)+pause | 1003.4 ms | 1000.8 ms |
| setitimer 20ms + sigsuspend | 20.3 ms | 20.9 ms |
| timer_create sig40 15ms + sigwaitinfo (SI_TIMER) | 15.06 ms | 16.4 ms |
| periodic 5ms ×10 via sigsuspend | 55.4 ms, early 0 | 50.7 ms, early 0 |
| read + SA_RESTART (writer at 150ms) | 150.3 ms, r=1 | 150.9 ms |
| read no SA_RESTART | EINTR 20.0 ms | EINTR 20.7 ms |
| polltest 10ms p50/p99 | 186/1654 µs | 1177/5262 µs |

`timer_periodic_overrun` rewritten: it relied on the sleeping process never seeing the expiry; now blocks SIGALRM for 300 ms.

## Shared files
sched/src/lib.rs (deliver_signal_process split into post_signal_process + try variant; no RUN_QUEUE/pick_next changes),
sched/src/signal.rs, kernel/src/syscall.rs, servers/tty/src/lib.rs.

## Open
si_value (sigev_value) not carried into siginfo; SIGEV_THREAD_ID delivered process-directed; futex wait not restartable.
