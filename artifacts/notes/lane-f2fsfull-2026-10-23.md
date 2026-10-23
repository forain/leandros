# lane/f2fsfull 2026-10-23 - near-full robustness (L4.11) - PARTIAL

FIXED
- (b) Mount-time checkpoint reads (servers/f2fs/src/lib.rs `mount_with`): a failed read of a pack is no longer parsed as a
  zeroed checkpoint; that pack counts as invalid, and with neither readable the mount fails. The superblock read was already
  checked. No automated test (blkdebug has no cheap way to fail only the mount-time read); code review only.
- (c) blkfaulttest now also does read() on the dead device and requires a failure with EIO. It FAILED on the unfixed tree
  (`dd` returned 0+0 records, exit 0): `read_unlocked` planned the read from a zero-scratch inode block (metadata miss failed)
  and returned Done(0) = clean EOF. Fix: `read_failed` after planning with n == 0 returns -EIO. blkfault: PASS.
- (a) tooling: `f2fstest fullchurn DIR SEED [ITERS]` / `fullscan DIR KMAX` (userland/f2fstest/src/full.rs) and
  `.claude/skills/run-leandros/f2fsfulltest.py [arch] [--iters N] [--seed S]`: fills a 64M volume to ~98%, churns
  create+fsync/rename/unlink, drops a fill file on each ENOSPC; iteration 0 no crash (all failures must be ENOSPC: PASS,
  20+ ENOSPC, space returns, fsck clean), later iterations use f2fs.crashpt and check acked fsyncs + fsck.

OPEN (found by the new test, NOT fixed - needs its own lane; hand-off)
- `f2fsfulltest.py aarch64 --iters 5 --seed 5` fails at iter 3 (reproduced twice): after crash recovery on a ~100% full volume
  the recovered volume loses fsynced files (k877, k1888..1895 missing), `fullscan` hangs/does not finish, and fsck after
  recovery reports a directory block corrupted + "block 12780 also used by ino 3761 data[6]" + 26 valid-but-unreachable blocks
  in the LAST segment (12779..12787). The fsck of the crashed image itself passes, so recovery (journal replay / roll-forward) near
  the volume end produces the double allocation. Seed 5 iter 1 also lost one acked file once (flaky, 1 of 3 runs).
  Suspect: roll-forward or allocator/SIT state when the free space is only the tail segment (partial last segment), or
  cp_throttle ext_free logic at ~0 free segments.
- Not run: x86_64; mount-time test for (b).
