# lane f2fslxcsum (design f2fslxrw S3) - FIXED

- Linux volumes with INODE_CHKSUM (0x20): seed = crc32_le(~0, sb.uuid); inode csum = crc(seed, ino, i_generation, block with the
  field @368 zeroed). Raw crc32_le = `!crc32_upd(!seed, d)` (servers/f2fs/src/lib.rs `icsum_raw`/`icsum_calc`).
- Read: every cache fill (load, install_fetched) of a block that is a checksummed inode (footer nid==ino, ofs 0, EXTRA_ATTR,
  extra_isize>=12, NAT maps nid to this block - the NAT is peeked without touching the cache) is verified; counters
  ICSUM_OK/BAD, klog on powers of two, first 8 mismatches, and a summary at unmount. Mismatch is logged and counted only (no EIO).
- Write: `icsum_stamp` before every write-back path (lx_cp_prep for all dirty slots = the checkpoint/journal path, flush_all, eviction).
  Stamped in place in the slot; no reference into the cache is held across the NAT peek.
- FEAT_RW_OK now includes 0x20. NATIVE_IMPORT_SAFE still false (flip in S4).
- Evidence: ro mount of lxext (cp -c): "ok 318 bad 0" over a full md5sum -c MANIFEST + ls -lR, image byte-identical.
  With a local NATIVE_IMPORT_SAFE=true: append to small/f5000, new file + dir, umount; on the box Linux mounts it
  (loop), reads root, ours.txt, ourdir/f, small/f5000 (changed as expected); fsck.f2fs no longer reports inode checksum
  errors, still fails on SSA/SIT/curseg (S4: "Invalid data seg summary", valid_*_mount with CP).
- Not done: i_ext zeroing on address changes (design S3 side note).
