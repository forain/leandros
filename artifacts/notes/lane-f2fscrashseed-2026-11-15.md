# lane/f2fscrashseed (2026-11-15)

Goal: reproduce the one f2fscrashtest fsck failure (20 errors, iter 5) seen by lane f2fscommit.
Done: f2fscrashtest logs per-iteration params, accepts --crashpt/--wseed, keeps failing images + fsck text (max 2).
Hunt: mix mode (kill + crashpt), aarch64 TCG, main bfe9a7ad: 2 x 20 iterations (seeds 1794732105 and a second run), all PASS
(crashed-image fsck, recovery claims, post-recovery strict-leaks fsck). Not reproduced; kill-mode timing is not replayable.
Not run: box regress gate (change is the test script only); x86_64 hunt.
