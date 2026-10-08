# lane compchurn — 2026-09-24/26

Branch `lane/compchurn` @ `918d8688` (9 WIP commits on top of `a5b5b62`; a
squash into two commits was drafted but the history rewrite + force push was
blocked by the permission classifier, so it's left to the orchestrator/user).

## Changes
- **Idle wakeups** (sched/src/lib.rs, kernel/src/syscall.rs, servers/vfs/src/lib.rs):
  - A timerfd expiry now wakes only the pollers whose mask names that timerfd (it used to wake every parked poller).
  - inotify fds get their own poll class (`poll_class::INOTIFY`) instead of `POLL_TAG_ALL`.
  - `epoll_ctl` wakes only that instance's waiters (`poll_class::EPOLL`, slot). `epoll_wait` parks and `epoll_tag_mask` (nested) include `epoll_self_tag(slot)`.
  - wait4/waitid park on `poll_tag(WAIT, tgid)` (1 s backstop) instead of re-polling every 20 ms.
- **mm/exec**: `prefault_range` stops at an absent file-backed page and the caller faults it on the unlocked path. execve loads the image and interpreter before taking the EXEC_ARGV/ENVP buffers. The interpreter is demand-mapped through the private-mmap registry.
- **virtio-gpu**: context ids <256 are recycled, preferring ids with parked chains, so a dead context's abandoned fence chain gets answered and reclaimed. Root ioctl 0x100A (`drmsmoke --ctx-drain-us N`) forces that path for testing.
- Instrumentation is compile-time off: `sched::idlestat`, `CHURN_STATS`.

## Evidence (laptop, x86_64/KVM, idle 10 s windows, stats-on build)
| | greeter disp/s | session disp/s |
|---|---|---|
| venus, after inotify tag only | 1072 | 741 |
| venus, + epoll_ctl narrow | 586 | 290 |
| virgl, after inotify tag only | 978 | 685 |
| virgl, + epoll_ctl narrow | 515 | 234 |

Final build (stats off): host QEMU CPU at idle was greeter 6.3 % / session 2.9 % (venus) and 6.6 % / 2.5 % (virgl), down from ~7.4–7.8 % / 3.6–4.1 %.

Reclaim: 3/3 abandoned chains were answered and reclaimed. drmsmoke failed=0.

Test suite (14 tests): all green on venus. On virgl everything was green except `timertest blocked_read_sa_restart` (see below).

## Open
- `timertest blocked_read_sa_restart` reported 144 ms < 150 ms once on virgl. The test is racy: the child starts its 150 ms sleep right after fork, and the parent takes t0 later, so el < 150 ms is possible without any kernel bug. It is not a lost wake, because the read returned early, not late. The rerun result and the aarch64 result are recorded in the final report.
- The greeter still does ~1000 dispatches/s (`sched/src/lib.rs:1886` deadline wakes, `net` 2024/2619) — next target.
