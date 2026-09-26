# Lane drmflake — 2026-09-25

Branch `lane/drmflake` (from `origin/integ-wave-0924` `ee7c029`), head `b50fda0`, pushed.
Worktree: laptop `~/Projects/leandros-drmflake`.

## Outcome: FIXED

Problem statement: `drmsmoke` had reported `failed=1` on the first scripted run
on each GPU path (virgl/iris and Venus→zink) on the laptop, then `failed=0` on
4 reruns, with the actual failing case never captured (the run script only
kept `tail -n 6` of the output).

## Repro

Captured full `drmsmoke` output (not `tail -6`) across 32 runs per path,
booting fresh every 5 runs (with a run immediately after each fresh boot).
Baseline (pre-fix, x86_64/KVM, Intel UHD 620):

- **virgl (iris)**: 5/32 runs failed — runs 7, 21, 26, 27, 30. Every single
  failure was the same case: `FLIP_EVENT_DELIVERED_ON_FENCE: FAIL`. No other
  case ever failed or was skipped; no master-conflict, no timeouts, no
  spurious interrupts, all 32 flips in every burst were delivered (functional
  correctness was never in question — only which of the two delivery paths
  got credited).
- Did not get to reproduce on Venus before applying the fix (same code path;
  see verification below).

## Root cause: kernel bug, not a test race

`FLIP_EVENT_DELIVERED_ON_FENCE` demands all 32 page flips in a burst be
delivered via the fence/IRQ path (`d_flips_on_fence >= 32`); a flip whose
fence isn't observed within a fallback window is instead force-delivered by
the 100 Hz tick. That fallback (`drivers/src/drm_device_interface.rs`,
`PENDING_FLIPS` / `drm_tick`) compared `sched::ticks()` — a 100 Hz counter
sampled at an arbitrary phase — against a threshold of 2 ("two ticks"). Because
the counter isn't phase-aligned to when a flip is queued, `now - queued >= 2`
can go true after as little as **one** real tick period (~10-11 ms) instead of
the intended two (~20 ms), depending on queue timing relative to the tick
boundary.

Measured real virgl/iris present latency on this hardware is ~11-16 ms
(`GPU_IRQ flip_event_latency_mean/max_us` in the failing runs) — comfortably
under a true 20 ms grace period, but sometimes loses the race against the
quantized ~10-20 ms one. This is the exact same class of bug already fixed
elsewhere in this kernel for tick-counted deadlines (see `monotonic_ns`'s own
doc comment, and the memory note on FUTEX_WAIT_BITSET + timerfd ABSTIME) — it
had just not been applied to this fallback path yet.

**Fix** (`drivers/src/drm_device_interface.rs`, one file, +33/-8): store the
flip's queued-at time as `arch_monotonic_ns()` instead of `sched::ticks()`,
and compare it against a real nanosecond threshold `FLIP_FALLBACK_NS = 2 *
10_000_000` (two full 10 ms tick periods) instead of a raw tick-count
difference. This is a genuine kernel fix, not a test weakening: the check
still demands the fallback only fire after a true ~20 ms grace, which is what
was originally intended.

## Verification (post-fix, x86_64/KVM)

- **virgl (iris)**: 32/32 clean (including the previously-failing run 7's
  slot and others), across boots.
- **Venus→zink (ANV)**: 32/32 clean, across boots.
- No other FAIL or SKIP case appeared in any of the 64 post-fix runs.

## Notes

- No file ownership overlaps: only `drivers/src/drm_device_interface.rs`
  touched, not owned by another lane in the table.
- Did not touch aarch64 — out of scope for this lane (laptop is x86_64/KVM
  only; the reported bug and its fix are architecture-independent since the
  tick/ns clock plumbing is shared, but this wasn't re-verified on aarch64/HVF
  on the Mac).
- Scratch: `~/Projects/drmflake-loop.sh`, `~/Projects/drmflake-out{,2,3}/` on
  the laptop (left in place in case useful for follow-up; not committed).
