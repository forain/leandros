# lane/flockwake 2026-11-10 - vfstagtest flock_wait / setlkw_wait wake storm

## Verdict: FIXED (deterministic repro)
`lock_wait` (servers/vfs) parked F_SETLKW / flock waiters with a bits-only mask
(`block_on_poll_prepare_masked(.., poll_tag(FLOCK, hash))`). A keyed wake of any
unrelated object (AF_UNIX/pipe round trip) whose key shares the 6-bit bloom bit
woke the waiter on every trip, which re-probed and re-parked: 751 wakes per 750
round trips. Which lock file collides depends on its tmpfs slot, so it only
showed in full regresses where earlier suites shifted the slot.

## Fix
Waiters park with `PollTag::keyed([lock_wake_key])` (exact 64-bit key, full-key
match since wakecatch) and releases use `wake_poll_key`. `lock_wake_tag` replaced
by `lock_wake_key`.

## Evidence
- New vfstag case `flock_collide`: 64 lock files, 100 ms of traffic each.
  Unfixed (main + test only), aarch64 box: `FAIL unrelated traffic woke the
  waiter 751 times (750 round trips)`. Fixed: PASS.
- 20 plain loops of vfstagtest on unfixed main passed 20/20 (the quiet VM's lock
  slot never collides), so only the forced collision is a reliable guard.
