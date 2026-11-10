# lane/mntsetns (M7, setns CLONE_NEWNS) - 2026-11-09

Status: FIXED (see verification).

## Behaviour
setns(nsfd, CLONE_NEWNS) and setns(pidfd, CLONE_NEWNS|...) join a mount namespace (Linux mntns_install):
- EPERM unless CAP_SYS_ADMIN over the target mntns's owner userns AND CAP_SYS_CHROOT + CAP_SYS_ADMIN in the caller's userns (a NEWUSER join in the same pidfd call grants them).
- EINVAL when the caller's fs struct (fs_id group, CLONE_FS) is shared with another live task; checked before any entry is applied.
- Fails closed (EINVAL) when the slot is not a live mount namespace (not IS_MNT or DYING).
- On success the task's mntns is replaced, root and cwd reset to the new namespace's root; old ref dropped outside the lock (DYING/reap rules from mntunshare apply). pid/user namespaces untouched.
- The /proc/<pid>/ns/mnt fd holds a reference, so the namespace outlives its creator (setns_mnt_fd_keeps_alive).
- Code: sched/src/ns.rs (check_join kind 4, join apply, setns_pid), tests in userland/mnttest (setns_mnt_*).
- nstest: two stale expectations updated (setns(mntfd, 0) is now a wrong-type probe; pidfd NEWNS probe uses CLONE_NEWTIME).
- No access path to /.leandros-sb: resolution is unchanged, root becomes the namespace root.

## Verification
- Mac aarch64: mnttest 0 fails (6 new setns_mnt_* PASS), nstest 0, chroottest 0, ffsandbox on=True off=False.
- Fail-first: on main the code returned -22 unconditionally for kind 4 / CLONE_NEWNS (ns.rs), so every setns_mnt_* join case failed with EINVAL; first run of nstest also caught the two stale probes.
- Box x86_64 and aarch64 full regress: see merge message / report.
