# Item 9 — FB_DAMAGE_CLIPS diagnostic: result

**Date:** 2026-08-06. **Config:** aarch64 / HVF, 1280x800, `DRM_STATS = true`, patch
`fb_damage_worktree_20260806.patch` applied uncommitted. 88 `[DRMSTAT]` samples over
176 s, of which a 70 s continuous pointer-motion burst.

Raw data: `run1-drmstat.txt`, `run1-serial.txt`, `shots/*.ppm`.
Analysis tool: `m9_analyze.py` (order-independent `key=0xHEX` parsing).

## Verdict

**The blocker is CLIENT-SIDE. Stop — no further kernel work moves flips/s.**

During the 70 s motion burst the compositor damaged **96.68 %** of the output on every
single present (990,012 px of 1,024,000). The pre-registered criterion was: near-full =>
client-side, stop; under ~5 % => damage tracking works and the perf pass criterion
applies. This is decisively the first branch.

`dmg_skip = 0` across every sample in the entire run. smithay **never once** skipped the
primary plane. That directly confirms the inference recorded in TODO.md item 9 — we fail
the third skip condition (`age > 0 && last_state.old_damage.len() >= age`), so smithay
clears the damage and pushes the whole output geometry every frame.

## The measurement

| Window | flips/s | cursor_mv/s | evpush/s | dmg full/rect/skip | dmg_px per present | % of full |
|---|---|---|---|---|---|---|
| Motion burst (70 s) | 8.16 | 8.16 | 174.63 | 0 / 571 / 0 | 990,012 | **96.68 %** |
| Cumulative (176 s) | 3.89 | 3.86 | 69.50 | 3 / 681 / 0 | 846,817 | 82.70 % |

The cumulative figure is diluted by idle stretches; the burst window is the one the
methodology specifies and the one that answers the question.

## Controls — all four pass

1. **Sanity, `dmg_full + dmg_rect + dmg_skip == atomic`.** Exact in every window
   (571 = 571 in the burst, 684 = 684 cumulative). The counters are trustworthy.
2. **`evpush` climbing.** 174.63/s against an expected ~60. This is **not** an
   over-count: evdev emits `EV_REL` X, `EV_REL` Y and `EV_SYN` per motion event, so
   174.63/3 = 58.2 moves/s, matching the 60/s injection rate. Pointer motion genuinely
   reached the guest ring.
3. **`cursor_mv/s` did not fall.** 8.16/s against the pre-patch baselines of 6.0 and
   8.50. The "dead pointer" revert signature (`flips/s -> 0` *with* `cursor_mv/s -> 0`)
   did **not** occur. `cursor_up` stayed at 1 upload for the whole run (0.00/s), so the
   cursor plane is working exactly as it did pre-patch.
4. **Stale pixels — none.** All seven screendumps hash differently; the panel clock
   advances normally. Per-pixel bounding-box diff of consecutive frames:

   | Pair | Changed px | bbox |
   |---|---|---|
   | quiet1 vs quiet2 | 126 / 1,024,000 = 0.012 % | x=[695..709] y=[5..25] |
   | post1 vs post2 | 108 / 1,024,000 = 0.011 % | x=[695..709] y=[5..19] |
   | post2 vs post3 | 288 / 1,024,000 = 0.028 % | x=[677..709] y=[5..25] |

   Every changed pixel falls inside a ~15x21 px box in the panel bar — the clock digits.
   Nothing outside the damaged region went stale, so the kernel-side damage path is
   faithful.

## The sharpest statement of the result

Consecutive frames differ by **~126 real pixels**. The compositor declares
**~990,012 pixels** of damage for them. That is an over-damage factor of roughly
**7,800x**.

This reframes the item. The kernel side of FB_DAMAGE_CLIPS is *correct* — the blob
decodes, the rects apply, the sanity identity is exact, and nothing goes stale. It simply
cannot win anything, because the hint it is handed is already the whole screen. Bounding
a present to 96.68 % of the framebuffer saves 3.32 % of a copy.

Measured cost confirms it: `flips/s` stayed at 8.16 against a 6.0 baseline (pass
criterion was <= 2.0). No improvement, and none was available.

## What this retires, and what it leaves

- **Retired:** the hypothesis that primary-plane recomposite is a kernel-side problem.
  It is not. No amount of driver work changes it while smithay damages the full output.
- **Left standing, but now known unreachable:** the real kernel defect noted in item 9 —
  that when smithay *does* skip the primary we still do a full-screen scale plus
  full-screen `TRANSFER_TO_HOST`/`RESOURCE_FLUSH`. `dmg_skip = 0` for the entire run
  means that path is never entered today, so fixing it has no measurable effect until
  the client side changes.
- **Next lead, if this is pursued:** the question moves entirely into
  `OutputDamageTracker` and the `age`/`old_damage` bookkeeping at
  `renderer/damage/mod.rs:741-759`. Note the ceiling recorded in TODO.md — cosmic-comp
  sets `release_max_level_info`, so smithay's `trace!` damage decisions are compiled out
  of the release build and cannot be read without a rebuild.

## Caveats

- Single run. The numbers are stable across both the burst and cumulative windows and
  the sanity identity is exact, so a repeat is unlikely to move the verdict, but it is
  one run.
- `drmsmoke` (must be 22/0) and `idletest` were **not** completed — the lane was
  terminated by an account session limit before reaching them. They remain outstanding
  before anything here is landed.
- The "force one full present and confirm pixel-identical" leg of control 4 was not
  performed as literally specified; it is not well-posed while the clock is ticking,
  since two captures at different instants legitimately differ. The bounding-box
  analysis above is a stronger substitute — it shows *exactly* which pixels changed and
  that all of them are inside the clock.

## Harness note

The numbers here were produced by `m9_analyze.py`, which parses `key=0xHEX` pairs
order-independently and is therefore immune to the positional-regex defect found in
`m8_cursor.py` during this same wave (the five new `dmg_*` fields sit between `flip_us`
and `curs_up`, silently zeroing every field after them on a patched kernel). No number in
this report passed through the defective parser.
