# lane/nsbind 2026-11-11
FIXED memfd exe pin not inherited by fork: `vfs::exe_pin_inherit` (called from `fork_dup_tables`) gives the child its own pin; test memfdtest `memfd_exe_pin_inherited_by_fork`.
FIXED fchmod/fchown/futimens on fds of a read-only mount -> EROFS: open through a mount-namespace mount tags the open file description with (mntns, mount id) (`ofd::set_mnt`, VFS_OPEN arm); the three fd handlers check the mount's current flags (so remount-ro after open is honoured, as Linux mnt_want_write). Test mnttest `mntro_fd_attr_erofs`.
Fail-first: both new cases red on the tests-only commit (box aarch64: mnttest fails=1, memfdtest fails=1), green after.
ALREADY FIXED (stale TODO): nsfs bind in a non-init mntns and self-binds copied into new mntns (lane mntfdpath 2026-11-10, bind_nsfs / ns0_snapshot); struck.
NOT DONE: rename on devfs. /dev is a static node set with fixed major/minor (handle_rename answers EROFS for RAMFS/devfs); Linux devtmpfs renames like tmpfs. Needs a mutable devfs name table; left in TODO.md.
