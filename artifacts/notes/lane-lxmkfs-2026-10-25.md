# Lane lxmkfs: fsck.f2fs-clean lx mkfs (L1 of design f2fsfsck, openbugs #46), 2026-10-25

Status: FIXED for the mkfs side (E1, E2, E6, E10 and the lxnode2 marker). Kernel allocator untouched (L2/L3).

Changes
- scripts/f2fslx.py: marker `LeandrOS lxnode2` (prefix `lxnode` still detected, so lxnode1 test images mount);
  helpers `f2fs_crc32`, `seal_cp`, `summary_block`, `sit_entry`, `ssa_segs_for`.
- scripts/mkfs-f2fs-home-lx.py: CP pack 512 blocks with CRC (checksum_offset 4092), 6 normal summaries at
  pack-7..pack-2 and the commit-block copy as the last block, both packs; inodes in seg 0 (HOT_NODE, SIT type 3),
  dentry blocks in seg 1 (HOT_DATA); six typed cursegs (segs 0..5, free_segment_count = main-6); SSA blocks for segs
  0..5 with node/data footers; NAT entries for ino 1 and 2 (blkaddr 1); SSA sized ceil(main/512); CP_OWN_HEADS set.
- servers/f2fs/src/lib.rs: LX_TAG matches the `LeandrOS lxnode` prefix (log text only otherwise).
- Legacy mkfs output unchanged (the lx script is only reached with --lx).

Evidence (box, fsck.f2fs 1.16.0 --dry-run)
- fresh 64 MiB lx image: rc=0, zero assert/fail/invalid lines, free segment_count matched [0x12].
- fresh 600 MiB lx image (>256 MiB, ssa_segs=2): rc=0, free segment_count matched [0x11e].
- Before: `Invalid CP CRC offset: 0` x2 / `Can't find valid checkpoint`.
- scripts/fsck-f2fs.py host check: PASS.
- aarch64: lx lxnode2 /home (vdc) mounted by the kernel, full regress PASS (boots failed 0/1).

Open: images written to by LeandrOS still hit the B-class errors until L2 lxsit / L3 lxssa.
