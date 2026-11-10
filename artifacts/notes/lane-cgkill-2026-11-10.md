# lane/cgkill 2026-11-10 - FIXED
- cgroup.kill (0200, write "1" SIGKILLs the subtree, read EINVAL) and cgroup.freeze added in vfs::cgroupfs. Both follow directory ownership, so chown of the dir delegates them (EACCES for non-owner).
- Freeze semantics chosen: recorded request; cgroup.events reports `frozen 1`. Tasks are NOT stopped by the scheduler.
- /sys/kernel/cgroup/delegate and features served by the cgroup2 intercept (listing procs, threads, subtree_control, freeze, kill, memory.oom.group).
- Fail-first: cgtest cg_freeze_delegate FAILED on unfixed branch (delegate ENOENT); passes after. Full aarch64 box regress PASS.
