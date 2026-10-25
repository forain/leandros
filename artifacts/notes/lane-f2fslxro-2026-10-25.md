# lane f2fslxro (design f2fslxrw S6 + S1) - FIXED

- S6: linux_import(.., ro) puts the patched SIT/NAT blocks in RO_OVERLAY (servers/f2fs/src/lib.rs), applied by every cache fill
  (io_read_block, fetch_read); nothing is dirty, so nothing is written back; no checkpoint, no device write. Cleared at unmount.
- Linux-made (native) volumes now mount read-only: `-o ro` explicitly, and a plain mount is downgraded with a klog line.
  remount rw of a native volume is ignored. rw stays refused until S4 flips NATIVE_IMPORT_SAFE.
- S1: inline data read (read_file_data_planned), inline dentry lookup (inline_dir_find) and getdents; inline xattr size is now
  per inode (flexible_inline_xattr). Open for read of inline files/dirs works; mutation of inline inodes still gives EIO (S2).
- Evidence (aarch64 QEMU, pristine Linux images lxdef/lxnoi/lxext, cp -c copies): all mount ro, md5sum -c MANIFEST all OK
  (lxdef incl. inline files, 301-entry dir1, symlink, inline readlink), images byte-identical (cmp) after mount+read+umount.
  Full aarch64 regress PASS.
- Open: S2 inline write, S3 inode checksums, S4 SSA/curseg carry-over (then flip NATIVE_IMPORT_SAFE), no automated case yet.
