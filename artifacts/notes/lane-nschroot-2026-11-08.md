# lane/nschroot (2026-11-08) - DELIVERED (non-userns part of L3)

- sys_chroot: any target under /proc maps to `vfs::VOID_ROOT` (/.leandros-void), an empty 0555 ramfs dir
  (kernel/src/syscall.rs). resolve_ns hides the void name (ENOENT) unless the caller's root is the void.
  Lookups below it fall to the root mount and are ENOENT, so /proc/self/status no longer leaks; ".." clamps; pre-opened fds work.
- VFS: /proc/<pid|self|thread-self>/fdinfo dir (lists fd numbers, DT_REG, stat'able) and fdinfo/N (pos, flags octal, mnt_id 0).
  pos is exact for tmpfs/ram files; mounted-file pos reads 0 (not tracked in the vnode).
- CLONE_FS: set_root already applies to the whole fs group; verified by chroottest with CLONE_FS|SIGCHLD and CLONE_VM|CLONE_FS helpers.
- Tests (chroottest, nstest not on main): procfd_info, ff_chroot_helper_{fs,vm}.
- Evidence: with the void mapping disabled the new cases FAIL ("LEAK: /proc/self/status opens", 23 pass/3 fail); fixed: 26/0.
  Default aarch64 regress: overall PASS.
- Open: CAP_SYS_CHROOT gate (caps lane), userns-aware chroot, /proc/<pid>/root readlink for void root ("/") not done,
  mounted-file fdinfo pos.
