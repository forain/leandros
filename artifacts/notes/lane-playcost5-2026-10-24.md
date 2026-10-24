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

## Laptop (trial 410d8dd22fdf, `#1 playcost5`, main 43aa2bcc; 3/3 runs PLAYING after 4 s, two 30 s windows each)
| metric | playlap (b5cfb4ea) | playcost5 r1 / r2 / r3 |
|---|---|---|
| TASKS acq/s | 27.8k | 27.5k / 27.7k / 28.1k |
| TASKS held ms/s | 68 | 58.4 / 59.8 / 61.0 |
| TASKS spin ms/s | 15.7 | 11.7 / 11.2 / 12.9 |
| TASKS contended/s | 3.23k | 2.68k / 2.66k / 2.88k |
| dlscan slots/pass | ~17 | 0.86 (603k slots / 698k passes, tfd 2.1/pass, miss 0) |
| RDD total/sys % | 15.1 / 9.2 | 14.8/8.9, 14.6/8.7, 14.6/8.7 |
| total CPU % | 154 | 153 / 146 / 147 |
lib.rs:4422 hold 3.1 us/pass (was 4.9). Ping heartbeat 306/306, 0 lost, max RTT 185 ms; the SSH heartbeat file came
out empty (perl output block-buffered, lost when phase1 stopped it early; fixed: `$|=1` in hb.sh). Promoted.
Top sites now futex.rs:595 (2.35 s held / 1.03M) and futex.rs:379 (0.80 s / 1.08M, spin 0.40 s): task 2 open.
