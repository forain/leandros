# lane/podcg 2026-11-10 - podman blocker #1 (crun pids.max ENOENT)

Result: FIXED. Root `podman run --network=none alpine echo hi` now passes pids.max; next failure is
`crun: open /proc/sys/net/ipv4/ping_group_range: ENOENT` (podtrace4 blocker #2, untouched).

## Changes
- init: after the cgroup2 mount writes `+cpu +io +memory +pids` to the root subtree_control, creates
  /user.slice and enables the same there.
- cgroupfs: children expose memory.{current,max,high,low,min,swap.max,swap.current,events,stat,oom.group},
  cpu.{max,weight,stat}, io.{max,weight,stat} next to pids.* whenever the parent enabled the controller.
  Only pids.max is enforced; the rest are stored (rw knobs, validated) or static zeros (ro). Knobs reset
  on mkdir/rmdir of the cgroup.
- cgroupfs dirfd: VFS_FD_PATH now names cgroupfs directory fds, so openat/openat2 relative to a cgroup
  directory work. This was the real cause behind crun's `openat2 .../cgroup.controllers: EBADF`
  (crun enables controllers through dirfd-relative opens). Test cg_dirfd_open.
- CG6 (partial): /bin/login creates /user.slice/user-<uid>.slice (owned by the user, controllers
  enabled) and a session-<pid>.scope leaf and joins it, before dropping privileges.
  NOT done: greetdlogin/greeter-launch, sshd, setuid launcher, getty autologin still stay in the root
  cgroup; a shared cgsession helper is needed to cover them (design-cgroup.md CG6).

## Evidence
- cgtest cg_boot_controllers, cg_child_ctrl_files FAIL on lane/podcg-guard (tests only), PASS with the fix.
- Mac aarch64 HVF guest: cgtest all PASS; podman gets past pids.max, fails at ping_group_range.
