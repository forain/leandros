# lane/devptsinst 2026-11-09 -- devpts instances

Status: FIXED (aarch64).

- `mount("devpts", dir, "devpts", flags, opts)` now creates a new instance (with or without
  `newinstance`, like Linux >= 4.7). Options parsed: `mode`, `ptmxmode`, `uid`, `gid` (octal modes,
  decimal ids); others ignored. Defaults as Linux: mode 0600, ptmxmode 0, owner = opener.
- Instance table + path parsing: `servers/vfs/src/devpts.rs` (16 instances). The mount is still an
  alias (`alias.rs`) from the mount point to the backing dir `/dev/.devpts/<id>` (listed in
  RAMFS_DIRS); the instance object does not depend on the mount table, so mntns can re-home it.
  `umount` frees the instance.
- Per-instance pty numbering: `servers/tty/src/pty.rs` `alloc_in(inst)` gives the lowest free
  number within the instance; `TIOCGPTN`, `/dev/pts/N` readlink and getdents use that number.
  The pool slot (global) stays the key for inode/rdev/PTS_META.
- `<mnt>/ptmx` per instance (mode = ptmxmode, root owned); `/dev/pts/ptmx` = default instance.
  The initial `/dev/pts` and `/dev/ptmx` are unchanged (instance 0, 0620 root:tty).
- mount(devpts) allowed with CAP_SYS_ADMIN in the caller's userns (`caps::ns_capable(21)`), not
  only euid 0.
- Test: ptytest `devpts_instances` (18th check): two instances mounted on /tmp dirs, ptmx modes,
  pty 0 in each while the default instance has 3 ptys open, slave path open + I/O, slave
  mode/gid, absent number in the other instance. On unmodified main kernel it FAILS
  (ptytest 17/18); with the change 18/18.
- Open: a devpts mount over a chroot that also bind-mounts host /dev/ptmx now gets a new
  instance, so such a chroot must use <mnt>/ptmx (Linux behavior). uid/gid options are not
  translated through the userns map. Slave nodes of an instance are not listed with `ptmx` in
  getdents.
