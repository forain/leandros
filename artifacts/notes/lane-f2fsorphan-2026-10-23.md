# lane/f2fsorphan (L4.11) - 2026-10-23

## FIXED
- Create-path ENOSPC inode leak: `handle_open` (O_CREAT), mkdir and symlink kept the inode `create_inode`
  had just made when `dir_add_entry` (or `write_file_data` for symlink) failed. Now `discard_new_inode()`
  (lib.rs, next to `reclaim_orphan`) frees it. This was the actual cause of the f2fsfulltest seed 23 leak
  (second `s1215` inode, size 0, i_links 1).
- Orphan list persisted in the checkpoint commit block: u32 count @3364, nids @3368.. (max 181, below the
  native CRC @4092), written by `durable::put_orphans` in both cp_prepare and cp_emergency; read via
  `durable::orphans_of`. No feature bit: old kernels ignore it. Mount reaps after `recover()` through
  `reap_persisted_orphans` (NAT->block footer nid==ino==nid, links 0, mode!=0, so stale entries are safe),
  logs "orphan inodes reaped at mount: N", then one checkpoint.
- scripts/fsck-f2fs.py reads the list; valid orphans are walk roots; prints "orphan list: N".

## Guard
`f2fstest holdorphan DIR` + `f2fsorphantest.py aarch64`. Unfixed kernel: strict fsck "leaked 513".
Fixed: reaped 1, leaked 0.

## Evidence (aarch64)
f2fstest PASS, f2fsorphantest PASS, f2fscptest PASS, f2fscrashtest --iters 6 --seed 11 PASS,
f2fsfulltest --iters 5 seeds 5/11/23 and regress: see merge message / results below.

## Open
- ro mount followed by rw remount drops the on-disk orphan list (leak as before).
- Design f2fsdur.md step 8b (cache backstop: request a checkpoint before evicting a metadata block
  in place) not started.
