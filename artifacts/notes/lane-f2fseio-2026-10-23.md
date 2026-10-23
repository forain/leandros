# lane/f2fseio 2026-10-23 - failed device reads are no longer cached as zeros (L4.11) - FIXED

Bug: `fetch_read` (unlocked miss fetch) and `BlockCache::load` (locked miss) turned a failed
virtio-blk read into valid zero blocks in the block cache -> mapped-page fault read zeros (no
SIGBUS), read() returned zeros/EOF, and RMW writes could persist zero-derived data.

Fix (servers/f2fs/src/lib.rs):
- `fetch_read` returns (buf, ok); a failed fetch installs nothing, and the request is finished
  with the read under the lock so it fails once with EIO.
- `load` on read failure: not installed, sets `cache.read_failed`, marks the device in
  `IO_ERR_DEVS`, returns the zero scratch block. `io_write_block(s)` refuse writes to a marked
  device (mount is read-only until remount), so metadata derived from zeros never reaches disk
  (checkpoint writes fail).
- Locked read fallback returns -EIO when nothing could be read (incl. failed inode block);
  partial reads return the bytes read. Fault path: pc_fill -> Io(-5) -> SIGBUS (existing).
Evidence: blkfaulttest aarch64 FAIL "silent read" -> PASS (frc=135 SIGBUS); regress: vfstest,
pcachetest, f2fstest green; fsck-f2fs PASS on both data images. pstest fails 4 pts tty_nr/ps
checks (pty, not f2fs; not touched here). blkfaulttest added as runtests.py `blkfault` (own boot,
not in regress).
Open: no dedicated read() EIO guard (the read path shares the fix; only mmap covered by the
test); mount-time superblock/CP reads (~line 7411) still unchecked; sticky read-only until remount.
