# lane/wakecatch 2026-11-09 - stray keyed wakes (nettag2 flake)

## Verdict: FIXED (root cause confirmed deterministically)
Parked pollers matched a keyed wake on a 16-bit fingerprint only
(`poll_key_fp(key) = (key >> 32) as u16`). A different key sharing the bloom
bit (low 6 bits) and the fingerprint woke the waiter falsely.

## Evidence
- `sched.keyselftest=1` (sched/src/gdwake.rs): a kernel thread parks on key K,
  a driver wakes K2 = K ^ (1<<20) (same bloom bit and fingerprint), then K.
  Unfixed (d5840d02, aarch64 box): `[KEYSELFTEST] result=FAIL false_wake=1`.
  Fixed (eeb22f7e): `result=PASS false_wake=0 lost_wake=0`.
- The catch runs with `sched.wakelog=1` never showed a `key-fp` stray wake in
  nettag2 (first run 6/6); the flake is too rare to catch, hence the selftest.

## Fix (eeb22f7e)
`Task::poll_fks[16]` holds the full 64-bit key of the first `POLL_FK_MAX`=16
fingerprints (index-aligned with `poll_fps`). `fp_has_key(fps, fks, key)`
requires a full-key match for slots < 16 and accepts a fingerprint hit beyond
(only ever an extra wake, never a lost one). Used by the wake scan, listener
match/ring, widen_port_masks and the timerfd deadline check. `PollSet` records
keys; park/listen APIs take an extra `fks` slice. Memory: +128 B per task
(poll_fps 1024 B -> 1152 B with keys).

## Using sched.wakelog=1
Kernel cmdline `sched.wakelog=1` prints `[WAKELOG] tid= tgid= mask= tag= key=
kind=key-fp|key-unrefined|bits waker= site=` for wakes landing on tasks parked
with masks of at most 2 bits. `key-fp` = delivered on a fingerprint hit.
`sched.keyselftest=1` runs the collision selftest and prints one
`[KEYSELFTEST]` line.

## Open
netd `wake_poll_tagged(wake_mask)` bits-only fallback (servers/net/src/lib.rs
~3627) is not shown to cause stray wakes; left unchanged.
