# lane/procfdx 2026-10-23 - L5.12 first half: open("/proc/<other pid>/fd/N") (FIXED)

- vfs `handle_open`: new `proc_pid_fd_number` branch; reopens fd N from the TARGET process's table (`reopen_fd_kind_of(opener, target, ...)`; tables are per tgid). ENOENT: pid missing, leaderless group, fd missing/unreopenable kind. EACCES: different non-root euid (same rule as /proc/<pid>/environ). File permission check still uses the opener's credentials.
- Guard: proctest `fd_reopen_other` (child uid 1000 holds a memfd, parent reads it via /proc/<child>/fd/N; ENOENT bad fd/pid; EACCES from uid 2000).
- Evidence (aarch64): unfixed main -> "open(/proc/43/fd/7): No such file or directory"; fixed -> PASS (x4 dirs); full regress overall PASS.
- Test-bug hit: child must close the pipe write end it inherits, else waitpid hangs (proctest timed out at 300s in the first regress).
- Open: kinds other than tmpfs/memfd/dev (pipes, mounted files) are still ENOENT on reopen, own or other.
