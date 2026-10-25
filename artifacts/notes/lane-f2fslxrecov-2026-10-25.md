# lane/f2fslxrecov: Linux fsync roll-forward at mount — 2026-10-25

FIXED. A Linux-made volume copied after an unclean shutdown with a pending fsync chain now mounts rw.

- `linux_chain` (servers/f2fs/src/lib.rs): Linux's find_fsync_dnodes. Starts at the WARM node head of the checkpoint
  (cur_node_segno[1]/blkoff[1]; the old `fsync_chain_pending` looked at the hot head), follows footer next_blkaddr while
  footer cp_ver == checkpoint_ver (high half = cp crc when CP_CRC_RECOVERY_FLAG 0x40 is set). A file is recovered if some
  node carries FSYNC|DENT; its nodes up to the last marked one are replayed. Supported: inode and direct dnodes (ofs 0..2,
  i.e. files up to ~8 MiB+); a chain node deeper in the tree (indirect levels, xattr node) keeps the mount ro.
- `durable::recover_linux`: the fsynced node blocks are adopted in place (NAT -> their address, owner ino from the footer),
  the old and new trees are diffed for SIT frees/uses, SSA and segment types (same machinery as our own `recover`), and the
  DENT inode's name is restored in the pino from the inode page (`recover_dentry`). Then the usual mount checkpoint.
- Policy: unclean Linux shutdown is rw when the chain is empty with CP_ORPHAN_PRESENT (S7) or non-empty and supported.
- Test `interop.sh fsync` (direction D): box mkfs+mount, sync, then new 5 MB file + small inline file + 10-block in-place
  overwrite of a checkpointed file + MANIFEST, all fsynced, image cp'd while mounted. LeandrOS mounts rw ("Linux
  roll-forward: 4 node(s) of 4 file(s)"), md5 of all four ok, writes; box fsck.f2fs rc 0; Linux mounts, reads, fsck rc 0.
  First attempt without the NAT owner (`nat_update` => ino=nid) failed fsck (nid 8 ino mismatch, unreachable, counts).
- Orphan checksum: REFUTED as a gap. Linux never computes `f2fs_orphan_block.check_sum` (written as 0, never verified), so
  there is nothing to verify; the S7 path keeps entry_count <= 1020.
- Evidence: interop.sh all 66 PASS / 0 FAIL; aarch64 regress green (futextest kwake_scan_indexed flaked once, green on rerun);
  F2FS_TEST_LX=1 f2fscrashtest seed 1792903575 PASS.
- Open: chains with indirect-level dnodes / xattr nodes / dirs stay ro; no x86_64 run (aarch64 only per lane).
