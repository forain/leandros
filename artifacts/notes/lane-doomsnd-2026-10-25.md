# lane/doomsnd 2026-10-25 (L12 #57): DOOM music quiet, SFX skips - FIXED

## Root cause
- Music was implemented (MUS->MIDI, TinySoundFont, FluidR3_GM.sf2) but rendered about -30 dBFS: aarch64 `-nosfx` music_peak 2151/32767, loud 6/12.6 s (48 %).
- SFX skips: the pipe lead of 2048 frames (46 ms) ran dry on a single 47 ms frame (1 underrun in 99 s). The startup stall (400-1500 ms) is the one-time sf2 load in GM_Init.
- Channel stealing at 8 channels is vanilla behaviour, not a bug.

## Fix
- `scripts/vendor/doomgeneric/leandros_sdl.c`: music bus (x6 gain + peak limiter, ceiling 0.70, ~0.3 s release); PIPE_LEAD_FRAMES 2048 -> 3072; `music_peak` in the audio stats line.
- `scripts/doom-music-check.sh` (new guard: music_peak > 1000 and >= 80 % loud seconds), `scripts/doom-audio-gap-check.sh` (KEEP_WAV, copies doom.log/audio.json to /tmp), `.claude/skills/run-leandros/doomsession.py` (`--doom-args`).

## Evidence
- aarch64 doom-music-check: PASS music_peak [22938, 12903], loud 13/12.3 s (106 %), rms 6625. Pre-fix FAIL 48 %.
- aarch64 doom-audio-gap-check: 1 underrun 2.7 ms in 98.85 s (pre-fix 1 underrun 10.7-21 ms; previous run of fixed build 0).
- Laptop (x86_64, real HDA, PipeWire sink): music_peak 12194-22938 over 70 s, dry=0 in 6 of 7 stat lines (one dry=2), PipeWire leandros-snd-sink underruns=0 at every report.

## Caveats
- `../doomgeneric` is a shared sibling: concurrent lanes overwrite its leandros_sdl.c, and a build in that window ships someone else's source. First music run FAILED with music_peak=[] for that reason; verify `strings target/doom/doom-<arch> | grep music_peak` after building.
- Old-vs-new laptop comparison was not completed (old build has no music_peak stat); the aarch64 pre/post numbers are the A/B.
