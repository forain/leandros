# lane/f2fscommit (2026-11-15)

Bug (review H3): `cp_write` / `cp_emergency_in` report `ok=false` when the commit block write or the flush after it
fails, but the commit may be on the medium. `settle()`'s failure branch left `rf_next`/`rf_end` open, so LRF1 fsyncs kept
appending records to pack A while pack B could hold era N+1: acknowledged fsyncs vanished at the next mount.

Fix (servers/f2fs/src/durable.rs): `CpIo::commit_sent`; `cp_finish` and `cp_emergency_in` set `Durable::commit_unknown`
and `rf_end = 0` when a commit was sent and `!ok`; `settle()` closes LRF1 in its failure branch while the flag is set and
clears it in the committed branch. fsync then takes a full checkpoint until a later commit succeeds. Hook for the Linux
node chain: `lx_open` must be false while `commit_unknown` (and when `!home_ok`); not wired (chain does not exist yet).

Test: `f2fs.cpunk=K` fault injection (K-th commit lands on the medium, reported failed) + `f2fstest commitunk|commitscan` +
`.claude/skills/run-leandros/f2fscommittest.py` (suite `f2fscommit`, also in regress after f2fsnid). Red before the fix
(`SCAN b missing`), green after.

Pre-existing, not from this lane: `f2fsgctest.py` fails identically on main (churn ENOSPC at round 27 with 14 MiB statfs
free, "no MANIFEST"). `f2fscrashtest` once failed "fsck of the crashed image" (iter 5, 20 errors, seed not logged) and passed
on rerun.
