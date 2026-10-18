# lane/procfix (2026-10-18): procfs and accounting gaps

Closes the "Open, procfs and accounting" group of TODO.md (laptop performance
wave). Follows lane procps2 (`lane-procps2-2026-10-17.md`).

## What changed

- **Thread ppid** (`sched::proc_stat_of`): `stat` field 4 and `status` PPid of
  any task are the leader's parent named by its tgid (Linux: a CLONE_THREAD
  child shares the leader's real_parent; `getppid` already did this). Also
  fixes a process forked from a non-main thread, whose `ppid` named that
  thread.
- **Thread starttime**: `stat` field 22 is the *named* task's `start_ns`
  (`sched::task_start_ns`): the leader's for `/proc/<pid>/stat`, the thread's
  own for `task/<tid>/stat` and `/proc/<tid>/stat`, as Linux.
- **Accounting** (`sched/src/lib.rs`: `ChildAcct`, `group_acct`,
  `credit_child`, `exit_acct`; `Task::{dead_cpu_ns, dead_sys_ns, c_cpu_ns,
  c_sys_ns, c_minflt, c_majflt}`):
  - a reaped thread's `cpu_ns`/`sys_ns` move to its leader, so the process's
    utime/stime no longer drop when a thread exits;
  - `log_exit` (now `&mut RunQueue`) stores a process's group total in its
    `ExitRecord`; the waiter that consumes it (`wait_scan`, both phases)
    adds it to its leader's `c_*`. Phase 1 (zombie still on the queue)
    computes it in place. Auto-reaped children are not credited (Linux).
  - stat 16/17 cutime/cstime, `times(2)` (was all zeros), and
    `getrusage(RUSAGE_CHILDREN)` read it.
  - page faults: `AddressSpace::{min_flt, maj_flt}` (atomics, bumped by
    `sched::page_fault` under `busy`; `Read` plans = major, so page-cache hits
    count as major). Carried across exec; folded into the task when the
    exiting address space is released early. stat 10-13 and getrusage
    `ru_minflt`/`ru_majflt` report them.
- **`/proc/sysstat`**: the ffhang `w` lines landed, now `w pid tgid state sc
  port listen poll_mask in_sc_ns` (sc `-1` = none; `in_sc_ns` = wall time
  inside the current syscall). `s` lines gained a 6th column, `wall` (entry to
  exit of completed calls; old parsers that take 5 fields still work). New `e
  tgid calls oncpu_ns wall_ns blocked_ns zero` lines sum the epoll wait
  syscalls per process. Cost: per-CPU `SC_T0` store at entry, one more
  `fetch_add` at exit, `Task::sc_saved_t0` carried at switch like
  `sc_saved_ns`; no locks. How to compare against deskbusy2's 613 µs/call:
  docs/development/debugging.md "epoll cost per call".
- **`/dev/tty`** (`servers/vfs` open): pty ctty -> the pty; the session that
  holds the console (`tty_server::console_is_ctty_of`) -> the console;
  otherwise ENXIO (was: the console proxy). `sys_ioctl` no longer judges a
  folded console proxy (fd > 2) by what fd 0 is (a /dev/tty fd answered
  ENOTTY to isatty when stdin was a pipe).
  sudo-rs patch 0003 removed; `ports/sudo-rs/build.sh` now stamps the tree
  with a hash of the patch set, so a removed patch triggers a re-clone.
- **st_rdev**: `/dev/pts/N` 136+N/256:N%256, `/dev/ptmx` and masters 5:2,
  `/dev/ttyN` 4:N, `/dev/console` 5:1, `/dev/tty` 5:0 (or the pty's),
  null/zero/random/urandom 1:3/1:5/1:8/1:9, fb0 29:0, by path and by fd
  (`char_dev_rdev`, `dev_node_rdev`).
- **ps** simple selections as procps-ng: `-a` (tty, not session leader), `-d`
  (not session leader), BSD `a` (tty), `x` (own euid), `ax`/`-ax` all; union
  with the list selectors.
- **prctl(PR_SET_MM, ARG/ENV_START/END)** (`sched::set_mm_arg_field`):
  euid 0 or EPERM; arg4/arg5 != 0, addr < 4096 or >= TASK_SIZE, unordered
  area -> EINVAL; no VMA ending above addr -> EFAULT. `read_proc_args` takes
  Linux's `get_mm_cmdline` rule: the title runs past arg_end only when
  env_start == arg_end; empty when arg_start >= arg_end.

## su backgrounding (item 6)

`su leandro -c 'sleep 30 >/dev/null 2>&1 </dev/null & ...'`:
- no terminal: su returns at once; the child lives on as uid/euid 1000,
  reparented to init (pid 5 here, not 1 — the test checks `comm` = init), in
  the shell's session and process group. Nothing needed fixing.
- on a pty (sudo-rs use_pty): su returns at once; the child survived on
  aarch64 with SIGHUP at default. On Linux this depends on which pgrp is in
  the foreground of sudo's pty when the monitor exits (the tty hangup itself
  only signals the session leader); not asserted, the test logs it. A child
  ignoring SIGHUP survives as leandro (asserted).
- sudotest now runs its checks in a forked `setsid` child (one session, no
  ctty), so `/dev/tty` is ENXIO for su/sudo as from ssh exec, and sudo's
  credential cache still carries between calls.

## Tests

pstest +27 checks (thread ppid/PPid/starttime, exited threads' CPU, minflt/
majflt, cutime/cminflt incl. grandchildren, times, getrusage self/children,
PR_SET_MM, sysstat w/e, st_rdev, ps -a/a/x/-d/-ax); sudotest +12 (/dev/tty
ENXIO, rdev, su background without and with a pty).

## Limits

- `wait4`'s `rusage` is not filled.
- majflt counts every file-backed fault (page-cache hits included).
- a thread's minflt/majflt are its process's.
- a thread exiting after its leader's slot was reaped loses its time.
- the other PR_SET_MM options are still accepted and ignored.

## Merge risks

- `sched/src/lib.rs`: log_exit signature (&mut), wait_scan phase 1/2 credit,
  page_fault counters, scheduler dispatch/switch-back (`sc_saved_t0`, one more
  field — lane percpu touches the same block), proc_stat_of, new fns near
  process_cpu_sys_ns and ARG_AREAS.
- `sched/src/sysacct.rs` (`for_each` callback gains `wall`).
- `servers/vfs/src/lib.rs`: gen_sysstat, /dev/tty open, fstat/stat rdev arms,
  stat fields 9-17/22.
- `kernel/src/syscall.rs`: sys_ioctl console-proxy guard, prctl PR_SET_MM,
  times, getrusage.
