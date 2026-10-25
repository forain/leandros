# lane/f2fslxorph (S7, F8): Linux orphan inodes — 2026-10-25

FIXED. A Linux checkpoint with CP_ORPHAN_PRESENT no longer forces a read-only mount.

- `servers/f2fs/src/lib.rs` mount: the orphan blocks (`pack + 1 + cp_payload`, `start_sum - 1 - cp_payload` of them;
  ino[1020] @0, entry_count u32 @4088) are read and their inos appended to `cp_orphans`, which `reap_persisted_orphans`
  frees before the first checkpoint (that checkpoint never sets the flag, so it is cleared as in Linux). Linux's own
  block checksum is not verified (entry_count <= 1020 is the only check); an unreadable block keeps the mount ro.
- A real orphan checkpoint is a `sync` checkpoint: no CP_UMOUNT_FLAG, and it carries the 3 data summaries only
  (`total - 4`), no node summaries. The import now reads data sums at `total - 4` for it and rebuilds the node
  summaries from the node footers of the hot/warm/cold node segments (Linux's restore_node_summary), writing them to
  the SSA. Without that fsck showed an invalid node seg summary for the root inode.
- Policy: "unclean Linux shutdown -> ro" is lifted only when CP_ORPHAN_PRESENT is set and there is no fsync chain
  (`fsync_chain_pending`: the node block at the hot node head has footer cp_ver == the checkpoint's, i.e. Linux would
  roll forward). Unclean volumes without orphans stay ro (not widened here).
- Test: `interop.sh orph` (direction C): box `exec 3<victim; rm victim; sync`, image copied while mounted (no sysrq);
  LeandrOS mounts rw ("orphan inodes reaped at mount: 1"), keep/ md5 ok, writes; box fsck.f2fs rc 0, no
  "is unreachable"/Fail/ASSERT; Linux mount reads keep + our file, fsck rc 0. Before the node-summary change the same
  test failed fsck (unreachable NIDs 4-8, invalid node seg summary).
