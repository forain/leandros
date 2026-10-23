# lane/noexec 2026-10-23 (L7.10 follow-ups)

FIXED.
- mmap(PROT_EXEC) of a file on a noexec tmpfs: EPERM for MAP_PRIVATE and MAP_SHARED
  (`sys_mmap` in kernel/src/syscall.rs asks `vfs::fd_on_noexec_mount`; MAP_ANONYMOUS is
  untouched; memfd/dmabuf/unlinked inodes are not on a mount and are never refused).
- mprotect adding PROT_EXEC to such a mapping: EPERM. A successful file mmap on a noexec
  mount tags its VMAs with the kernel-private `MAP_NOEXEC_MOUNT` (1<<28, mm/src/vmm.rs);
  `sys_mprotect` checks `range_noexec_mount`.
- statfs/fstatfs on tmpfs report f_flags: ST_NOSUID=2, ST_NODEV=4, ST_NOEXEC=8 from the
  root's mount flags (servers/vfs `tmpfs_st_flags`). `handle_statfs` used to send every
  /dev/* path (incl. /dev/shm) to the zero-capacity procfs answer; tmpfs roots now take the
  real tmpfs figures first. f2fs/mounted fs keep ST_RDONLY|ST_NOSUID (no noexec/nodev
  option exists for them).
- Not done: ST_RELATIME (tmpfs does not track atime options).
- Tests: privtest `tmpfs_statfs_*`, `tmpfs_fstatfs_*`, `tmpfs_noexec_mmap_{private,shared}_eperm`,
  `tmpfs_noexec_mmap_read_ok`, `tmpfs_noexec_mprotect_exec_eperm`, `tmpfs_noexec_anon_exec_ok`,
  `tmpfs_mmap_exec_after_clear`. Before the fix: 7 new cases failed on aarch64; after: all pass.
