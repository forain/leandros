# Lane pipewire (2026-10-02): FIXED (audio + ScreenCast)

Branch `lane/pipewire` (base origin/main 68a0cb7). Worktree ~/code/leandros-pipewire.

## Phase 1 inventory (measured from the tree, 2026-10-02)

What the kernel exposes for audio today:
- `drivers/src/snd.rs`: a virtio-sound (PCI 1af4:1059) driver, one TX stream, S16, 44.1/48 kHz.
- `servers/pipewire` is an IN-KERNEL audio server (not PipeWire at all, just the name). It
  registers `/dev/pipewire` (write = raw interleaved S16 PCM with backpressure into a 64 KiB
  spool, ioctl 0x101 = SET_PARAMS {u32 rate, u8 channels}), force-binds the unix path
  `/run/pipewire/pipewire-0` and publishes its IPC port in auxv tag 258 (AT_LEANDROS_AUDIO_PORT,
  used by aplay and MAME's leandros_sound driver). A 100 Hz tick pump pads silence so QEMU never
  sees an empty queue, and a 250 ms stall detector restarts the stream.
- There is no `/dev/snd/*`. No ALSA ABI (no hw_params/sw_params ioctls, no mmap status page).
- QEMU: `-device virtio-sound-pci,streams=1` + coreaudio (Mac) / pa|pipewire|alsa (Linux),
  `LEANDROS_AUDIO_WAV=` gives a wav capture (driver.py).
- Kernel prerequisites PipeWire needs and already has: memfd_create, MAP_SHARED of memfd across
  processes (wl_shm works), eventfd2, timerfd (ABSTIME fixed earlier), signalfd4, SCM_RIGHTS.
  FIFOs are NOT real (mknod S_IFIFO is a fake dirent), so module-pipe-tunnel is out.

What COSMIC needs (epoch-1.3 sources in m6-session-bins/src):
- NOT libpulse. cosmic-applet-audio, cosmic-osd and cosmic-settings' Sound page are all varlink
  clients (`cosmic-settings-audio-client`, socket `$XDG_RUNTIME_DIR/com.system76.CosmicSettings`)
  of cosmic-settings-daemon. Only the daemon talks to PipeWire (pipewire-rs 0.10, native
  libpipewire-0.3, 63 pw_* imports). It reads `default.audio.sink` from the "default" metadata
  (needs a session manager = WirePlumber) and sets volume with node Props channelVolumes /
  device Route. It retries the PipeWire connection in a loop, so start order is not critical.
- The shipped cosmic-settings-daemon links the inert stub (pipewire-gap, headers = 1.2.7).
  Alpine 3.21 ships pipewire 1.2.7: the same ABI. Swapping the stub .so for the real one should
  need no rebuild. Same for xdg-desktop-portal-cosmic (ScreenCast).
- cosmic-applet-audio is not built/shipped today; cosmic-osd and cosmic-settings are (both
  contain the CosmicSettings.Audio client).

## Plan
1. ports/pipewire: Alpine 3.21 prebuilt pipewire 1.2.7 + wireplumber + their closure, ELF-fixed
   like ports/portal (container of target arch). Real libpipewire replaces the stub.
2. Sink: no ALSA. A small C PipeWire client `leandros-snd-sink` (built in the same container
   against pipewire-dev): a pw_stream with media.class=Audio/Sink that writes the mixed S16LE
   stream to `/dev/pipewire`. Volume/mute are applied by the stream's audioconvert (node Props).
   (Implemented differently from this first plan: the sink is the graph DRIVER, timed like
   spa-alsa's tsched: a timer per quantum, new ioctl 0x102 GET_DELAY = spool + device ring,
   spa_dll steering the rate to keep 4096 frames queued. Blocking writes would have kept the
   64 KiB spool full and spun a CPU in push_spool_blocking; a Dummy-Driver-paced writer would
   have let the 100 Hz silence top-up interleave zeros.)
3. Config: pipewire.conf trimmed (no rt/rtkit hard requirement, no ALSA/v4l2/bluez/libcamera
   monitors in WirePlumber). Start pipewire + wireplumber + sink from the session launcher.
4. cosmic-applet-audio: build if cheap (same recipe as the other applets).
5. ScreenCast second: enable it in cosmic.portal, small portal client counting frames.

## Progress log
- Mac docker daemon hung (docker ps never returns) and the Mac disk hit 0 bytes free
  mid-build (906/926 GiB used, not by this lane: ~/code/leandros 108 GB, coreutils 86 GB,
  brush 68 GB, mame 62 GB, Docker VM 76 GB). All builds moved to the linux desktop:
  ports/pipewire stages aarch64 there WITHOUT emulation (apk --root --arch aarch64
  --no-scripts + clang cross-compile; build.sh LEANDROS_PW_PLATFORM=linux/amd64).
- First x86_64 KVM boot: pipewire + wireplumber came up in the session (profile main,
  no hardware monitors). leandros-snd-sink failed: SET_PARAMS -> ENOTTY. Root cause:
  kernel sys_ioctl only forwards FIONREAD/FB/DRM/evdev command numbers to the VFS;
  /dev/pipewire's private 0x101/0x102 fell through to the TTY server. Fixed by routing
  every ioctl on the audio server's device port to it (kernel/src/syscall.rs).
- After the ioctl fix (1feffbc), audio-session-test.py, greeter login as leandro:
  | | x86_64 KVM virgl (desktop) | aarch64 HVF virgl (Mac) |
  |---|---|---|
  | wpctl status clients | WirePlumber, leandros-snd-sink, cosmic-settings-daemon, xdg-desktop-portal | same |
  | default sink | * Virtio Sound Output, vol 1.00 | same |
  | tone1 (vol 1.00) in host wav | 10.05 s, RMS 5773 (ideal 5792), 0 holes, 3 sine-model discontinuities | 10.0 s, RMS 5788, 0 holes, 1 event at t=1.04 s (two adjacent hits) |
  | tone2 after `wpctl set-volume 0.30` | (analysis threshold bug, re-run) | 9.95 s, RMS 155.9 = 0.0269 x tone1 = 0.30^3 (cubic volume), 0 discontinuities |
  | sink DLL | corr 1.0011-1.0013, delay 2304-4352 frames (target 4096), underruns 0 | corr 1.0011-1.0013, delay 2432-4480, underruns 0 |
  | PF/SEGV/panic/WDOG/[SND] stalls/[PW] gaps | 0/0/0/0/0/0 | 0/0/0/0/0/0 |
- Settings -> Sound (aarch64 HVF, cosmic-settings via the launcher): Output volume slider,
  Output device "Virtio Sound Output", Balance, Amplification; the panel shows the Sound
  applet icon. Volume read 30 (the earlier wpctl 0.30, restored by WirePlumber). One click
  on the slider -> UI 80, `wpctl get-volume` 0.80; tone in the host wav: 10.05 s, RMS 2963
  = 0.5119 x the full-volume RMS 5788 (0.80^3 = 0.512, PipeWire's cubic volume).
- Merged origin/main f0c8994 (privcheck): /dev/pipewire stays 0666 under its policy
  ("everything else"); no /dev/snd nodes were added by this lane.

## Final state
Branch `lane/pipewire` @ 2768539 (merged origin/main f0c8994), pushed.

Changed: `ports/pipewire/` (new: build.sh, build-in-alpine.sh, leandros-snd-sink.c,
pw-screencast-probe.c, data/), `kernel/src/syscall.rs` (ioctl routing for /dev/pipewire),
`servers/pipewire/src/lib.rs` (GET_DELAY), `scripts/mkfs-f2fs-populated.py` (overlay
ports/pipewire first; stub skipped when the real lib is staged), `scripts/build-all.sh`
(build_pipewire, stamp-skipped, LEANDROS_SKIP_PIPEWIRE/LEANDROS_PIPEWIRE_REBUILD),
`artifacts/m6-session-data/start-cosmic-leandros` (starts pipewire once per runtime dir;
pipewire starts wireplumber + the sink via context.exec), `.claude/skills/run-leandros/
audio-session-test.py` (new). Shared files: syscall.rs, mkfs, build-all, the session launcher.

What ships: Alpine 3.21 pipewire 1.2.7 + wireplumber 0.5.7 (+ pw-cli/pw-cat/pw-play/wpctl,
lua, libsndfile closure; no ALSA/v4l2/bluez/libcamera), the real libpipewire replacing the stub
(cosmic-settings-daemon and xdg-desktop-portal-cosmic unchanged, same 1.2.7 ABI),
cosmic-applet-audio (unmodified, m6 recipe; panel Sound icon), a ScreenCast-enabled
cosmic.portal (overrides ports/portal's copy), /usr/share/sounds/leandros/tone-440-10s.wav.
Port outputs preserved in ~/code/leandros-artifacts/pipewire-out/ (copy to ports/pipewire/out
and touch out/<arch>/.stamp; the Mac docker daemon was hung all session, the desktop podman
can stage both arches, aarch64 with LEANDROS_PW_PLATFORM=linux/amd64).

## Evidence after the merge (dad7a30/2768539)
| | x86_64 KVM virgl (desktop) | aarch64 HVF virgl/gles31 (Mac) |
|---|---|---|
| sink in wpctl / Settings | Virtio Sound Output, default | same; Settings -> Sound lists it (screenshot) |
| tone at vol 1.00 | 10.10 s, RMS 5773, one ~40 ms zero gap ~1 s after the first stream start + 4 blips | 10.05 s, RMS 5788, min 5486, one blip at 0.18 s, no gap |
| tone at vol 0.30 | 10.05 s, RMS 156.3 (0.027 = 0.3^3), 0 discontinuities | 10.05 s, RMS 155.9, 0 discontinuities |
| Settings slider click -> 80 | (not repeated) | wpctl 0.80, wav RMS ratio 0.5119 (0.8^3 = 0.512) |
| ScreenCast probe (picker: select output, Share) | 373 frames / 8 s, 46.8 fps, RGBA 1920x1080 memfd | 367 frames, 46.0 fps, RGBA 1280x800 |
| PF/SEGV/panic/WDOG/SND stall/PW gap (session) | 0/0/0/0/0/0 | 0/0/0/0/0/0 |
| polltest / drmsmoke (no GPU) | 18/18, failed=0 | 18/18, failed=0 |

## Open
- First-playback glitch: a ~40 ms silent gap ~1 s into the FIRST pw-play on x86_64 (both
  x86 runs; aarch64 shows only a sub-ms blip). Sink reports underruns=0 and steady playback
  (second tone) is clean on both arches, so it is most likely a client-side xrun (pw-play's
  data thread has no RT priority; RTKit is a stub) — not measured further.
- The serial-console MAME/aplay path still writes /dev/pipewire directly; while a PipeWire
  session runs, both would interleave into one spool. The audio-glitch-test (MAME) was not
  re-run in this lane (it does not involve PipeWire; the kernel change is one new ioctl).
- /dev/pipewire stays 0666 (privcheck policy "everything else"); no /dev/snd was added.
- Mac disk hit 0 bytes free mid-lane (other trees: leandros 108 GB, coreutils 86 GB,
  brush 68 GB, mame 62 GB, Docker VM 76 GB), and Docker Desktop on the Mac never answered.
- pipewire warns `pthread_setname error` (no /proc/self/task/<tid>/comm write) and logs
  session-bus failures at startup (bus not up yet; module-portal/rtkit are nofail).
