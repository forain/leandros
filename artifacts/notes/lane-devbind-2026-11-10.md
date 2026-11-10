# lane devbind (2026-11-10)
FIXED: rootless `podman run --rm --network=none alpine:3.21 echo hi` (aarch64 Mac, idmap=test, leandro) prints `hi`, RC=0.
1. `crun: mount /dev/full to /dev/full: No such file or directory`: the host had NO /dev/full at all (RAMFS list and
   VnodeKind had null/zero/urandom/random only). Added VnodeKind::DevFull (1:7): reads zeros, writes ENOSPC, S_IFCHR 0666
   for stat/fstat, rdev for mknod/stat paths. No permission or nodev logic touched.
   Tests: mnttest devbind_{null,zero,full,tty,random,urandom}_{path,fd} (crun sequence as uid 1000 in userns+mntns:
   create file in tmpfs /dev, bind by path or /proc/self/fd/N, remount nosuid|noexec, then read/write semantics).
   devbind_full_path/_fd failed first (`host /dev/full errno=2`), pass after.
2. Next error was `crun: race condition detected remounting masked path /sys/firmware`: crun fstat's the maskdir
   open_tree clone, move_mounts it, open_tree-clones the target and requires equal st_dev/st_ino; the NsFd arm reported a
   per-clone synthetic ino. Now a directory clone reports a hash of (superblock, root path) of its root mount
   (mnt::anon_root_ident). mnttest devbind_maskdir_same_ident passes but did NOT fail on the unfixed tree (simple tmpfs
   variant); the real podman run is the fail-first evidence (error before, `hi` after).
