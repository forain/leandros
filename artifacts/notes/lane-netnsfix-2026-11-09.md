# lane/netnsfix 2026-11-09 (podtrace3 blocker #2)

FIXED (a): unshare(CLONE_NEWNET) EINVAL. Cause: sys_unshare rejected NEWNET/NEWIPC/NEWUTS from a multithreaded
caller (`flat != 0 && !alone`). Go (podman, netavark) runs LockOSThread + unshare on a thread of a multithreaded
process. Linux allows it (namespaces are per thread). Check removed; CAP_SYS_ADMIN check and owner userns unchanged.
The per-uid quota (idfix1) returns ENOSPC, not EINVAL; not involved.

FIXED (b): `mount --make-rshared /run/netns`. Cause: a self-bind (mountpoint == source) was a no-op in the ns-0 alias
table, so /run/netns never became a mount point and set_propagation answered EINVAL. Self-binds are now recorded
(mnt::ns0 SELF_BINDS), appear in ns0_mountpoints, mountinfo/mounts and umount, so MS_SHARED|MS_REC works and shows
`shared:N`.

FIXED (c): bind-mounting /proc/<pid>/ns/<name> onto a file (ip netns add, netavark) in ns 0. The alias source is the
synthetic `/.leandros-ns/<kind>/<id>`; the mount holds a namespace reference (released on umount/replace); open()
of the target yields an NsFd, so the netns outlives its creator and setns() works via the bound path.

NOT DONE: the same nsfs bind inside a non-initial mount namespace (rootless podman's own mntns): sys_mount_ns /
mnt::bind do not know the synthetic source. Self-bind registry is also not copied into a new mount namespace.
stat(2) on the bound path (not fstat on the opened fd) is not special-cased.

Tests: mnttest netns_suite (12 cases). On main: unshare_newnet_threaded, netns_make_rshared_ok/visible,
netns_nsfs_bound_survives_and_setns FAIL; all PASS with the fix. Mac aarch64: mnttest, nstest, nettest pass; ffsandbox on=True off=False.

FIXED (d): SIOCSIFFLAGS (0x8914) on a socket fd was ENOTTY (crun brings lo up). Now handled with SIOCGIF*: only "lo" of a
non-initial netns, CAP_NET_ADMIN over the netns owner userns (EPERM otherwise), writes the same LO_UP state netlink
RTM_NEWLINK uses. Other links: EOPNOTSUPP; ns 0 lo: EOPNOTSUPP. nstest: netns_siocsifflags_lo, netns_siocsifflags_lo_userns.
