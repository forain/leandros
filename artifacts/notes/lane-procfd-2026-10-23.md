# lane/procfd 2026-10-23 - L5.12b: /proc/<pid>/fd (FIXED)

Option B (Linux-exact).
- vfs: `/proc/<pid>/fd` opens/stats/lists for every live pid the caller may inspect (own euid or root, -EACCES otherwise); `/proc/thread-self/fd` lists the thread's table. Another pid's table is read with the caller's table lock released.
- kernel readlink: `/proc/<pid>/fd/N` resolves in the target's table with the same permission rule.
- sched::leaderless(pid): leader exited, group alive -> `/proc/<pid>/fd` and `/proc/self/fd` list 0 entries, `fd/N` and `exe` readlink are ENOENT (as Linux); thread-self/fd still full.
- killmt leaderless_exit: X4 now expects ENOENT; new X5 (empty fd dir, fd link ENOENT), X6 (thread-self/fd has res), parent X0 (dir exists for the other pid, own-pid listing + pipe: link).
Evidence (aarch64): killmt leaderless_exit 20/20, proctest, vfstest green; regress green except pollnarrowtest console_read_alias (1 stray wake, flaky; passes on rerun).
Bugs hit on the way: reacquiring the same table lock while held deadlocked exec_state; VFS `pid` is the tgid so thread-self needs sched::current_pid().
Not done: open("/proc/<other pid>/fd/N") (reopen) still own-table only.
