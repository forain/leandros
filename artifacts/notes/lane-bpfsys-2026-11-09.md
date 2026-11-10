# lane/bpfsys 2026-11-09 - bpf(2) for the cgroup device controller

Status: DELIVERED (no enforcement; lane bpfenf adds the open/mknod hooks).

Code
- `kernel/src/bpf.rs`: bpf(2) (aarch64 280, x86_64 321). PROG_LOAD (CGROUP_DEVICE only, prog_flags 0, license readable,
  log_level/log_buf/log_size validated, verifier message written to the log), PROG_ATTACH/DETACH/QUERY, PROG_GET_FD_BY_ID.
  Everything else EINVAL. attr tail beyond 128 bytes must be zero (E2BIG), size > 4096 E2BIG.
- Permissions via `sched::caps::host_capable` (init userns only): CAP_BPF|CAP_SYS_ADMIN for LOAD/GET_FD_BY_ID,
  CAP_NET_ADMIN|CAP_SYS_ADMIN for ATTACH/DETACH/QUERY. Root in a child userns gets EPERM (crun treats it as rootless).
- `sched/src/bpfcg.rs`: program table (refs = fds + attachments), per-cgroup attach lists tagged with the cgroup
  generation, ALLOW_MULTI/OVERRIDE/REPLACE rules, ancestor strictness check, effective list as Linux computes it,
  `cg_released` hook in `cgroup::remove`, `ATTACHED` counter and `dev_allowed` for the next lane.
- `servers/vfs`: `VnodeKind::BpfProg{id}` (inc on dup/fork, put on close/dup2 overwrite/exit, fdinfo `anon_inode:bpf-prog`,
  always O_CLOEXEC), `install_bpf_prog`, `bpf_prog_of_fd`, `cgroup_of_fd` (cgroupfs dir fd, O_PATH ok; else EBADF).
- QUERY: writes attach_flags and prog_cnt back; copies min(cnt,total) ids; ENOSPC only when ids did not fit. With
  the 64-prog cap crun's 7-try loop (64..4096) never sees ENOSPC.
- `userland/bpftest` (10 cases) registered in runtests.py regress list, build-userland.sh and the f2fs populate list.

Evidence
- Mac aarch64 HVF: 9/10 on first run (the failure was a test bug, non-multi DETACH passes fd 0 as Linux does); fixed.
- `crun --cgroup-manager=cgroupfs run` as root on the Mac guest (spec rootfs with bpftest): no bpf ENOSYS any more;
  the only ENOSYS left is keyctl (219, benign). crun now stops at `ioctl SIOCSIFFLAGS: Not a tty` (loopback
  bring-up in the new netns), i.e. the same point podtrace3 reached with --cgroup-manager=disabled.
- The guard fails on main by construction: every bpf(2) call returned ENOSYS there.

Open
- Enforcement (lane bpfenf). No BPF_LINK_CREATE, OBJ_PIN/GET, GET_INFO (systemd manager only).
