# Lane f2fslxinlw: inline write + inode checksum follow-ups (f2fslxrw S2), 2026-10-25

Status: FIXED (stale-extent-on-Linux demonstration not done).
- lx_convert_inline: an inline-data file or inline-dentry dir is converted to ordinary blocks (Linux's convert path) before
  any mutation: file open for write / O_TRUNC, dir_add_entry_lx, dir_remove_entry. Block 0 allocated with SSA, bytes or
  dentry bitmap/entries/names copied at the same slot indices, inline flags cleared, inline area zeroed, i_blocks+1,
  dirs: i_current_depth 1, i_size 4096. dir_is_empty scans inline dirs (rmdir of a non-empty inline dir used to look empty).
  lx_inline_refused remains only on the lookup-through-a-non-dir path. Small writes always convert (valid on Linux).
- Inode checksum mismatch: the block is no longer cached; the request gets EIO (read_failed), mount stays as is.
- i_ext (+348) is zeroed in inode_set_blkaddr / inode_set_nid / truncate_to (all lx volumes).
- inode_checksum volumes (lxext) mount rw already (FEAT_RW_OK has 0x20, verified by interop).
Evidence: interop.sh all: 30 PASS, 0 FAIL. New steps per lxdef/lxext: A.inline-write (append to 2 inline files,
120-entry overflow of an inline dir, rmdir empty ok / non-empty fails), A.inline-fsck1 rc 0, A.inline-linux (Linux md5 of
ours2.md5 + MANIFEST, stat of new names, Linux write), A.inline-fsck2 rc 0. aarch64 regress PASS.
Note: do not run interop and runtests from the same worktree at once (shared disk images, guest run failed).
