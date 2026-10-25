# lane/lxpop (L4): lx populated root image writer

Status: DELIVERED (writer); PARTIAL on the post-regress fsck gate (kernel-side, see below).

- scripts/f2fslxpop.py: lx root writer, used by scripts/mkfs-f2fs-populated.py when
  `LEANDROS_F2FS_LX=1` (hooks: add_files_to_dir, and before the legacy "5. Write directory
  blocks" step). Default build path untouched (all hooks guarded by `LX`).
- Two allocators (HOT_NODE segments 0..5, HOT_DATA segments 6..899, then the four empty
  WARM/COLD logs), typed SIT, SSA owner entries + node/data footer, NAT with owner ino,
  two cp packs with CRC and curseg summaries in the pack tail, SIT/SSA sized from the volume.
- Fresh aarch64 root: build 2.3 s for the lx writer, 3.83 GB sparse image (same size as legacy,
  3.69 GB vs 3.71 GB allocated). box `fsck.f2fs --dry-run`: rc=0, 0 asserts.
- aarch64 regress on the lx root: PASS (boots, lx format log line, login/ssh tests green).
- fsck after the regress run: rc=4, only 2 runtime-created inodes (0xb8f, 0xcde) with
  "i_blocks 2 but 1 block / invalid data seg summary", SIT bitmap + valid_block_count mismatch;
  checkpoint state is sudden-power-off (VM killed, no umount). Kernel-side (lxssa/lxrw lanes), not the writer.
- Bug found while building: NAT entries of direct/indirect nodes must carry the owning inode in
  `ino`, not the node's own nid (fsck sanity_check_nid).
