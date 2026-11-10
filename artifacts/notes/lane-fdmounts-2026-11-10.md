# lane/fdmounts 2026-11-10 (podtrace5 blocker 1)

PARTIAL (blocker 1 FIXED; `podman run --rm --network=none alpine echo hi` still does not print: next failure `crun: pivot_root: Invalid argument`).

- mount(2) target (and bind source) given as `/proc/self/fd/N` now resolves to the fd's directory in the caller's mount namespace for every form
  (`mount_target_kpath` / `fd_mount_ns_path` in kernel/src/syscall.rs; propagation shares it). Descriptor -> namespace path uses `mnt::reverse_newest`
  (newest round-tripping mount wins: a container's rootfs mounts, not the copies of the host's); a cgroup2 fd path is normalised to /sys/fs/cgroup.
- sys_mount_ns now serves proc, sysfs, mqueue, cgroup2 (backing roots /proc, /sys, /dev/mqueue, /sys/fs/cgroup; all reader-pidns/cgroupns-aware already)
  and devpts (fresh instance as sb) in addition to tmpfs. Flags (ro, nosuid...) recorded on the mount.
- Linux fully-visible rule: proc/sysfs from a non-initial userns need an existing unobstructed mount (root = whole fs, no locked child, no locked flag dropped)
  else EPERM (`mnt::fully_visible`). may_mount over the owner userns unchanged; /.leandros-sb is never reachable (is_sb_path -> EINVAL).
- Known limits: mqueue and cgroup2 share the global instance (no per-mount superblock); proc view follows the READER's pidns (equals the mounter's in crun).
- Tests: mnttest fdmounts_* (5 cases). On the tests-only branch (main code) all 5 FAIL on box x86_64; with the fix all pass (aarch64 Mac, box x86_64 regress PASS).
- podman steps passed now: mounts of proc/tmpfs/sysfs/devpts/mqueue/cgroup2, cgroup2 propagation, masked-path bind with fd source. Next: pivot_root in a mntns (EINVAL by design since mntunshare). See podtrace5-findings.md "after fdmounts".
