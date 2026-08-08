# The x86_64 console was never dropping output. It was delivering 0.6 lines/s.

2026-08-08, Linux box (`forain@172.16.158.150`), x86_64/KVM, QEMU 11.0.1.
Everything below is one arch, one box; aarch64 is the Mac lane's to confirm.

## What was believed

`artifacts/notes/m15-serial-stall-20260808/console-loss-preexisting.md` recorded
that 300 printed lines arrive as 19, identically on two different kernels and
identically with every read budget widened to 700 s, and localised it to
`drivers/src/serial.rs::write_byte` — whose x86_64 arm is a bare `out dx, al`
into the transmit holding register with no `LSR.THRE` check, while the
`cfg(not(x86_64))` arm goes through `arch_serial_putc`. That reads exactly like
a console with no flow control, and 19 lines is about what a burst into a
16-byte FIFO survives.

Two things are wrong with it, and the second is the interesting one.

## `Serial` has never run

Nothing constructs it. `drivers/src/lib.rs` says `pub mod serial;` and that is
the only reference in the tree — no `Serial::new`, no registry entry, so neither
`write_byte` nor the 16550 `probe` beside it has ever executed. Userspace
console output goes

```
sys_write / writev  ->  console_write_user      (kernel/src/syscall.rs)
                    ->  serial_write_raw
                    ->  serial_write_byte       (kernel/src/main.rs:109)
                    ->  arch_x86_64::putc
```

and `arch::putc` has checked `LSR.THRE` since long before this lane — with a
cycle-counter deadline since `af9f076`. The flow control was never missing.

## The 19 lines were the harness reading its own echo

`scripts/scmrun.py` stops reading at the first sight of its completion marker.
The tty echoes every character it is sent. So a marker spelled literally in the
command came back as part of the **echoed command line**, and the window closed
about a second after the command was typed — before the command had printed
anything.

`artifacts/m13_suite.py` sent `<test>; echo M13RC=$?` and passed `"M13RC="` as
the marker. The first line of its own recorded `vfstest.log` is the evidence:

```
brush-0.5# vfstest; echo M13RC=$?
```

Reproduced here on one boot, one kernel (`aaf1d140…`, the previous lane's own
control), 300 numbered lines:

| probe | marker | budget | delivered | window closed after |
|---|---|---|---|---|
| A | `CONSOLEDONE`, spelled in the command | 240 s | **16 / 300** | **0.9 s** |
| B | none at all | 90 s | 50 lines | 90 s (full burn) |

A reproduces the recorded 19 within one line and does it in **0.9 seconds of a
240 second budget**, which is why widening budgets never helped: nothing was
timing out, the reader was leaving. This is also why the loss looked identical
on the kernel with the `putc` deadline and the kernel without it — neither
kernel was involved.

B ran straight after A on the same boot, and the 50 lines it read are
`CONSOLELINE16` … `CONSOLELINE65` — A's loop, **still printing**, picked up
exactly where A's reader had abandoned it. That is the first half of the answer
on its own: the bytes A "lost" had not been lost, they had not been written yet.

## What B actually showed

B is the probe that cannot be truncated early, and it refutes loss outright:

```
CONSOLELINE16
CONSOLELINE17
CONSOLELINE18
...
CONSOLELINE65
```

Contiguous, in order, no gaps, no corrupted lines — **one line per ~2 seconds**,
interleaved with the 0.5 Hz `[EVSTAT]` census. Nothing is lost. The console is
slow.

## Per line, not per byte

Two probes, one boot, matched to ~4 KB of payload each:

| probe | shape | elapsed | per line | payload rate |
|---|---|---|---|---|
| P1 | 100 lines x 41 chars | 157.9 s | **1.579 s** | 26.0 B/s |
| P2 | 220 lines x 1 char | 400.6 s | **1.821 s** | 1.1 B/s |

