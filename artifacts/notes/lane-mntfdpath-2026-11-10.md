# lane/mntfdpath 2026-11-10 (podtrace4 blockers #3 and #4)

FIXED (a): errtrace `sched.errtrace=<comm>` now matches a comm PREFIX (crun's re-exec `crun.XXXXXX`); off = one relaxed atomic load as before.
FIXED (b) `crun: make <rootfs> private`: mount(2) propagation targets of the form /proc/self/fd/N (or /proc/<own pid>/fd/N) now follow the
link: fd's backing path mapped back through the caller's mount tree (mnt::reverse; hidden /.leandros-sb paths -> EINVAL, never leaked).
Before, the target stayed the literal magic path, so every step of crun's make_parent_mount_private walk was EINVAL and the walk never reached a mountpoint.
(`resolve_mount_target` in kernel/src/syscall.rs, used by sys_mount and sys_mount_ns.)
FIXED (c) nsfs bind inside a non-initial mntns: sys_mount_ns recognises /proc/<pid>/ns/<n> as bind source (mnt::bind_nsfs); the mount's root is
the synthetic `/.leandros-ns/<k>/<id>`, its superblock (kind nsfs) owns the namespace reference, so copies/umount/ns death drop it correctly.
FIXED (d): ns-0 self-binds and ns-0 nsfs binds are now copied into new mount namespaces (ns0_snapshot; nsfs copies take their own ns ref).
Tests (mnttest fdpath_suite, 9 cases): on unfixed main 8 FAIL (x86_64 box); all PASS on aarch64 Mac with fix. Userns cases make their fixtures before
unshare(NEWUSER) because without idmap=test the new userns has no uid map (files created inside are EOVERFLOW/unmapped).
NOT DONE: stat(2) on a bound nsfs path; mount(2) bind SOURCE/target given as /proc/self/fd/N (only propagation uses the link resolver).
