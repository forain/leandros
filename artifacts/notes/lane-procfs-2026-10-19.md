# lane/procfs (2026-10-19/20): accounting, /proc links, process-lifetime ABI

Bug list items L10.4, L10.5, L10.6 and L7.9 (`openbugs-2026-10-19.md`).

## What changed

### Accounting (L10.6)
- **wait4 rusage.** `WaitTry::Reaped` now carries the child's `ChildAcct`.
  `wait4` writes it out with `write_rusage`, the one 144-byte `struct
  rusage` writer that `getrusage` uses too. Fields: utime/stime, maxrss,
  minflt/majflt and nvcsw/nivcsw, the child's waited-for children included
  (Linux RUSAGE_BOTH). A bad pointer gives EFAULT after the reap, as on
  Linux. `getrusage` now returns EINVAL for an unknown `who`.
- **maxrss.** Before, it was a constant 4096. It is now the VmHWM peak.
  `Task::hwm_pages` keeps the peak of address spaces the process no longer
  has: the image before an execve (unless it was borrowed by vfork), and
  the space released early at exit. Children report it as the maximum,
  `c_maxrss_kb`.
- **nvcsw/nivcsw.** New per-slot counters, bumped at switch-back. Blocked
  or Stopped counts as voluntary. Still Ready counts as involuntary,
  `sched_yield` included (Linux). They are folded into the leader when a
  thread is reaped, and into the waiter as `c_nvcsw`.
- **majflt only for real I/O.** `drivers::blkdev::read_blocks` calls
  `sched::note_block_read()`, which bumps the running task's slot counter
  `io_rd`. The fault path's `Read` plan is major only when `io_rd` moved
  during its `file_read`. Before, every `Read` plan was major, page-cache
  hits included. The running slot comes from the new per-CPU
  `CURRENT_SLOT`, set at dispatch and cleared at switch-back. The slot
  array is static, so a stale index can only miscount.
- **Per-thread faults.** Slot counters `minflt`/`majflt` feed
  `task/<tid>/stat` and `RUSAGE_THREAD`. The process totals stay on the
  address space.

### /proc (L10.6, L7.9)
- `TMP_BUF_SIZE` is now 2048 (was 512). A status file with 30 groups was
  cut before VmRSS/Threads.
- **`/proc/sysstat` > 256 KiB.** The kernel never truncated it. Every test
  reader used a fixed 256 KiB buffer: epolltest, evtest2 and six C tests.
  They now use 4 MiB. The theoretical maximum is about 2 MiB.
- **`lstat` of `/proc/<pid>/{exe,cwd,root,fd/N}`** is now answered in
  `fstatat_into` (`lstat_magic_link`): S_IFLNK, mode 0777 (fd links 0700),
  the process's effective ids, ENOENT for a dead pid or fd. Before, it
  was ENOENT.
- **`execv("/proc/self/exe")`.** `sys_execve` runs the path through
  `follow_magic_final` (as open does). This also covers fd/N of a mounted
  file.
- **`/proc/self/fd/N` follows renames and shows " (deleted)".**
  - f2fs: `open_paths_renamed` and `open_paths_deleted` rewrite the stored
    open-time paths on rename (the whole subtree), unlink, rmdir and a
    replaced rename victim. `openat` on a renamed directory fd now works.
    On a removed directory fd it is ENOENT.
  - tmpfs: paths are names, so renames were already followed. A
    `TmpFileEntry::unlinked` flag (set by `make_nameless`) adds the
    suffix.
  - New fix: tmpfs `rmdir` of a directory still open released the slot,
    so the next file created reused it under the open fd. It now goes
    nameless until the last close.

### Lifetime (L10.4, L10.5)
- **clone3 / clone exit_signal.** `Task::exit_signal` comes from the
  CSIGNAL byte, or clone3's `exit_signal`. Threads have 0.
  - At exit the parent gets that signal, or nothing for 0.
  - Only a SIGCHLD child is auto-reaped.
  - `wait4`/`waitid` filter children by `__WCLONE`/`__WALL`
    (`WaitWhat::clone`, Linux `eligible_child`). `ExitRecord` carries the
    signal. `wait4` rejects unknown options with EINVAL.
  - An orphan reparented to init reports with SIGCHLD.
