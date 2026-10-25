# lane f2fslxbig (design f2fslxrw S5: F6, F7) -- FIXED

- F6: `meta_copy0_addr(base, b) = base + ((b>>9)<<10) + (b&511)` (SbInfo::nat_addr / sit_addr) at every NAT and SIT
  site (lib.rs, durable.rs, the block-cache inode-checksum peek). Linux import copies version-bit-set blocks
  copy1 -> copy0 per segment pair (copy 1 = copy 0 + 512) and applies the NAT/SIT journals through the helpers.
  Identity for this kernel's own one-segment-per-copy volumes.
- F7: when SIT+NAT version bitmaps reach offset 2048 (`SbInfo::big_cp`, ~32G: segment_count_nat 72), the private
  checkpoint fields (CP_OWN_VER/heads, CPJ journal header, ext list, incompat, orphans) are NOT kept in the commit
  block. They live in the pack's footer block (the Linux copy of the commit block; Linux checks only its version and
  CRC). `durable::commit_pack`: footer first + flush, then the commit block with 2048..4092 zeroed (bitmaps zero =
  all copy 0) and resealed; the commit block stays the atomic step. Mount (`cp_merge_footer`) validates the commit
  CRC, takes the private area from the footer (same version, valid CRC), reseals in memory. A Linux-rewritten pack
  has a footer copy of its commit block, so OWN_VER != VER -> foreign, as before. Small/legacy/lx volumes are
  untouched (same offsets, same write order).
- Kept refusals: cp_payload > 0, bitmaps past 4092 -> read-only ("volume too large for import yet").
- Gate: interop lx2g (nat 6) and lx32g (32G sparse, nat 72): mount rw, md5, write, fsck rc 0, Linux md5 + write,
  fsck rc 0. `interop.sh all` 46 PASS / 0 FAIL / 0 UNEXPECTED-PASS. interop.py now ships images sparse
  (python hole-writer on fetch, `dd conv=sparse` on push) and skips the sha compare for the big variants.
  aarch64 regress PASS.
- Not done: the guard was not run against unfixed main (the old code mounted these RO via the seg_cnt_nat != 2 policy);
  old-offset reading is trivially preserved because non-big volumes use the unchanged layout. cp_payload volumes (>~120G) stay RO.
