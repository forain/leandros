# lane/faultclaim 2026-10-24 — openbugs L5.4 (fault-path fatal exit claims the group exit in a later hold)

Status: FIXED (hardening). The window could not be hit on unfixed code (3×500 x86_64 TCG green), so the fix rests on the code argument below.

## Change (sched/src/signal.rs `fault_signal`)
`fault_signal` takes RUN_QUEUE to decide whether a user fault (SIGSEGV/SIGBUS/SIGILL/SIGFPE, and rseq's forced SIGSEGV)
gets a handler. When it decides on the kill (SIG_DFL/SIG_IGN or blocked), it now calls `claim_fatal_signal_locked`
in that same hold. Before, the claim came only in the caller's `exit_group_signal`, after the serial fault dump (with
IRQs open). Now either the fault owns the exit (a racing execve loses its claim and dies), or a non-leader exec
already owns it and the fault's signal becomes its forced_exit (exec fails, group dies by SIGSEGV). The caller's
`exit_group_signal` re-claims as owner, or forwards again (no-op) and parks. All three callers (x86 idt, aarch64
exception, rseq force_sigsegv) exit right after a `false`.
Not changed: the direct `exit_group_signal` sites (fatal_exception, kernel faults on user pointers, sigframe write/read
failures). No earlier hold makes a decision there, so their first hold already is the claim.

## Semantics note (premise correction)
The old outcome on the gap ("faulting thread reaped, exec'd image runs") is not by itself a Linux violation. If
de_thread's zap takes siglock before force_sig_fault, Linux also lets the exec win (complete_signal skips group exit
when group_exec_task is set). The invariant that must hold is "one outcome, never both". The guard checks that.
The fix moves the decision point to the fault-decision hold, so it is a cleaner linearization point and not a fix for an observed bug.

## Guard: killmt `exec_sibfault` (in `killmt` / `killmt proclife`, min 300 iters)
4 spinners. A sibling does a NULL store after (it%64)*8 us. execve runs after ((it*37)%64)*8 us, by the leader on even
iterations and by a third thread on odd ones. Outcome must be either WTERMSIG==SIGSEGV with no image (no P line, no
img-<pid> marker), or image ran and dies only by the parent's SIGKILL. Anything else, or a hang, is FAIL.
- Unfixed x86_64 TCG: 3×500 PASS (fault won 226/213/229, exec won 274/287/271). Both outcomes are well swept, never both.
- Fixed x86_64 TCG: exec_sibfault 500 ×2 PASS. Full killmt ×3: 2 PASS, 1 FAIL in exec_exit (below). exec_exit 500 PASS on rerun.
- Fixed aarch64: full regress PASS (nettest tcp_keepalive_probes, netctltest wired_admin_up_rebinds and sshsmoke failed once
  on host networking and passed on rerun). exec_sibfault 300 PASS (fault 47 / exec 253).

## New, open (not caused by this lane)
x86_64 TCG killmt exec_exit it 90: "child 22838 not reaped". After 20 s, 2 tasks remain (22838, 22840), both R, num_threads 2.
That is the leader exit_group(42) racing a non-leader execve. That mode had no fault at all (no user page fault lines), so
fault_signal is not on its path. 1 hit in ~1100 exec_exit iterations. Repro: `/bin/killmt 500 exec_exit` repeatedly on x86_64 TCG.
