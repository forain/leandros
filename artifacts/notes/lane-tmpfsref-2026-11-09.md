# lane/tmpfsref 2026-11-09

Bug: a mount namespace copied from ns 0 saw a tmpfs alias mount (instance /.leandros-sb/tmpfs/<n>) with sb 0, so it
took no reference; umount in ns 0 destroyed the instance under the copy.

Fix (FIXED for tmpfs): `ns0_snapshot` (servers/vfs/src/lib.rs) sets `Mnt.sb` to the tmpfs instance id parsed from the
alias source, so `mnt::copy` -> `sb_get` holds a counted reference in every copy and `release`/umount -> `sb_put`
frees the instance only with its last mount. Superblock ids come from the monotonic SB_NEXT, never reused, so a new
tmpfs can not alias an old copy's mount.

Tests (userland/mnttest): tfs_copy_keeps_instance, tfs_no_reuse_after_umount.

Open: devpts instances (dev/.devpts/<id>, first-free slot ids) have the same shape: ns copies hold sb 0 and an
id can be reused after ns 0 umounts. proc/devtmpfs/sysfs/mqueue/cgroup2 are singletons (no instance), unaffected.
