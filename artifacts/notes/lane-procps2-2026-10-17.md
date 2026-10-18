# lane/procps2 (2026-10-17): task dirs, real TTY, argv, ps selectors

Follows lane/procps (`lane-procps-2026-10-17.md`) and removes its listed limits.

## What changed

- **`/proc/<pid>/task/`** (`servers/vfs/src/lib.rs`: `proc_task_parse`,
  `open_proc_task`, getdents arms). Lists every tid of the thread group
  (`sched::thread_ids_from`, tid-keyed cursor like `/proc`'s). Each
  `task/<tid>/` holds the `PROC_PID_FILES` (stat, status, statm, cmdline, comm,
  smaps, smaps_rollup, environ); `/proc/<pid>/` now also lists `task`.
  `/proc/thread-self[/<file>]` and `/proc/self/task` work. A tid of another
  group is ENOENT under `task/`. Per-thread `stat`: tid in field 1, the
  thread's name, state, and the thread's own utime/stime
  (`sched::thread_cpu_sys_ns`); the process-level files keep the group sums.
  `task/<tid>/comm` is writable with the `/proc/<pid>/comm` rules (owner or
  root; `proc_comm_target` resolves task paths too).
  Linux semantics checked and kept: `/proc` getdents lists only tgids;
  `/proc/<tid>` of a non-leader resolves.
  `stat(2)` now answers for `/proc/<pid>`, `/proc/<pid>/<file>` and the task
  paths (dir 0555 / file 0444, comm 0644, owner = the process's euid/egid);
  before, they fell through to the root mount (ENOENT) and only the kernel's
  path-walk fallback papered over it.
- **TTY** (`servers/tty`): the controlling terminal is session-keyed here —
  a pty pair's `sid` (TIOCSCTTY / slave open), VT text sessions are pty pairs
  with `vt != 0`, the serial console is `CONSOLE_SID`. New
  `tty_server::ctty_dev_of_sid(sid) -> (major, minor, fg pgrp)` (via
  `pty::ctty_of_sid`): pts/N = (136 + N/256, N%256), VT N = (4, N), console =
  (5, 1). stat field 7 is `makedev(major, minor)` (Linux encoding), field 8
  the terminal's foreground pgrp, or `0 -1` with no terminal. (Device st_rdev
  of /dev/pts/N etc. is still 0; nothing here needed it.)
- **argv / environ** (`kernel/src/syscall.rs` sys_execve, `sched`):
  execve records arg_start..arg_end / env_start..env_end of the strings it
  lays on the new stack (`sched::set_arg_area`, tgid-keyed, inherited on
  fork, dropped at leader exit). `/proc/<pid>/cmdline` and `environ` read the
  target's stack through its address space (`sched::read_proc_args`,
  `with_process_as` + `read_user_buf`, buffer pre-sized outside the `busy`
  flag), so an in-place setproctitle shows; Linux's rule for a title that
  overruns arg_end (argv area no longer NUL-terminated: continue into env up
  to the first NUL) is implemented. Cap 64 KiB. Kernel tasks: empty cmdline.
  A user process with no recorded area (boot-loaded PID 1) still shows its
  exe path. `environ` is root/owner only (EACCES otherwise).
- **Kernel-thread names**: `comm_of` strips the `[name]` brackets
  `kthread::spawn` records (repeatedly: `[netctl]` is passed already
  bracketed), and an unnamed kernel task is `kthread`, not `init`. The
  `sched::spawn` sites name theirs: khungtaskd, netd, kiostat, klogd,
  netwatch, f2fs-pcache.
- **ps** (`userland/ps/src/main.rs`): TTY column decodes tty_nr (pts/N, ttyN,
  ttySN, console, `?`); default selection is procps's (same tty_nr and same
  euid as the caller); STAT gets `+` for the foreground group; `-T/-L` read
  `task/<tid>/stat`; args up to 4 KiB. New selectors (union, as procps):
  `-u/-U user|uid` (effective/real), `-g` (session id or effective group
  name), `-G` (real group), `-C comm`, `-t tty` (pts/N, ttyN, console, `?`).
  BSD `u` (ps aux) unchanged.

## Tests

`/bin/pstest` grew from 26 to 74 checks: 3 raw-clone threads (no TLS)
named via prctl — task listing (/proc/<pid>/task, /proc/self/task), names via
task/<tid>/comm and /proc/<tid>/comm, task-dir listing, write through the
task path, per-thread stat/status, per-thread CPU (busy vs idle thread),
/proc lists tgids only, foreign tid ENOENT, `ps -T`, `ps -L -o lwp,comm`,
thread exit leaves the task dir; an exec'd child with spaces in argv and an
env (`pstest --argv-child`): cmdline, environ, EACCES for another user,
`ps -o args` / `-o comm`, in-place retitle and a retitle into the env area;
kernel threads have empty cmdline, a real comm, `[comm]` in ps; a pty
session (setsid + TIOCSCTTY) running default `ps` lists exactly its two
processes as pts/N, tty_nr = 136:N, tpgid = the session's group, `ps -o tty`,
`ps -t pts/N`; `-u root`, `-u 1000`, `-U1000`, `-C pstest`, union, unknown
user.

Results 2026-10-18: procps, regress, net-e2e all PASS on aarch64 (HVF) and x86_64; pstest 74/74 on both.

## Limits

- `-a` / `a` still mean "all" (procps: all with a tty, minus session leaders).
- st_rdev of /dev/pts/N, /dev/ttyN, /dev/console is still 0.
- A thread's starttime is its group's; cutime/cstime, minflt etc. are 0.
- prctl(PR_SET_MM_ARG_START...) is not implemented (not needed for
  setproctitle in place).

## Merge risks

- `servers/vfs/src/lib.rs`: PROC_PID_FILES (+environ), proc_comm_target,
  new proc_task_parse/open_proc_task/proc_stat_kind/gen_proc_args,
  gen_proc_self (cmdline/environ routing), gen_proc_self_content (stat
  fields 7/8, per-thread times; cmdline arm removed), handle_open proc
  branches, handle_getdents64 pid-dir block, stat_common (proc arm first).
- `sched/src/lib.rs`: new thread_ids_from, is_kernel_task (next to
  process_pids_from), comm_of fallback, ARG_AREAS block, clear_exe_path and
  inherit_exe_path one-liners. No scheduler/wake-path changes.
- `kernel/src/syscall.rs` sys_execve (set_arg_area after set_exe_path);
  one-line set_comm at sched::spawn sites in kernel/src/{hungtask,init,iostat,
  klog,netwatch}.rs and servers/f2fs/src/lib.rs (pcache).
- `servers/tty/src/{lib.rs,pty.rs}`: two new functions, no behaviour change.
- `userland/ps/src/{main.rs,test.rs}`.