24x the bytes for the same cost per line. The ceiling is per newline. Sampling
`RIP` on all four vCPUs through the QEMU monitor put **12/12 samples in
`memcpy`** while a print loop ran; the other three vCPUs were in
`sched::scheduler_run_loop`.

## The scroll

`Framebuffer::scroll_vector` runs on every `\n` once the console has filled the
screen. It `core::ptr::copy`s the whole surface up one text row and then
`mark_dirty(0, 0, w, h)`, so the next character re-transmits the whole surface
to the host over virtio-gpu.

The surface is `1920x1080 pitch=7680` = **8.29 MB**, and `arch/x86_64/src/lib.rs`
maps it `PRESENT | WRITABLE | NO_CACHE` — PAT index 2, which `init_pat_bsp`
programs as UC-. So each newline is 8.29 MB read and 8.29 MB written through an
uncached mapping. 8.29 MB / 8 B per access x 2 x ~0.7 us is ~1.5 s, which is the
number measured.

## The fix

A scroll costs the same whether it advances one text row or eight, so it now
advances `SCROLL_ROWS = 8`.

| | before | after |
|---|---|---|
| 100 lines x 41 chars | 157.9 s | **19.0 s** |
| per line | 1.579 s | **0.190 s** |
| lines delivered | 100/100 | **100/100** |
| 300 numbered lines | (never measured undisturbed) | **300/300 in 62.7 s** |

Pre-committed before the run: `<= 45 s` and `300/300`. Both met. A screenshot
confirms the console still renders — 693 pixel rows of ink and a 75-pixel blank
band under the cursor, which is the chunk not yet filled.

The price is that the console jumps eight rows and leaves up to eight blank rows
below the cursor, the trade fbcon makes when it cannot pan. **Dividing the cost
is not removing it**: 0.190 s/line is still the same scroll, amortised. Removing
it needs the surface never to be read back (a write-back shadow) *and* a
write-combining mapping for it — and `arch/x86_64/src/paging.rs` is explicit
that a cached alias of host-visible memory is the bug its PAT setup exists to
prevent, so that belongs to a lane that can verify the memory type end to end.

## Also fixed

- **`scmrun.py` now refuses a marker that occurs in the command it is typed
  with.** The trap is silent by construction — a window that closes early leaves
  a *short* log, not an empty one, and a short log greps clean. Refusing beats
  warning.
- **`m13_suite.py` emits `echo "M13""RC=$?"`** — printed as `M13RC=`, typed as
  `M13""RC=`. Its dead per-test marker column (every call passed `"M13RC="`
  regardless) is gone.
- **`EV_STATS` is off.** Committed `true`, it wrote 28,900 of the 30,934 console
  bytes of the P1 measurement into the log. `artifacts/m15_serial_stall.py`
  needs it on — its guard works by making the timer IRQ print — and its
  docstring now says so.
- **`Serial::write_byte` routes through `arch_serial_putc`** on both arches and
  says what the real console path is. It is still dead code; the point is that
  there is nothing left in it to misread.

## Falsification

Harness fix, one live boot, one kernel, same test binary, same budget:

| | scmrun | command | elapsed | `M13RC` | PASS lines |
|---|---|---|---|---|---|
| mutant | pre-fix (`0da2f3d5…`) | `scmtest; echo M13RC=$?` | **0.6 s** | none | 1 |
| control | fixed (`3836d6a1…`) | `scmtest; echo "M13""RC=$?"` | 21.5 s | **0** | 33 |

Kernel fix — see the table in the commit and `mutation.md` beside this file.

Positive control `nosuchbinary_xyz42` was the first command of every boot in
every run above and reported `command not found`, rc 127, each time.

## aarch64

Not run from this box. `SCROLL_ROWS` is arch-neutral and aarch64 reaches the
same `scroll_vector`, so the same ceiling should exist there and the same
division should apply — but it is unverified here and the Mac lane's to confirm.
The harness fixes are host-side and arch-independent.
