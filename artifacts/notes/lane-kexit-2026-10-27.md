# lane/kexit 2026-10-27 -- returning kernel threads

Status: FIXED (aarch64 pstest 108/108 with the canary; see regress below).

## Root cause (bisected, differs from the design's suspects)
Pid 1 was never hidden: it was *taken*. `alloc_pid` hands out pids in order, and a kernel
thread spawned before /bin/init (btusb on the laptop, the new boot canary) got pid 1; /bin/init
became pid 3. When that kthread returned, pid 1 was gone, so `ps -e`, `ps -ax`, `ps x`, `ps aux`,
`ps -u root`, `/proc/1/stat` and sudotest's starttime all failed. Evidence: `ls /proc` on the
canary build showed 3..21 with no 1 or 2 (pids 1,2 = kexit0/kexit1).

## Changes
- `sched`: `alloc_kthread_pid` never hands out pid 1 while unclaimed; the first non-kthread task gets it.
- `Task.kthread`; `spawn_with_arg_named` (name set before enqueue, session/pgrp 0);
  `kthread_exit()` (born-reaped zombie, no `log_exit`, no process-exit hooks); `exit()` diverts for kthreads.
- `kthread::spawn` uses the named spawn; `kthread_main` ends in `kthread_exit`.
- Permanent canary in `kernel/src/init.rs` (kexit0, kexit1 -- they take pids 2 and 3, init stays 1).
- pstest case `test_kthread_exit` (/proc/1/stat parses, no kexit in ps -e, no kernel zombie).

## Evidence (aarch64 HVF)
- Canary on unfixed code: pstest FAILS (ps -e/aux/x/-ax/-u root pid 1, /proc/1/stat, new case).
- kthread_exit alone (without the pid-1 reservation): same failures remain -- this is what proved the cause.
- Final: pstest 108 checks, 0 failed.
