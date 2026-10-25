# lane/f2fslxprev: Linux fsync roll-forward, check_index_in_prev_nodes — 2026-10-25

FIXED. Continues lane/f2fslxrecov2.

- `recover_linux` (servers/f2fs/src/durable.rs): before a recovered data block gets its new SSA owner, `drop_prev_owner`
  (lib.rs) checks the SIT (checkpoint state) and the SSA: if the block was valid under a different (nid, ofs) and that
  node's slot still holds the address, the slot is zeroed, the owner inode's i_blocks refunded and its i_ext hint cleared.
- Our own `recover()` has no exposure: its records are allocated from blocks free in the checkpoint SIT.
- Test: interop.py fsync scenario `prevowner`. Linux will not reuse a checkpoint-valid block before the next checkpoint,
  so the overlap is forged: pb's newest inode gets i_addr[0] = pa's i_addr[0] (i_ext cleared). Expected: pa reads a hole
  at block 0, pb reads pa's old block, fsck.f2fs rc 0, Linux mounts and reads.
- Guard: with the fix disabled, D.fsck and D.linux-mount FAIL (fsck rc 4); with it, "block N taken from its checkpoint
  owner" is logged and all pass.
- Evidence: interop.sh all 124 PASS / 0 FAIL; aarch64 regress PASS; F2FS_TEST_LX=1 f2fscrashtest seed 1792903575 PASS.
- Open: aarch64 only; the forged overlap stands in for a natural one.
