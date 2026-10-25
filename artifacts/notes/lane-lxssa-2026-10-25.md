# Lane lxssa: SSA summaries on lx volumes (L3 of design f2fsfsck, openbugs #46), 2026-10-25

Status: FIXED (lx volumes only; legacy paths untouched).

Changes (servers/f2fs/src/lib.rs, durable.rs)
- Shared SSA writer: `ssa_blk(ms, seg)`, `ssa_set(ms, phys, nid, ofs, ver)` (ver None = read the NAT version byte),
  `ssa_set_kind(ms, seg, node)`, `lx_tail_sums(ms, logs)`. The f2fslxrw S4 import can call `ssa_blk`/`ssa_set` to
  write the Linux cursegs' summaries into the SSA area. SSA blocks go through the block cache, so the existing
  checkpoint journal covers them.
- Owners recorded: `alloc_data_owned(ms, nid, ofs)` at every data-block allocation (inode slot: nid=ino, ofs=i_addr
  index; dnode slot: dnode nid + index; dentry blocks: dir ino + block index); inode and node blocks get
  (nid, 0, version) in `create_inode` / `create_node_block`. `sit_set_type` sets the SSA footer (node/data).
- Checkpoint: the six pack-tail summary blocks are the cached SSA blocks of the six current logs (footer node for
  t>=3), written on every commit (`CpJob.tail_sums`, also the emergency path); were zeros.
- valid_inode_count: recounted from the NAT at (rw) mount (`nat_count_inodes`), fixes the post-crash drift left by lxsit.
- linux_import: `short-pack` check was `total < base + 2`, which rejected every Linux 7.2 umount checkpoint
  (total 8, sums at total-7). Now `total < base`. Without this, a Linux-written image could not be mounted at all.

Evidence (box fsck.f2fs 1.16.0 --dry-run, 64 MiB lx image)
- Before (main dd5e6533, from the lxsit note): rc 255, node summary invalid, 0xcf unreachable NIDs.
- After: design work.py workload on aarch64: rc=0, 0 ASSERT/Fail. crashtest seed 1792903575 on lx: PASS, fsck rc=4
  (valid_inode_count) before the recount, rc=0 after.
- Linux 7.2.7 mounted the image rw (noinline_*), md5 of our files OK, wrote an 800 KB file, unlinked, mkdir; umount;
  fsck rc=0. Image back on LeandrOS: "imported a Linux checkpoint", all md5s OK (Linux files read, ours written),
  umount + remount OK.
- f2fscptest lx + legacy PASS; f2fscrashtest legacy PASS.

Open
- After LeandrOS writes on top of a Linux-written volume, fsck is NOT clean (rc=255: "summary node block is not valid"
  for Linux-allocated nodes, plus `ino 3 i_links 5 vs 4`): Linux keeps its cursegs' summaries only in the cp pack, so
  the SSA area lacks them. That is f2fslxrw S4 (use `ssa_set`/`ssa_blk`); the i_links mismatch is unexplained
  (Linux `mkdir ldir` + our `rm`/create; check nlink on mkdir/rmdir of a dir on an imported volume).
