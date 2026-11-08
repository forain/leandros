# lane/seccore 2026-11-08 — seccomp S1 + S2 (storage only)

DELIVERED. No filter can be installed yet; nothing behaves differently except the probes.

- S1: prctl(PR_GET_SECCOMP) -> 0; PR_SET_SECCOMP -> EINVAL; seccomp(2) (aarch64 277 / x86_64 317, `nr::SECCOMP`, `sys_seccomp`) -> EINVAL for every op.
- S2: `sched/src/seccomp.rs`: `SeccompSlot` inline in `Task::seccomp` (AtomicPtr head, null = mode 0, sentinel = strict), `FilterNode` Arc chain
  (prog: seccomp_bpf::Program, prev, path_insns, depth, log). `FilterNode::push` enforces MAX_INSNS_PER_PATH.
  Inherited at fork/clone in clone.rs (both paths) under the same RUN_QUEUE hold that inserts the child (`child.seccomp.inherit_from(&parent.seccomp)`).
  execve: the exec'ing thread's Task struct is kept (the non-leader identity swap in lib.rs only moves ids/creds into `me`), so its slot stays
  put and the old leader's slot is dropped on reap; nothing to re-point until the per-CPU mirror (S3) exists.
- Fast path: `SeccompSlot::active()` is one Acquire load; no hook yet.
- Tests: userland/seccomptest (4 S1 cases), in regress list, build-userland.sh, mkfs-f2fs-populated.py. Groups A-L left for the hook lane.
- Open: per-CPU mirror (m_seccomp / CURRENT_SECCOMP), hook in syscall_dispatch, install ops, SIGSYS. pifw/mk.py does not exist (no registration needed).
