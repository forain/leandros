# lane/pidsec 2026-11-08 (ffsbreview2 G2 G3 G4 G7 P1)

FIXED
- G2: kernel/src/syscall.rs proc_magic translates numeric pids through sched::pidns::from_cur (invisible -> ENOENT, 1 -> ns init); exe/cwd/root readlink of another process also needs same euid or root (proc_link_allowed).
- G3: pidns::zap collects every live task by (tid, tgid), so leaderless groups are killed.
- G4: clone.rs re-checks ns::pid_dead under the RUN_QUEUE hold right before claim_init/enqueue (both fork paths; fails with ENOMEM); zap re-sends SIGKILL on each loop pass.
- G7: reaper_for skips dead namespaces and zombie inits.
- P1: reparent_children skips on_process_exit/reaper_for (two RUN_QUEUE takes) unless pidns::USED.

Tests (nstest): proc_magic_pidns, zap_leaderless, zap_forkrace.
Before the fix (aarch64): proc_magic_pidns FAIL, zap_leaderless FAIL; zap_forkrace PASS (race is narrow; it is a regression guard, not shown failing).
After: nstest, sigtest, killmt green on aarch64.
Open: bind-mounted /proc views via VFS were already pv_in-translated; fork into a child ns of a dying ns not re-checked (ancestor pid_dead).
