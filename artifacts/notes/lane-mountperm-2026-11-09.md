# lane mountperm — 2026-11-09 — FIXED (security)

Hole: devptsinst (de4bf391) allowed mount(devpts) to any task with CAP_SYS_ADMIN in *its own* userns. With CLONE_NEWNS
still rejected, that mount landed in the initial mount namespace: any user could unshare(CLONE_NEWUSER) and mount devpts
over host paths (e.g. /etc).

Fix: `may_mount()` (kernel/src/syscall.rs) = Linux may_mount: CAP_SYS_ADMIN in the user namespace that OWNS the caller's
mount namespace; mntns 0 => host_root(). Gates sys_mount (every fstype incl. fuse, devpts, cgroup2, mqueue, tmpfs, bind,
remount, propagation), sys_umount2 (incl. the FUSE unmount path) and sys_pivot_root. Non-host-root callers that pass
may_mount (only possible in a mntns != 0, not creatable before lane mntunshare) are limited to devpts.
Audit: sys_mount has a single permission gate for all fstypes; the devpts exception was the only bypass. umount2/pivot_root
were already host_root-only and now share the rule.

Tests (userland/mnttest): userns_root_mount_eperm (uid 1000 + NEWUSER + maps: devpts/mqueue/tmpfs/cgroup2/bind all EPERM,
mountinfo unchanged), host_root_devpts_ok.
Evidence (aarch64 HVF, Mac): fail-first with main's syscall.rs: userns_root_mount_eperm FAIL; fixed: PASS. After merging
main (FUSE; conflict in syscall.rs resolved keeping sys_mount_fuse + may_mount in umount2): mnttest, fusetest, ptytest,
nstest, mqtest, cgtest all PASS. x86_64 not run in this lane (box infra issues); covered by the next integration run.
