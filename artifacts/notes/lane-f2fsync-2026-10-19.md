# lane f2fsync — 2026-10-19 (bug list L4.2, L4.10)

Code: `servers/f2fs/src/durable.rs` (new), hooks in `servers/f2fs/src/lib.rs`.

## L4.10a Torn checkpoint — FIXED (redo journal in the checkpoint pack)
Premise confirmed: node/data blocks are updated in place and NAT/SIT have one copy, so the
old checkpoint wrote every dirty block home, then the commit block. A crash between the two
left new NAT/SIT/inode/dentry blocks beside the rest of the old state.
Now a checkpoint is a redo transaction. Dirty blocks are classified: regular-file data
(tagged by `write_file_data`) and blocks allocated since the last snapshot (free in the
durable checkpoint) are written in place. Everything else (NAT, SIT, existing node/dentry
blocks) goes as images into the 511 spare blocks of the pack being written, after an index of
home addresses. Overflow goes to whole free segments, which stay reserved until the next
commit. Then flush, commit block (CRC32 + journal length/CRC at 3072.., our format), flush,
and only then the home writes. Mount: the newest pack whose CRCs pass (a pack without the
journal magic is taken by version, as before), rewrite journaled blocks whose home
differs, then roll-forward. Dirty blocks the new checkpoint marks free are not written at all.
Otherwise a journaled stale copy could be replayed over the block's next owner.
`durable_pack/ver` are tracked separately from `ms.cp.active_pack`. Before, a failed
checkpoint flipped the pack in memory, so the next one overwrote the durable pack.

## L4.2 fsync — FIXED (roll-forward, upstream f2fs's design adapted)
An fsync of a regular file, with CP_MUTEX held: it writes the file's dirty data blocks in place
and flushes. Then it appends one record to the durable pack, after its journal: a header with
(nid, home) for every dirty node block of the file (footer owner, NAT-checked) plus their images.
The record carries a CRC, the durable checkpoint version and a sequence number. Then a flush.
No checkpoint. The node blocks reach home through the next checkpoint, which retires the records.
Mount: per inode, walk the committed tree, install the images, walk again, and move the
difference into the SIT/NAT. A file created since the checkpoint carries the dentry mark: its
name is re-added from i_pino/i_name, and an older file of that name is dropped. A directory, or
a file linked/unlinked/renamed since the checkpoint, falls back to the full checkpoint.
mkfs dnodes had footer ino = their own nid, so every dnode write now stamps the owner.
Lazy fsync is no longer the stick default (`nvidia-test-stick.sh`). The knob remains and now only
covers the fallback cases. `f2fs.rf=0` restores the old behaviour for A/B.

## L4.10b mkdir link counts — FIXED
New dirs get 2 and the parent +1. rmdir and rename adjust; a count of 1 (dirs made before this
fix) is left alone as the "unknown" convention. `fsck-f2fs.py --strict-links` fails on wrong counts.

## L4.10c Node ids recycled — FIXED
A free clears the NAT entry. The id goes to a pool once the checkpoint recording the free is
durable. When the bump allocator is exhausted, the NAT is scanned, and if only pending ids are
left, the request is doomed and retried after a checkpoint. `f2fs.nid_max=N` (/mnt volumes only)
is for tests.

## Found on the way (fixed)
- An inode unlinked or rmdir'ed while open was never reclaimed. Every directory `rm -rf` removed
  through a descriptor leaked its inode and dentry block (fsck: leaked dirs). It is now freed at
  the last close.

## Evidence (aarch64 HVF)
- `.claude/skills/run-leandros/f2fscrashtest.py --iters 6 --seed 11`: PASS. There were 3 random
  SIGKILLs and 3 `f2fs.crashpt=N` power cuts (writes dropped and failing from block N). After
  each crash, fsck of the crashed image PASS (0 leaks, strict links). Recovery checked 2062 / 296
  / 2019 / 188 / 1062 / 327 durability claims, all PASS. fsck after recovery PASS with strict
  leaks. One boot log: "roll-forward: 31 fsync record(s) of 19 file(s) recovered".
  The first run found the open-rmdir leak (and two bugs in the test itself).
- After the rebase: `--iters 4 --seed 23` PASS on a re-run, then `--mode kill --iters 8` with
  seeds 29 and 31: 16/16 PASS. Earlier, 3 kill iterations of ~34 failed with "gen G was never
  written" on one contiguous block range of a single gen. That fits a lost serial line: the
  workload prints `W` before it writes, and the kill can drop that line in flight, which the
  harness then reads as a gen nobody wrote. The harness now accepts a gen newer than the last
  `W` that arrived and counts it. Not proven: the tolerance never fired in the 16 runs after.
- `f2fsgctest.py --factor 4`: PASS, fsck PASS, 0 leaked. With `--cmdline f2fs.nid_max=40`
  (32 live inodes, ~200 created): PASS. Before the retry fix it gave ENOSPC in round 18;
  without reuse it would fail too.
- fsync latency, `fsbench fsyncdirty` (4 KiB pwrite+fsync after 256 KiB / 2 MiB of other dirty
  data, 100 rounds, qemu lock held), with `f2fs.rf=0` as the old behaviour:
  256 KiB: p50 4736 -> 317 us, avg 4788 -> 965 us.
  2 MiB: p50 10439 -> 348 us, avg 13284 -> 3735 us.
  The p99 with rf is ~51 ms: those are fallbacks or waits on a concurrent background checkpoint.
- regress (runtests --suite regress): all PASS except nettag2test connect_lo under load. It
  PASSes alone under the qemu lock.

## Compatibility with pre-f2fsync kernels (old <-> new, tested)
A 64 MiB volume was taken through old kernel -> new kernel -> old kernel. Old = this tree with
servers/f2fs from main f324477f; the script is in the lane's scratchpad, f2fsync-compat.py.
- Step 1: the old kernel ran churn, then a clean umount, then wrote a file, synced and was killed.
- Step 2: the new kernel mounted read-write and verified all 19 files plus that file. It wrote
  new data, unmounted cleanly, then remounted, wrote, synced and was killed.
- Step 3: the old kernel mounted the result, verified everything, read both files and unmounted.
- fsck PASSed after every step, with 0 leaks.

Bug found and fixed by this test: the first version treated a pack without the journal magic as
stale once the other pack had it. A pre-f2fsync kernel's newer checkpoint (the laptop's fallback
boot entry) was then ignored, and all its writes were lost. Legacy packs are now taken by
version (mount and fsck).

The old kernel ignores the journal and the roll-forward records. After a new-kernel unclean
shutdown, the old kernel mounts the last commit with whatever home writes landed (it never
replays) and loses fsyncs made after that commit. After a clean shutdown or a sync it is exact.

## Open
- The roll-forward area is what the journal leaves of the 512-block pack: when it is full,
  fsync checkpoints until the next checkpoint.
- Not crash-atomic: the cache's last-resort eviction (logged, should never happen) and the
  OOM/journal-overflow (>64 segments) fallback `cp_in_place`.
- Orphans pinned only by an mmap are still leaked if the munmap comes after the last close.
- Pre-f2fsync directories keep links 1, and old images keep stale NAT entries of deleted
  files: those ids are never reused.
- PC stick confirmation not done (strict fsync on the stick should now be cheap; needs a boot).
