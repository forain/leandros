# lane f2fsimg — 2026-10-19 (bug list L4.3, L4.4, L4.5)

## L4.3 "minimal f2fs image: stat/ls -> [VFS] ENOMEM: port::send failed" — FIXED
Premise was wrong: the image is fine. `sys_mount` created the mount's IPC port with
`owner_pid = current_pid()`, i.e. the `mount` process. Ports die with their owner
(`ipc::port::release_by_owner`, task-exit hook), so as soon as `mount` exited the port was
tombstoned while the VFS mount entry and the f2fs MountState stayed: every later call under the
mount failed in `call_port` with PortNotFound, reported as ENOMEM. Any volume mounted from a
shell had it (reproduced on main with the populated root device too); only init's mounts
(pid 1 never exits) worked.
- Fix: the mount port is owned by the kernel (pid 0), like the drm/evdev/netctl ports.
- Error mapping: a send to a gone port now answers ENOTCONN (Linux's dead-FUSE-mount errno)
  with its own one-time `[VFS] ENOTCONN` line; a full queue stays ENOMEM.
- The minimal image (scripts/mkfs-f2fs-minimal.py) now mounts, `ls -la`, `stat`, `umount` OK.
- Test: chroottest `f2fs_mount_child` (a child mounts the root's f2fs device read-only and
  exits; the parent stats/getdents64/stats bin, umounts). Fails with the old owner
  (errno 107 after the remap), passes fixed.

## L4.4 concurrent block `register` interleaving — FIXED
`drivers/src/blkdev.rs`: a sleeping `KMutex` (`REGISTER`) now serializes `register` (disk +
partition scan) and `init`, so partitions always follow their disk. `init` also scans exactly
the disks it added (the old `first..first + n` was wrong if one add failed). No deterministic
runtime test (needs two hot-plug probes at once); boots/regress green.

## L4.5 splice racing writer loses socket/console bytes — FIXED
Pipe rings gained a `reserved` count: `pipe_reserve` takes free space before the splice
reads its source, `pipe_commit` appends what was read and returns the rest. Every writer
(write, pipe_to_pipe, poll POLLOUT, pipe_ready, drain-wake thresholds) uses `free()` =
size - count - reserved. Test: syscalltest `splice_socket_race` — with reservation disabled
it FAILS (`splice=100, S=100 ... left in socket 0`: 3996 bytes lost), passes fixed.

## Evidence
aarch64 HVF: runtests regress = all PASS (boots failed 0/1), incl. syscalltest 43/43 and
chroottest 23/23.

## Open / noticed
- `/proc/mounts` reports `rw` for a `mount -r` f2fs mount.
- A reservation is held while the splice blocks in a socket read, so other writers to that
  pipe see less space meanwhile (Linux holds the pipe lock in similar paths).
