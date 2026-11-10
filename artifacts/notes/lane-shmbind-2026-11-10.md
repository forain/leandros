# lane shmbind (2026-11-10)
FIXED: fstat/statx on an open_tree(OPEN_TREE_CLONE) descriptor of a directory reported S_IFREG|0444
(servers/vfs NsFd arm). crun clones the bind source with open_tree, fstat's that fd, saw a regular file,
created the dev/shm mount point as a file (mode 0100700 in /.leandros-sb/tmpfs/N/shm), then
move_mount -> EISDIR and mount() -> ENOTDIR (sdir=1 tdir=0). Now a directory clone reports S_IFDIR|0755.
Path stat/translation was NOT the mismatch (deep /home path, fd-mounted tmpfs, relative open all pass).
Tests: mnttest shmbind_open_tree_fstat_dir[_tmpfs] failed first (fstat mode=100444), pass after.
