# lane/kvmaudio 2026-10-24 (L11.4 + L11.10)

## L11.4 x86 DOOM clipping: FIXED (13e208ea)
Root cause: userland/libc builds for Rust's soft-float `x86_64-unknown-none`, so its exported C
`pow/exp/log/log10/tan/sqrt` read doubles from rdi/rsi and returned rax, while clang-built C callers
(DOOM's TSF synth) pass xmm0/xmm1. Every libm call returned garbage -> random synth gains -> clipping,
x86_64 only (aarch64-unknown-none is hard-float). Fix: asm trampolines (xmm -> GPR -> xmm).
Guard: scripts/check-c-float-abi.py, run by build-all.sh on doom-x86_64; FAIL on the old binary
(6/6 symbols soft-float), OK on the fix.
Evidence (doomsession.py x86_64, Mac TCG, WAV): 0 clipped samples of 13.7 M, peak 23068 (was 32 % of
the loudest 10 s at full scale); desktop DOOM RMS 2246, 34/37 s loud. Regress: aarch64 PASS, x86_64 PASS (Mac).

## L11.4 40 ms first-play gap (PipeWire pw-play, box x86_64 KVM, main 5b111eef, first run)
10 s 440 Hz tone: 0 clipped, peak 8191, RMS 5790 (unity), 2 holes of 2.7 ms (one 128-frame quantum
at 48 kHz) at +150 ms and +622 ms after first sound, 2 discontinuities, 0 snd stalls, 0 pw gaps.
No 40 ms gap. A re-run on private image copies (tree lane/kvmvenus3 WIP 5d015271) captured an EMPTY wav
(tone_windows 0) - unexplained, likely that WIP tree / run setup; re-check on a main build.

## L11.10 Firefox audio on x86 KVM: NOT VERIFIED (handoff)
First run was invalidated (another lane rebuilt the box images mid-run, desktop never came up).
Re-run was in flight at handoff; see $SP/handoff/kvmaudio.md.
