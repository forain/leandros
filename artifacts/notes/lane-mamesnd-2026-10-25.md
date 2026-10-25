# lane/mamesnd (2026-10-25): MAME sound through PipeWire (openbugs #56)

FIXED. ../mame src/osd/leandros/leandros_sound.cpp (local commit on master, not pushed) now prefers PipeWire:
when $XDG_RUNTIME_DIR/pipewire-0 or /run/user/1000/pipewire-0 exists and /usr/bin/pw-cat is present it spawns
`pw-cat --playback --rate 44100 --channels 2 --format s16 --latency 40ms -` (XDG_RUNTIME_DIR passed) and writes the PCM
(endless WAV header first) into a non-blocking pipe (full pipe drops audio, never stalls the game). pw-cat death
(EPIPE) falls back to the direct IPC port when there is one. LEANDROS_MAME_AUDIO=direct forces the old path (A/B).

## Evidence (aarch64 QEMU, mamesession --vt --secs 30; audio = QEMU-captured device WAV, loud_seconds = seconds with rms>100)
- before (LEANDROS_MAME_AUDIO=direct): desktop window rms 0.0 / 0 loud s; VT2 (vttest chvt 2, desktop on VT1) rms 0.0 / 0 loud s.
- after: desktop window rms 470 / 7 loud s; VT2 rms 687 / 5 loud s. Speed 100%, single run, wl key all pass.
- mamesession.py gained gates mame-audio and (with --vt) mame-vt-audio, and MAME_ENV for env prefixes.
