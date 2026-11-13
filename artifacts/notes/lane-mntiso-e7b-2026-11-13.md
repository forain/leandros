# lane mntiso-e7b (2026-11-13) - FIXED

E7: `openat(dirfd, rel)` on a dirfd with no name in the caller's mount namespace (opened before pivot_root, or outside the chroot) had its backing path reinterpreted as a namespace path, so a same-spelled path in the new tree could be opened (a different file). TODO.md wording ("host_to_view hint") was imprecise: the bug is the raw-path fallback, not a missing hint.

Fix (kernel/src/syscall.rs): `fd_hidden_host` detects such an fd (mnt::reverse fails, or the ns path is outside the chroot); `resolve_at_path` and `/proc/self/fd/N/<below>` then walk below the fd's backing path (`hidden_below`, `..` above the fd dir = EXDEV). Other `resolve_at_ns` callers fail closed (ENOENT).
Caveat: ro/nodev flags of the fd's original mount are not enforced on this path (no mnt_id in the current ns).
Test: mnttest `e7_dirfd_keeps_object_{pivot,chroot}` (openat/fstatat/mkdirat/unlinkat, /proc/self/fd/N/f in pivot). Red on main: chroot case read 'VIEW'; green after fix.
