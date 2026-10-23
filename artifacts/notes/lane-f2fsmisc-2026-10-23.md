# lane/f2fsmisc (2026-10-23): L4.11 follow-ups - PARTIAL

- Seed guards (f2fsfulltest aarch64 --iters 5): seeds 5, 5, 11 PASS. Seed 23 FAILS (reproducible, kept images in
  /tmp/f2ffull-aarch64-f2fsmisc-aarch64/crashed-iter5.img): the crash image of iter 5 already holds one block valid in the SIT
  that nothing references (blk 4441, NAT 9283 -> 4441: a regular file inode, size 0, i_links 1, no dentry). Not a roll-forward
  bug: it is the open-then-unlinked file. `Mount::orphans` (lane f2fsfu) lives in memory only; if a checkpoint lands while the
  file is unlinked-but-open and the machine dies, the inode and its NAT entry survive with no name and nothing ever reaps them
  (Linux persists an orphan inode list in the checkpoint). Space leak only (1 block per such file); fsck non-strict still passes,
  --strict-leaks after recovery fails. Needs: persist orphan nids in the checkpoint journal and reap them at mount. NOT done.
- Feature mask: scripts/fsck-f2fs.py now reports "feature mask 0x.. known/unknown" (and that a kernel mounts it read-only).
  New .claude/skills/run-leandros/f2fscptest.py: sets an unknown bit on the host (CRC recomputed), boots: mounts read-only, the
  "unknown features ... read-only" line is logged, write refused (EROFS), old data readable. PASS.
- Mount-time checkpoint damage: same test: newest pack CRC corrupt -> mount uses the other pack, earlier data present, writable;
  both packs corrupt -> "both checkpoint packs fail their checks; using the newer one", kernel alive. (Mount does NOT fail in that
  case, by design; only two unreadable packs fail the mount, which needs a failing device and is not host-reproducible.)
- Eviction retry (design 8b): not implemented (design file not found in the usual places), left documented as open.
