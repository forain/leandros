# lane/f2fsrecov 2026-10-23 - near-full crash recovery (L4.11) - PARTIAL

FIXED (servers/f2fs/src/durable.rs `recover`)
- Root cause of the double ownership / garbled directory block / torn files: roll-forward replayed fsync
  records file by file and called `recover_dentry` right after each file. `dir_add_entry` may allocate a
  dentry block via `log_alloc`, which skips only SIT-valid blocks; the records of files LATER in the loop
  were not in the SIT yet, and all record blocks sit right after the checkpointed log write pointer, so on a
  near-full volume the log handed out exactly one of their blocks (`alloc_data_block` zeroes it, then the
  later file's node image / data shares it). Matches the f2fsfull evidence: "block 12780 also used by ino
  3761 data[6]" + corrupt dir block + leaked tail-segment blocks + `fullscan` hang.
- Fix: three phases - (1) all node images + NAT, (2) all SIT frees then all SIT uses, (3) restore names.
  Name-restore failures are now logged ("roll-forward: name of ino N NOT restored: <why>").
- Evidence, f2fsfulltest aarch64 --iters 5 --seed 5: unfixed main FAIL at iter 3 ("k1892 torn/garbled",
  "k883 fsynced but missing"); fixed: no torn/garbled, no double ownership in any run; seed 11: PASS (5/5).
- f2fsfulltest.py: the last crash iteration's recovery was never checked (loop ended one early); fixed.
  `F2FSFULL_KEEP=1` keeps crashed-iter<N>.img + recovery-iter<N>.log ([F2FS] serial lines) in /tmp/f2ffull-<arch>-<runid>/.

OPEN (second, independent bug - seed 5 iter 3 still loses ONE acked file per run: k1004, k966)
- Not recovery's name restore: with the new logging, 129 records of 129 files recovered (area 12..270 of 512)
  and no "NOT restored" line. The lost keys are mid-run (k966/k1004 of 1951), i.e. made durable by an earlier
  checkpoint, so suspect a later non-durable namespace op (REN s->t / unlink of a neighbour / dir block change
  hitting ENOSPC on the full volume) committed by a later checkpoint, or recover_dentry's "older file whose
  removal was not durable" branch freeing the wrong inode. Next: run with F2FSFULL_KEEP=1, fsck the kept
  crashed-iter3.img with a dentry dump of w3 for the lost key (is it in the crashed image's checkpoint?),
  then check rename's ENOSPC path (dir_remove_entry before a failing dir_add_entry) in lib.rs.
- fsck after recovery runs after `rm -rf` of the test dir, so it cannot see dentry damage; consider fsck first.

TESTS (aarch64, lane commit before this note): f2fscrashtest --iters 6 --seed 11 PASS; f2fstest PASS; pcachetest PASS;
regress: all PASS except host:pollnarrowtest.py (failed 2/2, incl. a re-run under lock). Not yet checked whether it also
fails on unfixed main 198ef300 - the change only touches f2fs mount-time roll-forward, so it is most likely unrelated,
but the branch is NOT merged until that baseline is run.
NEXT: (1) run `runtests.py aarch64 <tag> host:pollnarrowtest.py` on main; if it fails there too, merge lane/f2fsrecov.
(2) then chase the remaining lost-acked-file bug above (seed 5 iter 3), and run seeds 5/11/+1 to green.
