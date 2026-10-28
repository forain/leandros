# lane/lapaudio3 (2026-10-27/28): PARTIAL, QEMU-verified, laptop not reachable

Branch lane/lapaudio3 (main merged in, NOT merged to main). Fixes from review-lapaudio2:
- M1 spk pins: only pins with connectivity != none that playback enabled; original PIN_CTL saved, mute clears OUT_EN, unmute restores; first "nothing plugged" reading only records state.
- M2 read never returns 0: -EAGAIN (O_NONBLOCK recorded at VFS_OPEN), blocking waits, 2 s stall -> one restart -> -EIO; count<4 -> -EINVAL.
- M3 poll_jacks moved out of the tick into the `[audio]` kthread (100 ms); tick does capture_update (MMIO) only (also L1).
- M4 display codec probed with `no_fallback` (CORB timeout fails the verb, never switches the link to immediate); display AFG put in D3 at shutdown.
- L2 stream started only from capture_ensure (read/poll claim path, re-checks READER under STATE lock); L3 DMA errors counted/logged, stuck LPIB 100 ms restarts; L4 FIFOS subtracted + acquire fence; L5 pin ctl 0 at capture_stop.
- Poll: VFS_POLL claims reader + starts capture, POLLIN at >= 1920 B, dynamic tag woken by tick_pump; non-holders get POLLOUT on the static tag. vfstag pipewire_poll rewritten.
- ports/pipewire/leandros-snd-source.c (Audio/Source, opens device only while streaming, exits if ENODEV), built in build-in-alpine.sh, started from 50-leandros.conf.

Evidence: aarch64 virtio vfstag PASS; LEANDROS_SND=hda-micro vfstag PASS, `sndtest record 3` 47.6k fps (rms 0: QEMU gives silence, so the tool prints FAIL as before), tone ok; x86_64 INTEL=1 build ok.
NOT done: laptop (172.16.149.179 and subnet scan unreachable, laptop off/elsewhere); x86_64 leandros-snd-source binary (docker daemon hung, aarch64 binary built); x86_64 QEMU boot not run; no real-mic/pin/pw-cli checks.

## Laptop verification 2026-10-28 (trial b0ecf7d2d75c, then back to good)
- x86_64 source node cross-built with `zig cc -target x86_64-linux-musl` against the staged libpipewire (docker daemon hung and would not restart); image built INTEL=1, 0 errors; x86_64 QEMU vfstag PASS.
- Laptop: `sndtest record 3` PASS (rms L 2939 / R 2957, -20.9 dBFS, 47967 fps, 0 overruns/dmaerr); `sndtest tone` ok; vfstag pipewire_poll PASS (drm_read SETCRTC errno 13 = compositor holds DRM master, unrelated); jack: headphone pin 1d, speaker pin 17, all unplugged/sane; display codec probed fine.
- Desktop login: pw-dump shows node `leandros-snd-source` (Audio/Source) next to `leandros_output` (Audio/Sink).
- Pin ctl: 0x16 and 0x1d read 0xc0 at boot (0x16 not a speaker pin per the jack line; likely BIOS default, not touched by us); speaker pin 0x17 not enabled.
- Laptop returned to good (7a0d1c759331), NOT promoted.
