# Falsification

Two fixes, two mutations. Both are only worth what they fail on.

## 1. The kernel fix — `SCROLL_ROWS`

The mutation sets `SCROLL_ROWS` back to `1`, which is exactly the pre-change
behaviour, and changes nothing else: `scroll_px` and `scroll_shift_px` stay,
so the mutant differs in behaviour and not in surface.

x86_64/KVM, Linux box, QEMU 11.0.1, one boot per kernel, full release build
between each. Probe: 100 lines of 41 characters through `scripts/scmrun.py`
with a marker the shell assembles, so the read cannot end on the tty echo.

| kernel | `SCROLL_ROWS` | md5 (`target/final-x86_64/kernel`) | elapsed | per line | lines |
|---|---|---|---|---|---|
| control | 8 | `772fbbd53da3a4175e698baaf0ac6cdb` | **19.0 s** | **0.190 s** | 100/100 |
| mutant | 1 | `b1df2bf381d62488863651856753801a` | **145.2 s** | **1.452 s** | 100/100 |
| restored | 8 | `772fbbd53da3a4175e698baaf0ac6cdb` | — | — | — |

The restored kernel is **byte-identical** to the control, so the mutant differs
from the control in the guard and in nothing else. The ratio is 7.6x against a
change of 8x, which is what a cost paid per scroll rather than per line predicts.

**The mutant delivers 100 of 100 lines too.** That is the point of carrying the
line count: what the mutation moves is throughput and only throughput. Neither
kernel loses a byte, which is the whole correction to the record — the console
was never lossy, and no measurement here makes it so.

The independently recorded pre-change number on a different kernel
(`aaf1d14090a30ccb80bd32df3bd54327`, before any of this lane's changes) was
157.9 s / 1.579 s per line for the same probe, with `EV_STATS` still on.

## 2. The harness fix — the echoable marker

No kernel involved: one live boot, one kernel, the same test binary, the same
budget. The only difference is whether the marker is spelled in the command the
tty echoes back.

| | scmrun | command | elapsed | `M13RC` | PASS lines | bytes |
|---|---|---|---|---|---|---|
| mutant | pre-fix, `0da2f3d51bc4fc44899aa78dcccd1d86` | `scmtest; echo M13RC=$?` | **0.6 s** | none | 1 | 231 |
| control | fixed, `3836d6a1648b4e60d73e488151d3d544` | `scmtest; echo "M13""RC=$?"` | 21.5 s | **0** | 32 | 4025 |

The mutant is the pre-fix file taken straight from `git show HEAD:scripts/scmrun.py`,
so it has neither the refusal nor anything else new.

## Positive control

`nosuchbinary_xyz42` was the first command of every boot in every run above —
the two characterisation boots, the fix boot, the mutant boot and the suite —
and reported `command not found`, rc **127**, every time.

## Pre-committed numbers

Written before the fix build, in `/tmp/lane16/EXPECTATIONS.md` on the box:
`REQUIRE <= 45 s` for the 100-line probe and `300/300` for the long one.
Delivered 19.0 s and 300/300.
