# lane/runqlock — RUN_QUEUE: address-space lookup, pid lookup, pick_next (2026-09-24/25)

Branch `lane/runqlock` @ `afca3c2` (base `fc5a6bc`). Files: `sched/src/lib.rs`, `sched/src/runqueue.rs`,
`sched/src/lockwatch.rs`. Timer/poll code is untouched (the overlap with polltimer is `take_over_leader`
and the dispatch loop in lib.rs: a few added lines each).

## Method
`HOLD_PROFILE=true` (and `SC_STATS=true` for the syscall census), aarch64/HVF, 4 vCPU. Two Ctrl-T `[RQPROF]`
dumps 60 s apart, per-site deltas. New columns added to the profile: contended acquires and total spin
time per site, a pick_next census (picks, full scans, tasks visited, ns), `ready_hint_miss`, and an
address-space census (acquires, contended, wait, hold, max). Windows: greeter idle; COSMIC session start
(login via QMP keys, first 60 s); desktop idle. Scripts: `/tmp/rql-flow.sh`, `/tmp/rql-parse.py`.

## Findings (base = fc5a6bc)
| window | total RQ holds/s | held % CPU | contended spin % CPU | top site |
|---|---|---|---|---|
| greeter idle | 7.0–7.9 k | 0.12–0.14 | 0.10 | pick 585 ns; lock_leader_as 1.1–1.4 k/s at 130–170 ns |
| **session start** | **338 k** | **10.2** | **18.9** | **lock_leader_as 296 k/s, 275 ns avg, 8.1 % held** |
| desktop idle | 11.1–11.4 k | 1.0–1.07 | 2.2–2.4 | pick 2.4 k/s at 1841–1855 ns; has_deliverable_signal 3.8–3.9 k/s at 939–980 ns |

- The "~30 k/s" figure was a greeter-era average. The site is negligible when idle (≈50/s on the idle
  desktop) and is the whole problem during session start (page-fault storm).
- On the idle desktop RUN_QUEUE is ~1 % held; the long holds are the O(256) scans (pick_next, and every
  `find_pid` — `has_deliverable_signal` does two).

## Changes
1. `CURRENT_AS[cpu]`: the running task's own `Arc<AddressSpace>` pointer, published at dispatch, cleared
   at switch-back, re-published by `replace_address_space` (execve). `lock_leader_address_space(pid ==
   current)` takes only the `busy` flag. Other pids still use the locked path.
2. `RunQueue::find_pid*`: bounded-probe pid→slot hint table (1024 entries, 8-slot window), verified
   against the slot, scan fallback, so it can never return the wrong task. Re-indexed in `take_over_leader`.
3. `pick_next`: occupancy bitmap plus a `maybe_ready` superset bitmap (set by every `&mut Task` accessor
   and the wake loops; `tasks` is now private so nothing can bypass it); once per tick it does a full scan
   that repairs and counts misses. Pass 2 visits only candidates.

## Results (same windows)
| window | total holds/s | held % | contended % | notes |
|---|---|---|---|---|
| greeter idle | 5.8–7.8 k | 0.07–0.10 | 0.08–0.12 | pick 265–403 ns |
| session start | **38 k** | **0.96** | **0.55** | lock_leader_as gone from the list |
| desktop idle | 10.9 k | 0.40–0.46 | 1.2–1.3 | pick 873–1039 ns; has_deliverable_signal 127–140 ns |

pick_next census on the desktop: about 41 tasks visited per pick, not 256. `ready_hint_miss=0` in every run.
Tick try_lock failures: 0–61 of 7.7–28 k.

## Verification
- aarch64/HVF: COSMIC session paints (panel, dock, wallpaper) ×5 boots. pthreadtest, forktest,
  smpwaketest, idletest, sigtest, waittest, wakepolltest, racetest, epolltest, jobtest, killmt 20
  exec_worker and parked_mix all rc=0. timertest failed 6 cases once with host load ~9 (other lanes' QEMUs),
  then passed 3/3 immediately after in the same boot. polltest `poll_timeout_wake_latency` is the known red
  (polltimer's lane). 0 panic, 0 [WDOG].
- x86_64/TCG: greeter paints. With the COSMIC session up: pthread, fork, smpwake, idle, sig, wait, wakepoll,
  timer, race and job pass. epolltest (2 timing cases) and killmt (a 47–57 MiB memory drop, because the
  session was starting next to it) failed. Re-run without a greeter: new kernel epolltest rc=0 and killmt
  exec_worker/parked_mix PASS mem +0 KiB. The base kernel gives the same result. The earlier greeter-up run
  had exec_worker at 8796 KiB, just over its bound, with the greeter being killed mid-run. `[WDOG]`
  cosmic-comp in fork (0x39) and mmap (0x9) on x86 TCG also show up on base: sessmisc's issue.
- Note: the worktree's x86_64 f2fs image now carries `/etc/leandros/text-login`.

## For the orchestrator's mmap-latency question
AS census (aarch64, whole boot through session start): 577 k acquisitions, 1.4 k contended (0.25 %). The
total wait is 1.5 s, the longest single wait 13.3 ms. `busy` holds add up to 28 s, and one hold lasted
**1.50 s**, during greeter/session startup. So a slow small mmap is waiting on another thread's long
address-space hold, not on RUN_QUEUE. The candidates are fork's eager copy (forkcow: about 210 MiB) or a
file-backed mmap copy under `busy`. On TCG that stretches ~10×, which matches 100–700 ms.

## cosmic-comp never-idle thread (characterize only)
SCSTAT charges wall time spent in a syscall, including time spent blocked, so `epoll_pwait` (nr 22) using
200–645 % of a CPU is 2–6 threads parked. It is not spinning. The busy part:
- The greeter's compositor (tgid 0xe) does about 56 mmap/s (0xde) and 53 munmap/s (0xd7), each paired
  with an `epoll_pwait` wake. The 0.1 Hz task census catches it `Running` inside mmap (sc=0xde, on a
  changing CPU) in 3 of 14 samples. The screen is static, so this is a per-wakeup buffer
  allocate/free cycle, not rendering.
- The logged-in desktop compositor does far less: mmap 0.5–3/s, futex 1–4/s. The steady RUN_QUEUE load
  on the idle desktop (11 k holds/s) comes from dispatch and signal checks, not from mm.
Next step for whoever owns it: find which cosmic-comp/greeter thread wakes about 55×/s and why it maps a
fresh buffer each time (a smithay/softpipe allocator without a reuse pool?).
