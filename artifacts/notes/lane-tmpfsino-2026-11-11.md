# lane tmpfsino 2026-11-11 - FIXED
- Premise corrected: tmpfs st_ino was NOT the FNV `path_ino` (that only serves /proc, sysfs, initrd); it was `0x2000_0000 + slot`, so a freed slot reused by the next claim gave a replaced file/dir at the same path the SAME st_ino (defeating the bind pin).
- Fix: `TmpFileEntry.ino`, allocated from a pool counter at `claim` (0x2000_0000 + n, wraps at 2^28), kept across rename (slot and entry persist), hard links report the owner's ino (stat and fstat). Lock keys (tmpfs: slot, mounted: (port, ino)), inotify (path keyed), fdinfo untouched; st_dev unchanged.
- Tests (mnttest, red on baseline: dir_replaced_same_path, new_file_fresh, new_dir_fresh; green after): bindpin_dir_replaced_same_path (ESTALE, not NEW), tmpfs_ino_rename_stable, tmpfs_ino_new_file_fresh, tmpfs_ino_new_dir_fresh, tmpfs_ino_hardlink_shared.
- aarch64 (HVF): mnttest, vfstest, exitracetest, inotifytest 0 fails; podbridge + podpause pass; podpasta only pasta_portfwd (known).
