# lane f2fsfsync (2026-11-11): premise REFUTED, remaining gap narrowed

Premise in TODO ("fsync durable only via full checkpoint, no node log / roll-forward") was stale.
- servers/f2fs/src/durable.rs: `fsync_rf` writes the file's dirty data in place, then appends a CRC'd record
  (node images + home addresses, dentry mark for new files) to the durable pack; mount replays it (roll-forward).
  Lane f2fsync (10-19): `fsbench fsyncdirty` p50 4736 -> 317 us (256 KiB dirty), 10439 -> 348 us (2 MiB);
  crash/kill harness 16/16 PASS, fsck PASS, `f2fs.rf=0` is the old full-checkpoint behaviour (fail-first switch).
- Mount also rolls forward Linux-written fsync chains (lanes f2fslxrecov/recov2, interop fsync scenarios).

Genuinely open: the WRITE side is a private format, so Linux mount/fsck.f2fs after a crash without checkpoint
does not recover our fsynced file (it sees only the last checkpoint; image stays fsck-clean and mountable).
A Linux-format log means node blocks allocated sequentially in the warm/hot node curseg with next_blkaddr
footers, FSYNC_BIT/DENT_BIT marks, and curseg summary/SSA updates; this server updates nodes in place with a
single-copy NAT/SIT, so it is a redesign of node allocation, not a localized change. Not started: a half done
version could corrupt images. No code changed.
