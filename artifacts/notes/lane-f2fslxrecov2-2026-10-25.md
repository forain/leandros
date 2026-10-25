# lane/f2fslxrecov2: Linux fsync roll-forward, indirect levels and xattr nodes — 2026-10-25

FIXED (indirect-level dnodes, xattr nodes, typed dentries). Continues lane/f2fslxrecov.

- `ofs_bad` (servers/f2fs/src/lib.rs) now accepts any dnode offset (not the indirect/double-indirect node offsets
  themselves, which an fsync never writes) and the xattr node (`NODE_OFS_XATTR`); the ro policy for them is lifted.
- `durable::lx_attach` (Linux get_dnode_of_data(ALLOC_NODE)): Linux's fsync writes only the inode and dnodes, so the
  indirect nodes above a chained dnode may not exist on the medium although the fsynced inode already holds their nids.
  `recover_linux` adopts every chained node in place, then for each dnode with offset > 2 walks inode i_nid slot ->
  indirect (-> double indirect) creating missing indirect nodes (allocator does SIT/segment type; SSA set here) and
  stores the dnode nid at the last level. The xattr node is adopted like any chained node (inode i_xattr_nid already
  points at it; the old xattr node falls out of the old/new tree diff).
- `recover_dentry` now passes the dentry type from the inode mode instead of always DT_REG.
- Dirs: a directory fsync, and a file fsync whose parent is new, make Linux take a full checkpoint, so no chain ever
  holds a directory inode or dentry block; the recovered-dentry path (DENT mark -> name in an existing parent) is the
  only directory work and was already there. Rename + fsync and mkdir + new file + fsync were run and produce either a
  checkpoint or a one-inode chain; both pass.
- Test: `interop.sh fsync` now runs scenarios base / indirect (40 MB fsynced file: "12 node(s) of 2 file(s)") / xattr
  (700-byte user.big on a checkpointed file, spills to an xattr node: "3 node(s)") / rename / newdir. Each: box image copied
  mounted -> LeandrOS mounts rw, md5 of all files, box fsck.f2fs rc 0, Linux mounts, reads, xattr md5 matches.
- Open: no x86_64 run (aarch64 only per lane); Linux's check_index_in_prev_nodes (a chained block that another file
  owned at the checkpoint) is not replicated.
