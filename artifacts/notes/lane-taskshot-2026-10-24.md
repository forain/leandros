# lane/taskshot — 2026-10-24 — TASKS hold/acquisition cuts (L3.1 follow-up)

Status: FIXED in QEMU (aarch64/HVF); laptop confirmation left to a later lane.

## (a) percpu_check — consistency checker, now once a second
`percpu_check` -> `RunQueue::check_all` is a consistency net (rqmiss/rq_bad/gen_miss/mirror/state
mirrors, hung-ready watchdog, park-hint repair; every miss counter has been 0 in every gate since
lane percpu). It took TASKS + every RQ lock once per tick (laptop: 100 x ~144 us = 14.4 ms/s,
72 % of TASKS hold). Now every `PERCPU_CHECK_TICKS` ticks, default 100 (1 s = the hung-ready
watchdog's own resolution; a healed rqmiss waits <= 1 s instead of <= 10 ms). Boot knob
`sched.check=N` (1 = old per-tick behaviour, 0 = off), parsed in kernel/src/init.rs.

## (b) tgid_of — lock-free authoritative miss
The pid->tgid side table (lane procmax) already served hits lock-free; every MISS (dead pid,
pid 0) took TASKS. The table is authoritative (MAX_TASKS 4096 < 16384 slots; every insert into
tasks[] goes through `enqueue`/de-thread, both publish first) except during a removal's backward
shift. Added a seqcount around `pid_tgid_remove` and an overflow flag on `pid_tgid_insert`; a miss
returns `pid` without the lock unless a removal overlapped or the table ever overflowed. pid 0
returns 0 directly (never allocated). `tgid_of`/`current_tgid` are now `#[track_caller]`, so any
residual lock-path acquisition shows its real caller in /proc/rqhold.
The hot laptop caller does not reproduce in QEMU: on an idle QEMU COSMIC desktop the tgid_of
lock site is < 10/s, so the 26-29k/s is hardware-specific (likely kernel/IRQ-context callers with
pid 0 or dead pids). The fix covers every miss regardless of caller; the next laptop census shows it.

## Numbers (QEMU aarch64 HVF, idle COSMIC --nofirefox, /proc/rqhold, two 10 s windows)
| | main (+track_caller) | taskshot |
|---|---|---|
| TASKS acq/s | 404 / 404 | 303 / 306 |
| TASKS held ms/s | 3.5 / 2.7 | 0.5 / 0.5 |
| TASKS contended/s | 15 / 14 | 11 / 13 |
| RQ* acq/s, contended/s, spin ms/s | 1004, 49, 0.82 / 1002, 58, 0.73 | 611, 2, 0.05 / 613, 2, 0.03 |
| percpu_check site holds/s, held ms/s | 99, 2.60 | ~1 |
Gate counters all 0 (rqmiss, rq_bad, gen_miss, mirror/state miss, hung_ready, pexit_miss).
Regress (aarch64) PASS; `/bin/schedstress full` PASS (one earlier boot failure under a fresh run
id = the known Limine/early-boot host-state hang, re-run passed).

## Open
- Laptop: re-run laptopdesk census.sh; expect TASKS held ~20 -> ~6 ms/s and the 1837 site gone;
  if TASKS acq/s is still high, /proc/rqhold now names the caller (track_caller).