- **clone3 set_tid** (`sched::reserve_pid`/`release_reserved_pid`):
  - one level only (EINVAL for more), 1..PID_MAX;
  - root only (EPERM otherwise);
  - EEXIST when a live pid, tgid, pgid, sid or unreaped zombie uses it;
  - a pid chosen above the counter is skipped by `alloc_pid` (`PID_SETS.ahead`).
- **CLONE_PIDFD with a full table**: ALREADY-FIXED. `do_clone` already
  reserved the fd before the child existed. The `clone_pidfd_emfile` test
  confirms EMFILE and no child.
- **PR_SET_PDEATHSIG / PR_GET_PDEATHSIG.**
  - Delivered when the creating *thread* exits (`thread_children_on_exit`,
    called from `exit`), and when the parent process dies
    (`reparent_children`).
  - Cleared by a set-id exec.
  - New fix: children of an exited non-leader thread now get
    `ppid = tgid`. Their ppid used to name a dead pid, so they were
    unwaitable, and `reparent_children` missed them at process exit.
- **rseq.**
  - RSEQ_CS_FLAG_*: REFUTED. Linux 7.2 (checked on the desktop) ignores
    every flag, unknown bits included, and still aborts. LeandrOS already
    did that. The new `rseq_cs_flags` test pins it.
  - `mm_cid` is now compact. `rseq::cid_acquire`/`cid_release` hand out
    the lowest free id per tgid (a hashed 1024-entry table of 32-bit
    masks) from dispatch to switch-back, for registered threads only. A
    hash collision or more than 32 concurrent threads falls back to
    `32 + cpu`.
- **Non-leader execve now reports with SIGCHLD.** The exec'ing thread
  takes over the leader's pid but kept its own `exit_signal` (0), so the
  parent got no SIGCHLD and `wait4` without `__WALL` skipped the child
  (schedstress `execmt` hung). `take_over_leader` sets it to SIGCHLD
  (Linux `de_thread`).
- **CLONE_FS (L10.5).** `Task::fs_id` is a filesystem-context group id.
  `join_fs_group` sets it under RUN_QUEUE at enqueue, on both the fork and
  the clone_thread paths. It also re-copies cwd/root/umask at that instant.
  `set_cwd`, `set_root` and `umask` update every live member. The new
  `unshare(2)` handles CLONE_FS (`fs_id = 0`). CLONE_FILES, CLONE_SYSVSEM,
  CLONE_VM, CLONE_SIGHAND and CLONE_THREAD are no-ops for a single-threaded
  caller. Namespaces are EINVAL.

## Tests
- New `userland/proctest/proctest.c`, 22 cases, in regress. Built for the
  host too: all PASS on Linux 7.2 (desktop), as uid 1000 and as root.
- syscalltest gained `rseq_cs_flags` and `rseq_mm_cid` (Linux: PASS).
- aarch64 (HVF): proctest and syscalltest pass. A baseline run of the
  unfixed tree was lost: the first build picked up edits midway. The
  before-state was established by reading the code instead.

- pstest's accounting check required majflt > 0 for its own text. That
  encoded the old bug: the text comes from the page cache, and Linux reports
  majflt 0 then. It now checks minflt only.
- Not tested: that a cold read (real device I/O) counts as major. proctest
  only checks the cache-hit side.

## Open
- **CLOSE_RANGE_UNSHARE** (and `unshare(CLONE_FILES)` from a thread):
  fd tables are keyed by tgid at about 80 VFS/kernel sites, so a thread
  cannot get a private copy. It is correct for single-threaded callers, the
  vfork/posix_spawn child included. proctest SKIPs the case on LeandrOS.
- PDEATHSIG is checked on the child's leader thread only (Linux checks
  every thread of the child).
- A vfork child that execs copies its parent's fault counters into the
  new image (pre-existing, `replace_address_space`).
- `access("/proc/sysstat", F_OK)` failed in the guest though `open` works
  (seen, not investigated).
