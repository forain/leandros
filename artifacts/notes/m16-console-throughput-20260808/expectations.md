# Pre-committed expectations, written before the run

Kernel under test: target/final-x86_64/kernel md5 aaf1d14090a30ccb80bd32df3bd54327
(the merge HEAD 5b9a348 build; identical to the previous lane control kernel).
ONE boot, ONE kernel, FOUR probes. The variable is the HARNESS, not the kernel.

H1 (mine): the previously reported "300 printed lines arrive as 19" is caused by
scripts/scmrun.py breaking on a completion marker that appears in the TTY echo
of the command it just typed ("...; echo CONSOLEDONE" echoes the literal
CONSOLEDONE before the loop has printed anything). The x86_64 console is not
lossy; drivers/src/serial.rs::write_byte is dead code (never constructed) and
the real userspace console path is
  sys_write -> console_write_user -> serial_write_raw -> kernel::serial_write_byte
             -> arch_x86_64::putc,   which already checks LSR.THRE.

H0 (previous lane): x86_64 console writes have no flow control, so bytes are
lost at the UART regardless of harness.

## Probes and the numbers I require BEFORE running

A  marker = "CONSOLEDONE", literally present in the typed command. 240 s.
   H1 predicts a small number (previous lane got 19). REQUIRE: <= 60.
   H0 predicts the same small number.

B  NO marker at all; the reader burns the full 90 s window. This is the probe
   that cannot be truncated early.
   H1 REQUIRES exactly 300 / 300, CONSOLELINE0..CONSOLELINE299, no gaps.
   H0 predicts ~19.

C  marker built by the shell so it CANNOT appear in the echo
   ("M=DONE; ...; echo CONSOLE$M", marker "CONSOLEDONE"). 240 s.
   H1 REQUIRES exactly 300 / 300.
   H0 predicts ~19.

D  stress: 2000 lines, NO marker, 180 s.
   REQUIRE exactly 2000 / 2000. Any shortfall is genuine console loss (the
   TX_WEDGED drop latch firing against a draining consumer) and would mean the
   assigned fix is still needed on top of the harness fix.

Positive control: nosuchbinary_xyz42 is the FIRST command of the boot and must
report "command not found" / rc 127.

Decision rule: B and C at 300 with A small  => H1, harness defect, kernel innocent.
               B below 300                  => H0 survives, fix the console.

# ================= PART 2: what the measurement actually found =============

Written BEFORE the fix build.

H1 and H0 are BOTH refuted. Probe B settled it: with no marker at all and a
90 s window, CONSOLELINE16..CONSOLELINE65 arrived CONTIGUOUS, in order, with
not one byte missing -- but at one line per ~2 seconds. The console is not
lossy. It is slow.

Probe A reproduced the previous lane numbers exactly (16 of 300) and closed its
window in 0.9 s of a 240 s budget, which is the harness artifact: scmrun breaks
on a marker that the tty echoes back as part of the command it was sent.

Per-line, not per-byte, on one boot:
  P1  100 lines x 41 chars  157.9 s  =>  1.579 s/line,  26.0 B/s
  P2  220 lines x  1 char   400.6 s  =>  1.821 s/line,   1.1 B/s
24x the bytes for the same time per line. RIP sampling on all 4 vCPUs put
12/12 samples in memcpy. Framebuffer is 1920x1080 pitch 7680 = 8.29 MB, mapped
PRESENT|WRITABLE|NO_CACHE (PAT UC-) in arch/x86_64/src/lib.rs, and
Framebuffer::scroll_vector copies the whole surface on every newline.

## Pre-committed expectation for the fix

Fix = scroll SCROLL_ROWS = 8 text rows per scroll instead of 1. Same copy, one
eighth as often.

  REQUIRE P1 (100 lines x 41 chars) to drop from 157.9 s to <= 45 s.
  Ideal is 157.9/8 + shell cost ~= 22 s. I will call anything above 45 s a
  failed fix and say so.

  REQUIRE the delivered line count to stay exact: 100 of 100 long lines and,
  on the re-run of the no-marker probe, 300 of 300 CONSOLELINE. Nothing about
  this fix may make the console lossy.

## Mutation (kernel, the project bar)

  control  SCROLL_ROWS = 8   md5 recorded
  mutant   SCROLL_ROWS = 1   md5 recorded, P1 must collapse back to ~158 s
  restore  SCROLL_ROWS = 8   md5 must be BYTE-IDENTICAL to control

## Harness mutation (no kernel involved)

  m13_suite with the unechoable marker  -> windows run to the real M13RC
  m13_suite with the marker put back in the typed command -> windows collapse
  to ~1 s and the exit statuses shift by one row again.

## Suite payoff

  REQUIRE vfstests 36 subtests to arrive in vfstests OWN window, and
  wakepolltest / epolltest / timertest / f2fstest -- which reported no exit
  status at all -- to report one.
