# lane f2fslxdir (design f2fslxrw S9 + big-volume follow-up) -- FIXED

- S9 large dirs: `dir_add_entry_lx` places dentry blocks with Linux's exact multi-level hash geometry
  (`lx_dir_buckets` / `lx_bucket_blocks` / `lx_dir_bucket` = dir_buckets / bucket_blocks / dir_block_index,
  MAX_DIR_HASH_DEPTH 63) and allocates them with `inode_logical_to_phys_for_write`, so blocks past i_addr go
  through the inode's direct/indirect/double-indirect nodes (before: ENOSPC at hash level 9). Lookup and
  remove on lx volumes now walk only the hash bucket at each level up to i_current_depth (was a linear scan
  of every dentry block: 100k creates would have been quadratic). Legacy volumes keep the linear scan.
- Variant `lxdir` (4G): Linux makes a 100k-entry dir, we `ls` it and do 1500 lookups, create a 100k-entry
  dir ourselves, rm one; then fsck rc 0, Linux `ls` = 99999, stats, Linux write, fsck rc 0.
- cp_payload: pack records now start after the payload blocks (`pack_base` = commit block + payload,
  `pack_room` shrinks by it); `commit_pack` rewrites the payload blocks as zeros every commit (SIT version
  bitmap = all copy 0, even if the other pack still holds a Linux checkpoint's bits); `linux_import` reads the
  SIT version bitmap from the payload blocks and the NAT bitmap from the commit block (Linux `__bitmap_ptr`)
  and the "too large for import" read-only policy only remains for bitmaps that do not fit.
  Note: mkfs.f2fs 1.16 gives cp_payload 0 up to 2T (it shrinks NAT to keep bitmaps in the commit block);
  payload 2 appears at 4T. Variants `lx256g` (cp_payload 0, bitmaps 3840 B) and `lx4t` (payload 2).
- BUG FOUND in lane f2fslxbig's F6: the SIT is NOT interleaved like the NAT. Linux `current_sit_addr` =
  sit_base + block, copy 1 at + sit_blocks (second half). `SbInfo::sit_addr` used the NAT interleave, which only
  coincides for one segment per copy (<= 32G): at 256G fsck reported "Incorrect curseg ... SIT bitmap is 0x0".
  Fixed (`sit_addr = sit_blkaddr + b`, import copies copy1 = copy0 + half).
- interop.py: fetch/push ship only data extents (SEEK_DATA on the box, python) -- gzip of a 4T sparse image
  would take hours; guest timeout 3000 s for the 100k-create command; F2FSINTEROP_KEEP=1 keeps the box dir.
- Not done: the S9 guard was not run against the unfixed tree (the old code is known to return ENOSPC at
  level 9); ENOSPC/EMLINK of a dir that reaches MAX_DIR_HASH_DEPTH is untested.
