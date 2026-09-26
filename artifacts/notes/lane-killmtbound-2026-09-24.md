# lane/killmtbound — 2026-09-24

Machine: linux desktop, x86_64/KVM (also verified aarch64/TCG on the same box).
Base `origin/main` `d16c563`. Branch `lane/killmtbound`, head `c0f56d1` (pushed).
Worktree: `/run/media/forain/samsung970pro512/leandros-siblings/leandros-killmtbound`.

## Problem

`killmt`'s per-mode leak check does one before/after `sysinfo().freeram`
subtraction over the whole run and fails past an 8 MiB bound. With the
greeter/compositor alive, cosmic-comp's own per-frame buffer churn swings
whole-system free memory by up to ~12 MiB on its own — unrelated to killmt —
so `stopped`, `leader_kills`, `exec_worker` and others intermittently failed
even on a clean kernel. The same swing biased `greeterstorm.py`'s first
sample, which anchors every later per-death delta in that script.

## Fix

`userland/killmt/src/main.rs`:
- `checkpoint_free_ram()`: three `settled_free_ram()` reads a few ms apart,
  reduced to the median — cheap noise rejection at a single point in time.
- Each mode's measured iterations (1..iters) are split into up to
  `MEM_CHECKPOINTS` (5) chunks, each bounded by a `checkpoint_free_ram()`
  sample. The FAIL verdict is now the **median per-iteration loss across
  chunks**, projected over the run, compared to the unchanged 8 MiB bound —
  not one subtraction over the whole run. A real leak is systematic (present
  in every chunk) and survives the median; a burst of unrelated churn
  typically hits only one or two chunks and does not.
- `MEM_LEAK_BOUND` unchanged (8 MiB); the fix is in how the delta is
  measured, not in loosening the bound.

`scripts/greeterstorm.py` (lane `greeterleak`'s file — touched per this
lane's instructions to apply the same fix; flagging the overlap):
- `median_sample()` collapses several `sample()` records into one synthetic
  record using the per-key median (site/slab/heap pages, memfree_kib).
- The sample at `i==1` (the anchor `a` for every later per-death delta) is
  now the median of itself plus two more quick reads 5 s apart
  (`settled-calib` phase), instead of trusting one instantaneous reading.

## Evidence

x86_64/KVM, greeter running, clean kernel, `killmt 50` (all 11 modes),
**3/3 runs, 0 failures**. Raw per-mode deltas across the three runs show the
described swing directly and still pass, e.g.:
`spin_all mem -12096 KiB`, `exit_group_worker mem -12052 KiB`,
`leader_kills mem +11900 KiB`, `exec_worker mem -11992 KiB`,
`worker_parked mem +11948 KiB`, `parked_mix mem -11972/+9136 KiB`.

Bound stays meaningful: temporarily reintroduced a realistic-size leak
(order-5 block, 128 KiB, via `mm::buddy::alloc(5)` left unfreed in the
execve path — the magnitude originally hypothesized for this lane before
`lane/execleak` refuted it down to a real-but-tiny 56-byte Box leak, not
worth injecting on its own since a 56 B/iter leak needs >100k iterations to
clear an 8 MiB bound). With the 128 KiB/exec block injected: `killmt 200
exec_worker` → `FAIL free memory dropped (median 128 KiB/iter x 199
iterations = 25511 KiB, bound 8192 KiB; raw +25588 KiB)`. Reverted before
committing (`git status` clean on `kernel/src/syscall.rs`, confirmed by
diff before/after).

aarch64/TCG, same box, no greeter forced but same image/session flow:
`killmt 50`, 11/11 PASS.

`greeterstorm.py` smoke run (x86_64/KVM, `--settle 15 --first 20`, 2 deaths,
tag `smoke`): live evidence of the exact swing this lane targets — the raw
`i==1` reading was `free=359976` while the two `settled-calib` re-reads were
`356935` and `356898` (a ~3000-page, ~12 MiB difference from the single
reading alone); the median (`356935`) was correctly used as the delta
anchor. Not a full multi-death research run (time budget), just confirms
the new code path executes and behaves as designed.

## Open / notes

- Touched `scripts/greeterstorm.py`, owned by lane `greeterleak` per the
  ownership table — only the first-sample anchor logic, nothing else in
  that script changed.
- `[PF] handle_user_page_fault returned false` prints during the `segv`
  mode battery, as noted pre-existing in `lane-execleak-2026-09-24.md`.
- Did not re-verify `greeterstorm.py` on a full 15-death research run
  against a real leak; the killmt-side injected-leak test is the evidence
  that the median technique doesn't hide real leaks.
