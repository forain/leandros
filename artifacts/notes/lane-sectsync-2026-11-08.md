# lane/sectsync 2026-11-08 - seccomp TSYNC (design S4)

FIXED. seccomp(SET_MODE_FILTER, TSYNC[|TSYNC_ESRCH]) implemented in sched/src/seccomp.rs `set_mode_filter`.

- One RUN_QUEUE hold: check every other thread of the tgid is empty or its head is in the caller's old chain
  (pointer ancestry walk); strict-mode or divergent sibling -> return its TID (or -ESRCH with TSYNC_ESRCH).
  Otherwise install the new node on all siblings, copy no_new_privs if the caller has it, then on the caller.
- Mirrors: CURRENT_SECCOMP / SchedSlot::m_seccomp point at the Task's SeccompSlot (not its head), and the
  fast path reads the slot's atomic head, so a swap on a thread running on another CPU is seen at its next
  syscall entry; no per-CPU update needed. A thread blocked in a syscall is filtered from its next syscall.
- TSYNC_ESRCH without TSYNC -> EINVAL; NEW_LISTENER still EINVAL.
- seccomptest group I (5 cases): reaches spinning + futex-blocked threads, divergent thread -> TID,
  ESRCH variant, NNP propagation, ESRCH-without-TSYNC. Old verifier assertion "TSYNC -> EINVAL" removed.
- Evidence: before fix 4 of the 5 failed (tests-sectsync-before); after: all pass.
