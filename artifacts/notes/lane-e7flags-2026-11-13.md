# lane e7flags (2026-11-13) - FIXED

Follow-up to mntiso E7: a dirfd hidden from the caller's mount namespace (opened before pivot_root/chroot) walked its backing path without the per-mount flags of the mount it was opened on, so a dirfd from a ro bind could write after unshare+pivot.

Fix: `ofd` records the mount flags at open time (`set_mnt` gets `mfl`); `hidden_below` (kernel/src/syscall.rs) looks up the fd's mount (live flags if the mount still exists, else the recorded ones; recorded-unknown => MNT_RDONLY) and publishes a `CallCtx{hidden_flags}`; `rewrite_one` (servers/vfs/src/lib.rs) runs `mnt_flag_check` with them (EROFS for write/create/unlink/rename/symlink, EACCES for nodev device opens). Files opened through a hidden fd inherit the flags (ns u16::MAX marker).
Test: mnttest `e7_hidden_fd_keeps_mount_flags` (ro bind dirfd, pivot+detach old root, mkdirat/creat/open-wr/unlinkat/renameat/symlinkat => EROFS, host untouched).
Open: noexec is not enforced on this path (execveat on a hidden dirfd); an fd opened in ns 0 (no mount record) is treated as flag-free.
