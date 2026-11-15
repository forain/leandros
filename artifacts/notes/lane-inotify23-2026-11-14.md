# lane/inotify23 (L2 + L3 of design-inotify.md) 2026-11-14

Status: DELIVERED (path-keyed variant of the design).

- L2: dirent events for every filesystem the VFS routes (f2fs/FUSE/overlay, not only tmpfs); MOVE_SELF, DELETE_SELF+IGNORED for files and dirs, rename-over delivers DELETE_SELF to the clobbered target, watches follow file and dir renames, IN_UNMOUNT+IGNORED from `mnt::sb_put` and `sys_umount2` (f2fs/FUSE).
- L3: MODIFY (write, truncate, fallocate, O_TRUNC), ATTRIB (chmod/chown/utimens/xattr/link/unlink), OPEN, ACCESS, CLOSE_WRITE/NOWRITE on the last put of a description (an OFD note table, `ofd::mark_watched` + release hook), DELETE_SELF of an unlinked-open file after the last close. Hooks are gated by counters of watches that ask for inode events (ACCESS has its own), so the desktop pays nothing for reads.
- Deviation from the design: watches/notes are keyed by backing path, not (dev, ino) (no INO_BLOOM). Consequences: no self event through a second hard link; unlink of one of several hard links reports DELETE_SELF for that name.
- Fail-first: tests-only commit 7f4b0a3b on aarch64 box: 38 failures (every new case on tmpfs and f2fs); with the implementation 73 pass on the Mac, the 2 remaining were a test bug (L2/L3 rerun reused the L1 base dir), fixed.
- inotifytest now runs the shared case set on /run (tmpfs) and /root (f2fs): create/delete/move/dir-follow, rmdir/move/unlink self, rename-over, symlink/link/mkfifo, modify/trunc, close write/nowrite/dup, attrib, access, coalesce, unlink-while-open, umount, cross-bind.
