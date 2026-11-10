# lane mntro (2026-11-10) - FIXED

Per-mount MS_RDONLY/MS_NODEV/MS_NOEXEC enforcement in non-host mount namespaces.
- servers/vfs/src/lib.rs `mnt_flag_check`, called once from `rewrite_one` after ns resolution (ns-0 tasks never reach it): EROFS for open-for-write/O_TRUNC/creating O_CREAT, mkdir/mknod/unlink/rmdir/rename/link/symlink/chmod/chown/utimes/xattr set/remove (special files stay writable); EACCES opening chr/blk node on nodev. Already-open fds keep writing after remount-ro.
- kernel/src/syscall.rs: noexec -> EACCES in `exec_permitted`; MS_REMOUNT (bind or not) in a non-host mntns used to return EINVAL (so crun's readonlyPaths never applied); now sets the mount's ro/nosuid/nodev/noexec via mnt::setattr (locked flags -> EPERM).
- Fail-first (tag mntro-tests, tests only, aarch64 box): mntro_ro_bind_erofs, mntro_rw_fd_survives_remount_ro, mntro_nodev_eacces FAIL (mntro_noexec passed on main: exec was already refused elsewhere or not reachable; kept as a guard). Fixed: mnttest PASS, full aarch64 regress 80 suites 0 bad.
- Podman (root, Mac aarch64): `echo x > /proc/sys/kernel/hostname` -> "Read-only file system", rc=1.
- Not covered: fchmod/fchown/futimens on fds (Linux also EROFS), fd-relative *at paths without ns ctx.
