# lane f2fslxsafe (design f2fslxrw S0) - FIXED (as a refusal)

- SbInfo gains native (init_version "Linux version"), feat, segs_per_sec, cp_payload, multi_dev, log_blocksize; Fmt.lx = LX_TAG || native.
- Measured: even a mount that only reads makes linux_import commit a checkpoint that drops Linux's curseg summaries;
  box `fsck.f2fs --dry-run` on mounted lxdef: rc=255 (7 failures) vs rc=0 on the pristine image.
  So no rw (and no ro, S6 not there) mount of native volumes is safe: mount_with refuses with a klog line
  (gate const NATIVE_IMPORT_SAFE, flip it in S4). Guest sees mount EPERM. lxdef/lxnoi/lxext images stay byte-identical (cmp).
- Policy code (feature mask, segs_per_sec, CP flags, unclean shutdown, orphans, NAT>1 seg / bitmap overlap) is in place for S6.
- Refused outright: log_blocksize != 12, multi-device, BLKZONED.
- dir_lookup on an inline-dentry dir: EIO instead of ENOENT.
- Open: no automated case (needs Linux-made images from the box).
