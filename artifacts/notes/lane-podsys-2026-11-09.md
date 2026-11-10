# lane/podsys 2026-11-09 - podman blockers #3, #4, #5 (podtrace2-findings)

FIXED (pending both-arch regress, see bottom)

- #3 `/proc/sys/kernel/cap_last_cap` = 40 (const-asserted against `sched::caps::CAP_LAST`), plus `ngroups_max` = 32
  (asserted against `sched::task::NGROUPS_MAX`; the true limit, not Linux's 65536). pid_max, threads-max, osrelease,
  overflowuid/gid already existed.
- #4 tmpfs on any directory (design-mntns M5, ns 0): `mount -t tmpfs` outside the built-in roots creates a superblock
  (`sb_create`) with a backing instance under `/.leandros-sb/tmpfs/<n>` and exposes it as an alias mount; the alias holds
  the sb reference (`mnt::ns0::add` get / `remove` put), so umount frees the instance. Built-in roots keep the
  flags-only behaviour. Options: `mode=` (default 1777), `uid=`/`gid=` (translated through the caller's userns,
  EINVAL when unmapped), `size=` (bytes, k/m/g, %), nosuid/nodev/noexec per instance. `may_mount()` unchanged: ns 0 stays
  host-root only, non-root userns -> EPERM. The mntns tmpfs path now shares `tmpfs_new_instance` (same options; default
  mode is 1777 instead of the interim 0700).
- #5 init mounts `cgroup2` (data `nsdelegate`, accepted) on `/sys/fs/cgroup` after fstab. CG6 delegation (leaf cgroup per
  login session, delegated user slice) NOT done: left open.

Deviations: `size=` is enforced on write(2) only (a scan of the instance's resident pages per write, only for instances
with a limit); ftruncate-grow/mmap are sparse and not charged. `nr_inodes=` and unknown options are ignored. A mount
namespace copied from ns 0 sees the alias with sb 0, so umount in ns 0 frees an instance a copy still maps.

Tests: mnttest `tfs_*` (arbitrary dir, content independence, mode default, nosuid, sb count, umount frees, second
umount EINVAL, mode/uid/gid/size=64k ENOSPC at exactly 65536, noexec EACCES, userns EPERM in ns 0), `proc_cap_last_cap`,
`proc_ngroups_max`; cgtest `cg_boot_mounted` (statfs 0x63677270 before anything mounts). Fail-first on main (box
x86_64): mnttest FAILED (tfs_mount_arbitrary_dir), cgtest cg_boot_mounted FAIL. After: aarch64 Mac mnttest/cgtest 0 fails.
ffsandbox.py aarch64 (Mac, mode on): RESULT {'on': True}, 0 sandbox log lines.
