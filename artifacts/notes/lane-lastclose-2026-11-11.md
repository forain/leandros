# lane lastclose (2026-11-11) - FIXED

Last close of a mounted (f2fs) file walked every fd table under the exclusive FD_COPY_GATE (O(all fds)); tmpfs already had TMP_FDREFS.
Fix (servers/vfs/src/lib.rs): FdVec set/clear/clone/drop now also maintain MOUNT_FDREFS (port,file_id) and BLK_FDREFS (bdev slot); SCM_RIGHTS in-flight copies share the same counters (MOUNT_INFLIGHT/BLK_INFLIGHT removed). Last close = `mount_referenced()`. FD_COPY_GATE, scan_gate, any_fd removed; tmpfs census no longer needs the gate (orphan = not seen by scan and TMP_FDREFS==0). Docs: vfs.md, servers/README.md.
Measured (aarch64, `fdmaxtest closebench`, open+close of an f2fs file, N other open fds):
  before: N=0 9.0us, 1000 9.7us, 5000 12.4us, 20000 21.0us (grows)
  after:  N=0 8.7us, 1000 8.2us, 5000 8.7us, 20000 8.5us (flat)
Tests (fdmaxtest, in regress): lastclose_tmpfs / lastclose_mounted: unlink-while-open + last close frees (create/unlink/close rounds beyond pool/slot limits), across fork, dup2, SCM_RIGHTS in flight (and dropped unread), exec CLOEXEC, process death. They pass before and after (the old scan was correct, only slow); they guard the new counters against leaks (EMFILE/ENOSPC) and early close (read-after-close).
Both arches: box full regress aarch64 83/0, x86_64 see status line. Open: double VFS_CLOSE when two last fds close concurrently is pre-existing (both closers may see zero).
