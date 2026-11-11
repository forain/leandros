# lane mntiso-busy (E6) - FIXED 2026-11-11
- vfs `rewrite_one` (mntns callers): unlink/rmdir/rename of a path that is exactly a mount root -> EBUSY (new `mnt::is_mount_root`); rename/link whose two paths sit on different mounts -> EXDEV (compared by mnt_id in `handle_as`). Checks only added; no permission check loosened.
- Fail-first mnttest cases (userland/mnttest, mntiso_busy_suite): busy_rename_bind_root, busy_unlink_file_bind, busy_rmdir_mountpoint, xdev_rename_across_binds, xdev_link_across_binds (all FAIL on main, box aarch64 lane/mntiso-busy-red), plus busy_same_mount_rename_ok and busy_host_sentinel_intact (guards). All PASS after the fix (Mac aarch64 mnttest ALL PASSED).
- Known gap: dst of link() being a mount root is not specially handled (EEXIST path); `/` itself is excluded.
- TODO: no E6-specific item existed; the M6 line stays (bindpin lane).
