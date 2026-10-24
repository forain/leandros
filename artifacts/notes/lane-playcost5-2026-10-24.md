# lane/playcost5 (L3.1) - 2026-10-24 - deadline-scan slots/pass explained and fixed

## Why ~17 slots/pass on the laptop
New `/proc/rqhold` row `# dlsplit due stale repush tfdpass tfdslots drop miss` splits the deadline-pass slots.
futextest `deadline_short_waiters` reproduces the laptop pattern (48 threads in epoll_wait with 1-4 ms timeouts,
a pinger waking some early, 4 threads on 1 ms timerfds): unfixed 15 slots/pass, of which
- heap pops: due 38.0k (genuine, ~1.3 per pass), stale 2.5k (superseded entries, small), repush 0;
- timerfd-expiry walks: 5.9k passes x 67 slots = 396k slots = 91 % of all slots.
Cause: the timerfd walk skipped words by `wd_mask` (OR of the word's park masks); with a few dozen reactors the
OR covers all 64 bits, so every expiry walked the whole parked + listening set. Not heap duplication.

## Fix (cf8464e4)
The timerfd walk now takes its candidates from the wake index (lane playcost4 rows): `wx_all | wx_row[b]` for
tag bits without an exact key, `kx_cand` per exact key. Rows are exactly `row_mask` (poll-channel park mirror |
listener filter), a superset of what the walk can wake or record. `sched.wakeidx=2` walks everything and counts
slots the index would have missed (`miss`); `=0` is the old walk.

## Evidence (aarch64 HVF)
| | slots/pass | tfd slots/pass | miss |
|---|---|---|---|
| unfixed / wakeidx=2 walk | 15 | 67 | 0 |
| fixed | 1 | 1 | - |
Guard `deadline_short_waiters` (tfd slots/pass <= 8, miss 0) FAILs on the old walk, PASSes fixed.
regress, futextest, polltest, epolltest, `schedstress full` PASS; wakeidx=2 polltest/epolltest/futextest miss 0.

## Also
- scripts/deskplay: `dp.sh waitplay MAX` (bounded wait until i915 exec_vcs >= 20/s twice); phase1.sh measures only
  after playback starts, records never-playing runs in `$D/invalid`.
- Not done: shortening futex.rs:379 / :595 holds (task 2).
