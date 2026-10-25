# lane lxreserve — 2026-10-25 — openbugs #46 (lx volumes only)

Status: FIXED (lx crashtest green; legacy unchanged).

## Cause
The "fsck of the crashed image" failure was not (only) a full volume: with the four filler log heads
(+ the SSR path) the allocator consumed every empty segment. `ssr_pick` treats an EMPTY segment as the best
victim (most usable blocks), so after `sit_find_free_seg` stopped, SSR still ate the empties the next
checkpoint needed for its six Linux log heads; `linux_logs` then fell to its last resort and put the
cold-node curseg on a valid block ("curseg 5 segment 1: SIT type 0 != 5").

## Change (servers/f2fs/src/lib.rs, `log_alloc`, `ssr_pick`)
lx volumes only:
- `LX_RESERVE_SEGS = 6` empty segments are never handed to the general allocator: the four filler heads plus
  one spare for each of our two own heads (a full own head is moved to an empty segment by `linux_logs`).
  `lx_empty_segs` counts empties (not own heads, not pre-free); a free segment is taken only if >6 exist.
- `ssr_pick` (non-heads) skips empty segments; SSR reuses only dirty ones.
- valid blocks capped at user_block_count ((main - ovp) * blocks_per_seg): ENOSPC beyond.
- the old "heads" last-resort passes are gone for lx. Legacy volumes keep the old path.

## Capacity cost (6 segments of 2 MiB = 12 MiB, plus the cp's existing 1 segment overprovision)
| volume | main segs (approx) | reserve | share |
|---|---|---|---|
| 64 MiB | 23 | 6 seg = 12 MiB | ~26 % |
| 1 GiB | ~510 | 6 seg = 12 MiB | ~1.2 % |
| 32 GiB | ~16300 | 6 seg = 12 MiB | ~0.04 % |

## Evidence (aarch64)
- `F2FS_TEST_LX=1 f2fscrashtest.py --iters 2 --mode kill --seed 1792903575`: all steps PASS both iters
  (was FAIL at "fsck of the crashed image"; Mac fsck-f2fs).
- `F2FS_TEST_LX=1 f2fsfulltest.py --iters 3 --seed 5`: PASS (ENOSPC x20, all crashed/recovered fscks PASS,
  peak 9193 valid blocks < user_block_count 11264). Legacy f2fsfulltest --iters 2: PASS.
- Box: the crashtest volume (which hit ENOSPC) fsck.f2fs -f rc=0, Linux mounted it rw, dd filled it to
  ENOSPC, rm, 5 new files + sync, umount, fsck.f2fs rc=0.
