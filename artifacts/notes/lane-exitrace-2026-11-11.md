# lane/exitrace 2026-11-11: FIXED

Exit 127 of `podman run --rm` (aarch64 bridge). Cleanup error captured with a C exit-command shim (/bin/exitwrap, no sh delay):
`Error: disk I/O error: no such file or directory` (sqlite on f2fs).
H1 (reaped pid visible) and H2 (rename visibility): refuted, exitracetest 0/3000 each.
Root cause: VFS fcntl/flock lock records were keyed by the mount server's open-file slot (one per open(2)), not the inode.
Two processes opening db.sql both got EXCLUSIVE; each deleted the other's db.sql-journal (unlink ENOENT -> sqlite IOERR).
The sh wrapper's delay merely avoided the overlap. Fix: key by (port, st_ino), release by slot, owner = tgid.
Evidence: exitracetest fcntl-lock-cross-open red before, green after. churn127loop 40x: 2/40 before, 0/40 after.
