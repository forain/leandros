# lane/ffcache (L11.11) — Firefox Cache Storage bodies read empty (AbortError) — FIXED (aarch64 QEMU; laptop below)

## Root cause
Firefox's parent opens a Cache morgue file on f2fs, dup()s it, sends the dup over SCM_RIGHTS and closes both fds.
The VFS sent VFS_CLOSE to the mount server once no fd TABLE named (port, file_id), ignoring the copy queued on the
socket (tmpfs had TMP_INFLIGHT; mounted files had nothing). The receiver installed a stale file_id: read returned 0
(laptop) or another file's bytes (guard). Laptop trace: artifacts/remote-logs/20261024-151229-172.16.149.179/.

## Fix
servers/vfs/src/lib.rs: MOUNT_INFLIGHT (port, file_id, count), counted by tmp_inflight_inc/dec; handle_close and
release_vnode MountedFile arms keep the mount open while a copy is in flight. drop_transfer still closes it if the
copy is never received (dec, then release_vnode).

## Guard: scmtest fd_pass_mounted_closed
Picks the first dir statfs reports as F2FS (fails if none; /mnt in QEMU). Mode 0 = dup+send+close both
(handle_close), mode 1 = forked sender exits before recvmsg (release_vnode). Churns the server with a different file.
- main's vfs: FAIL (content wrong, slot reused by the churn file)
- fix minus the release_vnode hook: mode0 ok, mode1 content_ok=0 -> FAIL
- fix: PASS x3. After `git merge main`: regress PASS, scmtest+vfstest+f2fstest PASS (aarch64).
FFTRACE kernel diagnostics from the laptop hunt were dropped (not merged).
