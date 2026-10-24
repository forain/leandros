# lane/exitexec 2026-10-24 — openbugs L5.5 (x86_64 killmt exec_exit "child not reaped", 2 tasks left)

Status: FIXED (the root cause was shown red with a new guard; the original 1-in-1100 exec_exit hit was not re-run to red).

## Root cause
`clone`/`fork` did not check for a group exit in the RUN_QUEUE hold that enqueues the child. In exec_exit's child the leader
spawns worker 0 (it execs after 1 ms) and then worker 1 (it spins). Under TCG the second `clone` can still be in flight when
worker 0's `execve` de-threads. The de-thread kills the other siblings (`kill_next_group_member_except`), and only after that
does `take_over_leader` handle the leader. The leader is on CPU, so it is only asked to park (stop_pending), and it finishes its
clone first. The new thread joins the group after the sibling kill loop has already looked past it. The exec completes with a
second thread still running the old image. That matches the L5.5 dump: 2 tasks (22838 = image under the leader pid,
22840 = worker 1), both R, num_threads 2. Exit-group owners are not affected, because their kill loop rescans until nothing is left.
Linux: copy_process fails with -ERESTARTNOINTR on a pending fatal signal, and de_thread's zap sends one to every other thread.

## Fix (240f5a21)
New `group_exiting_locked(rq, pid)` in sched/src/lib.rs: the caller is group_killed, or another thread owns `group_exit_owner`.
Both enqueue holds in sched/src/clone.rs (the thread/clone path and the fork path) check it under the same lock that the owner's
claim takes. If it is true, the child is dropped and the call returns -EINTR (the caller is dying and never returns to user space).
So a child is either visible to the owner's loop or never created: the same single-hold linearization as claim_fatal_signal_locked.

## Guard (6c2d35f6): killmt `exec_clone` (in `killmt` / `killmt proclife`, min 300 iters)
The leader creates up to 48 spinning threads back to back. Worker 0 execs after (it%64)*40 us. The exec must win, and the image
must be the only thread (/proc status Threads: 1). It must then die by the parent's SIGKILL within 20 s.
The exec_exit "not reaped" path now appends proc_diag (status, task list, stat).
- Unfixed x86_64 TCG: FAIL at it 112, "image runs with 2 threads ... tasks=[1191, 1195]" (4 s into the run).
- Fixed x86_64 TCG: exec_clone 300 ×2 PASS; exec_exit 500 ×4 PASS (exit won 139-155 / exec won 345-361); full killmt ×3 PASS.
- Fixed aarch64 TCG: full regress PASS (includes `killmt 40 proclife`, so exec_clone ran 300 iterations).

## Open
- The unfixed red run's leaked thread was still reaped by SIGKILL. The L5.5 run's leaked thread survived SIGKILL for 20 s.
  That second symptom was not reproduced. It could not occur without the leaked thread, but why that leak was also unkillable
  (old address space after the exec, `delayed_leader`/mm release) was not investigated.
