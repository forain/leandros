# Lane doomgap (L11.13) 2026-10-24: DOOM underruns on x86_64 KVM

Status: FIXED. Box x86_64/KVM, private image copies, WAV audiodev, no screenshots.

## Root cause
DOOM hands its 44.1 kHz PCM to pw-cat over a pipe with only a WAV header to give
the rate. pw-cat drained that pipe at ~48.1 k frames per guest second (fill-paced
DOOM stats: 467189 and 482304 frames per 10 s), i.e. at its 48 kHz default, while
DOOM produced 44.1 k/s by its clock. The pipe ran dry: whole PipeWire quanta of
silence, 21.3 ms every 85.3 ms (strictly periodic), and the sound played ~9 % fast.
Guest clock is fine on KVM (clockdrift.py 20 s: ratio 1.0003). Not float ABI: the
same 285 holes with main 70b79902 (floatabi merged, hard-float libc).

## Fix (lane/doomgap)
- leandros_sdl.c: pw-cat gets `--rate 44100 --channels 2 --format s16`; the pump
  keeps the pipe topped up to a 2048-frame lead by FIONREAD (consumer-paced, like
  an SDL callback), clock pacing only as fallback (`LEANDROS_DOOM_AUDIO_PACE=clock`
  for A/B); `LEANDROS_DOOM_AUDIO_STATS=1` prints a pacing line every 10 s.
- vfs: FIONREAD on a pipe's write end returns the queued bytes (Linux semantics).
- scripts/wav-gaps.py: counts holes that cut and resume a signal (underruns), not
  quiet passages. scripts/doom-audio-gap-check.sh: scripted guard (FAIL > N).
- doomsession.py: `--doom-env`, uppercase/`=` typing.

## Evidence (wav-gaps.py, x86_64 KVM)
| build | pacing | underruns | total |
|---|---|---|---|
| 88ffeee9 (before) | clock | 286 | 6077 ms |
| lane + floatabi, no --rate | clock | 285 | 6037 ms |
| lane + floatabi, no --rate | fill | 2 | 24 ms (but plays 9 % fast) |
| lane final (--rate) | fill | 0 | 0 ms, 44.2 k frames/s consumed |
| lane final (--rate) | clock | 0 | 0 ms |
Guard: `scripts/doom-audio-gap-check.sh x86_64 10` rc=0 on the fix; the old binary has 286 > 10.

## New finding (open)
The x86_64 static libc DOOM links misreads variadic arguments passed on the stack:
`snprintf` with more than 3 variadic args after (buf,size,fmt) printed garbage from
the 4th on (`dropped=140737488342432B max_dt=18446744073709551615`). Repro: any
snprintf("%u %u %u %u %u", 1,2,3,4,5) in doom-x86_64. Worked around in the stats line.

## Not done
aarch64 DOOM on HVF not re-measured for gaps (aarch64 regress run only).
