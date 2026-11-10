# lane/ptracecap - 2026-11-10 - FIXED

Gap: a non-dumpable sibling (same uid) could still open /proc/<pid>/ns and /proc/<pid>/environ of another process.

Fix:
- sched/src/cred.rs `ptrace_read_check` is now `ids_ok && dumpable` (PTRACE_MODE_READ_FSCREDS + get_dumpable rule in
  `__ptrace_may_access`). The CAP_SYS_PTRACE-over-target-userns bypass (capable_snap) is kept.
- servers/vfs `proc_environ_allowed` only compared euid; it now calls `ptrace_read_check`.

Test: nstest `sibling_nondumpable_denied` - FAILED on unfixed code (9be8e053), passes with the fix; `nobody_opens_child_ns` passes.

Evidence: see regress and ffsandbox results in the merge report (aarch64 box regress overall PASS; ffsandbox on=True all children Seccomp 2).
