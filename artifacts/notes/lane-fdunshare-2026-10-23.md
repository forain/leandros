# lane/fdunshare 2026-10-23 - L10.4: per-thread fd tables (FIXED)

unshare(CLONE_FILES) / close_range(CLOSE_RANGE_UNSHARE) from a thread with siblings now gives that thread a private copy of its fd table (before: EINVAL / silently shared).
- sched/src/fdkey.rs: lock-free tid -> private table id registry (64 slots, id = 0x8000_0000|n).
- vfs: `tbl_key(pid)` resolves the table id in `find_tbl`/`get_or_create` (no-op unless a thread has unshared); `ProcFdTable.owner` (POSIX locks released under the tgid); `copy_fd_table` (fork_dup refactored), `rekey_table`.
- kernel: `fd_unshare_current` (copies table, re-dups socket/epoll handles), thread clone inherits creator's private table, private table freed with its last user (`vfs_close_all_for`), exec folds the private table into the process table, epoll number-keyed registrations left alone for private-table closes.
- /proc/thread-self/fd lists the thread's table; /proc/self/fd and /proc/<pid>/fd the leader's (process) table.
- proctest: un-skipped close_range_unshare, new unshare_files_thread (pipe EOF after the unsharing thread exits). Before the fix: unshare_files_thread FAIL status 0xb00 (EINVAL). After: PASS.
Evidence (aarch64): proctest, pthreadtest, epolltest, scmtest green; killmt leaderless_exit/exec_leaderless failed once (X0 race) and passed on rerun; regress failures are wake-count noise (pollnarrow, evherd, listentag, nettag2) and pstest tty_nr (also seen in other lanes' runs aff3, gpuq2r).
Deviations / open: readlink /proc/self/fd/N and open of /proc/self/fd/N from an unshared non-leader thread use the thread's table; epoll_ctl registrations are per process; fork from an unshared thread leaves net sockets of the parent's other tables in the child (step 9 skipped); clone(CLONE_THREAD) without CLONE_FILES still shares; setrlimit(NOFILE) only updates the caller's table.
