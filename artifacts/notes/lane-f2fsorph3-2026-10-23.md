# lane/f2fsorph3 (L4.11 follow-ups) - 2026-10-23

## FIXED
1. ro -> rw remount dropped the persisted orphan list. A read-only mount now keeps the committed list
   in `MountState::ro_orphans` ("orphan inodes kept for the read-write remount: N"); `remount_by_port`
   on the ro -> rw transition reaps it with `reap_persisted_orphans` ("orphan inodes reaped at remount: N")
   and commits a checkpoint. The mount tool learned `-o remount,ro|rw <target>` (it rejected it before).
2. Cache backstop, f2fsdur.md step 8b. When `take_slot` has nothing evictable, no udata victim and the
   request is no-I/O (PROBE_ON, not inside a checkpoint), it dooms the request (`cp_wanted`, no slot);
   `with_mount` runs `checkpoint_unlocked(CpWhy::Pressure)` and reruns it. After 2 such checkpoints the
   request runs locked and the metadata in-place write stays the logged last resort. Unmount logs
   "cache backstop: checkpoint-first N, metadata in place M" when nonzero.
   Test knobs: `f2fs.cachemax=N` (cap on growth past the cache cap) and `f2fs.backstop_force=N` (every
   Nth slot taken by a no-I/O request goes down the doom path).

## Guard / evidence (aarch64)
- f2fsorphantest ro phase: unfixed kernel "fsck --strict-leaks: leaked 513" + no remount reap; fixed PASS.
- f2fstest under `f2fs.cache_kib=4096 f2fs.cachemax=1024 f2fs.backstop_force=50`: RC=0, the doom path
  fired many times, no "metadata block written in place".
- f2fscrashtest --iters 6 --seed 11: PASS plain, with the tiny cache, and with backstop_force=50.
- f2fsfulltest --iters 5 --seed 5 PASS; regress PASS.

## Open
- A real (unforced) backstop is still not reachable in tests: the writer throttle checkpoints first.
- PC-stick verification of the orphan list.
