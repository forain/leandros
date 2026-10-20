# lane/vfsabi — VFS ABI gaps (L10.7, L10.10, L4.7, L4.9)

## What changed

- **fd 0/1/2 closed state (L10.7).** The implicit console had no fd-table entry, so
  after `close(1)` a write still reached the console. `FdVec` now records a closed
  console slot (`vfs::close_console`, `vfs::fd_redirected`): write/fstat/dup/fcntl/a
  second close are EBADF, the next open() takes the number, fork inherits the state.
  Kernel stdio fast paths (`kernel/src/syscall.rs` write/ioctl/dup/fd_ofd) consult it.
- **`make` on the serial console EBADF (L10.7).** `make` is not in the image; the
  reported failure is the close-stdout-at-exit mechanism (gnulib `close_stdout`), the
  same one git hit (fixed in adbcb9c5). Covered by `stdio_close_ebadf`.
- **getdents cursor and static-file offset per OFD (L10.7).** `ofd.rs` Slot gained
  `pos`; a dup'd or inherited directory fd shares the cursor; `O_NONBLOCK` set
  through one descriptor is seen through its dups (per description).
- **`ls /dev/input` (L10.7).** `/dev/input` listed as a directory from the dynamic
  device registry.
- **RamFS /etc shadowing (L10.7).** `etc_on_disk()`: once / is mounted, /etc comes from
  disk (except /etc/mtab and /etc/resolv.conf); the RamFS defaults were added to
  `mkfs-f2fs-populated.py` so the image carries them.
- **Path limits (L10.7).** tmpfs paths and symlink bodies heap-allocated (PATH_MAX
  4096, link body 4095); fixed 256-byte buffers in stat/open `/proc/<pid>/root` paths
  removed; comm/basename taken from the whole exe path (`sched::exe_basename`).
  Kernel KPATH_MAX was already PATH_MAX on main.
- **pty ownership (L10.7).** `PTS_META`: slave owned by the ptmx opener, gid tty(5),
  mode 0620; chmod/chown on node or fd; open of the slave checked against it.
- **tmpfs (L10.10).** Per-slot fd reference count `TMP_FDREFS` (fd entries + queued
  SCM_RIGHTS) replaces the fd-table walk on unlink/release; xattr arenas moved to an
  on-demand side table (8 MiB BSS gone); parent/child index makes rmdir emptiness
  O(1) and directory rename visit only its subtree.
- **Jan-1 timestamps (L4.7).** Image builders stamp atime/mtime/ctime (host mtime for
  copied files, build time / SOURCE_DATE_EPOCH otherwise). Needs a rebuilt image.
- **/proc/mounts `rw` for `mount -r` (L4.9).** `set_mount_rdonly_by_port` keeps the
  VFS mount table's flag in step for /proc/mounts and statfs.
- **New bug found + fixed:** execve and faccessat did not follow a final
  `/proc/<pid>/exe` magic link (ENOENT; `env /proc/self/exe`, brush, and syscalltest's
  exec cases when started by bare name). Both now call `follow_magic_final`.

## Tests

syscalltest new cases: stdio_close_ebadf, getdents_shared(/tmp,/root,/dev),
nonblock_shared, dev_input_dir, etc_on_root_fs, tmpfs_long_paths, comm_deep_path,
pts_owner, mount_ro_flag, image_timestamps, proc_self_exe_access. Cases were checked
against Linux (desktop box) by the previous agent; all PASS on aarch64 (HVF).

regress (aarch64): 37 suites green under host load; pollnarrowtest and nettag2test
(wake-count timing) failed only while other QEMUs loaded the host, and pass 4/4 each
under the qemu lock (runs `rr`, `rr3 --repeat 3`) on the rebased build. After the rebase onto main 81091ac6 (conflicts with lane/procfs
execve magic-link follow and tmppool `unlinked`), full regress 41/41 RC=0 (tag reg2), and again 41/41 after rebasing onto f3e71d3e (reg3).

## Open

- `lstat("/proc/self/exe")` ENOENT (L10.6 item, not this lane).
- Old faccessat (3-arg) reads a3 as flags; garbage there can skip the follow.
