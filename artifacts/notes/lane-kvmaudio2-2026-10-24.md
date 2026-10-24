# Lane kvmaudio2 (L11.10) 2026-10-24: x86_64 KVM audio verification on main 88ffeee9

Status: VERIFIED (no code change). Box x86_64/KVM, private image copies, WAV audiodev, no screenshots.

## Firefox media (L11.10): PASS
- file:///tmp/kv.html + tone880.webm: `ATEST ct=` advanced 0.87 -> 12.00 (paused=false, readyState 4), one transient `error 3` log at ~9 s, playback continued to the end.
- WAV: 880 Hz, active 12.01 s, RMS per second 1013-1028 (expected ~1020), peak 1483, 0 clipped samples. gaps>=2 ms: 3 (15.5 ms total, all within the first 0.2 s of sound).

## PipeWire 440 Hz tone on clean main
- pw-play tone-440-10s.wav: 10.05 s tone, RMS 5790 (min 5633, max 5790), peak 8191, 0 clipped, 0 holes >= 50 ms, one 2.6 ms gap at start, no 40 ms gap; 0 pf/segv/panic/wdog/snd_stalls/pw_gaps. The earlier empty WAV was the WIP kvmvenus3 tree, not main.
- Delay from command to first sound was not isolated (capture timeline and session log use different clocks); first_sound=38.5 s from capture start, which includes boot.

## DOOM on KVM with libc float fix (88ffeee9)
- doomsession.py x86_64 --no-vt: 0 clipped of 3.2 M frames, peak 20072 (was full scale), desktop-doom RMS 2491, 12/12 loud seconds, silence after quit.
- Observation: 346 gaps >= 2 ms (7.0 s total) in the DOOM capture, mostly ~21 ms: DOOM underruns on KVM (not clipping). Not investigated.
