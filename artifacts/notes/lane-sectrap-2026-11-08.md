# lane/sectrap 2026-11-08 (Firefox sandbox wave)

Status: FIXED (SECCOMP_RET_TRAP / KILL_THREAD) + FIXED (aarch64 sigframe ABI).

- sched/src/seccomp.rs: RET_TRAP delivers SIGSYS via force_sigsys_seccomp (si_call_addr, si_syscall, si_arch, si_errno=data);
  handler returning rolls the syscall back (x86 rax==nr, a64 x0 restored); blocked/ignored/default SIGSYS is fatal.
  RET_KILL_THREAD kills only the calling thread (exit_group if it is the last).
- ABI bug: aarch64 rt_sigframe placed uc_mcontext at frame+296; Linux/musl align it to 16 => 304
  (fault_address@304, regs@312, pc@576, __reserved@592, SIGFRAME_SIZE 4688). musl handlers (chromium/Firefox)
  read registers shifted one slot before.
- ABI audit of consumers of the old layout:
  - kernel sched/src/signal.rs prepare()+restore() share the same offset consts: consistent. kernel/src/syscall.rs: none.
  - userland/sigtest edit_regs_handler: updated 176 -> 184 (done in the lane commit); seccomptest uses ucontext_t from libc headers.
  - servers/, ports patches: no hard-coded sigframe/ucontext offsets found.
  - relibc (src/header/signal/linux.rs): aarch64 used the x86 ucontext layout (wrong, pre-existing). Fixed in relibc master
    89537d78: Linux aarch64 ucontext_t (sigmask at 168, __unused[120], 16-aligned mcontext at 176) and mcontext_t
    (fault_address, regs[31], sp, pc, pstate, __reserved[4096]). relibc has no getcontext/setcontext users.
  - Rust userland via libc crate: Linux layout, fine.
- Tests: see lane report (seccomptest, sigtest, sigtest2, default regress aarch64 on Mac; x86_64 full regress on box).
- Open: namespace probe for the Firefox sandbox is not complete.
