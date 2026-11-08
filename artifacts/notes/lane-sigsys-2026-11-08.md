# lane/sigsys 2026-11-08 - SIGSYS delivery helper for seccomp RET_TRAP

Status: DELIVERED (aarch64 only tested, per lane rule).

- Helper: `sched::signal::force_sigsys_seccomp(syscall_nr: i32, arch: u32, call_addr: u64, errno_data: u32) -> bool`
  (sched/src/signal.rs, re-exported as `sched::force_sigsys_seccomp`). Thread-directed, like `fault_signal`:
  if SIGSYS is blocked or SIG_DFL/IGN the group is claimed to die by SIGSYS and it returns false (caller then
  `exit_group_signal(31)`); otherwise queues SIGSYS on the current thread and returns true.
- siginfo: si_signo=31, si_errno=errno_data (+4), si_code=SYS_SECCOMP(1), si_call_addr (+16), si_syscall (+24),
  si_arch (+28). Packed into existing `SigInfo` slots (`SigInfo::sigsys`) so Task does not grow; `write_siginfo` unpacks.
- rt_sigreturn: audited both arches; restore already restores all GPRs, sp, pc and sanitised flags from the
  user-edited ucontext, and sys_rt_sigreturn returns the restored x0/rax. Only change: the ERESTARTSYS /
  ERESTARTNOHAND / RESTARTBLOCK noting in the syscall exit is now skipped for rt_sigreturn, so a handler-written
  return value equal to one of those codes cannot arm a syscall restart. No behavioural bug found otherwise,
  hence no failing-before demo.
- Tests (userland/sigtest): handler_edits_saved_regs (SA_SIGINFO handler rewrites ucontext x0/rax + x20/r12 after a
  raw kill syscall; asserted after return) and sigsys_siginfo_signo (raise(SIGSYS) si_signo==31). Both PASS aarch64.
  ucontext offsets: aarch64 regs at ucontext+176 (handler arg is the ucontext, not the sigframe); x86_64 gregs at +40.
- Not covered: end-to-end trigger of the helper (no seccomp hook yet); x86_64 untested.
