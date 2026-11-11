# lane/procfd 2026-11-11 - fd links follow rename/unlink (FIXED, mostly already done)

- TODO item was largely stale: tmpfs (path is the name) and f2fs (open_paths_renamed/deleted, d2fecaca; proctest fd_rename_follow/fd_deleted on /tmp and /root) already followed rename and appended " (deleted)".
- Real gap: FUSE mounts kept the open-time path. servers/fuse/src/ops.rs now rewrites open-file paths on RENAME/RENAME2 (incl. children, exchange) and appends " (deleted)" on UNLINK/RMDIR/replaced target.
- Guard: fusetest fd_link_opened_path / fd_link_follows_rename / fd_link_deleted_suffix (daemon gained RENAME op 12). Red on 73383eb5 (box aarch64: fails=2), green after fix.
- Not touched: no per-open mnt_id (mntiso-procfd E1 builds that separately); ovl fds not checked.
