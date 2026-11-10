# lane fdbindprop (2026-11-10) - FIXED

Symptom: rootless `podman run` -> `crun: set propagation for etc/hosts: No such file or directory`.

Root cause (not a file-bind lookup problem): crun opens_tree(OPEN_TREE_CLONE) the source, move_mount()s the
detached tree onto the target, then calls mount_setattr(fd, "", AT_EMPTY_PATH|AT_RECURSIVE) and
mount(NULL, "/proc/self/fd/N", MS_REC|MS_PRIVATE) on the SAME fd. After move_mount the fd still named the
(now empty) detached namespace: tree_src returned Anon(id) -> setattr walked an empty table (ENOENT), and
fd_mount_ns_path hit VFS_FD_PATH on an nsfd (ENOENT).

Fix: mnt::ATTACHED records (detached id -> ns, path) at move_attach; tree_src / fd_mount_ns_path resolve an
attached detached-tree fd to the attach path; a second move_mount of such an fd stays EINVAL; ids are
forgotten on recycle (open_tree_clone). Also: mnt::reverse_newest falls back to the covering mount for an
O_PATH fd opened BEFORE a bind landed on its object (crun's /proc/self/fd/N bind flow): EINVAL -> works.

Tests (mnttest, fdbindprop_*): open_tree+move_mount file/dir then setattr+private (fail-first: mount_setattr
errno=2 on main), plus 16 variants of bind-then-O_PATH / O_PATH-then-bind (tmp/disk, relative, file/dir).
(Remount of a bind mount inside a mntns is still EINVAL: mntro lane.)

Next error after the fix (rootless podman, Mac aarch64): crun: mount `.../userdata/shm` to `dev/shm`: Not a directory
(bind mount(src dir, "/proc/self/fd/17", bind) = -ENOTDIR; sys_mount_ns sdir != tdir). Open.
