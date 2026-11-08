# lane/sechook 2026-11-08 — seccomp S3 (install + syscall hook), no TRAP/TSYNC

FIXED (aarch64): seccomp(2) STRICT/FILTER(+LOG, SPEC_ALLOW)/GET_ACTION_AVAIL, prctl PR_SET/GET_SECCOMP real.
- Hook in `syscall_dispatch` (kernel/src/syscall.rs), both arches; fast path = per-CPU `CURRENT_SECCOMP`
  (mirrored via `SchedSlot::m_seccomp`, like sig_hint) + one Acquire load; no RUN_QUEUE when unused.
- Actions: ALLOW, ERRNO (capped 4095), LOG (serial line), TRACE/USER_NOTIF -> ENOSYS, KILL_PROCESS/unknown/
  KILL_THREAD/TRAP -> `exit_group_signal(SIGSYS)`; strict -> SIGKILL outside read/write/exit/exit_group/rt_sigreturn.
- exec non-leader exchange swaps pids, not Task boxes, so the caller keeps its own `Task::seccomp`;
  CURRENT_SECCOMP needs no repoint. Pinned by seccomptest `inherit_exec_from_nonmain_thread`.
- seccomptest: 19 cases (errno, cap, zero, kill thread/process via fork+wait, log, trace, stacking, fork/execve/
  non-main-thread-exec inheritance, NNP EACCES, verifier EINVAL/EFAULT, flags, strict, action_avail).
- Baseline with hook disabled (filters install, not enforced): 12 of 19 FAIL; with hook: 19/19.

Regress: default aarch64 suite PASS on Mac (box build failed: Alpine v3.24 404 in alpine-lock, infra).

OPEN: TRAP -> real SIGSYS (lane sectrap); KILL_THREAD in a multithreaded process kills the whole group
(TODO in on_syscall_slow); TSYNC (lane sectsync), NEW_LISTENER; /proc Seccomp lines; ip is the frame pc
(insn after svc/syscall, as Linux).
